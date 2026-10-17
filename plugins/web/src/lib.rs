//! Bundled web extension. Every request is a host call that the gateway
//! authorizes against the caller's `network` grant and sends through its
//! SSRF guard.
use a206_extension_sdk::{Args, Extension, GuestError, ToolOutput, host, string_arg};

struct Web;
a206_extension_sdk::export!(Web);

/// Longest text returned to the caller; the rest is cut with a notice.
const MAX_TEXT: usize = 192 * 1024;

impl Extension for Web {
    fn tool(name: &str, args: &Args) -> Result<ToolOutput, GuestError> {
        if name != "fetch_url" {
            return Err(GuestError::new("unknown_tool", format!("no tool `{name}`")));
        }
        let url = string_arg(args, "url")?
            .ok_or_else(|| GuestError::new("invalid_argument", "`url` is required"))?;
        // A fragment never reaches the server; drop it rather than refuse.
        let url = url.split('#').next().unwrap_or_default();
        let response = host::fetch(url)?;
        let mut text = format!("HTTP {}", response.status);
        if let Some(content_type) = &response.content_type {
            text.push_str(&format!("\nContent-Type: {content_type}"));
        }
        match readable(&response.body, response.truncated) {
            Some(body) => {
                text.push_str("\n\n");
                text.push_str(prefix(body, MAX_TEXT));
                if body.len() > MAX_TEXT || response.truncated {
                    text.push_str("\n[the response continues; it was cut at a limit]");
                }
            }
            None => text.push_str(&format!(
                "\n\n[{} bytes of non-text content not shown]",
                response.body.len()
            )),
        }
        Ok(ToolOutput { text })
    }
}

/// The body as UTF-8 text. When the host cut the body, a character split by
/// the cut is dropped rather than making the whole page unreadable.
fn readable(body: &[u8], truncated: bool) -> Option<&str> {
    match std::str::from_utf8(body) {
        Ok(text) => Some(text),
        // `error_len() == None`: the bytes end mid-character, nothing invalid.
        Err(error) if truncated && error.error_len().is_none() => {
            std::str::from_utf8(&body[..error.valid_up_to()]).ok()
        }
        Err(_) => None,
    }
}

/// The longest prefix of `text` within `max` bytes, on a character boundary.
fn prefix(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
