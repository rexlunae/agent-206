//! WASM extensions: manifests, the bounded interpreter and the worker side of
//! the host-call relay. Host I/O and policy live in the gateway, which
//! authorizes each relayed request; nothing here touches files or networks.
#![forbid(unsafe_code)]
pub mod engine;
pub mod manifest;
pub mod worker;

pub use a206_extension_abi as abi;
pub use engine::{AbiViolation, BridgeError, EngineError, HostBridge, Limits};
pub use manifest::{ArgError, Manifest, ManifestError, allows_host, check_args};

use a206_core::{Capability, ErrorKind};
use std::error::Error;

/// The capabilities a host request needs, all of them. Extension-scoped
/// state and logs need none. Approved skill imports are separately
/// restricted to the bundled skills identity and current approval pins by
/// the gateway.
pub fn required_capabilities(request: &abi::HostRequest) -> &'static [Capability] {
    use Capability::*;
    use abi::HostRequest as R;
    match request {
        R::ReadFile { .. }
        | R::ListDirectory { .. }
        | R::Search { .. }
        | R::SearchPattern { .. }
        | R::Find { .. } => &[ReadWorkspace],
        R::WriteFile { .. }
        | R::EditFile { .. }
        | R::CreateDirectory { .. }
        | R::PatchFiles { .. } => &[WriteWorkspace],
        R::GitStatus | R::GitLog { .. } | R::GitDiff { .. } | R::GitBranches | R::GitTags => {
            &[GitRead]
        }
        R::GitCommit { .. }
        | R::GitCommitStaged { .. }
        | R::GitStage { .. }
        | R::GitCreateBranch { .. }
        | R::GitCreateTag { .. } => &[GitWrite],
        // A switch rewrites project files as well as git state.
        R::GitSwitchBranch { .. } => &[GitWrite, WriteWorkspace],
        R::Fetch { .. } => &[Network],
        R::StateGet { .. }
        | R::StatePut { .. }
        | R::Log { .. }
        | R::SkillList
        | R::SkillRead { .. } => &[],
    }
}

/// A bundled extension: a reviewed manifest and its compiled WASM.
#[derive(Debug, Clone)]
pub struct Builtin {
    pub manifest: Manifest,
    pub wasm: &'static [u8],
}

/// Manifest and module of each bundled extension, built by `build.rs`.
const BUNDLED: &[(&str, &[u8])] = &[
    (
        include_str!("../../../plugins/workspace/plugin.toml"),
        include_bytes!(concat!(env!("OUT_DIR"), "/a206_plugin_workspace.wasm")),
    ),
    (
        include_str!("../../../plugins/git/plugin.toml"),
        include_bytes!(concat!(env!("OUT_DIR"), "/a206_plugin_git.wasm")),
    ),
    (
        include_str!("../../../plugins/skills/plugin.toml"),
        include_bytes!(concat!(env!("OUT_DIR"), "/a206_plugin_skills.wasm")),
    ),
    (
        include_str!("../../../plugins/web/plugin.toml"),
        include_bytes!(concat!(env!("OUT_DIR"), "/a206_plugin_web.wasm")),
    ),
    (
        include_str!("../../../plugins/memory/plugin.toml"),
        include_bytes!(concat!(env!("OUT_DIR"), "/a206_plugin_memory.wasm")),
    ),
];

pub fn builtins() -> Result<Vec<Builtin>, ManifestError> {
    BUNDLED
        .iter()
        .map(|(manifest, wasm)| {
            Ok(Builtin {
                manifest: Manifest::parse(manifest)?,
                wasm,
            })
        })
        .collect()
}
pub fn builtin(id: &str) -> Result<Option<Builtin>, ManifestError> {
    Ok(builtins()?.into_iter().find(|b| b.manifest.id == id))
}

/// The client-facing kind of an extension failure, for error reports.
pub fn classify(error: &(dyn Error + 'static)) -> Option<ErrorKind> {
    if error.is::<ManifestError>() {
        return Some(ErrorKind::InvalidRequest);
    }
    if let Some(error) = error.downcast_ref::<ArgError>() {
        return Some(match error {
            ArgError::UnknownTool(_) | ArgError::UnknownPanel(_) | ArgError::UnknownTrigger(_) => {
                ErrorKind::NotFound
            }
            _ => ErrorKind::InvalidRequest,
        });
    }
    if error.is::<AbiViolation>() {
        return Some(ErrorKind::Protocol);
    }
    if error.is::<BridgeError>() {
        return Some(ErrorKind::Unavailable);
    }
    if let Some(error) = error.downcast_ref::<worker::RunError>() {
        use worker::RunError as R;
        return match error {
            R::Builtins(_) => Some(ErrorKind::Internal),
            R::Unknown(_) => Some(ErrorKind::NotFound),
            R::Engine(error) => classify(error),
        };
    }
    let error = error.downcast_ref::<EngineError>()?;
    use EngineError as E;
    Some(match error {
        E::ModuleTooLarge { .. } | E::Module(_) | E::Instantiate(_) | E::InputTooLarge { .. } => {
            ErrorKind::InvalidRequest
        }
        // The extension broke its ABI contract.
        E::MissingExport { .. }
        | E::MissingMemory
        | E::Abi(_)
        | E::OutputTooLarge { .. }
        | E::OutputBounds(_)
        | E::Output(_)
        | E::WrongOutcome { .. }
        | E::View(_)
        | E::Cursor => ErrorKind::Protocol,
        // Fuel is the invocation's compute budget.
        E::OutOfFuel(_) => ErrorKind::Timeout,
        E::Bridge(_) => ErrorKind::Unavailable,
        E::Encode(_) | E::InputBounds(_) => ErrorKind::Internal,
        // A guest trap is the extension's own failure.
        E::Trap(_) => return None,
    })
}

#[cfg(test)]
mod tests;
