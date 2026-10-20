//! Bundled web extension. Every request is a host call that the gateway
//! authorizes against the caller's `network` grant and sends through its
//! SSRF guard.
use a206_extension_sdk::{
    Args, ByteBuf, Extension, FetchResponse, GuestError, ToolOutput, TriggerOutput, bool_arg,
    fingerprint, host, int_arg, prefix, string_arg,
};
use extract::Format;

mod extract;

struct Web;
a206_extension_sdk::export!(Web);

/// Longest text returned to the caller; the rest is cut with a notice.
const MAX_TEXT: usize = 192 * 1024;
const DEFAULT_BYTES: i64 = 50_000;
const CUT: &str = "\n[the response continues; it was cut at a limit]";
/// Most bytes of a changed page's text a `page_changed` report quotes.
const REPORT_TEXT: usize = 8 * 1024;

impl Extension for Web {
    fn tool(name: &str, args: &Args) -> Result<ToolOutput, GuestError> {
        if !matches!(name, "fetch_url" | "read_web_page") {
            return Err(GuestError::new("unknown_tool", format!("no tool `{name}`")));
        }
        let url = string_arg(args, "url")?
            .ok_or_else(|| GuestError::new("invalid_argument", "`url` is required"))?;
        // A fragment never reaches the server; drop it rather than refuse.
        let url = url.split('#').next().unwrap_or_default();
        match name {
            "fetch_url" => fetch_url(url),
            _ => read_web_page(url, args),
        }
    }

    fn trigger(
        name: &str,
        _job: &str,
        args: &Args,
        cursor: Option<&[u8]>,
    ) -> Result<TriggerOutput, GuestError> {
        if name != "page_changed" {
            return Err(GuestError::new(
                "unknown_trigger",
                format!("no trigger `{name}`"),
            ));
        }
        let url = string_arg(args, "url")?
            .ok_or_else(|| GuestError::new("invalid_argument", "`url` is required"))?;
        page_changed(url.split('#').next().unwrap_or_default(), cursor)
    }
}

/// Fire when the page's readable text no longer has the fingerprint the
/// previous check kept. The first check keeps one and does not fire.
fn page_changed(url: &str, cursor: Option<&[u8]>) -> Result<TriggerOutput, GuestError> {
    let response = host::fetch(url)?;
    if !(200..300).contains(&response.status) {
        return Err(GuestError::new(
            "http_status",
            format!("the page answered HTTP {}", response.status),
        ));
    }
    let body = readable(&response.body, response.truncated)
        .ok_or_else(|| GuestError::new("not_text", "the page is not text"))?;
    let html = response
        .content_type
        .as_deref()
        .is_none_or(|kind| kind.to_ascii_lowercase().contains("html"));
    let text = if html {
        extract::extract(body, None, Format::Text, extract::BUDGET).text
    } else {
        body.to_owned()
    };
    let print = fingerprint(text.as_bytes()).to_le_bytes();
    let fired = match cursor {
        Some(kept) if kept != print => {
            let mut report =
                format!("The page at {url} changed. Its readable text now begins:\n\n");
            report.push_str(prefix(&text, REPORT_TEXT));
            if text.len() > REPORT_TEXT {
                report.push_str("\n[the text continues]");
            }
            Some(report)
        }
        _ => None,
    };
    Ok(TriggerOutput {
        fired,
        cursor: Some(ByteBuf::from(print.to_vec())),
    })
}

fn fetch_url(url: &str) -> Result<ToolOutput, GuestError> {
    let response = host::fetch(url)?;
    let mut text = status(&response);
    match readable(&response.body, response.truncated) {
        Some(body) => {
            text.push_str("\n\n");
            text.push_str(prefix(body, MAX_TEXT));
            if body.len() > MAX_TEXT || response.truncated {
                text.push_str(CUT);
            }
        }
        None => text.push_str(&binary(&response)),
    }
    Ok(ToolOutput { text })
}

fn read_web_page(url: &str, args: &Args) -> Result<ToolOutput, GuestError> {
    let format = match string_arg(args, "format")?.unwrap_or("markdown") {
        "markdown" => Format::Markdown,
        "text" => Format::Text,
        other => {
            return Err(GuestError::new(
                "invalid_argument",
                format!("`format` is `markdown` or `text`, not `{other}`"),
            ));
        }
    };
    let metadata = bool_arg(args, "include_metadata")?.unwrap_or(false);
    let max = int_arg(args, "max_bytes")?.unwrap_or(DEFAULT_BYTES);
    let max = usize::try_from(max.clamp(1, MAX_TEXT as i64)).unwrap_or(MAX_TEXT);
    let response = host::fetch(url)?;
    let mut text = status(&response);
    let Some(body) = readable(&response.body, response.truncated) else {
        text.push_str(&binary(&response));
        return Ok(ToolOutput { text });
    };
    let html = response
        .content_type
        .as_deref()
        .is_none_or(|kind| kind.to_ascii_lowercase().contains("html"));
    let (content, page_title, description, cut_source) = if html {
        // After a redirect the page's address is not known (the host never
        // returns it), so relative links cannot be resolved.
        let base = (!response.redirected).then_some(url);
        let page = extract::extract(body, base, format, extract::BUDGET);
        (page.text, page.title, page.description, page.cut)
    } else {
        (body.to_string(), None, None, false)
    };
    if metadata {
        for (label, value) in [("Title", &page_title), ("Description", &description)] {
            if let Some(value) = value {
                text.push_str(&format!("\n{label}: {value}"));
            }
        }
    }
    text.push_str("\n\n");
    text.push_str(if content.is_empty() {
        "[the page has no readable text]"
    } else {
        prefix(&content, max)
    });
    if content.len() > max || cut_source || response.truncated {
        text.push_str(CUT);
    }
    Ok(ToolOutput { text })
}

fn status(response: &FetchResponse) -> String {
    let mut text = format!("HTTP {}", response.status);
    if let Some(content_type) = &response.content_type {
        text.push_str(&format!("\nContent-Type: {content_type}"));
    }
    text
}
fn binary(response: &FetchResponse) -> String {
    format!(
        "\n\n[{} bytes of non-text content not shown]",
        response.body.len()
    )
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
