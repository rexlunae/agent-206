//! `agent206 extension …`: the same gateway commands the Extensions page uses.
use a206_core::{
    Capability, Command,
    extension::{Args, Node, UiEvent, Value, View},
};
use a206_runtime::extension_host::packages;
use anyhow::Context;
use clap::Subcommand;
use serde::{Deserialize, de::IntoDeserializer};

#[derive(Subcommand)]
pub enum ExtensionAction {
    /// List installed extensions with their tools, panels and grants, and
    /// packages agents prepared for review.
    List,
    /// Replace the capabilities granted to an extension; none revokes all.
    Grant {
        id: String,
        #[arg(long = "capability", value_parser = capability)]
        capabilities: Vec<Capability>,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    /// Install or update an extension from its `plugin.toml` and compiled
    /// WASM module. It arrives disabled with nothing granted.
    Install {
        manifest: std::path::PathBuf,
        module: std::path::PathBuf,
    },
    /// Remove an installed extension and everything it stored.
    Uninstall {
        id: String,
    },
    /// Install a package an agent prepared, by the digest `list` shows. It
    /// arrives disabled with nothing granted, also when it is an update.
    InstallProposal {
        digest: String,
    },
    /// Discard a package an agent prepared.
    Dismiss {
        digest: String,
    },
    /// Render a panel as text, optionally after pressing one of its buttons.
    Panel {
        id: String,
        panel: String,
        #[arg(long)]
        project: Option<uuid::Uuid>,
        #[arg(long)]
        press: Option<String>,
    },
    /// Run a tool with TOML arguments, e.g. --args 'path = "src"'.
    Tool {
        id: String,
        tool: String,
        #[arg(long)]
        project: Option<uuid::Uuid>,
        #[arg(long, default_value = "")]
        args: String,
    },
}

fn capability(text: &str) -> Result<Capability, serde::de::value::Error> {
    Capability::deserialize(text.into_deserializer())
}

/// The gateway command for an action; `List` reads the snapshot instead.
pub fn command(action: ExtensionAction) -> anyhow::Result<Option<Command>> {
    Ok(Some(match action {
        ExtensionAction::List => return Ok(None),
        ExtensionAction::Grant { id, capabilities } => Command::GrantExtension {
            extension: id,
            capabilities: capabilities.into_iter().collect(),
        },
        ExtensionAction::Enable { id } => Command::EnableExtension {
            extension: id,
            enabled: true,
        },
        ExtensionAction::Disable { id } => Command::EnableExtension {
            extension: id,
            enabled: false,
        },
        ExtensionAction::Install { manifest, module } => {
            let manifest = std::fs::read_to_string(&manifest)
                .with_context(|| format!("could not read {}", manifest.display()))?;
            let module = std::fs::read(&module)
                .with_context(|| format!("could not read {}", module.display()))?;
            // Refuse here what the gateway would, before sending megabytes.
            packages::prepare(manifest.clone(), module.clone())?;
            Command::InstallExtension { manifest, module }
        }
        ExtensionAction::Uninstall { id } => Command::UninstallExtension { extension: id },
        ExtensionAction::InstallProposal { digest } => Command::InstallProposal { digest },
        ExtensionAction::Dismiss { digest } => Command::DismissProposal { digest },
        ExtensionAction::Panel {
            id,
            panel,
            project,
            press,
        } => Command::OpenPanel {
            extension: id,
            panel,
            project,
            event: press.map(|element| UiEvent { element }),
        },
        ExtensionAction::Tool {
            id,
            tool,
            project,
            args,
        } => Command::CallTool {
            extension: id,
            tool,
            args: toml_args(&args)?,
            project,
        },
    }))
}

fn toml_args(text: &str) -> Result<Args, toml::de::Error> {
    fn value(toml: toml::Value) -> Value {
        match toml {
            toml::Value::String(text) => Value::Str(text),
            toml::Value::Integer(number) => Value::Int(number),
            toml::Value::Float(number) => Value::Float(number),
            toml::Value::Boolean(flag) => Value::Bool(flag),
            toml::Value::Datetime(time) => Value::Str(time.to_string()),
            toml::Value::Array(items) => Value::List(items.into_iter().map(value).collect()),
            toml::Value::Table(table) => {
                Value::Map(table.into_iter().map(|(k, v)| (k, value(v))).collect())
            }
        }
    }
    Ok(toml::from_str::<toml::Table>(text)?
        .into_iter()
        .map(|(name, v)| (name, value(v)))
        .collect())
}

/// Plain-text rendering of a panel for terminals. Buttons show the id to
/// pass to `--press`.
pub fn render(view: &View) -> String {
    fn walk(nodes: &[Node], depth: usize, out: &mut Vec<String>) {
        for node in nodes {
            match node {
                Node::Heading { text } => out.push(format!("{} {text}", "#".repeat(depth))),
                Node::Text { text } => out.push(text.clone()),
                Node::Code { text } => out.extend(text.lines().map(|line| format!("    {line}"))),
                Node::Badge { text, .. } => out.push(format!("[{text}]")),
                Node::Fields { rows } => out.extend(
                    rows.iter()
                        .map(|row| format!("{}: {}", row.label, row.value)),
                ),
                Node::List { items } => out.extend(items.iter().map(|item| format!("- {item}"))),
                Node::Button { id, label } => out.push(format!("[{label}] (--press {id})")),
                Node::Section { title, nodes } => {
                    if let Some(title) = title {
                        out.push(format!("{} {title}", "#".repeat(depth + 1)));
                    }
                    walk(nodes, depth + 1, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&view.nodes, 1, &mut out);
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_arguments_keep_their_types() {
        let args = toml_args("path = \"src\"\ndepth = 2\nall = true").unwrap();
        assert_eq!(args["path"], Value::Str("src".into()));
        assert_eq!(args["depth"], Value::Int(2));
        assert_eq!(args["all"], Value::Bool(true));
        assert!(toml_args("").unwrap().is_empty());
        assert!(toml_args("path =").is_err());
    }

    #[test]
    fn capabilities_parse_by_their_wire_names() {
        assert_eq!(
            capability("read_workspace").unwrap(),
            Capability::ReadWorkspace
        );
        assert!(capability("everything").is_err());
    }
}
