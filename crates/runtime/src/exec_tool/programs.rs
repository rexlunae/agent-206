//! The user's list of extra program directories, checked when the settings
//! are saved. Commands resolve and check each entry again when they start,
//! since the filesystem can change in between.
use a206_core::ErrorKind;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

/// Why a program directory cannot be saved.
#[derive(Debug, thiserror::Error)]
pub enum ProgramsError {
    #[error("program directory {} must be an absolute path", path.display())]
    NotAbsolute { path: PathBuf },
    #[error("program directory {} is missing or inaccessible", path.display())]
    Missing {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("program directory {} is not a directory", path.display())]
    NotDirectory { path: PathBuf },
    #[error(
        "program directory {} is inside protected gateway or credential state",
        path.display()
    )]
    Protected { path: PathBuf },
    #[error(
        "program directory {} contains the home directory or gateway state; choose the toolchain's own directory",
        path.display()
    )]
    TooBroad { path: PathBuf },
    #[error("program directory {} is listed twice", path.display())]
    Duplicate { path: PathBuf },
}
impl crate::error::Classified for ProgramsError {
    fn kind(&self) -> Option<ErrorKind> {
        Some(match self {
            Self::NotAbsolute { .. } | Self::NotDirectory { .. } | Self::Duplicate { .. } => {
                ErrorKind::InvalidRequest
            }
            // Absent and unreadable paths need different recovery.
            Self::Missing { .. } => return None,
            Self::Protected { .. } | Self::TooBroad { .. } => ErrorKind::PermissionDenied,
        })
    }
}

/// `entries` in canonical form, or why one of them cannot be offered to
/// commands. `hidden` are the paths commands may never see, `home` the
/// user's home directory and `state_root` the gateway's state.
pub fn validate(
    entries: &[PathBuf],
    hidden: &[PathBuf],
    home: Option<&Path>,
    state_root: &Path,
) -> Result<Vec<PathBuf>, ProgramsError> {
    let resolve = |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let hidden: Vec<PathBuf> = hidden.iter().map(|path| resolve(path)).collect();
    let broad: Vec<PathBuf> = home.into_iter().chain([state_root]).map(resolve).collect();
    let mut seen = BTreeSet::new();
    entries
        .iter()
        .map(|path| {
            if !path.is_absolute() {
                return Err(ProgramsError::NotAbsolute { path: path.clone() });
            }
            let resolved =
                std::fs::canonicalize(path).map_err(|source| ProgramsError::Missing {
                    path: path.clone(),
                    source,
                })?;
            if !resolved.is_dir() {
                return Err(ProgramsError::NotDirectory { path: path.clone() });
            }
            if hidden.iter().any(|hidden| resolved.starts_with(hidden)) {
                return Err(ProgramsError::Protected { path: path.clone() });
            }
            if broad.iter().any(|inner| inner.starts_with(&resolved)) {
                return Err(ProgramsError::TooBroad { path: path.clone() });
            }
            if !seen.insert(resolved.clone()) {
                return Err(ProgramsError::Duplicate { path: path.clone() });
            }
            Ok(resolved)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Layout {
        _directory: tempfile::TempDir,
        base: PathBuf,
        home: PathBuf,
        state: PathBuf,
        hidden: Vec<PathBuf>,
    }

    fn layout() -> Layout {
        let directory = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(directory.path()).unwrap();
        let home = base.join(uuid::Uuid::new_v4().to_string());
        let state = home.join(uuid::Uuid::new_v4().to_string());
        let secret = home.join(uuid::Uuid::new_v4().to_string());
        for path in [&state, &secret] {
            std::fs::create_dir_all(path).unwrap();
        }
        Layout {
            _directory: directory,
            base,
            home,
            state,
            hidden: vec![secret],
        }
    }

    impl Layout {
        fn validate(&self, entries: &[PathBuf]) -> Result<Vec<PathBuf>, ProgramsError> {
            validate(entries, &self.hidden, Some(&self.home), &self.state)
        }
        fn directory(&self, under: &Path) -> PathBuf {
            let path = under.join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir(&path).unwrap();
            path
        }
    }

    #[test]
    fn directories_are_stored_canonically() {
        let layout = layout();
        let tools = layout.directory(&layout.base);
        let inside_home = layout.directory(&layout.home);
        let alias = layout.base.join(uuid::Uuid::new_v4().to_string());
        std::os::unix::fs::symlink(&tools, &alias).unwrap();
        assert_eq!(
            layout.validate(&[alias, inside_home.clone()]).unwrap(),
            [tools.clone(), inside_home]
        );
        assert!(matches!(
            layout.validate(&[tools.clone(), tools.join(".")]),
            Err(ProgramsError::Duplicate { .. })
        ));
    }

    #[test]
    fn unusable_or_protected_directories_are_refused() {
        let layout = layout();
        let file = layout.base.join(uuid::Uuid::new_v4().to_string());
        std::fs::write(&file, "").unwrap();
        let relative = PathBuf::from(uuid::Uuid::new_v4().to_string());
        let missing = layout.base.join(uuid::Uuid::new_v4().to_string());
        let secret = layout.directory(&layout.hidden[0]);
        let state = layout.directory(&layout.state);
        let cases = [
            (relative, "NotAbsolute"),
            (missing, "Missing"),
            (file, "NotDirectory"),
            (secret, "Protected"),
            (layout.home.clone(), "TooBroad"),
            (layout.base.clone(), "TooBroad"),
            (state, "Protected"),
        ];
        // The state root is protected like any other hidden path.
        let mut hidden = layout.hidden.clone();
        hidden.push(layout.state.clone());
        for (path, expected) in cases {
            let error = validate(
                std::slice::from_ref(&path),
                &hidden,
                Some(&layout.home),
                &layout.state,
            )
            .unwrap_err();
            let variant = format!("{error:?}");
            assert!(
                variant.starts_with(expected),
                "{}: {variant}",
                path.display()
            );
        }
    }
}
