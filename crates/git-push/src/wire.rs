//! The messages of one `git-receive-pack` exchange.
use crate::pkt::{self, Reader};
use gix::ObjectId;

/// Why a server's message could not be used.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("the server's answer ended early")]
    Truncated,
    #[error("the server's answer is malformed: {0}")]
    Malformed(&'static str),
    #[error("the server's answer is not for git-receive-pack")]
    Service,
    #[error("the repository uses {remote} object ids, but the local one uses {local}")]
    ObjectFormat { remote: String, local: String },
    #[error("the server does not report the outcome of a push (report-status)")]
    NoReportStatus,
}

/// The references a server holds, as it advertises them before a push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertisement {
    /// Each reference with the object it points at. References whose names
    /// are not UTF-8 are left out: nothing here can name them.
    pub refs: Vec<(String, ObjectId)>,
    /// Further objects the server says it has (`.have` lines).
    pub haves: Vec<ObjectId>,
    pub capabilities: Vec<String>,
}
impl Advertisement {
    /// The object `name` points at, if the server has that reference.
    pub fn get(&self, name: &str) -> Option<ObjectId> {
        self.refs
            .iter()
            .find(|(reference, _)| reference == name)
            .map(|(_, id)| *id)
    }
    fn capability(&self, name: &str) -> bool {
        self.capabilities.iter().any(|c| c == name)
    }
    /// Every object the server says it has.
    pub fn known(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.refs
            .iter()
            .map(|(_, id)| *id)
            .chain(self.haves.iter().copied())
            .filter(|id| !id.is_null())
    }
}

/// Parse a smart HTTP advertisement (`info/refs?service=git-receive-pack`)
/// for a repository whose object ids are `hash`.
pub fn parse_advertisement(body: &[u8], hash: gix::hash::Kind) -> Result<Advertisement, WireError> {
    let mut reader = Reader::new(body);
    let first = reader.next()?.ok_or(WireError::Service)?;
    if pkt::line(first) != b"# service=git-receive-pack" {
        return Err(WireError::Service);
    }
    if reader.next()?.is_some() {
        return Err(WireError::Malformed(
            "the service line is not followed by a flush",
        ));
    }
    let mut advertised = Advertisement {
        refs: vec![],
        haves: vec![],
        capabilities: vec![],
    };
    let mut first = true;
    let mut empty = false;
    while let Some(payload) = reader.next()? {
        let mut line = pkt::line(payload);
        if first {
            let (refs, capabilities) = split_once(line, 0).ok_or(WireError::Malformed(
                "the first reference has no capabilities",
            ))?;
            line = refs;
            advertised.capabilities = std::str::from_utf8(capabilities)
                .map_err(|_| WireError::Malformed("capabilities are not UTF-8"))?
                .split(' ')
                .filter(|c| !c.is_empty())
                .map(str::to_owned)
                .collect();
        } else if empty {
            return Err(WireError::Malformed(
                "an empty repository advertised references",
            ));
        }
        if line.starts_with(b"shallow ") {
            // A shallow server repository: it is missing history, which a
            // fast-forward check below cannot see past.
            return Err(WireError::Malformed("the server repository is shallow"));
        }
        let (id, name) =
            split_once(line, b' ').ok_or(WireError::Malformed("a reference line has no name"))?;
        let id = object_id(id, hash)?;
        if first && name == b"capabilities^{}" {
            if !id.is_null() {
                return Err(WireError::Malformed(
                    "the capabilities line names an object",
                ));
            }
            empty = true;
        } else if name == b".have" {
            advertised.haves.push(id);
        } else if let Ok(name) = std::str::from_utf8(name) {
            advertised.refs.push((name.to_owned(), id));
        }
        first = false;
    }
    if !reader.is_empty() {
        return Err(WireError::Malformed("data follows the final flush"));
    }
    if first {
        return Err(WireError::Malformed(
            "no references or capabilities were advertised",
        ));
    }
    let local = hash.to_string();
    let remote = advertised
        .capabilities
        .iter()
        .find_map(|c| c.strip_prefix("object-format="))
        .unwrap_or("sha1");
    if remote != local {
        return Err(WireError::ObjectFormat {
            remote: clean(remote.as_bytes()),
            local,
        });
    }
    Ok(advertised)
}

fn object_id(hex: &[u8], hash: gix::hash::Kind) -> Result<ObjectId, WireError> {
    if hex.len() != hash.len_in_hex() {
        return Err(WireError::Malformed("an object id has the wrong length"));
    }
    ObjectId::from_hex(hex).map_err(|_| WireError::Malformed("an object id is not hexadecimal"))
}

fn split_once(line: &[u8], at: u8) -> Option<(&[u8], &[u8])> {
    let index = line.iter().position(|&b| b == at)?;
    Some((&line[..index], &line[index + 1..]))
}

/// One reference update: `old` is the null id to create the reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub old: ObjectId,
    pub new: ObjectId,
    pub name: String,
}
impl Update {
    /// The `git-receive-pack` request: this command, then `pack`. The
    /// server must report the outcome, so `report-status` is required.
    pub fn request(&self, advertised: &Advertisement, pack: &[u8]) -> Result<Vec<u8>, WireError> {
        if !advertised.capability("report-status") {
            return Err(WireError::NoReportStatus);
        }
        let mut capabilities = vec!["report-status".to_owned()];
        if let Some(format) = advertised
            .capabilities
            .iter()
            .find(|c| c.starts_with("object-format="))
        {
            capabilities.push(format.clone());
        }
        if advertised
            .capabilities
            .iter()
            .any(|c| c.starts_with("agent="))
        {
            capabilities.push(concat!("agent=git/agent206-", env!("CARGO_PKG_VERSION")).into());
        }
        let command = format!(
            "{} {} {}\0{}\n",
            self.old,
            self.new,
            self.name,
            capabilities.join(" ")
        );
        let mut out = Vec::with_capacity(command.len() + 8 + pack.len());
        pkt::data(&mut out, command.as_bytes());
        pkt::flush(&mut out);
        out.extend_from_slice(pack);
        Ok(out)
    }
}

/// What the server reported for a push of one reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Whether the server could unpack the objects, with its reason if not.
    pub unpack: Result<(), String>,
    /// The outcome for the pushed reference, with the server's reason if it
    /// refused it; `None` if the server did not mention it.
    pub reference: Option<Result<(), String>>,
}

/// Parse a `report-status` answer for the update of `name`.
pub fn parse_report(body: &[u8], name: &str) -> Result<Report, WireError> {
    let mut reader = Reader::new(body);
    let first = reader
        .next()?
        .ok_or(WireError::Malformed("the report is empty"))?;
    let unpack = pkt::line(first)
        .strip_prefix(b"unpack ")
        .ok_or(WireError::Malformed(
            "the report does not start with the unpack status",
        ))?;
    let unpack = if unpack == b"ok" {
        Ok(())
    } else {
        Err(clean(unpack))
    };
    let mut reference = None;
    while let Some(payload) = reader.next()? {
        let line = pkt::line(payload);
        let (status, rest) =
            split_once(line, b' ').ok_or(WireError::Malformed("a status line has no reference"))?;
        let (reported, reason) = match split_once(rest, b' ') {
            Some((reported, reason)) => (reported, Some(reason)),
            None => (rest, None),
        };
        let outcome = match (status, reason) {
            (b"ok", None) => Ok(()),
            (b"ng", Some(reason)) => Err(clean(reason)),
            (b"ng", None) => Err(String::new()),
            _ => return Err(WireError::Malformed("a status line is neither ok nor ng")),
        };
        if reported == name.as_bytes() {
            reference = Some(outcome);
        }
    }
    if !reader.is_empty() {
        return Err(WireError::Malformed("data follows the report"));
    }
    Ok(Report { unpack, reference })
}

/// The most characters of server text passed on.
const MAX_REASON: usize = 200;

/// Server text as one printable line of at most [`MAX_REASON`] characters.
fn clean(text: &[u8]) -> String {
    let text = String::from_utf8_lossy(text);
    let mut cleaned: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(MAX_REASON)
        .collect();
    if text.chars().count() > MAX_REASON {
        cleaned.push('…');
    }
    cleaned.trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::hash::Kind;

    fn id() -> ObjectId {
        let hex: String = (0..2)
            .map(|_| uuid::Uuid::new_v4().simple().to_string())
            .collect();
        ObjectId::from_hex(&hex.as_bytes()[..40]).unwrap()
    }
    fn body(lines: &[Option<&[u8]>]) -> Vec<u8> {
        let mut out = vec![];
        for line in lines {
            match line {
                Some(line) => pkt::data(&mut out, line),
                None => pkt::flush(&mut out),
            }
        }
        out
    }
    fn service() -> Option<&'static [u8]> {
        Some(b"# service=git-receive-pack\n")
    }

    #[test]
    fn advertisements_list_references_haves_and_capabilities() {
        let (main, topic, have) = (id(), id(), id());
        let first = format!("{main} refs/heads/main\0report-status delete-refs agent=x\n");
        let second = format!("{topic} refs/heads/topic\n");
        let third = format!("{have} .have\n");
        let advertised = parse_advertisement(
            &body(&[
                service(),
                None,
                Some(first.as_bytes()),
                Some(second.as_bytes()),
                Some(third.as_bytes()),
                None,
            ]),
            Kind::Sha1,
        )
        .unwrap();
        assert_eq!(advertised.get("refs/heads/main"), Some(main));
        assert_eq!(advertised.get("refs/heads/topic"), Some(topic));
        assert_eq!(advertised.get("refs/heads/other"), None);
        assert_eq!(advertised.haves, [have]);
        assert_eq!(advertised.known().collect::<Vec<_>>(), [main, topic, have]);
        assert_eq!(
            advertised.capabilities,
            ["report-status", "delete-refs", "agent=x"]
        );

        let empty = format!(
            "{} capabilities^{{}}\0report-status\n",
            ObjectId::null(Kind::Sha1)
        );
        let advertised = parse_advertisement(
            &body(&[service(), None, Some(empty.as_bytes()), None]),
            Kind::Sha1,
        )
        .unwrap();
        assert!(advertised.refs.is_empty() && advertised.known().next().is_none());
    }

    #[test]
    fn malformed_or_foreign_advertisements_are_refused() {
        let main = format!("{} refs/heads/main\0report-status\n", id());
        let more = format!("{} refs/heads/more\n", id());
        let empty = format!(
            "{} capabilities^{{}}\0report-status\n",
            ObjectId::null(Kind::Sha1)
        );
        let named_empty = format!("{} capabilities^{{}}\0report-status\n", id());
        let short = format!(
            "{} refs/heads/main\0report-status\n",
            &id().to_string()[..39]
        );
        let sha256 = format!(
            "{} refs/heads/main\0report-status object-format=sha256\n",
            id()
        );
        let shallow = format!("shallow {}\n", id());
        let cases: Vec<Vec<Option<&[u8]>>> = vec![
            vec![
                Some(b"# service=git-upload-pack\n"),
                None,
                Some(main.as_bytes()),
                None,
            ],
            vec![service(), Some(main.as_bytes()), None],
            vec![service(), None, Some(more.as_bytes()), None],
            vec![service(), None, Some(main.as_bytes())],
            vec![service(), None, None],
            vec![
                service(),
                None,
                Some(empty.as_bytes()),
                Some(more.as_bytes()),
                None,
            ],
            vec![service(), None, Some(named_empty.as_bytes()), None],
            vec![service(), None, Some(short.as_bytes()), None],
            vec![
                service(),
                None,
                Some(main.as_bytes()),
                Some(shallow.as_bytes()),
                None,
            ],
            vec![service(), None, Some(sha256.as_bytes()), None],
        ];
        for case in cases {
            assert!(
                parse_advertisement(&body(&case), Kind::Sha1).is_err(),
                "{case:?}"
            );
        }
        let mut trailing = body(&[service(), None, Some(main.as_bytes()), None]);
        trailing.extend_from_slice(b"0000");
        assert!(parse_advertisement(&trailing, Kind::Sha1).is_err());
    }

    #[test]
    fn requests_carry_the_command_and_only_capabilities_the_server_offers() {
        let update = Update {
            old: ObjectId::null(Kind::Sha1),
            new: id(),
            name: "refs/heads/topic".into(),
        };
        let pack = uuid::Uuid::new_v4().as_bytes().to_vec();
        let mut advertised = Advertisement {
            refs: vec![],
            haves: vec![],
            capabilities: vec!["report-status".into(), "side-band-64k".into()],
        };
        let request = update.request(&advertised, &pack).unwrap();
        let mut reader = Reader::new(&request);
        let command = format!(
            "{} {} {}\0report-status\n",
            update.old, update.new, update.name
        );
        assert_eq!(reader.next().unwrap(), Some(command.as_bytes()));
        assert_eq!(reader.next().unwrap(), None);
        assert!(request.ends_with(&pack));

        advertised.capabilities = vec!["delete-refs".into()];
        assert!(matches!(
            update.request(&advertised, &pack),
            Err(WireError::NoReportStatus)
        ));
    }

    #[test]
    fn reports_give_the_unpack_and_reference_outcome_with_clean_reasons() {
        let name = "refs/heads/topic";
        let ok = format!("ok {name}\n");
        let report = parse_report(
            &body(&[Some(b"unpack ok\n"), Some(ok.as_bytes()), None]),
            name,
        )
        .unwrap();
        assert_eq!(report.unpack, Ok(()));
        assert_eq!(report.reference, Some(Ok(())));

        let refused = format!("ng {name} protected\x1b[31m branch\n");
        let report = parse_report(
            &body(&[
                Some(b"unpack index-pack failed\n"),
                Some(refused.as_bytes()),
                None,
            ]),
            name,
        )
        .unwrap();
        assert_eq!(report.unpack, Err("index-pack failed".into()));
        assert_eq!(report.reference, Some(Err("protected [31m branch".into())));

        let long = format!("ng {name} {}\n", "x".repeat(MAX_REASON * 2));
        let report = parse_report(
            &body(&[Some(b"unpack ok\n"), Some(long.as_bytes()), None]),
            name,
        )
        .unwrap();
        let Some(Err(reason)) = report.reference else {
            panic!("{report:?}")
        };
        assert_eq!(reason.chars().count(), MAX_REASON + 1);

        let other = "ok refs/heads/other\n";
        let report = parse_report(
            &body(&[Some(b"unpack ok\n"), Some(other.as_bytes()), None]),
            name,
        )
        .unwrap();
        assert_eq!(report.reference, None);

        for bad in [
            body(&[None]),
            body(&[Some(b"ok refs/heads/topic\n"), None]),
            body(&[
                Some(b"unpack ok\n"),
                Some(b"maybe refs/heads/topic\n"),
                None,
            ]),
            body(&[Some(b"unpack ok\n")]),
        ] {
            assert!(parse_report(&bad, name).is_err());
        }
    }
}
