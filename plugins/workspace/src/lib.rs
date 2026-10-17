//! Bundled workspace extension. Every file access is a host call that the
//! gateway authorizes against the caller's `read_workspace` grant.
use a206_extension_sdk::{
    Args, DirEntry, EntryKind, Extension, Field, GuestError, Node, Tone, ToolOutput, UiEvent, View,
    bool_arg, excerpt, host, int_arg, string_arg,
};

struct Workspace;
a206_extension_sdk::export!(Workspace);

/// State key for the overview's "show hidden entries" preference.
const SHOW_HIDDEN: &str = "show_hidden";
const TOGGLE_HIDDEN: &str = "toggle-hidden";
const README_LINES: usize = 40;
const DEFAULT_MATCHES: i64 = 100;

impl Extension for Workspace {
    fn tool(name: &str, args: &Args) -> Result<ToolOutput, GuestError> {
        match name {
            "list_directory" => {
                let path = string_arg(args, "path")?.unwrap_or(".");
                let mut entries = host::list_directory(path)?;
                sort(&mut entries);
                Ok(ToolOutput {
                    text: entries.iter().map(line).collect::<Vec<_>>().join("\n"),
                })
            }
            "read_text_file" => {
                let path = required(args, "path")?;
                Ok(ToolOutput {
                    text: excerpt(text(host::read_file(path)?)?),
                })
            }
            "search_text" => {
                let query = required(args, "query")?;
                let path = string_arg(args, "path")?.unwrap_or(".");
                let max = int_arg(args, "max_matches")?.unwrap_or(DEFAULT_MATCHES);
                let max = u32::try_from(max.clamp(1, 500)).unwrap_or(1);
                let found = host::search(path, query, max)?;
                let mut lines: Vec<String> = found
                    .matches
                    .iter()
                    .map(|m| format!("{}:{}: {}", m.path, m.line, m.text))
                    .collect();
                if lines.is_empty() {
                    lines.push("no matches".into());
                }
                if found.truncated {
                    lines.push("[more matches may exist; the search stopped at a limit]".into());
                }
                Ok(ToolOutput {
                    text: excerpt(lines.join("\n")),
                })
            }
            "write_text_file" => {
                let path = required(args, "path")?;
                let text = required(args, "text")?;
                let overwrite = bool_arg(args, "overwrite")?.unwrap_or(false);
                if let Some((parent, _)) = path.rsplit_once('/')
                    && !parent.is_empty()
                {
                    host::create_directory(parent)?;
                }
                host::write_file(path, text.as_bytes(), overwrite)?;
                Ok(ToolOutput {
                    text: format!("wrote {} bytes to {path}", text.len()),
                })
            }
            "edit_text_file" => {
                let path = required(args, "path")?;
                host::edit_file(
                    path,
                    required(args, "old_text")?,
                    required(args, "new_text")?,
                )?;
                Ok(ToolOutput {
                    text: format!("edited {path}"),
                })
            }
            _ => Err(GuestError::new("unknown_tool", format!("no tool `{name}`"))),
        }
    }

    fn panel(id: &str, event: Option<&UiEvent>) -> Result<View, GuestError> {
        if id != "overview" {
            return Err(GuestError::new("unknown_panel", format!("no panel `{id}`")));
        }
        let mut show_hidden = host::state_get(SHOW_HIDDEN)?.is_some();
        // List first: a toggle is saved only when the panel can show it.
        let mut entries = host::list_directory(".")?;
        if event.is_some_and(|event| event.element == TOGGLE_HIDDEN) {
            show_hidden = !show_hidden;
            host::state_put(SHOW_HIDDEN, show_hidden.then_some(&[1][..]))?;
        }
        let hidden = entries.iter().filter(|e| e.name.starts_with('.')).count();
        if !show_hidden {
            entries.retain(|entry| !entry.name.starts_with('.'));
        }
        sort(&mut entries);
        let count = |kind| entries.iter().filter(|e| e.kind == kind).count();
        let mut nodes = vec![
            Node::Heading {
                text: "Workspace files".into(),
            },
            Node::Fields {
                rows: vec![
                    field("Folders", count(EntryKind::Directory)),
                    field("Files", count(EntryKind::File)),
                    field("Hidden entries", hidden),
                ],
            },
            Node::List {
                items: entries.iter().map(line).collect(),
            },
            Node::Button {
                id: TOGGLE_HIDDEN.into(),
                label: if show_hidden {
                    "Hide hidden entries"
                } else {
                    "Show hidden entries"
                }
                .into(),
            },
        ];
        if let Some(readme) = entries
            .iter()
            .find(|e| e.kind == EntryKind::File && e.name.eq_ignore_ascii_case("readme.md"))
        {
            // A README that cannot be shown is reported in the panel, not
            // allowed to hide the rest of the overview.
            let excerpt = host::read_file(&readme.name)
                .map_err(GuestError::from)
                .and_then(text)
                .map(|text| Node::Code {
                    text: text
                        .lines()
                        .take(README_LINES)
                        .collect::<Vec<_>>()
                        .join("\n"),
                });
            nodes.push(Node::Section {
                title: Some(readme.name.clone()),
                nodes: vec![excerpt.unwrap_or_else(|error| Node::Badge {
                    text: error.message,
                    tone: Tone::Warning,
                })],
            });
        }
        Ok(View { nodes })
    }
}

fn sort(entries: &mut [DirEntry]) {
    entries.sort_by(|a, b| {
        (a.kind != EntryKind::Directory, &a.name).cmp(&(b.kind != EntryKind::Directory, &b.name))
    });
}
fn line(entry: &DirEntry) -> String {
    match (entry.kind, entry.size) {
        (EntryKind::Directory, _) => format!("{}/", entry.name),
        (EntryKind::Symlink, _) => format!("{} (link)", entry.name),
        (_, Some(size)) => format!("{} ({size} bytes)", entry.name),
        _ => entry.name.clone(),
    }
}
fn field(label: &str, value: usize) -> Field {
    Field {
        label: label.into(),
        value: value.to_string(),
    }
}
fn text(bytes: Vec<u8>) -> Result<String, GuestError> {
    String::from_utf8(bytes).map_err(|_| GuestError::new("not_text", "the file is not UTF-8 text"))
}
fn required<'a>(args: &'a Args, name: &str) -> Result<&'a str, GuestError> {
    string_arg(args, name)?
        .ok_or_else(|| GuestError::new("invalid_argument", format!("`{name}` is required")))
}
