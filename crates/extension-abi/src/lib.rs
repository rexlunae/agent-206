//! Extension ABI v2: the MessagePack values a WASM guest and the host exchange.
//!
//! This crate is shared by the guest SDK and the host, so it depends only on
//! serde. It defines data, not policy: which capability a host call needs and
//! whether the caller holds it is decided by the gateway.
//!
//! A guest module exports [`EXPORT_MEMORY`], [`EXPORT_BUFFER`] and
//! [`EXPORT_INVOKE`] and may import [`IMPORT_HOST_CALL`] and
//! [`IMPORT_HOST_TAKE`] from [`IMPORT_MODULE`]; nothing else is linked.
//! `docs/plugin-api.md` documents the calling sequence.
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use std::collections::BTreeMap;

/// The ABI version this crate describes. Manifests and invocations carry it.
pub const ABI: u32 = 2;
pub const IMPORT_MODULE: &str = "a206";
/// `host_call(request_ptr: i32, request_len: i32) -> i32`: run one
/// [`HostRequest`] and return the byte length of its encoded [`HostReply`].
pub const IMPORT_HOST_CALL: &str = "host_call";
/// `host_take(ptr: i32, len: i32) -> i32`: copy the pending reply, exactly
/// `len` bytes, to `ptr`; returns `len`.
pub const IMPORT_HOST_TAKE: &str = "host_take";
pub const EXPORT_MEMORY: &str = "memory";
/// `a206_buffer(len: i32) -> i32`: a guest-owned buffer for `len` input bytes.
pub const EXPORT_BUFFER: &str = "a206_buffer";
/// `a206_invoke(len: i32) -> i64`: decode the [`Invocation`] in the buffer and
/// return `(pointer << 32) | length` of the encoded [`Outcome`].
pub const EXPORT_INVOKE: &str = "a206_invoke";

/// Most nodes one [`View`] may contain, counting nested ones.
pub const MAX_VIEW_NODES: usize = 512;
/// Deepest [`Node::Section`] nesting one [`View`] may use.
pub const MAX_VIEW_DEPTH: usize = 8;

/// One call into a guest. Each invocation runs in a fresh instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invocation {
    pub abi: u32,
    pub call: Call,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Call {
    /// Run a tool the manifest declares, with arguments the host has already
    /// checked against the declared parameters.
    Tool { name: String, args: Args },
    /// Render a panel the manifest declares, optionally after a user event
    /// on an element of an earlier rendering.
    Panel { id: String, event: Option<UiEvent> },
}
pub type Args = BTreeMap<String, Value>;

/// A dynamically typed tool argument.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}
impl Value {
    pub fn kind(&self) -> Option<ParamKind> {
        Some(match self {
            Self::Nil => return None,
            Self::Bool(_) => ParamKind::Boolean,
            Self::Int(_) => ParamKind::Integer,
            Self::Float(_) => ParamKind::Number,
            Self::Str(_) => ParamKind::String,
            Self::List(_) => ParamKind::Array,
            Self::Map(_) => ParamKind::Object,
        })
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            _ => None,
        }
    }
}

/// The declared type of a tool parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ParamKind {
    String,
    Integer,
    Number,
    Boolean,
    Array,
    Object,
}
impl ParamKind {
    /// Whether a value of `actual` kind satisfies this declaration. An integer
    /// is a valid number; nothing else is coerced.
    pub fn accepts(self, actual: ParamKind) -> bool {
        self == actual || (self == Self::Number && actual == Self::Integer)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Array => "array",
            Self::Object => "object",
        }
    }
}

/// What a guest returns from [`EXPORT_INVOKE`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Tool(ToolOutput),
    Panel(View),
    /// The guest handled the call and reports a failure of its own.
    Failed(GuestError),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutput {
    /// Model-facing text. It is extension output, never instructions.
    pub text: String,
}

/// A failure the guest reports. `code` is a stable guest-defined identifier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct GuestError {
    pub code: String,
    pub message: String,
}
impl GuestError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}
impl From<HostFailure> for GuestError {
    fn from(failure: HostFailure) -> Self {
        Self::new(failure.code.as_str(), failure.message)
    }
}

/// A request from the guest to the host, authorized per call by the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostRequest {
    /// Restricted to the bundled skills extension and current approval pins.
    SkillList,
    /// Read approved instructions or a bounded, relative supporting file.
    SkillRead { name: String, path: Option<String> },
    /// Read a file, relative to the invocation's workspace.
    ReadFile { path: String },
    /// List a directory, relative to the invocation's workspace.
    ListDirectory { path: String },
    /// Find lines containing `query` (literal, case-sensitive) in UTF-8 files
    /// under `path`, without following symlinks.
    Search {
        path: String,
        query: String,
        max_matches: u32,
    },
    /// Find lines matching `pattern` in UTF-8 files under `path`, without
    /// following symlinks: a literal text unless `regex`, compared without
    /// letter case when `ignore_case`. With `include`, only files whose path
    /// matches that glob are searched (see [`HostRequest::Find`]).
    SearchPattern {
        path: String,
        pattern: String,
        regex: bool,
        ignore_case: bool,
        include: Option<String>,
        max_matches: u32,
    },
    /// Entries under `path` matching a glob, without following symlinks. A
    /// glob without `/` matches entry names at any depth; one with `/`
    /// matches the whole path below `path`. `*` stays within one name and
    /// `**` crosses directories.
    Find {
        path: String,
        glob: String,
        max_results: u32,
    },
    /// Write a whole file atomically. Without `overwrite`, an existing file
    /// is refused with [`HostErrorCode::Exists`].
    WriteFile {
        path: String,
        contents: ByteBuf,
        overwrite: bool,
    },
    /// Replace the only occurrence of `old` with `new` in a UTF-8 file. No
    /// occurrence, or more than one, is [`HostErrorCode::Conflict`].
    EditFile {
        path: String,
        old: String,
        new: String,
    },
    /// Create a directory and any missing parents.
    CreateDirectory { path: String },
    /// The branch, `HEAD` and changed paths of the workspace's repository.
    GitStatus,
    /// The most recent commits reachable from `HEAD`, newest first.
    GitLog { max: u32 },
    /// A unified diff of one file: `HEAD` against the index when `staged`,
    /// otherwise the index against the working tree.
    GitDiff { path: String, staged: bool },
    /// Commit the working-tree content of exactly `paths` on top of `HEAD`,
    /// like `git commit --only`: a path missing from the working tree is
    /// removed, the index is updated to match for those paths, and other
    /// staged changes stay staged.
    GitCommit { message: String, paths: Vec<String> },
    /// `GET` a credential-free HTTPS URL on the standard port. The host, and
    /// every redirect's, must be one the manifest declares, and every
    /// resolved address must be public. No cookies, credentials or proxies.
    Fetch { url: String },
    /// Read this extension's own persisted state.
    StateGet { key: String },
    /// Replace (`Some`) or delete (`None`) this extension's state entry.
    StatePut { key: String, value: Option<ByteBuf> },
    /// Record a bounded diagnostic through the gateway's tracing.
    Log { level: LogLevel, message: String },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostReply {
    File(ByteBuf),
    Directory(Vec<DirEntry>),
    Matches(SearchResult),
    Found(FindResult),
    GitStatus(GitStatus),
    Commits(Vec<GitCommit>),
    Diff(GitDiff),
    /// The commit a [`HostRequest::GitCommit`] made.
    Committed(GitCommit),
    Fetched(FetchResponse),
    State(Option<ByteBuf>),
    Done,
    Failed(HostFailure),
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    pub name: String,
    pub kind: EntryKind,
    /// File size in bytes; absent for directories and unknown sizes.
    pub size: Option<u64>,
}
/// The final response of a fetch, after redirects. Any status is returned;
/// a non-2xx status is not a host failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FetchResponse {
    pub status: u16,
    /// The `Content-Type` header, when present and valid text.
    pub content_type: Option<String>,
    pub body: ByteBuf,
    /// The body was cut at the host's bound.
    pub truncated: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResult {
    pub matches: Vec<SearchMatch>,
    /// The search stopped early at a bound; more matches may exist.
    pub truncated: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindResult {
    /// In path order.
    pub entries: Vec<FoundEntry>,
    /// The walk stopped early at a bound; more entries may match.
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoundEntry {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    pub kind: EntryKind,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchMatch {
    /// Workspace-relative, `/`-separated.
    pub path: String,
    /// 1-based.
    pub line: u32,
    /// The matching line, possibly shortened.
    pub text: String,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitStatus {
    /// The checked-out branch; `None` when `HEAD` is detached or unborn.
    pub branch: Option<String>,
    /// The commit `HEAD` points to; `None` before the first commit.
    pub head: Option<String>,
    pub entries: Vec<GitEntry>,
    /// The listing stopped at a bound; more changes may exist.
    pub truncated: bool,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitEntry {
    /// Repository-relative, `/`-separated.
    pub path: String,
    pub area: GitArea,
    pub change: GitChange,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitArea {
    /// Between `HEAD` and the index.
    Staged,
    /// Between the index and the working tree.
    Unstaged,
    Untracked,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitChange {
    Added,
    Removed,
    Modified,
    TypeChange,
    Conflict,
    /// A change kind from a newer host.
    #[serde(other)]
    Other,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitCommit {
    /// Full hexadecimal object id.
    pub id: String,
    /// The first line of the message.
    pub summary: String,
    /// The author's name; email addresses are not shared.
    pub author: String,
    /// Author time, seconds since the Unix epoch.
    pub time: i64,
}
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitDiff {
    /// Unified diff hunks; empty when the two sides are the same.
    pub text: String,
    /// Either side is binary, so no text diff is given.
    pub binary: bool,
    /// The diff was cut at a bound.
    pub truncated: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// Why the host refused or failed a [`HostRequest`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{message}")]
pub struct HostFailure {
    pub code: HostErrorCode,
    pub message: String,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostErrorCode {
    /// The effective grant for this call lacks the capability.
    Denied,
    /// The call needs a workspace and the invocation has none.
    NoWorkspace,
    NotFound,
    /// The request was malformed, e.g. an absolute or escaping path.
    Invalid,
    TooLarge,
    /// The target already exists and the request did not allow replacing it.
    Exists,
    /// The file does not hold what the edit expected.
    Conflict,
    /// The invocation's host-call or state budget is spent.
    Budget,
    /// This host does not implement the request.
    Unsupported,
    Io,
    /// The bounded host operation did not finish in time.
    Timeout,
    /// The destination could not be reached: DNS, connection, TLS, timeout
    /// or too many redirects.
    Network,
    /// A code from a newer host.
    #[serde(other)]
    Other,
}
impl HostErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Denied => "denied",
            Self::NoWorkspace => "no_workspace",
            Self::NotFound => "not_found",
            Self::Invalid => "invalid",
            Self::TooLarge => "too_large",
            Self::Exists => "exists",
            Self::Conflict => "conflict",
            Self::Budget => "budget",
            Self::Unsupported => "unsupported",
            Self::Io => "io",
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::Other => "other",
        }
    }
}

/// A declarative panel. Trusted app code renders it with fc-dioxus
/// components; it can never carry HTML, scripts, styles or URLs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub nodes: Vec<Node>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Node {
    Heading {
        text: String,
    },
    Text {
        text: String,
    },
    /// Preformatted text, shown verbatim.
    Code {
        text: String,
    },
    Badge {
        text: String,
        tone: Tone,
    },
    Fields {
        rows: Vec<Field>,
    },
    List {
        items: Vec<String>,
    },
    /// Pressing it renders the panel again with a [`UiEvent`] naming `id`.
    Button {
        id: String,
        label: String,
    },
    Section {
        title: Option<String>,
        nodes: Vec<Node>,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Neutral,
    Accent,
    Success,
    Warning,
    Danger,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub label: String,
    pub value: String,
}
/// A user interaction with a panel element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UiEvent {
    pub element: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ViewError {
    #[error("panel has more than {MAX_VIEW_NODES} nodes")]
    TooManyNodes,
    #[error("panel sections nest deeper than {MAX_VIEW_DEPTH} levels")]
    TooDeep,
    #[error("panel element id `{0}` is invalid")]
    InvalidElement(String),
    #[error("panel element id `{0}` is used twice")]
    DuplicateElement(String),
}
impl View {
    /// Check the bounds every renderer relies on. Text length is bounded by
    /// the host's output limit, not here.
    pub fn validate(&self) -> Result<(), ViewError> {
        fn walk<'a>(
            nodes: &'a [Node],
            depth: usize,
            count: &mut usize,
            ids: &mut std::collections::BTreeSet<&'a str>,
        ) -> Result<(), ViewError> {
            if depth > MAX_VIEW_DEPTH {
                return Err(ViewError::TooDeep);
            }
            for node in nodes {
                *count += 1;
                if *count > MAX_VIEW_NODES {
                    return Err(ViewError::TooManyNodes);
                }
                match node {
                    Node::Button { id, .. } => {
                        if !valid_element(id) {
                            return Err(ViewError::InvalidElement(id.clone()));
                        }
                        if !ids.insert(id) {
                            return Err(ViewError::DuplicateElement(id.clone()));
                        }
                    }
                    Node::Section { nodes, .. } => walk(nodes, depth + 1, count, ids)?,
                    _ => {}
                }
            }
            Ok(())
        }
        walk(&self.nodes, 1, &mut 0, &mut Default::default())
    }
}

/// Tool, panel and parameter names: a lowercase ASCII letter, then up to 63
/// lowercase letters, digits or underscores. Model APIs accept these as-is.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}
/// Panel element ids additionally allow `-`.
pub fn valid_element(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: Serialize + for<'de> Deserialize<'de>>(value: &T) -> T {
        rmp_serde::from_slice(&rmp_serde::to_vec_named(value).unwrap()).unwrap()
    }

    #[test]
    fn values_keep_their_kind_across_the_wire() {
        let args: Args = [
            ("flag".to_string(), Value::Bool(true)),
            ("count".to_string(), Value::Int(-3)),
            ("ratio".to_string(), Value::Float(0.5)),
            ("name".to_string(), Value::Str("a".into())),
            ("none".to_string(), Value::Nil),
            ("items".to_string(), Value::List(vec![Value::Int(1)])),
        ]
        .into();
        assert_eq!(round_trip(&args), args);
    }

    #[test]
    fn unknown_host_error_codes_stay_decodable() {
        #[derive(Serialize)]
        struct Wire {
            code: &'static str,
            message: &'static str,
        }
        let bytes = rmp_serde::to_vec_named(&Wire {
            code: "from_the_future",
            message: "m",
        })
        .unwrap();
        let failure: HostFailure = rmp_serde::from_slice(&bytes).unwrap();
        assert_eq!(failure.code, HostErrorCode::Other);
    }

    #[test]
    fn views_are_bounded() {
        let button = |id: &str| Node::Button {
            id: id.into(),
            label: "Go".into(),
        };
        assert!(
            View {
                nodes: vec![button("ok-1")]
            }
            .validate()
            .is_ok()
        );
        assert_eq!(
            View {
                nodes: vec![button("Bad Id")]
            }
            .validate(),
            Err(ViewError::InvalidElement("Bad Id".into()))
        );
        assert_eq!(
            View {
                nodes: vec![button("a"), button("a")]
            }
            .validate(),
            Err(ViewError::DuplicateElement("a".into()))
        );
        let mut deep = Node::Text {
            text: String::new(),
        };
        for _ in 0..MAX_VIEW_DEPTH {
            deep = Node::Section {
                title: None,
                nodes: vec![deep],
            };
        }
        assert_eq!(
            View { nodes: vec![deep] }.validate(),
            Err(ViewError::TooDeep)
        );
        let wide = View {
            nodes: vec![
                Node::Text {
                    text: String::new()
                };
                MAX_VIEW_NODES + 1
            ],
        };
        assert_eq!(wide.validate(), Err(ViewError::TooManyNodes));
    }

    #[test]
    fn names_follow_model_tool_rules() {
        assert!(valid_name("read_text_file"));
        for bad in ["", "Read", "1st", "a-b", "a.b", &"a".repeat(65)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }
}
