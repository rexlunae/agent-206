//! Which workspace paths the memory tools accept.
//!
//! This scopes the tools, it is not the security boundary: the gateway
//! authorizes every host call against the user's grant, cap-std keeps every
//! path inside the project, and writes refuse symbolic links and `.git`.

/// The project's long-term memory file, at the project root.
pub const ROOT_FILE: &str = "MEMORY.md";
/// The folder of notes, at the project root.
pub const DIRECTORY: &str = "memory";
pub const SUFFIX: &str = ".md";
/// Most folders between `memory/` and a note.
pub const MAX_DEPTH: usize = 8;
pub const MAX_PATH: usize = 512;

/// What a tool may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `MEMORY.md` or a note under `memory/`.
    Memory,
    /// Only a note under `memory/`; `MEMORY.md` stays the user's to curate.
    Notes,
}

/// Why a path is not one the memory tools accept.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PathError {
    #[error("the path is empty")]
    Empty,
    #[error("the path is longer than {MAX_PATH} bytes")]
    TooLong,
    #[error("`{0}` is not MEMORY.md or a note under memory/")]
    OutsideMemory(String),
    #[error("`{0}` is MEMORY.md; notes are saved under memory/")]
    RootFile(String),
    #[error(
        "`{0}` has an empty, hidden, `.` or `..` component, or a character paths here never use"
    )]
    Component(String),
    #[error("`{0}` is not a .md file")]
    NotMarkdown(String),
    #[error("`{0}` is nested more than {MAX_DEPTH} folders below memory/")]
    TooDeep(String),
}

/// Check `path` against `scope`. Only `/`-separated plain names are
/// accepted, so the path the host resolves is the path checked here.
pub fn check(path: &str, scope: Scope) -> Result<&str, PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.len() > MAX_PATH {
        return Err(PathError::TooLong);
    }
    if path == ROOT_FILE {
        return match scope {
            Scope::Memory => Ok(path),
            Scope::Notes => Err(PathError::RootFile(path.into())),
        };
    }
    let mut components = path.split('/');
    if components.next() != Some(DIRECTORY) {
        return Err(PathError::OutsideMemory(path.into()));
    }
    let rest: Vec<&str> = components.collect();
    let Some((name, folders)) = rest.split_last() else {
        return Err(PathError::OutsideMemory(path.into()));
    };
    if !rest.iter().all(|component| plain(component)) {
        return Err(PathError::Component(path.into()));
    }
    if name.len() <= SUFFIX.len() || !name.ends_with(SUFFIX) {
        return Err(PathError::NotMarkdown(path.into()));
    }
    if folders.len() > MAX_DEPTH {
        return Err(PathError::TooDeep(path.into()));
    }
    Ok(path)
}

/// A name that is not hidden, not `.`/`..`, and carries no separator,
/// drive or control character on any platform.
pub fn plain(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '\\' | ':' | '/'))
}

/// The folder holding `path`, if it is not the project root.
pub fn parent(path: &str) -> Option<&str> {
    path.rsplit_once('/').map(|(parent, _)| parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::unique;

    #[test]
    fn memory_files_and_notes_are_accepted() {
        let name = format!("{}.md", unique("note"));
        let nested = format!("memory/{}/{}/{name}", unique("a"), unique("b"));
        let deepest = format!("memory/{}{name}", "d/".repeat(MAX_DEPTH));
        for scope in [Scope::Memory, Scope::Notes] {
            for path in [format!("memory/{name}"), nested.clone(), deepest.clone()] {
                assert_eq!(check(&path, scope), Ok(path.as_str()));
            }
        }
        assert_eq!(check(ROOT_FILE, Scope::Memory), Ok(ROOT_FILE));
        let unicode = format!("memory/{}-日記.md", unique("ü"));
        assert_eq!(check(&unicode, Scope::Notes), Ok(unicode.as_str()));
    }

    #[test]
    fn everything_else_is_refused() {
        let name = unique("n");
        let refused = [
            (String::new(), PathError::Empty),
            ("m".repeat(MAX_PATH + 1), PathError::TooLong),
            (
                format!("{name}.md"),
                PathError::OutsideMemory(String::new()),
            ),
            (
                format!("src/{name}.md"),
                PathError::OutsideMemory(String::new()),
            ),
            (
                format!("/memory/{name}.md"),
                PathError::OutsideMemory(String::new()),
            ),
            (
                format!("./memory/{name}.md"),
                PathError::OutsideMemory(String::new()),
            ),
            (
                format!("Memory/{name}.md"),
                PathError::OutsideMemory(String::new()),
            ),
            ("memory.md".into(), PathError::OutsideMemory(String::new())),
            ("memory".into(), PathError::OutsideMemory(String::new())),
            (
                format!("{ROOT_FILE}/{name}.md"),
                PathError::OutsideMemory(String::new()),
            ),
            (
                format!("x/memory/{name}.md"),
                PathError::OutsideMemory(String::new()),
            ),
            (
                format!("memory/../src/{name}.md"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory/../{ROOT_FILE}"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory/./{name}.md"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory//{name}.md"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory/{name}.md/"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory/.{name}.md"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory/.git/{name}.md"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory/a\\..\\..\\{name}.md"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory/C:{name}.md"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory/{name}\n.md"),
                PathError::Component(String::new()),
            ),
            (
                format!("memory/{name}\0.md"),
                PathError::Component(String::new()),
            ),
            ("memory/".into(), PathError::Component(String::new())),
            (
                format!("memory/{name}.txt"),
                PathError::NotMarkdown(String::new()),
            ),
            (
                format!("memory/{name}.MD"),
                PathError::NotMarkdown(String::new()),
            ),
            (
                format!("memory/{name}"),
                PathError::NotMarkdown(String::new()),
            ),
            ("memory/.md".into(), PathError::Component(String::new())),
            (
                format!("memory/{}{name}.md", "d/".repeat(MAX_DEPTH + 1)),
                PathError::TooDeep(String::new()),
            ),
        ];
        for scope in [Scope::Memory, Scope::Notes] {
            for (path, expected) in &refused {
                let refusal = check(path, scope).expect_err(path);
                assert_eq!(
                    std::mem::discriminant(&refusal),
                    std::mem::discriminant(expected),
                    "{path:?}: {refusal}"
                );
            }
        }
        assert_eq!(
            check(ROOT_FILE, Scope::Notes),
            Err(PathError::RootFile(ROOT_FILE.into()))
        );
    }
}
