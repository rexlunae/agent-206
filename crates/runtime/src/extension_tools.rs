//! The WASM extension [`ToolHost`]: offers enabled extensions' tools to the
//! native run engine and runs them through [`ExtensionHost::call_tool`].
//!
//! Only tools of enabled extensions whose declared capabilities the user
//! granted to the extension are listed; nothing here can enable an extension
//! or change a grant. Listing a tool makes it available to be permitted: the
//! conversation's own setting still defaults to deny.
//!
//! A call runs within the intersection of the user's grant to the extension,
//! the conversation's grant within its mode's ceiling (which already carries
//! every ancestor's attenuation) and the tool's manifest declaration: the
//! gateway refuses a tool whose declaration exceeds the conversation's
//! grant, and [`ExtensionHost::call_tool`] enforces the same ceiling again
//! together with the extension grant, for every relayed host call too.
//!
//! Model-facing names are `ext__<extension id>__<tool>`, with every `.` of the
//! id written as `_`. A name that would be longer than [`tools::MAX_NAME`] or
//! that two extension tools would share is not offered at all and is reported
//! in [`ToolListing::unavailable`]; names are never truncated or guessed.
use crate::{
    extension_host::{ExtensionError, ExtensionHost, ExtensionTools, Scope},
    tools::{self, ToolHost, ToolListing, ToolOutput, ToolRequest, ToolScope, ToolSpec},
};
use a206_core::{
    ErrorKind, ErrorReport, ToolPermission, UnavailableTools,
    extension::{self as ext, Args, ParamKind},
};
use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;

/// The prefix of every extension tool's model-facing name.
pub const PREFIX: &str = "ext__";
const SEPARATOR: &str = "__";
/// The source named when the installed extensions cannot be read at all.
pub const SOURCE: &str = "extensions";

/// Why an extension tool is not offered or a call could not reach it.
#[derive(Debug, thiserror::Error)]
pub enum ExtensionToolError {
    #[error(
        "tool `{tool}` of extension `{extension}` is not offered: its model-facing name `{name}` is longer than {} characters",
        tools::MAX_NAME
    )]
    NameTooLong {
        extension: String,
        tool: String,
        name: String,
    },
    #[error(
        "tool `{tool}` of extension `{extension}` is not offered: its model-facing name `{name}` is also derived for a tool of {others:?}"
    )]
    NameCollision {
        extension: String,
        tool: String,
        name: String,
        others: Vec<String>,
    },
    #[error("extension `{extension}` could not list its tools")]
    Source {
        extension: String,
        #[source]
        cause: ExtensionError,
    },
    #[error("could not read the installed extensions")]
    Extensions(#[source] ExtensionError),
    #[error("no enabled extension offers tool `{0}`")]
    NotOffered(String),
    #[error("argument `{0}` holds a number outside the range extensions accept")]
    Number(String),
}
impl crate::error::Classified for ExtensionToolError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::NameTooLong { .. } | Self::Number(_) => ErrorKind::InvalidRequest,
            Self::NameCollision { .. } => ErrorKind::Conflict,
            Self::NotOffered(_) => ErrorKind::NotFound,
            Self::Source { .. } | Self::Extensions(_) => return None,
        })
    }
}

/// The model-facing name of `tool` of `extension`, before validation.
pub fn tool_name(extension: &str, tool: &str) -> String {
    format!("{PREFIX}{}{SEPARATOR}{tool}", extension.replace('.', "_"))
}

/// An extension tool reachable by its model-facing name.
#[derive(Debug, Clone, PartialEq)]
struct Offered {
    extension: String,
    spec: ext::ToolSpec,
}
#[derive(Debug, Default)]
struct Offers {
    tools: BTreeMap<String, Offered>,
    unavailable: Vec<UnavailableTools>,
}
fn unavailable(source: &str, error: &ExtensionToolError) -> UnavailableTools {
    UnavailableTools {
        source: source.into(),
        error: crate::error::report(error),
    }
}

/// Name every listed tool. Listing and calling both derive names here, so a
/// call reaches exactly the tool that was offered under its name.
fn offers(sources: Result<Vec<ExtensionTools>, ExtensionError>) -> Offers {
    let mut result = Offers::default();
    let sources = match sources {
        Ok(sources) => sources,
        Err(cause) => {
            let error = ExtensionToolError::Extensions(cause);
            result.unavailable.push(unavailable(SOURCE, &error));
            return result;
        }
    };
    let mut named: BTreeMap<String, Vec<Offered>> = BTreeMap::new();
    for source in sources {
        let specs = match source.tools {
            Ok(specs) => specs,
            Err(cause) => {
                let error = ExtensionToolError::Source {
                    extension: source.extension.clone(),
                    cause,
                };
                result
                    .unavailable
                    .push(unavailable(&source.extension, &error));
                continue;
            }
        };
        for spec in specs {
            let name = tool_name(&source.extension, &spec.name);
            if name.len() > tools::MAX_NAME {
                let error = ExtensionToolError::NameTooLong {
                    extension: source.extension.clone(),
                    tool: spec.name,
                    name,
                };
                result
                    .unavailable
                    .push(unavailable(&source.extension, &error));
                continue;
            }
            named.entry(name).or_default().push(Offered {
                extension: source.extension.clone(),
                spec,
            });
        }
    }
    for (name, mut claimants) in named {
        if claimants.len() == 1 {
            if let Some(offered) = claimants.pop() {
                result.tools.insert(name, offered);
            }
            continue;
        }
        // Routing a shared name to either tool would be a guess: offer neither.
        for claimant in &claimants {
            let others = claimants
                .iter()
                .filter(|other| *other != claimant)
                .map(|other| other.extension.clone())
                .collect();
            let error = ExtensionToolError::NameCollision {
                extension: claimant.extension.clone(),
                tool: claimant.spec.name.clone(),
                name: name.clone(),
                others,
            };
            result
                .unavailable
                .push(unavailable(&claimant.extension, &error));
        }
    }
    result
}

/// The model-facing name of each tool [`ExtensionToolHost`] offers now,
/// keyed by extension id and tool name, with the failure of any extension
/// whose tools could not be read.
pub(crate) fn offered_names(
    extensions: &ExtensionHost,
) -> (BTreeMap<(String, String), String>, Vec<UnavailableTools>) {
    let offers = offers(extensions.tool_sources());
    let names = offers
        .tools
        .into_iter()
        .map(|(name, offered)| ((offered.extension, offered.spec.name), name))
        .collect();
    (names, offers.unavailable)
}

fn json_type(kind: ParamKind) -> &'static str {
    match kind {
        ParamKind::String => "string",
        ParamKind::Integer => "integer",
        ParamKind::Number => "number",
        ParamKind::Boolean => "boolean",
        ParamKind::Array => "array",
        ParamKind::Object => "object",
    }
}
/// The JSON Schema object for a tool's declared parameters. Undeclared
/// arguments are refused by the host, so the schema says so too.
pub fn parameters_schema(spec: &ext::ToolSpec) -> Value {
    let properties: Map<String, Value> = spec
        .params
        .iter()
        .map(|param| {
            let schema = json!({"type": json_type(param.kind), "description": param.description});
            (param.name.clone(), schema)
        })
        .collect();
    let required: Vec<&str> = spec
        .params
        .iter()
        .filter(|param| param.required)
        .map(|param| param.name.as_str())
        .collect();
    let mut schema = json!({
        "type": "object",
        "properties": properties,
        "additionalProperties": false,
    });
    if !required.is_empty() {
        schema["required"] = json!(required);
    }
    schema
}

/// Provider JSON arguments as extension values. Kinds are kept exactly:
/// `2.0` stays a float and is refused for an integer parameter by the host.
pub fn arguments(object: Map<String, Value>) -> Result<Args, ExtensionToolError> {
    object
        .into_iter()
        .map(|(name, value)| {
            let value = convert(&name, value)?;
            Ok((name, value))
        })
        .collect()
}
fn convert(parameter: &str, value: Value) -> Result<ext::Value, ExtensionToolError> {
    Ok(match value {
        Value::Null => ext::Value::Nil,
        Value::Bool(value) => ext::Value::Bool(value),
        Value::Number(number) => match (number.as_i64(), number.is_u64(), number.as_f64()) {
            (Some(int), _, _) => ext::Value::Int(int),
            (None, false, Some(float)) => ext::Value::Float(float),
            _ => return Err(ExtensionToolError::Number(parameter.into())),
        },
        Value::String(text) => ext::Value::Str(text),
        Value::Array(items) => ext::Value::List(
            items
                .into_iter()
                .map(|item| convert(parameter, item))
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(entries) => ext::Value::Map(
            entries
                .into_iter()
                .map(|(key, item)| Ok((key, convert(parameter, item)?)))
                .collect::<Result<_, ExtensionToolError>>()?,
        ),
    })
}

/// Offers enabled extensions' tools to native runs. Registered by default.
#[derive(Clone)]
pub struct ExtensionToolHost {
    extensions: ExtensionHost,
}
impl ExtensionToolHost {
    pub fn new(extensions: ExtensionHost) -> Self {
        Self { extensions }
    }
    fn offers(&self) -> Offers {
        offers(self.extensions.tool_sources())
    }
}
impl ToolHost for ExtensionToolHost {
    fn list<'a>(&'a self, _: &'a ToolScope) -> BoxFuture<'a, ToolListing> {
        Box::pin(async move {
            let offers = self.offers();
            ToolListing {
                tools: offers
                    .tools
                    .into_iter()
                    .map(|(name, offered)| ToolSpec {
                        name,
                        parameters: parameters_schema(&offered.spec),
                        description: offered.spec.description,
                        capabilities: offered.spec.capabilities,
                        // Extensions add no policy of their own; the user's
                        // setting (deny when unset) decides.
                        permission: ToolPermission::Allow,
                        whole_output: false,
                    })
                    .collect(),
                unavailable: offers.unavailable,
            }
        })
    }

    /// Re-reads the extensions, so a tool disabled or ungranted since the
    /// listing is not reached. `request.approved` adds nothing here:
    /// extensions have no approval step of their own. Dropping the future
    /// kills the extension worker (see [`ExtensionHost::call_tool`]).
    fn call<'a>(
        &'a self,
        scope: &'a ToolScope,
        request: ToolRequest,
    ) -> BoxFuture<'a, Result<ToolOutput, ErrorReport>> {
        Box::pin(async move {
            let report = |error: &(dyn std::error::Error + 'static)| crate::error::report(error);
            let Some(offered) = self.offers().tools.remove(&request.name) else {
                return Err(report(&ExtensionToolError::NotOffered(request.name)));
            };
            let args = arguments(request.arguments).map_err(|e| report(&e))?;
            let scope = Scope {
                workspace: scope.workspace.clone(),
                ceiling: Some(scope.capabilities.clone()),
            };
            let output = self
                .extensions
                .call_tool(&offered.extension, &offered.spec.name, args, scope)
                .await
                .map_err(|e| report(&e))?;
            // A guest's own failure is an `ExtensionError::Guest`, which
            // keeps its gateway-recorded cause as a report; a result here
            // is always a success.
            Ok(ToolOutput {
                text: output.text,
                is_error: false,
            })
        })
    }
}

#[cfg(test)]
mod tests;
