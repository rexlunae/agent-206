//! The tools against an in-memory workspace that behaves like the gateway's
//! host calls, and the queue and changelog transformations directly.
use crate::{changelog, error::QueueError, host::Host, queue, tools::*};
use a206_extension_sdk::{Args, GuestError, HostErrorCode, HostFailure, Value};
use std::collections::{BTreeMap, BTreeSet};

/// A unique word, so no assertion can pass on text fixed in the guest.
fn unique(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{prefix}{}x{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[derive(Default)]
struct Fake {
    files: BTreeMap<String, String>,
    directories: BTreeSet<String>,
    edits: usize,
}
fn fail(code: HostErrorCode) -> HostFailure {
    HostFailure {
        code,
        message: unique("host says "),
    }
}
impl Fake {
    fn file(mut self, path: &str, text: &str) -> Self {
        self.files.insert(path.into(), text.into());
        self
    }
    fn text(&self, path: &str) -> &str {
        &self.files[path]
    }
}
impl Host for Fake {
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostFailure> {
        self.files
            .get(path)
            .map(|text| text.clone().into_bytes())
            .ok_or_else(|| fail(HostErrorCode::NotFound))
    }
    fn create_directory(&mut self, path: &str) -> Result<(), HostFailure> {
        self.directories.insert(path.into());
        Ok(())
    }
    fn write_file(
        &mut self,
        path: &str,
        contents: &[u8],
        overwrite: bool,
    ) -> Result<(), HostFailure> {
        if let Some((parent, _)) = path.rsplit_once('/')
            && !self.directories.contains(parent)
        {
            return Err(fail(HostErrorCode::NotFound));
        }
        if self.files.contains_key(path) && !overwrite {
            return Err(fail(HostErrorCode::Exists));
        }
        self.files
            .insert(path.into(), String::from_utf8(contents.into()).unwrap());
        Ok(())
    }
    fn edit_file(&mut self, path: &str, old: &str, new: &str) -> Result<(), HostFailure> {
        let text = self
            .files
            .get(path)
            .ok_or_else(|| fail(HostErrorCode::NotFound))?;
        if old.is_empty() || text.matches(old).count() != 1 {
            return Err(fail(HostErrorCode::Conflict));
        }
        let edited = text.replacen(old, new, 1);
        self.files.insert(path.into(), edited);
        self.edits += 1;
        Ok(())
    }
}

fn args(pairs: &[(&str, &str)]) -> Args {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), Value::Str(v.to_string())))
        .collect()
}
fn with_number(mut args: Args, number: i64) -> Args {
    args.insert("number".into(), Value::Int(number));
    args
}
fn code(error: QueueError) -> String {
    GuestError::from(error).code
}

struct Item {
    id: String,
    title: String,
    tasks: [String; 2],
    proofs: [String; 2],
}
fn item(id: &str) -> Item {
    Item {
        id: id.into(),
        title: unique("title "),
        tasks: [unique("task "), unique("task ")],
        proofs: [unique("proof "), unique("proof ")],
    }
}
fn record_args(item: &Item) -> Args {
    args(&[
        ("id", &item.id),
        ("title", &item.title),
        ("priority", "P1"),
        ("owner", &unique("owner ")),
        ("direction", &unique("direction ")),
        ("conclusion", &unique("conclusion ")),
        (
            "implementation",
            &format!("- {}\n\n{}", item.tasks[0], item.tasks[1]),
        ),
        (
            "evidence",
            &format!("- [ ] {}\n{}", item.proofs[0], item.proofs[1]),
        ),
    ])
}

// ── Recording ───────────────────────────────────────────────────────────────

#[test]
fn the_first_record_creates_the_queue_in_literate_ai_shape() {
    let mut fake = Fake::default();
    let item = item("API-001");
    let out = record(&mut fake, &record_args(&item)).unwrap();
    assert!(out.contains("API-001"), "{out}");
    assert!(fake.directories.contains("docs/roadmap"));
    let text = fake.text(queue::QUEUE);
    assert!(text.starts_with("# Active work\n"), "{text}");
    let expected = format!("### [ ] API-001 — {}\n\n- **Priority:** P1\n", item.title);
    assert!(text.contains(&expected), "{text}");
    assert!(text.contains("- **Depends on:** none\n- **Implementation:**\n"));
    let checklists = format!(
        "  - [ ] {}\n  - [ ] {}\n- **Evidence:**\n  - [ ] {}\n  - [ ] {}\n",
        item.tasks[0], item.tasks[1], item.proofs[0], item.proofs[1]
    );
    assert!(text.ends_with(&checklists), "{text}");
}

#[test]
fn a_duplicate_id_is_refused_without_writing() {
    let mut fake = Fake::default();
    record(&mut fake, &record_args(&item("API-001"))).unwrap();
    let before = fake.text(queue::QUEUE).to_string();
    let refused = record(&mut fake, &record_args(&item("API-001"))).unwrap_err();
    assert_eq!(code(refused), "duplicate");
    assert_eq!(fake.text(queue::QUEUE), before);
}

#[test]
fn an_area_allocates_the_next_free_id() {
    let mut fake = Fake::default();
    record(&mut fake, &record_args(&item("API-007"))).unwrap();
    record(&mut fake, &record_args(&item("CLI-020"))).unwrap();
    let mut next = record_args(&item("unused"));
    next.remove("id");
    next.insert("area".into(), Value::Str("API".into()));
    assert!(record(&mut fake, &next).unwrap().contains("API-008"));
    next.insert("area".into(), Value::Str("NEW".into()));
    assert!(record(&mut fake, &next).unwrap().contains("NEW-001"));
    next.insert("id".into(), Value::Str("NEW-009".into()));
    assert_eq!(
        code(record(&mut fake, &next).unwrap_err()),
        "invalid_argument"
    );
}

#[test]
fn records_validate_ids_fields_and_checklists() {
    let mut fake = Fake::default();
    for bad in ["api-1", "A1", "API 1", "1API"] {
        let mut a = record_args(&item("X"));
        a.insert("id".into(), Value::Str(bad.into()));
        assert_eq!(
            code(record(&mut fake, &a).unwrap_err()),
            "invalid_argument",
            "{bad}"
        );
    }
    let mut a = record_args(&item("API-001"));
    a.insert("title".into(), Value::Str("two\nlines".into()));
    assert!(matches!(
        record(&mut fake, &a),
        Err(QueueError::MultiLine("title"))
    ));
    let mut a = record_args(&item("API-001"));
    a.insert("evidence".into(), Value::Str("-\n - [ ]\n".into()));
    assert!(matches!(
        record(&mut fake, &a),
        Err(QueueError::EmptyChecklist("evidence"))
    ));
    assert!(fake.files.is_empty());
}

#[test]
fn a_record_goes_under_its_priority_section() {
    let text = "# Active work\n\n## P0\n\n### [ ] A-001 — a\n\n## P1\n\n### [ ] B-001 — b\n\n## Later\n\nprose\n";
    let mut fake = Fake::default().file(queue::QUEUE, text);
    let mut a = record_args(&item("C-001"));
    a.insert("priority".into(), Value::Str("P0 (headline)".into()));
    a.insert("depends_on".into(), Value::Str("A-001\nB-001".into()));
    a.insert(
        "issue".into(),
        Value::Str("https://github.com/o/r/issues/42".into()),
    );
    record(&mut fake, &a).unwrap();
    let text = fake.text(queue::QUEUE);
    let c = text.find("### [ ] C-001").unwrap();
    assert!(text.find("### [ ] A-001").unwrap() < c);
    assert!(c < text.find("## P1").unwrap(), "{text}");
    assert!(text.contains("- **Depends on:** A-001, B-001\n"));
    assert!(text.contains("- **GitHub issue:** [#42](https://github.com/o/r/issues/42)\n"));
}

// ── Checking, adding and closing ──────────────────────────────────────────

#[test]
fn evidence_is_checked_only_with_its_result_and_closing_needs_every_box() {
    let mut fake = Fake::default();
    let item = item("API-001");
    record(&mut fake, &record_args(&item)).unwrap();
    let id = [("id", "API-001")];

    let refused = close(&mut fake, &args(&id)).unwrap_err();
    assert_eq!(code(refused), "incomplete");

    let task = |section: &str, number: i64, result: Option<&str>| {
        let mut pairs = vec![("id", "API-001"), ("section", section)];
        if let Some(result) = result {
            pairs.push(("result", result));
        }
        with_number(args(&pairs), number)
    };
    let refused = check(&mut fake, &task("evidence", 1, None)).unwrap_err();
    assert_eq!(code(refused), "evidence_needs_result");
    assert!(fake.text(queue::QUEUE).contains("### [ ] API-001"));

    check(&mut fake, &task("Implementation", 2, None)).unwrap();
    let text = fake.text(queue::QUEUE);
    assert!(text.contains("### [~] API-001"), "{text}");
    assert!(text.contains(&format!("  - [x] {}\n", item.tasks[1])));
    let refused = check(&mut fake, &task("implementation", 2, None)).unwrap_err();
    assert_eq!(code(refused), "already_checked");
    assert_eq!(
        code(check(&mut fake, &task("implementation", 3, None)).unwrap_err()),
        "not_found"
    );
    assert_eq!(
        code(check(&mut fake, &task("tests", 1, None)).unwrap_err()),
        "not_found"
    );
    assert_eq!(
        code(check(&mut fake, &task("evidence", 0, Some("x"))).unwrap_err()),
        "invalid_argument"
    );

    let proof = unique("cargo test passed at ");
    check(
        &mut fake,
        &task("implementation", 1, Some(&unique("commit "))),
    )
    .unwrap();
    check(&mut fake, &task("evidence", 1, Some(&proof))).unwrap();
    assert!(
        fake.text(queue::QUEUE)
            .contains(&format!("  - [x] {} — {proof}\n", item.proofs[0]))
    );
    assert_eq!(
        code(close(&mut fake, &args(&id)).unwrap_err()),
        "incomplete"
    );
    check(&mut fake, &task("evidence", 2, Some(&unique("review ")))).unwrap();
    assert!(
        close(&mut fake, &args(&id))
            .unwrap()
            .contains("changelog_add")
    );
    assert!(fake.text(queue::QUEUE).contains("### [x] API-001"));
    assert!(
        close(&mut fake, &args(&id))
            .unwrap()
            .contains("already closed")
    );

    // A follow-up found later opens the item again.
    let follow_up = unique("follow-up ");
    let out = add(
        &mut fake,
        &args(&[
            ("id", "API-001"),
            ("section", "evidence"),
            ("text", &follow_up),
        ]),
    )
    .unwrap();
    assert!(out.contains("open again"), "{out}");
    let text = fake.text(queue::QUEUE);
    assert!(text.contains("### [~] API-001"), "{text}");
    assert!(text.ends_with(&format!("  - [ ] {follow_up}\n")), "{text}");
}

#[test]
fn adding_to_a_missing_group_creates_it_inside_the_item() {
    let text = "# Q\n\n### [ ] A-001 — a\n\n- **Owner:** o\n\n### [ ] B-001 — b\n";
    let (updated, reopened) = queue::add(text, "A-001", "Evidence", "proof").unwrap();
    assert!(!reopened);
    assert_eq!(
        updated,
        "# Q\n\n### [ ] A-001 — a\n\n- **Owner:** o\n- **Evidence:**\n  - [ ] proof\n\n### [ ] B-001 — b\n"
    );
    let items = queue::items(&updated);
    assert_eq!(items[0].tasks.len(), 1);
    assert!(items[1].tasks.is_empty());
}

#[test]
fn a_change_made_meanwhile_is_not_overwritten() {
    struct Racing(Fake);
    impl Host for Racing {
        fn read_file(&mut self, path: &str) -> Result<Vec<u8>, HostFailure> {
            let read = self.0.read_file(path);
            // Someone edits the middle of the queue between read and write.
            let changed = self.0.text(path).replacen("Owner", &unique("Owner "), 1);
            self.0.files.insert(path.into(), changed);
            read
        }
        fn create_directory(&mut self, path: &str) -> Result<(), HostFailure> {
            self.0.create_directory(path)
        }
        fn write_file(&mut self, path: &str, c: &[u8], o: bool) -> Result<(), HostFailure> {
            self.0.write_file(path, c, o)
        }
        fn edit_file(&mut self, path: &str, old: &str, new: &str) -> Result<(), HostFailure> {
            self.0.edit_file(path, old, new)
        }
    }
    let mut seed = Fake::default();
    record(&mut seed, &record_args(&item("API-001"))).unwrap();
    let mut host = Racing(seed);
    let refused = close(&mut host, &args(&[("id", "API-001")])).unwrap_err();
    assert_ne!(
        code(refused),
        "conflict",
        "an incomplete item is refused first"
    );
    let refused = record(&mut host, &record_args(&item("API-002"))).unwrap_err();
    assert_eq!(code(refused), "conflict");
    assert_eq!(host.0.edits, 0);
}

#[test]
fn listing_shows_open_items_progress_and_next_task() {
    let mut fake = Fake::default();
    assert!(
        list(&mut fake, &Args::new())
            .unwrap()
            .contains("no work queue")
    );
    let first = item("API-001");
    let second = item("API-002");
    record(&mut fake, &record_args(&first)).unwrap();
    record(&mut fake, &record_args(&second)).unwrap();
    for (section, n) in [("implementation", 1), ("implementation", 2)] {
        check(
            &mut fake,
            &with_number(args(&[("id", "API-002"), ("section", section)]), n),
        )
        .unwrap();
    }
    for n in 1..=2 {
        let a = args(&[("id", "API-002"), ("section", "evidence"), ("result", "ok")]);
        check(&mut fake, &with_number(a, n)).unwrap();
    }
    close(&mut fake, &args(&[("id", "API-002")])).unwrap();

    let listed = list(&mut fake, &Args::new()).unwrap();
    assert!(
        listed.contains(&format!("[ ] API-001 — {}", first.title)),
        "{listed}"
    );
    assert!(
        listed.contains("(implementation 0/2, evidence 0/2)"),
        "{listed}"
    );
    assert!(listed.contains(&format!("next: implementation: {}", first.tasks[0])));
    assert!(!listed.contains("API-002 —"), "{listed}");
    assert!(listed.contains("[1 closed items not shown]"));

    let mut all = Args::new();
    all.insert("include_closed".into(), Value::Bool(true));
    assert!(list(&mut fake, &all).unwrap().contains("[x] API-002"));
    let shown = list(&mut fake, &args(&[("id", "API-001")])).unwrap();
    assert!(shown.starts_with("### [ ] API-001") && shown.contains(&first.proofs[1]));
    assert!(!shown.contains("API-002"));
}

#[test]
fn headings_and_checkboxes_in_code_blocks_are_not_items() {
    let text = "# Q\n\n```markdown\n### [ ] FAKE-001 — no\n  - [ ] no\n```\n\n### [ ] REAL-001 — yes\n- **Evidence:**\n  - [ ] proof\n```\n  - [ ] fenced\n```\n";
    let items = queue::items(text);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].id, "REAL-001");
    assert_eq!(items[0].tasks.len(), 1);
}

#[test]
fn a_shorter_fence_inside_a_longer_one_does_not_close_it() {
    let text = "# Q\n\n````markdown\n```rust\n### [ ] FAKE-001 — example\n```\n````\n\n### [ ] REAL-001 — task\n- **Evidence:**\n  - [ ] proof\n~~~\n### [ ] TILDE-001 — no\n```\n### [ ] STILL-001 — no\n~~~~\n";
    let ids: Vec<&str> = queue::items(text).iter().map(|i| i.id).collect();
    assert_eq!(ids, ["REAL-001"]);
    // A backtick fence followed by more text does not close the block.
    let text = "```\n``` not a close\n### [ ] FAKE-001 — no\n```\n### [ ] REAL-001 — yes\n";
    let ids: Vec<&str> = queue::items(text).iter().map(|i| i.id).collect();
    assert_eq!(ids, ["REAL-001"]);
    // The changelog finds Unreleased past a nested example, not inside it.
    let log = "# Changelog\n\n````\n```\n## Unreleased\n```\n````\n\n## Unreleased\n\n- a\n";
    assert_eq!(
        changelog::add(Some(log), "b", None).unwrap(),
        "# Changelog\n\n````\n```\n## Unreleased\n```\n````\n\n## Unreleased\n\n- a\n- b\n"
    );
}

#[test]
fn an_empty_changelog_is_refused_rather_than_overwritten() {
    // An empty file gives `edit_file` nothing to compare, so a write could
    // replace text added since the read; the tool refuses with guidance.
    let mut fake = Fake::default().file("CHANGELOG.md", "");
    let refused = changelog_add(&mut fake, &args(&[("text", "t")])).unwrap_err();
    assert_eq!(code(refused), "empty_file");
    assert_eq!(fake.text("CHANGELOG.md"), "");
    // Whitespace alone is still text to compare, so it is initialized.
    let mut fake = Fake::default().file("CHANGELOG.md", "\n");
    changelog_add(&mut fake, &args(&[("text", "t")])).unwrap();
    assert_eq!(
        fake.text("CHANGELOG.md"),
        "# Changelog\n\n## Unreleased\n\n- t\n"
    );
}

#[test]
fn oversized_and_non_text_queues_are_refused() {
    let mut fake = Fake::default().file(queue::QUEUE, &"x".repeat(MAX_FILE + 1));
    assert_eq!(
        code(list(&mut fake, &Args::new()).unwrap_err()),
        "too_large"
    );
    let mut fake = Fake::default().file(queue::QUEUE, "");
    assert_eq!(
        code(record(&mut fake, &record_args(&item("A-001"))).unwrap_err()),
        "empty_file"
    );
}

// ── Changelog ─────────────────────────────────────────────────────────────

#[test]
fn changelog_entries_join_unreleased_in_its_existing_style() {
    let mut fake = Fake::default();
    let entry = unique("Added ");
    changelog_add(&mut fake, &args(&[("text", &entry)])).unwrap();
    assert_eq!(
        fake.text("CHANGELOG.md"),
        format!("# Changelog\n\n## Unreleased\n\n- {entry}\n")
    );

    // Blank-separated entries, as Literate AI writes them.
    let blank = "# Changelog\n\n## Unreleased\n\n- one\n  wrapped\n\n- two\n\n## 1.0.0\n\n- old\n";
    let added = changelog::add(Some(blank), "three\nmore", None).unwrap();
    assert_eq!(
        added,
        "# Changelog\n\n## Unreleased\n\n- one\n  wrapped\n\n- two\n\n- three\n  more\n\n## 1.0.0\n\n- old\n"
    );
    // Keep a Changelog: categories, adjacent entries, a bracketed heading.
    let kac = "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- a\n- b\n\n## [1.0.0]\n";
    assert_eq!(
        changelog::add(Some(kac), "c", Some("added")).unwrap(),
        "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- a\n- b\n- c\n\n## [1.0.0]\n"
    );
    assert_eq!(
        changelog::add(Some(kac), "f", Some("Fixed")).unwrap(),
        "# Changelog\n\n## [Unreleased]\n\n### Added\n\n- a\n- b\n\n### Fixed\n\n- f\n\n## [1.0.0]\n"
    );
    // No Unreleased section yet: it goes before the newest release.
    assert_eq!(
        changelog::add(Some("# Changelog\n\n## 1.0.0\n\n- old\n"), "new", None).unwrap(),
        "# Changelog\n\n## Unreleased\n\n- new\n\n## 1.0.0\n\n- old\n"
    );
}

#[test]
fn changelog_paths_must_name_a_changelog() {
    let mut fake = Fake::default();
    for path in [
        "README.md",
        "/etc/CHANGELOG.md",
        "../CHANGELOG.md",
        "a//CHANGELOG.md",
    ] {
        let refused =
            changelog_add(&mut fake, &args(&[("text", "t"), ("path", path)])).unwrap_err();
        assert!(matches!(refused, QueueError::NotChangelog(_)), "{path}");
    }
    changelog_add(
        &mut fake,
        &args(&[("text", "t"), ("path", "crates/x/CHANGELOG.md")]),
    )
    .unwrap();
    assert!(fake.directories.contains("crates/x"));
    assert!(fake.files.contains_key("crates/x/CHANGELOG.md"));
}
