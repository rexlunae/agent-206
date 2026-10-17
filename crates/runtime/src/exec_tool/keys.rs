//! Named keys for a background session's terminal, as `process_write`
//! sends them: each name becomes the bytes a terminal sends for that key.
//!
//! The names and sequences follow RustyClaw's `translate_keys` (MIT, see
//! NOTICE), except that Enter sends a carriage return, as a terminal does,
//! and an unknown name is refused rather than typed as text; text goes in
//! `data`.

/// Longest list of keys one call may send.
pub const MAX_KEYS: usize = 256;

/// A key name that names no key.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error(
    "unknown key `{0}`; use Enter, Tab, Escape, Space, Backspace, Delete, Insert, Up, Down, \
     Left, Right, Home, End, PageUp, PageDown, F1 to F12 or Ctrl-A to Ctrl-Z"
)]
pub struct UnknownKey(pub String);

/// The bytes for `keys`, in order.
pub fn translate<'a>(keys: impl IntoIterator<Item = &'a str>) -> Result<Vec<u8>, UnknownKey> {
    let mut bytes = Vec::new();
    for key in keys {
        bytes.extend_from_slice(&sequence(key).ok_or_else(|| UnknownKey(key.into()))?);
    }
    Ok(bytes)
}

fn sequence(key: &str) -> Option<Vec<u8>> {
    let name = key.to_ascii_lowercase();
    let fixed: &[u8] = match name.as_str() {
        "enter" | "return" | "cr" => b"\r",
        "tab" => b"\t",
        "escape" | "esc" => b"\x1b",
        "space" => b" ",
        "backspace" | "bs" => b"\x7f",
        "delete" | "del" => b"\x1b[3~",
        "insert" | "ins" => b"\x1b[2~",
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "pageup" | "pgup" => b"\x1b[5~",
        "pagedown" | "pgdn" => b"\x1b[6~",
        "f1" => b"\x1bOP",
        "f2" => b"\x1bOQ",
        "f3" => b"\x1bOR",
        "f4" => b"\x1bOS",
        "f5" => b"\x1b[15~",
        "f6" => b"\x1b[17~",
        "f7" => b"\x1b[18~",
        "f8" => b"\x1b[19~",
        "f9" => b"\x1b[20~",
        "f10" => b"\x1b[21~",
        "f11" => b"\x1b[23~",
        "f12" => b"\x1b[24~",
        _ => {
            let rest = name
                .strip_prefix("ctrl-")
                .or_else(|| name.strip_prefix("c-"))?;
            let [byte] = rest.as_bytes() else {
                return None;
            };
            let control = match byte {
                b'a'..=b'z' => byte - b'a' + 1,
                b'@' => 0,
                b'[' => 0x1b,
                b'\\' => 0x1c,
                b']' => 0x1d,
                b'^' => 0x1e,
                b'_' => 0x1f,
                _ => return None,
            };
            return Some(vec![control]);
        }
    };
    Some(fixed.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_become_terminal_sequences_in_order() {
        assert_eq!(
            translate(["Up", "down", "LEFT", "Right"]).unwrap(),
            b"\x1b[A\x1b[B\x1b[D\x1b[C"
        );
        assert_eq!(translate(["Enter"]).unwrap(), b"\r");
        assert_eq!(
            translate(["Ctrl-C", "c-d", "ctrl-["]).unwrap(),
            [3, 4, 0x1b]
        );
        assert_eq!(translate(["F5", "PgDn"]).unwrap(), b"\x1b[15~\x1b[6~");
    }

    #[test]
    fn unknown_names_are_refused_rather_than_typed() {
        for key in ["hello", "Ctrl-", "Ctrl-AB", "Ctrl-1", "F13", ""] {
            assert_eq!(
                translate(["Enter", key]),
                Err(UnknownKey(key.into())),
                "{key}"
            );
        }
    }
}
