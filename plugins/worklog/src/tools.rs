//! The work queue tools, over any [`Host`].
use crate::{
    changelog::{self, CHANGELOG},
    error::QueueError,
    host::Host,
    queue::{self, QUEUE, Record},
};
use a206_extension_sdk::{Args, HostErrorCode, bool_arg, int_arg, string_arg};

/// Largest queue or changelog the tools read or write. A change sends the
/// old and new text to the host in one edit, which this keeps well inside
/// the request limit.
pub const MAX_FILE: usize = 256 * 1024;

pub fn list(host: &mut impl Host, args: &Args) -> Result<String, QueueError> {
    let Some(text) = read(host, QUEUE)? else {
        return Ok(format!(
            "no work queue: {QUEUE} does not exist. Record the first item with work_record."
        ));
    };
    if let Some(id) = optional(args, "id")? {
        return Ok(queue::show(&text, id)?.to_string());
    }
    let include_closed = bool_arg(args, "include_closed")
        .map_err(QueueError::Argument)?
        .unwrap_or(false);
    Ok(queue::list(&text, include_closed))
}

pub fn record(host: &mut impl Host, args: &Args) -> Result<String, QueueError> {
    let existing = read(host, QUEUE)?;
    let text = existing.as_deref().unwrap_or(queue::NEW_QUEUE);
    let id = match (optional(args, "id")?, optional(args, "area")?) {
        (Some(_), Some(_)) => return Err(QueueError::IdAndArea),
        (Some(id), None) => id.trim().to_string(),
        (None, Some(area)) => queue::next_id(text, area)?,
        (None, None) => return Err(QueueError::Missing("id")),
    };
    let record = Record {
        id: &id,
        title: required(args, "title")?,
        priority: required(args, "priority")?,
        owner: required(args, "owner")?,
        direction: required(args, "direction")?,
        conclusion: required(args, "conclusion")?,
        depends_on: queue::entries("depends_on", optional(args, "depends_on")?.unwrap_or(""))?,
        implementation: queue::entries("implementation", required(args, "implementation")?)?,
        evidence: queue::entries("evidence", required(args, "evidence")?)?,
        release_target: optional(args, "release_target")?,
        issue: optional(args, "issue")?,
    };
    let updated = queue::record(text, &record)?;
    replace(host, QUEUE, existing.as_deref(), &updated)?;
    Ok(format!("recorded {id} in {QUEUE}"))
}

pub fn add(host: &mut impl Host, args: &Args) -> Result<String, QueueError> {
    let id = required(args, "id")?;
    let group = required(args, "section")?;
    let text = existing_queue(host)?;
    let (updated, reopened) = queue::add(&text, id, group, required(args, "text")?)?;
    replace(host, QUEUE, Some(&text), &updated)?;
    let mut out = format!("added a `{}` task to {id}", group.trim().to_lowercase());
    if reopened {
        out.push_str("; the item was closed and is open again");
    }
    Ok(out)
}

pub fn check(host: &mut impl Host, args: &Args) -> Result<String, QueueError> {
    let id = required(args, "id")?;
    let group = required(args, "section")?;
    let number = int_arg(args, "number")
        .map_err(QueueError::Argument)?
        .ok_or(QueueError::Missing("number"))?;
    let number = usize::try_from(number)
        .ok()
        .filter(|n| *n > 0)
        .ok_or(QueueError::Number("number"))?;
    let text = existing_queue(host)?;
    let updated = queue::check(&text, id, group, number, optional(args, "result")?)?;
    replace(host, QUEUE, Some(&text), &updated)?;
    let summary = queue::list(queue::show(&updated, id)?, false);
    Ok(format!("checked it.\n{summary}"))
}

pub fn close(host: &mut impl Host, args: &Args) -> Result<String, QueueError> {
    let id = required(args, "id")?;
    let text = existing_queue(host)?;
    match queue::close(&text, id)? {
        None => Ok(format!("{id} was already closed")),
        Some(updated) => {
            replace(host, QUEUE, Some(&text), &updated)?;
            Ok(format!(
                "closed {id}. Record its user-facing outcome with changelog_add, and commit the queue, the change and the changelog together."
            ))
        }
    }
}

pub fn changelog_add(host: &mut impl Host, args: &Args) -> Result<String, QueueError> {
    let path = optional(args, "path")?.unwrap_or(CHANGELOG).trim();
    let file = path.rsplit('/').next().unwrap_or(path);
    if file != CHANGELOG
        || path.starts_with('/')
        || path
            .split('/')
            .any(|part| part == ".." || part == "." || part.is_empty())
    {
        return Err(QueueError::NotChangelog(path.into()));
    }
    let existing = read(host, path)?;
    let updated = changelog::add(
        existing.as_deref(),
        required(args, "text")?,
        optional(args, "category")?,
    )?;
    replace(host, path, existing.as_deref(), &updated)?;
    Ok(format!("added to the Unreleased section of {path}"))
}

/// The queue's text, which must exist.
fn existing_queue(host: &mut impl Host) -> Result<String, QueueError> {
    read(host, QUEUE)?.ok_or(QueueError::NoQueue)
}

/// The file's text, or `None` when it does not exist.
fn read(host: &mut impl Host, path: &str) -> Result<Option<String>, QueueError> {
    match host.read_file(path) {
        Ok(bytes) if bytes.len() > MAX_FILE => Err(QueueError::TooLarge(path.into())),
        Ok(bytes) => String::from_utf8(bytes)
            .map(Some)
            .map_err(|_| QueueError::NotText(path.into())),
        Err(failure) if failure.code == HostErrorCode::NotFound => Ok(None),
        Err(failure) => Err(failure.into()),
    }
}

/// Replace `old` with `new`: create the file when there was none, else
/// through `edit_file` with the whole old text as the expected text, so the
/// host refuses with `conflict` if the file changed since it was read.
fn replace(
    host: &mut impl Host,
    path: &str,
    old: Option<&str>,
    new: &str,
) -> Result<(), QueueError> {
    if new.len() > MAX_FILE {
        return Err(QueueError::TooLarge(path.into()));
    }
    match old {
        None => {
            if let Some((parent, _)) = path.rsplit_once('/') {
                host.create_directory(parent)?;
            }
            host.write_file(path, new.as_bytes(), false)?;
        }
        // `edit_file` needs old text to compare, and a plain overwrite could
        // replace text written since the read.
        Some("") => return Err(QueueError::EmptyFile(path.into())),
        Some(old) => host.edit_file(path, old, new)?,
    }
    Ok(())
}

fn optional<'a>(args: &'a Args, name: &'static str) -> Result<Option<&'a str>, QueueError> {
    Ok(string_arg(args, name)
        .map_err(QueueError::Argument)?
        .filter(|value| !value.trim().is_empty()))
}
fn required<'a>(args: &'a Args, name: &'static str) -> Result<&'a str, QueueError> {
    optional(args, name)?.ok_or(QueueError::Missing(name))
}
