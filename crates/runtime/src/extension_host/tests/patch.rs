//! The patch host call: checked before anything is written, confined to
//! the workspace like every other write.
use super::*;
use a206_extensions::abi::{FileChange, FilePatch, Hunk, PatchFailure, PatchLine, PatchResult};
use std::path::Path;

fn line(kind: char, text: &str) -> PatchLine {
    let text = format!("{text}\n");
    match kind {
        ' ' => PatchLine::Context(text),
        '-' => PatchLine::Remove(text),
        _ => PatchLine::Add(text),
    }
}
fn file(path: &str, change: FileChange, old_start: u32, lines: &[(char, &str)]) -> FilePatch {
    FilePatch {
        path: path.into(),
        change,
        hunks: vec![Hunk {
            old_start,
            lines: lines.iter().map(|&(kind, text)| line(kind, text)).collect(),
        }],
    }
}
fn patch(files: Vec<FilePatch>, check_only: bool) -> HostRequest {
    HostRequest::PatchFiles { files, check_only }
}
fn failed(path: &str, reply: HostReply) -> HostErrorCode {
    match reply {
        HostReply::Patched(PatchResult {
            changed,
            failed: Some(PatchFailure { path: at, failure }),
        }) => {
            assert!(changed.is_empty(), "nothing was written: {changed:?}");
            assert_eq!(at, path);
            failure.code
        }
        other => panic!("expected a failed patch, got {other:?}"),
    }
}
fn read(root: &Path, path: &str) -> Option<String> {
    std::fs::read_to_string(root.join(path)).ok()
}
/// Every regular file under `root`, so a test sees stray temporary files.
fn files(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else {
                found.push(path.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    found.sort();
    found
}

#[tokio::test]
async fn a_patch_changes_creates_and_deletes_files_together() {
    let (dir, store) = store();
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let (old, new) = (
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    );
    std::fs::write(root.join("src/a.txt"), format!("one\n{old}\nthree\n")).unwrap();
    std::fs::write(root.join("gone.txt"), "bye\n").unwrap();
    let mut host = workspace_calls(&store, Some(root.clone()), &[Capability::WriteWorkspace]);
    let files_patch = vec![
        file(
            "src/a.txt",
            FileChange::Modify,
            1,
            &[(' ', "one"), ('-', &old), ('+', &new), (' ', "three")],
        ),
        file("new/dir/b.txt", FileChange::Create, 0, &[('+', &new)]),
        file("gone.txt", FileChange::Delete, 1, &[('-', "bye")]),
    ];

    assert_eq!(
        host.handle(patch(files_patch.clone(), true)).await,
        HostReply::Patched(PatchResult::default()),
        "a check writes nothing"
    );
    assert_eq!(
        read(&root, "src/a.txt"),
        Some(format!("one\n{old}\nthree\n"))
    );
    assert!(!root.join("new").exists(), "a check makes no directories");

    assert_eq!(
        host.handle(patch(files_patch.clone(), false)).await,
        HostReply::Patched(PatchResult {
            changed: vec![
                "src/a.txt".into(),
                "new/dir/b.txt".into(),
                "gone.txt".into()
            ],
            failed: None,
        })
    );
    assert_eq!(
        read(&root, "src/a.txt"),
        Some(format!("one\n{new}\nthree\n"))
    );
    assert_eq!(read(&root, "new/dir/b.txt"), Some(format!("{new}\n")));
    assert_eq!(files(&root), vec!["new/dir/b.txt", "src/a.txt"]);

    // Applied again, the first file no longer matches, so nothing changes:
    // not even the files after it, which would apply on their own.
    std::fs::write(root.join("gone.txt"), "bye\n").unwrap();
    std::fs::remove_file(root.join("new/dir/b.txt")).unwrap();
    assert_eq!(
        failed("src/a.txt", host.handle(patch(files_patch, false)).await),
        HostErrorCode::Conflict
    );
    assert_eq!(read(&root, "gone.txt").as_deref(), Some("bye\n"));
    assert!(!root.join("new/dir/b.txt").exists());
}

#[tokio::test]
async fn each_file_must_hold_what_its_change_expects() {
    let (dir, store) = store();
    let root = dir.path().join("project");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("f.txt"), "a\r\nb\r\n").unwrap();
    std::fs::write(root.join("bin"), [0xff, 0xfe]).unwrap();
    let mut host = workspace_calls(&store, Some(root.clone()), &[Capability::WriteWorkspace]);

    let create = file("f.txt", FileChange::Create, 0, &[('+', "x")]);
    assert_eq!(
        failed("f.txt", host.handle(patch(vec![create], false)).await),
        HostErrorCode::Exists
    );
    let missing = file("none.txt", FileChange::Modify, 1, &[('-', "a")]);
    assert_eq!(
        failed("none.txt", host.handle(patch(vec![missing], false)).await),
        HostErrorCode::NotFound
    );
    let partial_delete = file("f.txt", FileChange::Delete, 1, &[('-', "a")]);
    assert_eq!(
        failed(
            "f.txt",
            host.handle(patch(vec![partial_delete], false)).await
        ),
        HostErrorCode::Conflict
    );
    let binary = file("bin", FileChange::Modify, 1, &[('+', "x")]);
    assert_eq!(
        failed("bin", host.handle(patch(vec![binary], false)).await),
        HostErrorCode::Invalid
    );
    // A CRLF file matches LF hunk lines and keeps its line ending.
    let crlf = file("f.txt", FileChange::Modify, 2, &[('-', "b"), ('+', "B")]);
    assert!(matches!(
        host.handle(patch(vec![crlf], false)).await,
        HostReply::Patched(PatchResult { failed: None, .. })
    ));
    assert_eq!(read(&root, "f.txt").as_deref(), Some("a\r\nB\r\n"));
}

#[tokio::test]
async fn patches_need_the_write_grant_and_stay_inside_the_workspace() {
    let (dir, store) = store();
    let root = dir.path().join("project");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    std::fs::write(dir.path().join("outside.txt"), "a\n").unwrap();
    let add = |path: &str| file(path, FileChange::Create, 0, &[('+', "x")]);

    let mut reader = workspace_calls(&store, Some(root.clone()), &[Capability::ReadWorkspace]);
    assert_eq!(
        code(reader.handle(patch(vec![add("f.txt")], false)).await),
        Some(HostErrorCode::Denied)
    );
    let mut nowhere = workspace_calls(&store, None, &[Capability::WriteWorkspace]);
    assert_eq!(
        code(nowhere.handle(patch(vec![add("f.txt")], false)).await),
        Some(HostErrorCode::NoWorkspace)
    );
    let mut host = workspace_calls(&store, Some(root.clone()), &[Capability::WriteWorkspace]);
    for path in [
        "../outside.txt",
        "/etc/passwd",
        ".git/config",
        "a/.GIT/x",
        "",
    ] {
        assert_eq!(
            code(host.handle(patch(vec![add(path)], false)).await),
            Some(HostErrorCode::Invalid),
            "{path}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(dir.path(), root.join("link")).unwrap();
        assert_eq!(
            failed(
                "link/new.txt",
                host.handle(patch(vec![add("link/new.txt")], false)).await
            ),
            HostErrorCode::Invalid
        );
        assert!(!dir.path().join("new.txt").exists());
    }
    assert_eq!(
        code(
            host.handle(patch(vec![add("d/f.txt"), add("d//f.txt")], false))
                .await
        ),
        Some(HostErrorCode::Invalid),
        "the same file twice"
    );
    assert_eq!(
        code(host.handle(patch(Vec::new(), false)).await),
        Some(HostErrorCode::Invalid)
    );
    let many = (0..=crate::extension_host::workspace::MAX_PATCH_FILES)
        .map(|n| add(&format!("f{n}.txt")))
        .collect();
    assert_eq!(
        code(host.handle(patch(many, false)).await),
        Some(HostErrorCode::TooLarge)
    );
    assert_eq!(read(dir.path(), "outside.txt").as_deref(), Some("a\n"));
    assert!(!root.join("f.txt").exists());
}
