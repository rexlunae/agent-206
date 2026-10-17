//! The ELF program interpreter (`PT_INTERP`) of an executable.
//!
//! Landlock checks execute permission on the dynamic loader as well as on the
//! program, so the sandbox grants execution of exactly that loader rather than
//! of every system library directory.
use std::{
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const PT_INTERP: u32 = 3;
/// Longest loader path accepted; real ones are a few dozen bytes.
const MAX_INTERPRETER: u64 = 4096;
/// Most program headers read; real executables have a dozen or so.
const MAX_HEADERS: u16 = 256;

/// The loader `path` names, or `None` when it is not a dynamically linked ELF
/// file (a static binary, or a script, which the sandbox does not widen for).
pub fn interpreter(path: &Path) -> std::io::Result<Option<PathBuf>> {
    let mut file = std::fs::File::open(path)?;
    let mut ident = [0u8; 16];
    if file.read_exact(&mut ident).is_err() || &ident[..4] != b"\x7fELF" {
        return Ok(None);
    }
    let wide = match ident[4] {
        1 => false,
        2 => true,
        _ => return Ok(None),
    };
    let big = match ident[5] {
        1 => false,
        2 => true,
        _ => return Ok(None),
    };
    let word = |bytes: &[u8]| -> u64 {
        let mut value = 0u64;
        let ordered: Vec<u8> = if big {
            bytes.to_vec()
        } else {
            bytes.iter().rev().copied().collect()
        };
        for byte in ordered {
            value = (value << 8) | u64::from(byte);
        }
        value
    };
    // Offsets of e_phoff, e_phentsize and e_phnum, and of p_offset and
    // p_filesz within one program header, for 32- and 64-bit files.
    let (phoff, phentsize, phnum, offset, size, width) = if wide {
        ((32, 8), 54, 56, 8, 32, 8)
    } else {
        ((28, 4), 42, 44, 4, 16, 4)
    };
    let mut header = [0u8; 64];
    file.seek(SeekFrom::Start(0))?;
    let read = read_up_to(&mut file, &mut header)?;
    if read < phnum + 2 {
        return Ok(None);
    }
    let table = word(&header[phoff.0..phoff.0 + phoff.1]);
    let entry = word(&header[phentsize..phentsize + 2]);
    let count = word(&header[phnum..phnum + 2]);
    if count > u64::from(MAX_HEADERS) || entry < (size + width) as u64 {
        return Ok(None);
    }
    let mut program = vec![0u8; entry as usize];
    for index in 0..count {
        file.seek(SeekFrom::Start(table.saturating_add(index * entry)))?;
        if read_up_to(&mut file, &mut program)? < program.len() {
            return Ok(None);
        }
        if word(&program[..4]) != u64::from(PT_INTERP) {
            continue;
        }
        let start = word(&program[offset..offset + width]);
        let length = word(&program[size..size + width]);
        if length == 0 || length > MAX_INTERPRETER {
            return Ok(None);
        }
        let mut name = vec![0u8; length as usize];
        file.seek(SeekFrom::Start(start))?;
        if read_up_to(&mut file, &mut name)? < name.len() {
            return Ok(None);
        }
        let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
        use std::os::unix::ffi::OsStrExt;
        let name = Path::new(std::ffi::OsStr::from_bytes(name));
        return Ok(name.is_absolute().then(|| name.to_path_buf()));
    }
    Ok(None)
}

fn read_up_to(file: &mut std::fs::File, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match file.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ELF image whose second program header is `PT_INTERP`.
    fn image(loader: &[u8], big: bool, wide: bool) -> Vec<u8> {
        let put = |out: &mut Vec<u8>, at: usize, value: u64, width: usize| {
            let bytes = value.to_be_bytes();
            let mut field = bytes[8 - width..].to_vec();
            if !big {
                field.reverse();
            }
            out[at..at + width].copy_from_slice(&field);
        };
        let (header, entry, word) = if wide { (64, 56, 8) } else { (52, 32, 4) };
        let mut out = vec![0u8; header + 2 * entry];
        out[..4].copy_from_slice(b"\x7fELF");
        out[4] = if wide { 2 } else { 1 };
        out[5] = if big { 2 } else { 1 };
        let (phoff, phentsize, phnum) = if wide { (32, 54, 56) } else { (28, 42, 44) };
        put(&mut out, phoff, header as u64, word);
        put(&mut out, phentsize, entry as u64, 2);
        put(&mut out, phnum, 2, 2);
        // A PT_LOAD header first, so the search must skip it.
        put(&mut out, header, 1, 4);
        let second = header + entry;
        put(&mut out, second, u64::from(PT_INTERP), 4);
        let (offset, size) = if wide { (8, 32) } else { (4, 16) };
        let end = out.len() as u64;
        put(&mut out, second + offset, end, word);
        put(&mut out, second + size, loader.len() as u64 + 1, word);
        out.extend_from_slice(loader);
        out.push(0);
        out
    }

    #[test]
    fn finds_the_loader_in_every_layout() {
        let directory = tempfile::tempdir().unwrap();
        for big in [false, true] {
            for wide in [false, true] {
                let loader = format!("/{}/{}", uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
                let path = directory.path().join(uuid::Uuid::new_v4().to_string());
                std::fs::write(&path, image(loader.as_bytes(), big, wide)).unwrap();
                assert_eq!(
                    interpreter(&path).unwrap(),
                    Some(PathBuf::from(&loader)),
                    "big={big} wide={wide}"
                );
            }
        }
    }

    #[test]
    fn scripts_truncated_files_and_relative_loaders_have_none() {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("script");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        assert_eq!(interpreter(&script).unwrap(), None);
        let full = image(b"/loader", false, true);
        let truncated = directory.path().join("truncated");
        std::fs::write(&truncated, &full[..full.len() - 4]).unwrap();
        assert_eq!(interpreter(&truncated).unwrap(), None);
        let relative = directory.path().join("relative");
        std::fs::write(&relative, image(b"loader", false, true)).unwrap();
        assert_eq!(interpreter(&relative).unwrap(), None);
        assert!(interpreter(&directory.path().join("missing")).is_err());
    }
}
