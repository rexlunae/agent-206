//! Reads the user's agent profiles (`a206_core::profiles`) from the gateway
//! profile's `agent-profiles` directory.
//!
//! Each `<id>.toml` file is one profile. A file that cannot be used is
//! reported beside the profiles that can, so one mistake does not hide the
//! rest. Only regular files are read, each bounded; the directory is in the
//! gateway's protected state, which agents' tools cannot write.
use a206_core::{
    AgentProfile, AgentProfiles, Capability, ErrorKind, Mode, ModelKey, ProfileProblem,
    ProviderConfig,
    profiles::{MAX_DESCRIPTION, MAX_FILE, MAX_INSTRUCTIONS, MAX_NAME, MAX_PROFILES, valid_id},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    io::Read,
    path::{Path, PathBuf},
};

/// Why profiles, or one profile, could not be read.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error("could not list the agent profile directory {path}")]
    Directory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read the profile file")]
    Read(#[source] std::io::Error),
    #[error("profile files must be regular files")]
    NotRegular,
    #[error("profile files are limited to {MAX_FILE} bytes")]
    TooLarge,
    #[error("the profile file is not UTF-8 text")]
    Utf8(#[source] std::string::FromUtf8Error),
    #[error("the profile file is not valid profile TOML")]
    Parse(#[source] toml::de::Error),
    #[error(
        "profile file names must be 1–64 lowercase letters, digits, `-` or `_`, ending in `.toml`"
    )]
    Id,
    #[error("`name` must be 1–{MAX_NAME} bytes")]
    Name,
    #[error("`description` is limited to {MAX_DESCRIPTION} bytes")]
    Description,
    #[error("`instructions` are limited to {MAX_INSTRUCTIONS} bytes")]
    Instructions,
    #[error("`connection` names which connection lists `model`, so it needs `model`")]
    ConnectionWithoutModel,
    #[error("only the first {MAX_PROFILES} profile files are read")]
    TooMany,
    #[error("no agent profile is named `{0}`")]
    Unknown(String),
    #[error("agent profile `{0}` could not be read")]
    Unusable(String, #[source] Box<ProfileError>),
    #[error("profile `{profile}` uses {expected:?} mode, not {requested:?}")]
    Mode {
        profile: String,
        expected: Mode,
        requested: Mode,
    },
    #[error(
        "profile `{profile}` names model `{model}`, which is not permitted for these subagents"
    )]
    ModelDenied { profile: String, model: String },
    #[error(
        "profile `{profile}` names model `{model}`, which several connections list; add `connection`"
    )]
    AmbiguousModel { profile: String, model: String },
    #[error("profile `{profile}` uses model `{model}`, not the one requested")]
    ModelMismatch { profile: String, model: String },
}
impl crate::error::Classified for ProfileError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::Directory { .. } | Self::Read(_) => return None,
            Self::NotRegular
            | Self::TooLarge
            | Self::Utf8(_)
            | Self::Parse(_)
            | Self::Id
            | Self::Name
            | Self::Description
            | Self::Instructions
            | Self::ConnectionWithoutModel
            | Self::TooMany
            | Self::Mode { .. }
            | Self::AmbiguousModel { .. }
            | Self::ModelMismatch { .. } => ErrorKind::InvalidRequest,
            Self::Unusable(..) => ErrorKind::InvalidRequest,
            Self::Unknown(_) => ErrorKind::NotFound,
            Self::ModelDenied { .. } => ErrorKind::PermissionDenied,
        })
    }
}

/// A profile file's fields, as the user writes them.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    name: String,
    description: Option<String>,
    mode: Option<Mode>,
    model: Option<String>,
    connection: Option<String>,
    capabilities: Option<BTreeSet<Capability>>,
    #[serde(default)]
    instructions: String,
}

/// Every profile in `dir`, by ID, with the files that could not be used. A
/// missing directory holds no profiles.
pub fn load(dir: &Path) -> Result<AgentProfiles, ProfileError> {
    let directory = |source| ProfileError::Directory {
        path: dir.to_owned(),
        source,
    };
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AgentProfiles::default());
        }
        Err(source) => return Err(directory(source)),
    };
    let mut files = BTreeSet::new();
    let mut excess = false;
    for entry in entries {
        let path = entry.map_err(directory)?.path();
        if path.extension().is_some_and(|e| e == "toml") {
            files.insert(path);
            if files.len() > MAX_PROFILES {
                files.pop_last();
                excess = true;
            }
        }
    }
    let mut loaded = AgentProfiles::default();
    for path in &files {
        let file = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        match read(path) {
            Ok(profile) => loaded.profiles.push(profile),
            Err(error) => loaded.problems.push(ProfileProblem {
                file,
                error: crate::error::report(&error),
            }),
        }
    }
    if excess {
        loaded.problems.push(ProfileProblem {
            file: String::from("Additional profile files"),
            error: crate::error::report(&ProfileError::TooMany),
        });
    }
    Ok(loaded)
}

/// The profile named `id` in `dir`.
pub fn find(dir: &Path, id: &str) -> Result<AgentProfile, ProfileError> {
    if !valid_id(id) {
        return Err(ProfileError::Unknown(id.into()));
    }
    let path = dir.join(format!("{id}.toml"));
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ProfileError::Unknown(id.into()))
        }
        _ => read(&path).map_err(|error| ProfileError::Unusable(id.into(), Box::new(error))),
    }
}

fn read(path: &Path) -> Result<AgentProfile, ProfileError> {
    let id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|id| valid_id(id))
        .ok_or(ProfileError::Id)?;
    let metadata = std::fs::symlink_metadata(path).map_err(ProfileError::Read)?;
    if !metadata.is_file() {
        return Err(ProfileError::NotRegular);
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(ProfileError::Read)?
        .take(MAX_FILE + 1)
        .read_to_end(&mut bytes)
        .map_err(ProfileError::Read)?;
    if bytes.len() as u64 > MAX_FILE {
        return Err(ProfileError::TooLarge);
    }
    let digest = crate::instructions::hex(&Sha256::digest(&bytes));
    let text = String::from_utf8(bytes).map_err(ProfileError::Utf8)?;
    parse(id, &text, digest)
}

/// A profile from its file's text.
pub fn parse(id: &str, text: &str, digest: String) -> Result<AgentProfile, ProfileError> {
    if !valid_id(id) {
        return Err(ProfileError::Id);
    }
    let file: File = toml::from_str(text).map_err(ProfileError::Parse)?;
    let name = file.name.trim();
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(ProfileError::Name);
    }
    let description = file
        .description
        .map(|d| d.trim().to_owned())
        .filter(|d| !d.is_empty());
    if description
        .as_ref()
        .is_some_and(|d| d.len() > MAX_DESCRIPTION)
    {
        return Err(ProfileError::Description);
    }
    if file.instructions.len() > MAX_INSTRUCTIONS {
        return Err(ProfileError::Instructions);
    }
    let model = file
        .model
        .map(|m| m.trim().to_owned())
        .filter(|m| !m.is_empty());
    let connection = file
        .connection
        .map(|c| c.trim().to_owned())
        .filter(|c| !c.is_empty());
    if connection.is_some() && model.is_none() {
        return Err(ProfileError::ConnectionWithoutModel);
    }
    Ok(AgentProfile {
        id: id.into(),
        name: name.into(),
        description,
        mode: file.mode,
        model,
        connection,
        capabilities: file.capabilities,
        instructions: file.instructions.trim().to_owned(),
        digest,
    })
}

/// The one permitted model `profile` names, if it names one.
pub fn model(
    profile: &AgentProfile,
    permitted: &BTreeSet<ModelKey>,
    providers: &[ProviderConfig],
) -> Result<Option<ModelKey>, ProfileError> {
    let Some(name) = &profile.model else {
        return Ok(None);
    };
    let mut matching = permitted.iter().filter(|key| profile.names(key, providers));
    match (matching.next(), matching.next()) {
        (Some(key), None) => Ok(Some(key.clone())),
        (Some(_), Some(_)) => Err(ProfileError::AmbiguousModel {
            profile: profile.id.clone(),
            model: name.clone(),
        }),
        (None, _) => Err(ProfileError::ModelDenied {
            profile: profile.id.clone(),
            model: name.clone(),
        }),
    }
}

/// Check a subagent request against `profile`: its mode and model must be
/// the profile's, where it names them. Returns the capabilities left of
/// `requested` once the profile narrows them.
pub fn admit(
    profile: &AgentProfile,
    mode: Mode,
    key: &ModelKey,
    permitted: &BTreeSet<ModelKey>,
    providers: &[ProviderConfig],
    requested: BTreeSet<Capability>,
) -> Result<BTreeSet<Capability>, ProfileError> {
    if let Some(expected) = profile.mode.filter(|m| *m != mode) {
        return Err(ProfileError::Mode {
            profile: profile.id.clone(),
            expected,
            requested: mode,
        });
    }
    if let Some(expected) = model(profile, permitted, providers)?
        && &expected != key
    {
        return Err(ProfileError::ModelMismatch {
            profile: profile.id.clone(),
            model: expected.model,
        });
    }
    Ok(match &profile.capabilities {
        Some(most) => requested.intersection(most).copied().collect(),
        None => requested,
    })
}

#[cfg(test)]
mod tests;
