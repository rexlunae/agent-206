//! Guest SDK for Agent 206 WASM extensions (ABI v2).
//!
//! Implement [`Extension`] and call [`export!`] once; build the crate for
//! `wasm32-unknown-unknown` as a `cdylib`. Host calls in [`host`] are checked
//! per call against the grant of the invocation, so a guest must handle
//! [`HostErrorCode::Denied`] like any other failure.
//!
//! ```ignore
//! struct Hello;
//! impl a206_extension_sdk::Extension for Hello {}
//! a206_extension_sdk::export!(Hello);
//! ```
pub use a206_extension_abi::*;
use std::cell::RefCell;

/// The calls a guest answers. Unimplemented calls fail with `unknown_tool`
/// or `unknown_panel`; the host only routes names the manifest declares.
pub trait Extension {
    fn tool(name: &str, args: &Args) -> Result<ToolOutput, GuestError> {
        let _unused = args;
        Err(GuestError::new(
            "unknown_tool",
            format!("this extension has no tool `{name}`"),
        ))
    }
    fn panel(id: &str, event: Option<&UiEvent>) -> Result<View, GuestError> {
        let _unused = event;
        Err(GuestError::new(
            "unknown_panel",
            format!("this extension has no panel `{id}`"),
        ))
    }
}

/// Export the ABI entry points for an [`Extension`] implementation.
#[macro_export]
macro_rules! export {
    ($extension:ty) => {
        #[allow(unsafe_code)]
        #[unsafe(no_mangle)]
        pub extern "C" fn a206_buffer(len: i32) -> i32 {
            $crate::__private::buffer(len)
        }
        #[allow(unsafe_code)]
        #[unsafe(no_mangle)]
        pub extern "C" fn a206_invoke(len: i32) -> i64 {
            $crate::__private::invoke::<$extension>(len)
        }
    };
}

/// Longest tool text a guest should return: the outcome limit less room for
/// its framing and any notice a tool appends.
pub const MAX_TOOL_TEXT: usize = 192 * 1024;

/// Cut long text at a character boundary and say so, rather than fail the
/// call by exceeding the outcome limit.
pub fn excerpt(mut text: String) -> String {
    if text.len() <= MAX_TOOL_TEXT {
        return text;
    }
    let total = text.len();
    let mut end = MAX_TOOL_TEXT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text.push_str(&format!(
        "\n\n[truncated: showing the first {end} of {total} bytes]"
    ));
    text
}

/// A string argument. The host has already checked declared types, so a
/// value of another type is reported rather than coerced.
pub fn string_arg<'a>(args: &'a Args, name: &str) -> Result<Option<&'a str>, GuestError> {
    match args.get(name) {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::Str(text)) => Ok(Some(text)),
        Some(_) => Err(GuestError::new(
            "invalid_argument",
            format!("`{name}` must be a string"),
        )),
    }
}

/// An optional boolean argument; the host already checked its declared kind.
pub fn bool_arg(args: &Args, name: &str) -> Result<Option<bool>, GuestError> {
    match args.get(name) {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(GuestError::new(
            "invalid_argument",
            format!("`{name}` must be a boolean"),
        )),
    }
}
/// An optional integer argument; the host already checked its declared kind.
pub fn int_arg(args: &Args, name: &str) -> Result<Option<i64>, GuestError> {
    match args.get(name) {
        None | Some(Value::Nil) => Ok(None),
        Some(Value::Int(value)) => Ok(Some(*value)),
        Some(_) => Err(GuestError::new(
            "invalid_argument",
            format!("`{name}` must be an integer"),
        )),
    }
}

thread_local! {
    static INPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static OUTPUT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Guest addresses fit in 32 bits; the host reads them as unsigned.
fn address(pointer: *const u8) -> i32 {
    pointer as usize as u32 as i32
}

#[doc(hidden)]
pub mod __private {
    use super::*;

    pub fn buffer(len: i32) -> i32 {
        let len = usize::try_from(len).unwrap_or(0);
        INPUT.with_borrow_mut(|input| {
            input.clear();
            input.resize(len, 0);
            address(input.as_ptr())
        })
    }

    pub fn invoke<E: Extension>(len: i32) -> i64 {
        let invocation = INPUT.with_borrow(|input| {
            let bytes = usize::try_from(len)
                .ok()
                .and_then(|len| input.get(..len))
                .unwrap_or_default();
            rmp_serde::from_slice::<Invocation>(bytes)
        });
        let outcome = match invocation {
            Err(_) => Outcome::Failed(GuestError::new(
                "invalid_invocation",
                "the host sent an invocation this guest cannot decode",
            )),
            Ok(invocation) if invocation.abi != ABI => Outcome::Failed(GuestError::new(
                "unsupported_abi",
                format!("this guest implements ABI {ABI}"),
            )),
            Ok(Invocation {
                call: Call::Tool { name, args },
                ..
            }) => E::tool(&name, &args).map_or_else(Outcome::Failed, Outcome::Tool),
            Ok(Invocation {
                call: Call::Panel { id, event },
                ..
            }) => E::panel(&id, event.as_ref()).map_or_else(Outcome::Failed, Outcome::Panel),
        };
        // An encoding failure leaves an empty output, which the host rejects
        // as an ABI violation instead of guessing at the guest's intent.
        let encoded = rmp_serde::to_vec_named(&outcome).unwrap_or_default();
        OUTPUT.with_borrow_mut(|output| {
            *output = encoded;
            ((address(output.as_ptr()) as u32 as i64) << 32) | output.len() as i64
        })
    }
}

/// Host calls. Each one is authorized by the gateway for this invocation.
/// How [`host::search_pattern`] matches. The default is a literal,
/// case-sensitive text in every file.
#[derive(Debug, Clone, Copy, Default)]
pub struct SearchOptions<'a> {
    pub regex: bool,
    pub ignore_case: bool,
    /// Search only files whose path matches this glob.
    pub include: Option<&'a str>,
    /// Also search what git ignores.
    pub include_ignored: bool,
}

pub mod host {
    use super::*;
    use serde_bytes::ByteBuf;

    #[cfg(target_arch = "wasm32")]
    #[allow(unsafe_code)]
    mod ffi {
        // `safe fn`: the host only reads or writes the guest memory range
        // passed to it and traps on any out-of-bounds range.
        #[link(wasm_import_module = "a206")]
        unsafe extern "C" {
            pub safe fn host_call(pointer: i32, len: i32) -> i32;
            pub safe fn host_take(pointer: i32, len: i32) -> i32;
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn transport(request: &[u8]) -> Result<Vec<u8>, HostFailure> {
        let len = i32::try_from(request.len()).map_err(|_| too_large())?;
        let reply_len = ffi::host_call(address(request.as_ptr()), len);
        let mut reply = vec![0; usize::try_from(reply_len).map_err(|_| unexpected())?];
        ffi::host_take(address(reply.as_mut_ptr()), reply_len);
        Ok(reply)
    }
    #[cfg(not(target_arch = "wasm32"))]
    fn transport(_request: &[u8]) -> Result<Vec<u8>, HostFailure> {
        Err(HostFailure {
            code: HostErrorCode::Unsupported,
            message: "host calls are only available inside the Agent 206 WASM host".into(),
        })
    }

    fn too_large() -> HostFailure {
        HostFailure {
            code: HostErrorCode::TooLarge,
            message: "host request is too large".into(),
        }
    }
    fn unexpected() -> HostFailure {
        HostFailure {
            code: HostErrorCode::Other,
            message: "the host sent an unexpected reply".into(),
        }
    }

    /// Send one request. A `Failed` reply is returned as `Err`.
    pub fn call(request: &HostRequest) -> Result<HostReply, HostFailure> {
        let bytes = rmp_serde::to_vec_named(request).map_err(|_| too_large())?;
        match rmp_serde::from_slice(&transport(&bytes)?).map_err(|_| unexpected())? {
            HostReply::Failed(failure) => Err(failure),
            reply => Ok(reply),
        }
    }

    /// Read the complete approved skill index. The host restricts this import
    /// to the skills builtin; other extensions receive Denied.
    pub fn skill_list() -> Result<String, HostFailure> {
        skill_text(HostRequest::SkillList)
    }
    /// Read an approved skill or one supporting file without workspace authority.
    pub fn skill_read(name: &str, path: Option<&str>) -> Result<String, HostFailure> {
        skill_text(HostRequest::SkillRead {
            name: name.into(),
            path: path.map(str::to_owned),
        })
    }
    fn skill_text(request: HostRequest) -> Result<String, HostFailure> {
        match call(&request)? {
            HostReply::File(bytes) => String::from_utf8(bytes.into_vec()).map_err(|_| unexpected()),
            _ => Err(unexpected()),
        }
    }
    /// Read a file relative to the invocation's workspace.
    pub fn read_file(path: &str) -> Result<Vec<u8>, HostFailure> {
        match call(&HostRequest::ReadFile { path: path.into() })? {
            HostReply::File(bytes) => Ok(bytes.into_vec()),
            _ => Err(unexpected()),
        }
    }
    /// List a directory relative to the invocation's workspace.
    pub fn list_directory(path: &str) -> Result<Vec<DirEntry>, HostFailure> {
        match call(&HostRequest::ListDirectory { path: path.into() })? {
            HostReply::Directory(entries) => Ok(entries),
            _ => Err(unexpected()),
        }
    }
    /// Lines containing `query` in UTF-8 files under `path`.
    pub fn search(path: &str, query: &str, max_matches: u32) -> Result<SearchResult, HostFailure> {
        match call(&HostRequest::Search {
            path: path.into(),
            query: query.into(),
            max_matches,
        })? {
            HostReply::Matches(result) => Ok(result),
            _ => Err(unexpected()),
        }
    }
    /// Lines matching `pattern` in UTF-8 files under `path`; see
    /// [`HostRequest::SearchPattern`].
    pub fn search_pattern(
        path: &str,
        pattern: &str,
        options: SearchOptions<'_>,
        max_matches: u32,
    ) -> Result<SearchResult, HostFailure> {
        match call(&HostRequest::SearchPattern {
            path: path.into(),
            pattern: pattern.into(),
            regex: options.regex,
            ignore_case: options.ignore_case,
            include: options.include.map(Into::into),
            max_matches,
            include_ignored: options.include_ignored,
        })? {
            HostReply::Matches(result) => Ok(result),
            _ => Err(unexpected()),
        }
    }
    /// Entries under `path` matching `glob` that git does not ignore; see
    /// [`HostRequest::Find`].
    pub fn find(path: &str, glob: &str, max_results: u32) -> Result<FindResult, HostFailure> {
        find_including(path, glob, false, max_results)
    }
    /// [`find`], also returning what git ignores when `include_ignored`.
    pub fn find_including(
        path: &str,
        glob: &str,
        include_ignored: bool,
        max_results: u32,
    ) -> Result<FindResult, HostFailure> {
        match call(&HostRequest::Find {
            path: path.into(),
            glob: glob.into(),
            max_results,
            include_ignored,
        })? {
            HostReply::Found(result) => Ok(result),
            _ => Err(unexpected()),
        }
    }
    /// Write a whole file atomically; `overwrite` allows replacing one.
    pub fn write_file(path: &str, contents: &[u8], overwrite: bool) -> Result<(), HostFailure> {
        done(call(&HostRequest::WriteFile {
            path: path.into(),
            contents: ByteBuf::from(contents.to_vec()),
            overwrite,
        })?)
    }
    /// Replace the only occurrence of `old` with `new` in a text file.
    pub fn edit_file(path: &str, old: &str, new: &str) -> Result<(), HostFailure> {
        done(call(&HostRequest::EditFile {
            path: path.into(),
            old: old.into(),
            new: new.into(),
        })?)
    }
    /// Apply `files` together; see [`HostRequest::PatchFiles`].
    pub fn patch_files(
        files: Vec<FilePatch>,
        check_only: bool,
    ) -> Result<PatchResult, HostFailure> {
        match call(&HostRequest::PatchFiles { files, check_only })? {
            HostReply::Patched(result) => Ok(result),
            _ => Err(unexpected()),
        }
    }
    /// Create a directory and any missing parents.
    pub fn create_directory(path: &str) -> Result<(), HostFailure> {
        done(call(&HostRequest::CreateDirectory { path: path.into() })?)
    }
    /// The branch, `HEAD` and changed paths of the workspace's repository.
    pub fn git_status() -> Result<GitStatus, HostFailure> {
        match call(&HostRequest::GitStatus)? {
            HostReply::GitStatus(status) => Ok(status),
            _ => Err(unexpected()),
        }
    }
    /// Up to `max` commits reachable from `HEAD`, newest first.
    pub fn git_log(max: u32) -> Result<Vec<GitCommit>, HostFailure> {
        match call(&HostRequest::GitLog { max })? {
            HostReply::Commits(commits) => Ok(commits),
            _ => Err(unexpected()),
        }
    }
    /// A unified diff of one file, staged or unstaged.
    pub fn git_diff(path: &str, staged: bool) -> Result<GitDiff, HostFailure> {
        match call(&HostRequest::GitDiff {
            path: path.into(),
            staged,
        })? {
            HostReply::Diff(diff) => Ok(diff),
            _ => Err(unexpected()),
        }
    }
    /// Commit the working-tree content of exactly `paths` on top of `HEAD`.
    pub fn git_commit(message: &str, paths: &[&str]) -> Result<GitCommit, HostFailure> {
        match call(&HostRequest::GitCommit {
            message: message.into(),
            paths: paths.iter().map(|path| (*path).into()).collect(),
        })? {
            HostReply::Committed(commit) => Ok(commit),
            _ => Err(unexpected()),
        }
    }
    /// Stage the working-tree content of exactly `paths`, or with
    /// `unstage`, set them back to `HEAD`. Only the index changes.
    pub fn git_stage(paths: &[&str], unstage: bool) -> Result<(), HostFailure> {
        done(call(&HostRequest::GitStage {
            paths: paths.iter().map(|path| (*path).into()).collect(),
            unstage,
        })?)
    }
    /// `GET` a public HTTPS URL on a host the manifest declares. Any final
    /// status is a response, not a failure.
    pub fn fetch(url: &str) -> Result<FetchResponse, HostFailure> {
        match call(&HostRequest::Fetch { url: url.into() })? {
            HostReply::Fetched(response) => Ok(response),
            _ => Err(unexpected()),
        }
    }
    fn done(reply: HostReply) -> Result<(), HostFailure> {
        match reply {
            HostReply::Done => Ok(()),
            _ => Err(unexpected()),
        }
    }
    /// This extension's persisted value for `key`.
    pub fn state_get(key: &str) -> Result<Option<Vec<u8>>, HostFailure> {
        match call(&HostRequest::StateGet { key: key.into() })? {
            HostReply::State(value) => Ok(value.map(ByteBuf::into_vec)),
            _ => Err(unexpected()),
        }
    }
    /// Store (`Some`) or delete (`None`) this extension's value for `key`.
    pub fn state_put(key: &str, value: Option<&[u8]>) -> Result<(), HostFailure> {
        let value = value.map(|bytes| ByteBuf::from(bytes.to_vec()));
        match call(&HostRequest::StatePut {
            key: key.into(),
            value,
        })? {
            HostReply::Done => Ok(()),
            _ => Err(unexpected()),
        }
    }
    /// Record a diagnostic in the gateway's log.
    pub fn log(level: LogLevel, message: &str) -> Result<(), HostFailure> {
        match call(&HostRequest::Log {
            level,
            message: message.into(),
        })? {
            HostReply::Done => Ok(()),
            _ => Err(unexpected()),
        }
    }
}
