//! Bounded snapshots: audit and approval use exactly the bytes later served.
use super::*;
use a206_core::{AuditBuilder, AuditFinding, AuditLocation, AuditRule};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt, OpenOptionsSyncExt};
use cap_std::fs::Dir;

pub(super) fn inspect(
    root: &Path,
    name: &str,
    instructions: &[u8],
    skill: &mut Skill,
) -> Result<(), SkillError> {
    let dir = Dir::open_ambient_dir(root, cap_std::ambient_authority())
        .and_then(|dir| dir.open_dir_nofollow(name))
        .map_err(SkillError::Read)?;
    let mut files = BTreeMap::new();
    walk(&dir, "", 0, &mut 0, &mut 0, &mut files)?;
    // Do not combine instructions from one read with approval of another.
    if files.remove("SKILL.md").as_deref() != Some(instructions) {
        return Err(SkillError::Changed);
    }
    if files.is_empty() {
        return Ok(()); // Retain existing approvals for standalone SKILL.md.
    }
    let mut hash = Sha256::new();
    hash.update(b"agent206.skill-package.v1\0");
    hash_file(&mut hash, "SKILL.md", instructions);
    let mut audit = AuditBuilder::default();
    for finding in &skill.audit.findings {
        audit.push(finding.clone());
    }
    let mut omitted = skill.audit.more;
    for (path, bytes) in &files {
        hash_file(&mut hash, path, bytes);
        match std::str::from_utf8(bytes)
            .ok()
            .filter(|text| !text.contains('\0'))
        {
            Some(text) => {
                let report = a206_audit::text(text);
                omitted = omitted.saturating_add(report.more);
                for finding in report.findings {
                    let AuditLocation::Line(line) = finding.location else {
                        continue; // The text scanner produces line locations only.
                    };
                    audit.push(AuditFinding {
                        rule: finding.rule,
                        location: AuditLocation::File {
                            path: path.clone(),
                            line: Some(line),
                        },
                    });
                }
            }
            None => audit.push(AuditFinding {
                rule: AuditRule::OpaqueFile,
                location: AuditLocation::File {
                    path: path.clone(),
                    line: None,
                },
            }),
        }
    }
    skill.audit = audit.finish();
    skill.audit.more = skill.audit.more.saturating_add(omitted);
    skill.digest = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    skill.files = files;
    Ok(())
}

fn hash_file(hash: &mut Sha256, path: &str, bytes: &[u8]) {
    hash.update((path.len() as u64).to_le_bytes());
    hash.update(path.as_bytes());
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn walk(
    dir: &Dir,
    prefix: &str,
    depth: usize,
    entries: &mut usize,
    total: &mut u64,
    files: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), SkillError> {
    for entry in dir.entries().map_err(SkillError::Read)? {
        let entry = entry.map_err(SkillError::Read)?;
        *entries += 1;
        if *entries > 128 || depth > 16 {
            return Err(SkillError::PackageLimit);
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| SkillError::Path)?;
        if name.contains(['\\', '/', '\0']) || name.chars().any(char::is_control) {
            return Err(SkillError::Path);
        }
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        let kind = entry.file_type().map_err(SkillError::Read)?;
        if kind.is_dir() {
            if depth == 16 {
                return Err(SkillError::PackageLimit);
            }
            let child = dir.open_dir_nofollow(&name).map_err(SkillError::Read)?;
            walk(&child, &path, depth + 1, entries, total, files)?;
        } else if kind.is_file() {
            let mut options = cap_std::fs::OpenOptions::new();
            options.read(true).follow(FollowSymlinks::No).nonblock(true);
            let file = dir.open_with(&name, &options).map_err(SkillError::Read)?;
            if !file.metadata().map_err(SkillError::Read)?.is_file() {
                return Err(SkillError::Path);
            }
            let mut bytes = Vec::new();
            file.take(MAX_FILE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map_err(SkillError::Read)?;
            if bytes.len() as u64 > MAX_FILE_BYTES {
                return Err(SkillError::FileTooLarge {
                    limit: MAX_FILE_BYTES,
                });
            }
            *total += bytes.len() as u64;
            if *total > 2 << 20 {
                return Err(SkillError::PackageLimit);
            }
            files.insert(path, bytes);
        } else {
            return Err(SkillError::Path); // Never follow links or open devices/FIFOs.
        }
    }
    Ok(())
}
