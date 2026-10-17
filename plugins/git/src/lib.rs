//! Bundled git extension. Every repository read or commit is a host call
//! that the gateway authorizes against the caller's `git_read` or
//! `git_write` grant.
use a206_extension_sdk::{
    Args, Extension, GitArea, GitChange, GuestError, ToolOutput, bool_arg, excerpt, host, int_arg,
    string_arg,
};

struct Git;
a206_extension_sdk::export!(Git);

const DEFAULT_COMMITS: i64 = 20;

impl Extension for Git {
    fn tool(name: &str, args: &Args) -> Result<ToolOutput, GuestError> {
        let text = match name {
            "git_status" => {
                let status = host::git_status()?;
                let mut lines = vec![match (&status.branch, &status.head) {
                    (Some(branch), Some(head)) => format!("on {branch} at {}", short(head)),
                    (Some(branch), None) => format!("on {branch}, no commits yet"),
                    (None, Some(head)) => format!("detached at {}", short(head)),
                    (None, None) => "no HEAD".into(),
                }];
                if status.entries.is_empty() {
                    lines.push("no changes".into());
                }
                lines.extend(status.entries.iter().map(|entry| {
                    format!(
                        "{} {} {}",
                        area(entry.area),
                        change(entry.change),
                        entry.path
                    )
                }));
                if status.truncated {
                    lines.push("[more changes may exist; the listing stopped at a limit]".into());
                }
                lines.join("\n")
            }
            "git_log" => {
                let max = int_arg(args, "max")?
                    .unwrap_or(DEFAULT_COMMITS)
                    .clamp(1, 200);
                let commits = host::git_log(u32::try_from(max).unwrap_or(1))?;
                if commits.is_empty() {
                    "no commits yet".into()
                } else {
                    commits
                        .iter()
                        .map(|c| {
                            format!(
                                "{} {} ({}, {}) {}",
                                short(&c.id),
                                c.time,
                                c.author,
                                c.id,
                                c.summary
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "git_diff" => {
                let path = string_arg(args, "path")?
                    .ok_or_else(|| GuestError::new("invalid_argument", "`path` is required"))?;
                let staged = bool_arg(args, "staged")?.unwrap_or(false);
                let diff = host::git_diff(path, staged)?;
                if diff.binary {
                    format!("binary file {path} differs")
                } else if diff.text.is_empty() {
                    format!(
                        "no {} changes in {path}",
                        if staged { "staged" } else { "unstaged" }
                    )
                } else {
                    let mut text = format!("--- a/{path}\n+++ b/{path}\n{}", diff.text);
                    if diff.truncated {
                        text.push_str("\n[diff truncated at 192 KiB]");
                    }
                    text
                }
            }
            "git_commit" => {
                let required = |name: &str| {
                    string_arg(args, name)?.ok_or_else(|| {
                        GuestError::new("invalid_argument", format!("`{name}` is required"))
                    })
                };
                let message = required("message")?;
                let paths: Vec<&str> = required("paths")?
                    .lines()
                    .filter(|path| !path.is_empty())
                    .collect();
                let commit = host::git_commit(message, &paths)?;
                format!("committed {} {}", commit.id, commit.summary)
            }
            _ => return Err(GuestError::new("unknown_tool", format!("no tool `{name}`"))),
        };
        Ok(ToolOutput {
            text: excerpt(text),
        })
    }
}

fn short(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}
fn area(area: GitArea) -> &'static str {
    match area {
        GitArea::Staged => "staged",
        GitArea::Unstaged => "unstaged",
        GitArea::Untracked => "untracked",
    }
}
fn change(change: GitChange) -> &'static str {
    match change {
        GitChange::Added => "added",
        GitChange::Removed => "removed",
        GitChange::Modified => "modified",
        GitChange::TypeChange => "type changed",
        GitChange::Conflict => "conflicted",
        GitChange::Other => "changed",
    }
}
