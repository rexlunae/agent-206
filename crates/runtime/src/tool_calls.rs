//! Tool calls assembled from streamed provider responses, and the repairs the
//! run engine applies before deciding on them.
//!
//! Argument healing and in-response de-duplication are adapted from
//! RustyClaw's `tool_healing.rs` (MIT, see NOTICE). Only repairs that cannot
//! change what a call means are kept: a code fence or prose around the
//! object, and trailing commas. Truncated arguments are *not* completed, since
//! closing a cut-off value would run a different call than the model intended
//! (for example writing half a file); they are reported back to the model.
use a206_core::ErrorKind;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Most tool calls accepted in one model response.
pub const MAX_CALLS: usize = 64;
/// Longest argument text accepted for one call.
pub const MAX_ARGUMENTS: usize = 64 * 1024;
/// Longest provider call ID or replay signature kept.
const MAX_TOKEN: usize = 16 * 1024;

/// A call as the provider streamed it, before any decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamedCall {
    pub id: Option<String>,
    pub name: String,
    /// Argument text exactly as received.
    pub arguments: String,
    pub signature: Option<String>,
}

/// Why a streamed call is unusable as a whole response.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    #[error("the provider streamed more than {MAX_CALLS} tool calls in one response")]
    TooMany,
    #[error("tool call arguments exceed {MAX_ARGUMENTS} bytes")]
    TooLarge,
    #[error("the provider streamed a tool call without a name")]
    Unnamed,
    #[error("the provider streamed tool call data for a call it never started")]
    Orphan,
}
impl crate::error::Classified for StreamError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(ErrorKind::Provider)
    }
}

/// Tool calls being assembled from one response, keyed by the protocol's
/// own block or call index so interleaved deltas land on the right call.
#[derive(Debug, Default)]
pub struct Assembler {
    calls: BTreeMap<u64, StreamedCall>,
    /// The next index for protocols that send whole calls without one.
    next: u64,
}
impl Assembler {
    /// Start a call or extend one (OpenAI-style deltas). The first non-empty
    /// ID and name win; providers that repeat them in later deltas are fine.
    pub fn delta(
        &mut self,
        index: u64,
        id: Option<&str>,
        name: Option<&str>,
        arguments: Option<&str>,
    ) -> Result<(), StreamError> {
        if !self.calls.contains_key(&index) && self.calls.len() >= MAX_CALLS {
            return Err(StreamError::TooMany);
        }
        let call = self.calls.entry(index).or_insert_with(|| StreamedCall {
            id: None,
            name: String::new(),
            arguments: String::new(),
            signature: None,
        });
        if call.id.is_none()
            && let Some(id) = id.filter(|id| !id.is_empty())
        {
            call.id = Some(bounded(id));
        }
        if call.name.is_empty()
            && let Some(name) = name
        {
            call.name = bounded(name);
        }
        if let Some(more) = arguments {
            if call.arguments.len() + more.len() > MAX_ARGUMENTS {
                return Err(StreamError::TooLarge);
            }
            call.arguments.push_str(more);
        }
        Ok(())
    }
    /// Extend a call that must already have been started (Anthropic blocks).
    pub fn append(&mut self, index: u64, arguments: &str) -> Result<(), StreamError> {
        if !self.calls.contains_key(&index) {
            return Err(StreamError::Orphan);
        }
        self.delta(index, None, None, Some(arguments))
    }
    pub fn started(&self, index: u64) -> bool {
        self.calls.contains_key(&index)
    }
    /// A call that arrives whole (Gemini parts).
    pub fn whole(&mut self, call: StreamedCall) -> Result<(), StreamError> {
        if self.calls.len() >= MAX_CALLS {
            return Err(StreamError::TooMany);
        }
        if call.arguments.len() > MAX_ARGUMENTS {
            return Err(StreamError::TooLarge);
        }
        let index = self.next;
        self.next += 1;
        self.calls.insert(
            index,
            StreamedCall {
                id: call.id.as_deref().filter(|id| !id.is_empty()).map(bounded),
                name: bounded(&call.name),
                arguments: call.arguments,
                signature: call.signature.as_deref().map(bounded),
            },
        );
        Ok(())
    }
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
    pub fn finish(self) -> Result<Vec<StreamedCall>, StreamError> {
        let calls: Vec<_> = self.calls.into_values().collect();
        if calls.iter().any(|call| call.name.is_empty()) {
            return Err(StreamError::Unnamed);
        }
        Ok(calls)
    }
}
fn bounded(text: &str) -> String {
    let mut end = text.len().min(MAX_TOKEN);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// Why a call's arguments cannot be used.
#[derive(Debug, thiserror::Error)]
pub enum ArgumentError {
    #[error("tool arguments must be a JSON object")]
    NotObject,
    #[error("tool arguments were cut off before the JSON object ended; send the complete call")]
    Truncated,
    #[error("tool arguments are not valid JSON")]
    Invalid(#[source] serde_json::Error),
}
impl crate::error::Classified for ArgumentError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(ErrorKind::InvalidRequest)
    }
}

/// Arguments ready to run: the object, its canonical text (keys sorted, so
/// equal calls have equal text) and whether a repair was needed.
#[derive(Debug, Clone, PartialEq)]
pub struct Arguments {
    pub object: Map<String, Value>,
    pub text: String,
    pub healed: bool,
}

/// Parse a call's argument text, repairing only meaning-preserving damage.
/// An empty string is an empty object: some servers send that for tools
/// without parameters.
pub fn parse_arguments(raw: &str) -> Result<Arguments, ArgumentError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(finish(Map::new(), false));
    }
    let original = match serde_json::from_str::<Value>(trimmed) {
        Ok(Value::Object(object)) => return Ok(finish(object, false)),
        Ok(_) => return Err(ArgumentError::NotObject),
        Err(error) => error,
    };
    // Prose or a fence around the object: take the outermost braces.
    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}'))
        && start < end
    {
        let candidate = &trimmed[start..=end];
        for attempt in [candidate.to_owned(), strip_trailing_commas(candidate)] {
            if let Ok(Value::Object(object)) = serde_json::from_str::<Value>(&attempt)
                && balanced(&attempt)
            {
                return Ok(finish(object, true));
            }
        }
    }
    if original.is_eof() || !balanced(trimmed) {
        return Err(ArgumentError::Truncated);
    }
    Err(ArgumentError::Invalid(original))
}
fn finish(object: Map<String, Value>, healed: bool) -> Arguments {
    let object = sorted(object);
    // Serializing a `Value` with string keys cannot fail.
    let text = serde_json::to_string(&object).unwrap_or_default();
    Arguments {
        object,
        text,
        healed,
    }
}
fn sorted(object: Map<String, Value>) -> Map<String, Value> {
    let sorted: BTreeMap<_, _> = object
        .into_iter()
        .map(|(key, value)| (key, canonical(value)))
        .collect();
    sorted.into_iter().collect()
}
/// Sort object keys recursively.
fn canonical(value: Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(sorted(object)),
        Value::Array(items) => Value::Array(items.into_iter().map(canonical).collect()),
        other => other,
    }
}
/// Braces and brackets outside strings close in order and strings end.
fn balanced(text: &str) -> bool {
    let mut stack = vec![];
    let (mut in_string, mut escaped) = (false, false);
    for c in text.chars() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => stack.push('}'),
            '[' => stack.push(']'),
            '}' | ']' if stack.pop() != Some(c) => return false,
            _ => {}
        }
    }
    !in_string && stack.is_empty()
}
/// Remove commas directly before a closing brace or bracket, outside strings.
fn strip_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let (mut in_string, mut escaped) = (false, false);
    for c in input.chars() {
        if in_string {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '}' | ']' => {
                let kept = out.trim_end().len();
                out.truncate(kept);
                if out.ends_with(',') {
                    out.pop();
                }
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// What an approval is bound to: this tool with exactly this argument text.
pub fn digest(tool: &str, arguments: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(tool.as_bytes());
    hash.update([0]);
    hash.update(arguments.as_bytes());
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key() -> String {
        uuid::Uuid::new_v4().simple().to_string()
    }
    #[test]
    fn well_formed_arguments_are_canonical_and_not_healed() {
        let (a, b) = (key(), key());
        let parsed =
            parse_arguments(&format!(r#"{{"{b}": 1, "{a}": {{"{b}": 2, "{a}": 3}}}}"#)).unwrap();
        assert!(!parsed.healed);
        let (first, second) = if a < b { (&a, &b) } else { (&b, &a) };
        let inner = if a < b {
            format!(r#"{{"{a}":3,"{b}":2}}"#)
        } else {
            format!(r#"{{"{b}":2,"{a}":3}}"#)
        };
        let expected = if first == &a {
            format!(r#"{{"{first}":{inner},"{second}":1}}"#)
        } else {
            format!(r#"{{"{first}":1,"{second}":{inner}}}"#)
        };
        assert_eq!(parsed.text, expected);
        assert_eq!(parse_arguments("  ").unwrap().text, "{}");
    }
    #[test]
    fn fences_prose_and_trailing_commas_heal() {
        let (name, value) = (key(), key());
        for damaged in [
            format!("```json\n{{\"{name}\": \"{value}\"}}\n```"),
            format!("Calling now: {{\"{name}\": \"{value}\"}} done"),
            format!("{{\"{name}\": [\"{value}\",],}}"),
        ] {
            let parsed = parse_arguments(&damaged).unwrap();
            assert!(parsed.healed, "{damaged}");
            assert!(parsed.text.contains(&value));
        }
    }
    #[test]
    fn truncated_or_non_object_arguments_are_refused() {
        let (name, value) = (key(), key());
        for cut in [
            format!("{{\"{name}\": \"{value}"),
            format!("{{\"{name}\": [\"{value}\""),
            format!("{{\"{name}\":"),
            // A brace inside a string must not make a cut-off call look whole.
            format!("{{\"{name}\": \"}}{value}"),
        ] {
            assert!(
                matches!(parse_arguments(&cut), Err(ArgumentError::Truncated)),
                "{cut}"
            );
        }
        assert!(matches!(
            parse_arguments(&format!("[\"{value}\"]")),
            Err(ArgumentError::NotObject)
        ));
        assert!(matches!(
            parse_arguments(&format!("{{{value}}}")),
            Err(ArgumentError::Invalid(_))
        ));
    }
    #[test]
    fn interleaved_deltas_assemble_by_index() {
        let mut calls = Assembler::default();
        let (a, b) = (key(), key());
        calls.delta(1, Some(&b), Some(&b), Some("{\"x\":")).unwrap();
        calls.delta(0, Some(&a), Some(&a), Some("{")).unwrap();
        calls.delta(1, Some(&b), None, Some("1}")).unwrap();
        calls.delta(0, None, None, Some("}")).unwrap();
        assert!(matches!(calls.append(7, "{}"), Err(StreamError::Orphan)));
        let calls = calls.finish().unwrap();
        assert_eq!(calls[0].name, a);
        assert_eq!(calls[0].arguments, "{}");
        assert_eq!(
            (calls[1].id.as_deref(), calls[1].arguments.as_str()),
            (Some(b.as_str()), "{\"x\":1}")
        );
        let mut unnamed = Assembler::default();
        unnamed.delta(0, None, None, Some("{}")).unwrap();
        assert!(matches!(unnamed.finish(), Err(StreamError::Unnamed)));
    }
    #[test]
    fn digests_bind_tool_and_arguments() {
        let (tool, arguments) = (key(), key());
        assert_eq!(digest(&tool, &arguments), digest(&tool, &arguments));
        assert_ne!(digest(&tool, &arguments), digest(&tool, &key()));
        assert_ne!(digest(&tool, &arguments), digest(&key(), &arguments));
    }
}
