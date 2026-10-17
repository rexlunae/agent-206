//! Readable text from an HTML page, in one pass over the bytes and without
//! a DOM: the invocation's fuel would not cover a full HTML5 parser on a
//! large page. Scripts, styles, navigation, forms and other page chrome are
//! dropped; when the page marks its content with `<main>` or `<article>`,
//! only that content is kept.
//!
//! Extraction meters its own work, in approximate interpreter fuel, and
//! stops with [`Page::cut`] set before the invocation's fuel runs out. The
//! costs are measured, with a margin, by the web extension's fuel tests.

/// What [`extract`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Page {
    pub title: Option<String>,
    pub description: Option<String>,
    pub text: String,
    /// The work budget ran out before the end of the page.
    pub cut: bool,
}

/// Fuel the extraction may spend: the invocation has 50,000,000, of which
/// the fetch, argument and output encoding take a few million.
pub const BUDGET: u64 = 36_000_000;
/// Approximate fuel per byte of page read, per tag, and per link written.
const BYTE_COST: u64 = 150;
const TAG_COST: u64 = 4_500;
const LINK_COST: u64 = 14_000;
/// Further fuel for resolving a page-relative link's path.
const RELATIVE_COST: u64 = 20_000;
const REFERENCE_COST: u64 = 1_500;
const WORD_COST: u64 = 600;
/// Most text read between two checks of the budget.
const TEXT_CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Markdown,
    Text,
}

/// Whether an element's contents are never page text, and if so whether
/// they are raw: text that is not HTML and runs to the closing tag.
fn dropped(name: &str) -> Option<bool> {
    match name {
        "script" | "style" | "textarea" => Some(true),
        "noscript" | "template" | "svg" | "math" | "nav" | "header" | "footer" | "aside"
        | "form" | "button" | "select" | "iframe" | "object" | "canvas" | "dialog" => Some(false),
        _ => None,
    }
}
/// Elements that start and end a block of text.
fn block(name: &str) -> bool {
    matches!(
        name,
        "p" | "div"
            | "section"
            | "article"
            | "main"
            | "blockquote"
            | "table"
            | "tr"
            | "ul"
            | "ol"
            | "dl"
            | "dt"
            | "dd"
            | "figure"
            | "figcaption"
            | "details"
            | "summary"
            | "address"
            | "body"
    )
}
/// Longest tag name compared; every name this module knows is shorter.
const MAX_NAME: usize = 12;

/// The readable text of `html`. Relative links are made absolute against
/// `base`, the page's address; without one, only absolute links are kept.
pub fn extract(html: &str, base: Option<&str>, format: Format, budget: u64) -> Page {
    let mut state = State {
        format,
        base: base.and_then(|base| origin(base).map(|origin| (origin, base))),
        all: Out::default(),
        main: Out::default(),
        main_depth: 0,
        dropped: Vec::new(),
        pre: 0,
        lists: Vec::new(),
        link: None,
        title: None,
        description: None,
        work: 0,
    };
    let mut at = 0;
    let mut cut = false;
    while at < html.len() {
        // Every scan below stays within what the budget can still pay for,
        // so one long script, comment or text run cannot outspend it.
        let room =
            usize::try_from(budget.saturating_sub(state.work) / BYTE_COST).unwrap_or(usize::MAX);
        let mut end = at.saturating_add(room).min(html.len());
        while !html.is_char_boundary(end) {
            end -= 1;
        }
        let whole = end == html.len();
        let view = &html[..end];
        // Looked for no further than one chunk, which is all a text run
        // needs before the budget is checked again.
        let look = end.min(at.saturating_add(TEXT_CHUNK + 1));
        let text_end = view.as_bytes()[at..look]
            .iter()
            .position(|&byte| byte == b'<')
            .map_or(look, |i| at + i);
        if text_end - at > TEXT_CHUNK {
            // A long run of text is read a chunk at a time, ending at a
            // space where there is one, so the budget is checked in between.
            let mut stop = at + TEXT_CHUNK;
            while !view.is_char_boundary(stop) {
                stop -= 1;
            }
            if let Some(space) = view.as_bytes()[at..stop]
                .iter()
                .rposition(u8::is_ascii_whitespace)
            {
                stop = at + space + 1;
            }
            state.charge(stop - at);
            state.text(&view[at..stop]);
            at = stop;
            continue;
        }
        let Some(open) = (text_end < end).then_some(text_end) else {
            // Text to the end of what can be read; a cut ends at a space so
            // no word is split.
            let stop = if whole {
                end
            } else {
                view.as_bytes()[at..]
                    .iter()
                    .rposition(u8::is_ascii_whitespace)
                    .map_or(at, |space| at + space)
            };
            state.text(&view[at..stop]);
            cut = !whole;
            break;
        };
        if open > at {
            state.charge(open - at);
            state.text(&view[at..open]);
        }
        let next = state.markup(view, open);
        if next == end && !whole {
            // The markup runs past what the budget can read.
            cut = true;
            break;
        }
        state.charge(next - open);
        state.work = state.work.saturating_add(TAG_COST);
        at = next;
    }
    let main = state.main.finish();
    Page {
        title: state.title,
        description: state.description,
        text: if main.is_empty() {
            state.all.finish()
        } else {
            main
        },
        cut,
    }
}

struct State<'a> {
    format: Format,
    /// The page's origin (`scheme://host`) and its whole address.
    base: Option<(&'a str, &'a str)>,
    all: Out,
    /// Text inside `<main>` or `<article>`, which wins when there is any.
    main: Out,
    main_depth: usize,
    /// Dropped elements open around the current position, innermost last.
    dropped: Vec<String>,
    pre: usize,
    /// For each open list, the next item number, or `None` when unordered.
    lists: Vec<Option<u32>>,
    /// The open link's target, once its text has started.
    link: Option<String>,
    title: Option<String>,
    description: Option<String>,
    /// Fuel spent so far, approximately.
    work: u64,
}

impl State<'_> {
    fn charge(&mut self, bytes: usize) {
        self.work = self
            .work
            .saturating_add(BYTE_COST.saturating_mul(bytes as u64));
    }
    fn markdown(&self) -> bool {
        self.format == Format::Markdown
    }
    fn emit(&mut self, write: impl Fn(&mut Out)) {
        if !self.dropped.is_empty() {
            return;
        }
        write(&mut self.all);
        if self.main_depth > 0 {
            write(&mut self.main);
        }
    }
    fn text(&mut self, raw: &str) {
        let (text, references) = decode_counted(raw);
        self.work = self
            .work
            .saturating_add(references.saturating_mul(REFERENCE_COST));
        let pre = self.pre > 0;
        let words = if pre {
            1
        } else {
            text.as_bytes()
                .windows(2)
                .filter(|pair| !pair[0].is_ascii_whitespace() && pair[1].is_ascii_whitespace())
                .count() as u64
                + 1
        };
        self.work = self.work.saturating_add(words.saturating_mul(WORD_COST));
        self.emit(|out| out.words(&text, pre));
    }
    fn block(&mut self) {
        self.emit(|out| out.breaks(2));
    }

    /// Handle the markup starting at `open`, returning where text resumes.
    fn markup(&mut self, html: &str, open: usize) -> usize {
        let rest = &html[open..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            return comment
                .find("-->")
                .map_or(html.len(), |end| open + 4 + end + 3);
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            return rest.find('>').map_or(html.len(), |end| open + end + 1);
        }
        let closing = rest.as_bytes().get(1) == Some(&b'/');
        let name_start = open + 1 + usize::from(closing);
        let name_len = html[name_start..]
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric())
            .count();
        if name_len == 0 || !html.as_bytes()[name_start].is_ascii_alphabetic() {
            // A `<` that starts no tag is text.
            self.text("<");
            return open + 1;
        }
        // Lowercased in place: an allocation per tag costs more fuel than
        // the rest of the tag's handling.
        let mut buffer = [0u8; MAX_NAME];
        let name = match html.as_bytes().get(name_start..name_start + name_len) {
            Some(raw) if raw.len() <= MAX_NAME => {
                for (to, from) in buffer.iter_mut().zip(raw) {
                    *to = from.to_ascii_lowercase();
                }
                std::str::from_utf8(&buffer[..raw.len()]).unwrap_or_default()
            }
            _ => "",
        };
        let Some((attributes, end)) = tag_end(html, name_start + name_len) else {
            // The tag never ends: none of it is page text, and its partial
            // attributes are not worth reading.
            return html.len();
        };
        if closing {
            self.close(name);
            return end;
        }
        let self_closing = attributes.trim_end().ends_with('/');
        if let Some(raw) = dropped(name) {
            if raw {
                // Raw text runs to the matching close tag, whatever it holds.
                return raw_end(html, end, name);
            }
            if !self_closing {
                self.dropped.push(name.to_string());
            }
            return end;
        }
        if !self.dropped.is_empty() {
            // Inside dropped content nothing else is opened, so its links,
            // lists and `<title>` (as in an `<svg>`) leave the page alone.
            return end;
        }
        if name == "title" {
            let close = raw_end(html, end, "title");
            let inner = html[end..close]
                .rsplit_once("</")
                .map_or("", |(text, _)| text);
            if self.title.is_none() {
                self.title = Some(collapse(&decode(inner))).filter(|title| !title.is_empty());
            }
            return close;
        }
        if name == "meta" {
            self.meta(attributes);
            return end;
        }
        self.open(name, attributes);
        end
    }

    fn open(&mut self, name: &str, attributes: &str) {
        let markdown = self.markdown();
        match name {
            "main" | "article" => {
                self.main_depth += 1;
                self.block();
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.block();
                if markdown {
                    let level = usize::from(name.as_bytes()[1] - b'0');
                    self.emit(|out| {
                        out.raw(&"######"[..level]);
                        out.raw(" ");
                    });
                }
            }
            "br" => self.emit(|out| out.breaks(1)),
            "hr" => {
                self.block();
                if markdown {
                    self.emit(|out| out.raw("---"));
                }
                self.block();
            }
            "pre" => {
                self.block();
                if markdown {
                    self.emit(|out| out.raw("```\n"));
                }
                self.pre += 1;
            }
            "code" if markdown && self.pre == 0 => self.emit(|out| out.inline("`")),
            "ul" | "ol" => {
                self.block();
                self.lists.push((name == "ol").then_some(1));
            }
            "li" => {
                let depth = self.lists.len().saturating_sub(1);
                let number = match self.lists.last_mut() {
                    Some(Some(next)) => {
                        *next += 1;
                        Some(*next - 1)
                    }
                    _ => None,
                };
                self.emit(|out| {
                    out.breaks(1);
                    for _ in 0..depth {
                        out.raw("  ");
                    }
                    match number {
                        Some(number) => {
                            out.number(number);
                            out.raw(". ");
                        }
                        None => out.raw("- "),
                    }
                });
            }
            "td" | "th" => self.emit(|out| out.space()),
            "a" if markdown && self.link.is_none() => {
                if let Some(target) =
                    attribute(attributes, "href").and_then(|href| self.absolute(&href).map(escape))
                {
                    self.emit(|out| out.inline("["));
                    self.link = Some(target);
                    self.work += LINK_COST;
                }
            }
            _ if block(name) => self.block(),
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        if !self.dropped.is_empty() {
            if self.dropped.last().is_some_and(|open| open == name) {
                self.dropped.pop();
            }
            return;
        }
        let markdown = self.markdown();
        match name {
            "main" | "article" => {
                self.block();
                self.main_depth = self.main_depth.saturating_sub(1);
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => self.block(),
            "pre" if self.pre > 0 => {
                self.pre -= 1;
                if markdown {
                    self.emit(|out| {
                        out.breaks(1);
                        out.raw("```");
                    });
                }
                self.block();
            }
            "code" if markdown && self.pre == 0 => self.emit(|out| out.inline("`")),
            "ul" | "ol" => {
                self.lists.pop();
                self.block();
            }
            "a" => {
                if let Some(target) = self.link.take() {
                    self.emit(|out| {
                        out.inline("](");
                        out.raw(&target);
                        out.raw(")");
                    });
                }
            }
            _ if block(name) => self.block(),
            _ => {}
        }
    }

    fn meta(&mut self, attributes: &str) {
        let name = attribute(attributes, "name")
            .or_else(|| attribute(attributes, "property"))
            .map(|name| name.to_ascii_lowercase());
        if matches!(name.as_deref(), Some("description" | "og:description"))
            && self.description.is_none()
        {
            self.description = attribute(attributes, "content")
                .map(|content| collapse(&content))
                .filter(|content| !content.is_empty());
        }
    }

    /// `href` as an absolute `http(s)` URL, or `None` for anything else
    /// (scripts, mail, fragments, or a relative link without a base).
    fn absolute(&mut self, href: &str) -> Option<String> {
        let href = href.trim();
        if href.starts_with("https://") || href.starts_with("http://") {
            return Some(href.to_string());
        }
        if let Some(rest) = href.strip_prefix("//") {
            return Some(["https://", rest].concat());
        }
        let scheme = href
            .find(':')
            .is_some_and(|colon| !href[..colon].contains(['/', '?', '#']));
        if href.is_empty() || href.starts_with('#') || scheme {
            return None;
        }
        let (origin, base) = self.base?;
        self.work += RELATIVE_COST;
        if href.starts_with('/') {
            return Some([origin, &remove_dots(href)].concat());
        }
        let after = &base[origin.len()..];
        let path = &after[..after.find(['?', '#']).unwrap_or(after.len())];
        if href.starts_with('?') {
            let path = if path.is_empty() { "/" } else { path };
            return Some([origin, path, href].concat());
        }
        let directory = path.rfind('/').map_or("/", |slash| &path[..=slash]);
        Some([origin, &remove_dots(&[directory, href].concat())].concat())
    }
}

/// The attributes text of a tag and where it ends, honouring quotes so a
/// `>` inside a value does not end it; `None` when nothing ends it.
fn tag_end(html: &str, from: usize) -> Option<(&str, usize)> {
    let bytes = html.as_bytes();
    let mut at = from;
    // Jumps between the bytes that matter, which the interpreter does far
    // more cheaply than stepping through every byte with a quote state.
    loop {
        at += bytes[at..]
            .iter()
            .position(|&byte| matches!(byte, b'>' | b'"' | b'\''))?;
        let byte = bytes[at];
        if byte == b'>' {
            return Some((&html[from..at], at + 1));
        }
        at += 1 + bytes[at + 1..].iter().position(|&next| next == byte)? + 1;
    }
}

/// Where the raw text of `name` opened at `from` ends: past `</name`'s `>`.
fn raw_end(html: &str, from: usize, name: &str) -> usize {
    let bytes = html.as_bytes();
    let mut at = from;
    // A search for one byte is far cheaper in the interpreter than for "</".
    while let Some(found) = html[at..].find('<') {
        let start = at + found + 1;
        let after = start + 1 + name.len();
        let candidate = bytes.get(start + 1..after);
        // `</scripture>` does not close a script: the name must end there.
        let ends = bytes
            .get(after)
            .is_none_or(|&byte| byte == b'>' || byte == b'/' || byte.is_ascii_whitespace());
        if bytes.get(start) == Some(&b'/')
            && candidate.is_some_and(|tag| tag.eq_ignore_ascii_case(name.as_bytes()))
            && ends
        {
            return html[start..]
                .find('>')
                .map_or(html.len(), |end| start + end + 1);
        }
        at = start;
    }
    html.len()
}

/// An attribute's decoded value, by case-insensitive name.
fn attribute(attributes: &str, wanted: &str) -> Option<String> {
    let bytes = attributes.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        while at < bytes.len() && (bytes[at].is_ascii_whitespace() || bytes[at] == b'/') {
            at += 1;
        }
        let start = at;
        while at < bytes.len()
            && !matches!(bytes[at], b'=' | b'/' | b'>')
            && !bytes[at].is_ascii_whitespace()
        {
            at += 1;
        }
        let name = &attributes[start..at];
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let value = if bytes.get(at) == Some(&b'=') {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            match bytes.get(at) {
                Some(&quote @ (b'"' | b'\'')) => {
                    let begin = at + 1;
                    let end = attributes[begin..]
                        .find(char::from(quote))
                        .map_or(attributes.len(), |end| begin + end);
                    at = end + 1;
                    &attributes[begin..end]
                }
                _ => {
                    let begin = at;
                    while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                        at += 1;
                    }
                    &attributes[begin..at]
                }
            }
        } else {
            ""
        };
        if name.eq_ignore_ascii_case(wanted) {
            return Some(decode(value).into_owned());
        }
        if name.is_empty() {
            at += 1;
        }
    }
    None
}

/// `scheme://host` of an `http(s)` URL.
fn origin(url: &str) -> Option<&str> {
    let after = url.find("://")? + 3;
    let end = url[after..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |end| after + end);
    url.starts_with("http").then(|| &url[..end])
}

/// A URL as a Markdown link destination: bytes that would end or break it
/// (whitespace, controls, brackets, quotes) are percent-encoded, as a
/// browser encodes them in a request.
fn escape(url: String) -> String {
    let unsafe_byte =
        |byte: u8| byte <= b' ' || matches!(byte, b'<' | b'>' | b'(' | b')' | b'"' | b'`' | 0x7f);
    if !url.bytes().any(unsafe_byte) {
        return url;
    }
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(url.len() + 8);
    for c in url.chars() {
        match u8::try_from(c) {
            Ok(byte) if unsafe_byte(byte) => {
                out.push('%');
                out.push(char::from(HEX[usize::from(byte >> 4)]));
                out.push(char::from(HEX[usize::from(byte & 15)]));
            }
            _ => out.push(c),
        }
    }
    out
}

/// An absolute path (with any query or fragment) with its `.` and `..`
/// segments resolved, as a browser does.
fn remove_dots(path: &str) -> String {
    let (path, tail) = path.split_at(path.find(['?', '#']).unwrap_or(path.len()));
    let parts: Vec<&str> = path.split('/').skip(1).collect();
    let mut kept: Vec<&str> = Vec::with_capacity(parts.len());
    for (index, part) in parts.iter().enumerate() {
        let last = index + 1 == parts.len();
        match *part {
            "." => {}
            ".." => {
                kept.pop();
            }
            part => {
                kept.push(part);
                continue;
            }
        }
        // `a/.` and `a/..` name a directory, so they keep a final slash.
        if last {
            kept.push("");
        }
    }
    ["/", &kept.join("/"), tail].concat()
}

/// Text with its character references decoded. Unknown names stay as they
/// were written.
fn decode(text: &str) -> std::borrow::Cow<'_, str> {
    decode_counted(text).0
}
/// [`decode`], and how many `&` it met, for the work meter.
fn decode_counted(text: &str) -> (std::borrow::Cow<'_, str>, u64) {
    if !text.contains('&') {
        return (text.into(), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut count = 0;
    while let Some(amp) = rest.find('&') {
        count += 1;
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        // Only the next few bytes can end a reference: a page of `&` with no
        // `;` must not make every one search to the end.
        let window = &rest.as_bytes()[1..rest.len().min(MAX_REFERENCE + 2)];
        let reference = window
            .iter()
            .position(|&byte| byte == b';')
            .and_then(|end| character(&rest[1..1 + end]).map(|c| (c, end + 2)));
        match reference {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    (out.into(), count)
}
/// Longest character reference name decoded, in bytes.
const MAX_REFERENCE: usize = 32;
fn character(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        return char::from_u32(code).filter(|c| *c != '\0');
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "times" => '×',
        _ => return None,
    })
}
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Text being written: runs of whitespace become one space, and breaks
/// between blocks are at most one blank line.
#[derive(Default)]
struct Out {
    text: String,
    /// Line breaks owed before the next text, at most 2.
    breaks: usize,
    space: bool,
}
impl Out {
    fn words(&mut self, text: &str, pre: bool) {
        if pre {
            self.raw(text);
            return;
        }
        // By byte rather than by `char`: HTML whitespace is ASCII, and
        // decoding every character costs the interpreter several times more.
        let bytes = text.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at].is_ascii_whitespace() {
                self.space = true;
                at += 1;
                continue;
            }
            let start = at;
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            self.inline(&text[start..at]);
        }
    }
    /// A list item's number, without the formatting machinery.
    fn number(&mut self, number: u32) {
        let mut digits = [0u8; 10];
        let mut at = digits.len();
        let mut rest = number;
        loop {
            at -= 1;
            digits[at] = b'0' + (rest % 10) as u8;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
        self.raw(std::str::from_utf8(&digits[at..]).unwrap_or_default());
    }
    /// Text that joins the words around it, like a link's brackets.
    fn inline(&mut self, text: &str) {
        self.flush();
        if self.space && !self.text.is_empty() && !self.text.ends_with('\n') {
            self.text.push(' ');
        }
        self.space = false;
        self.text.push_str(text);
    }
    /// Text that starts at the current position as written.
    fn raw(&mut self, text: &str) {
        self.flush();
        self.space = false;
        self.text.push_str(text);
    }
    fn space(&mut self) {
        self.space = true;
    }
    fn breaks(&mut self, count: usize) {
        self.breaks = self.breaks.max(count);
        self.space = false;
    }
    fn flush(&mut self) {
        if self.breaks > 0 && !self.text.is_empty() {
            let kept = self.text.trim_end_matches([' ', '\t']).len();
            self.text.truncate(kept);
            let have = self.text.len() - self.text.trim_end_matches('\n').len();
            for _ in have..self.breaks {
                self.text.push('\n');
            }
        }
        self.breaks = 0;
    }
    fn finish(self) -> String {
        self.text.trim().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "https://site.example/docs/page?x=1";

    fn markdown(html: &str) -> String {
        extract(html, Some(BASE), Format::Markdown, BUDGET).text
    }

    #[test]
    fn page_chrome_and_scripts_are_dropped_and_main_content_wins() {
        let html = r#"<!doctype html><html><head><title> The  Title </title>
<meta name="Description" content="About &amp; more">
<style>p { color: red }</style><script>let x = "</p>";</script></head>
<body><header><a href="/">Home</a></header><nav><ul><li>Menu</li></ul></nav>
<main><h1>Heading</h1><p>First <b>bold</b> line.</p><!-- hidden --><p>Second</p></main>
<aside>Ads</aside><footer>Copyright</footer><form><button>Go</button></form></body></html>"#;
        let page = extract(html, Some(BASE), Format::Markdown, BUDGET);
        assert_eq!(page.title.as_deref(), Some("The Title"));
        assert_eq!(page.description.as_deref(), Some("About & more"));
        assert_eq!(page.text, "# Heading\n\nFirst bold line.\n\nSecond");
        // Without <main> or <article>, the whole body is kept, chrome aside.
        let plain = "<body><nav>Menu</nav><div>One</div><div>Two</div><footer>F</footer></body>";
        assert_eq!(markdown(plain), "One\n\nTwo");
    }

    #[test]
    fn markdown_keeps_structure_and_text_does_not() {
        let html = "<article><h2>Title</h2><ul><li>a</li><li>b<ol><li>c</li><li>d</li></ol></li></ul>\
<p>See <a href=\"/other\">the other</a>, <a href=\"https://x.example/y\">x</a>, \
<a href=\"javascript:alert(1)\">no</a> and <a href=\"rel\">rel</a>.</p>\
<p>Use <code>cargo</code>:</p><pre>fn  main() {\n    x();\n}</pre><hr>after<br>line</article>";
        assert_eq!(
            markdown(html),
            "## Title\n\n- a\n- b\n\n  1. c\n  2. d\n\n\
See [the other](https://site.example/other), [x](https://x.example/y), no and \
[rel](https://site.example/docs/rel).\n\n\
Use `cargo`:\n\n```\nfn  main() {\n    x();\n}\n```\n\n---\n\nafter\nline"
        );
        assert_eq!(
            extract(html, Some(BASE), Format::Text, BUDGET).text,
            "Title\n\n- a\n- b\n\n  1. c\n  2. d\n\n\
See the other, x, no and rel.\n\nUse cargo:\n\nfn  main() {\n    x();\n}\n\nafter\nline"
        );
    }

    #[test]
    fn references_and_stray_brackets_decode_as_text() {
        assert_eq!(
            markdown(
                "<p>a &lt; b &amp;&amp; c &gt; d &#x263A; &#9731; &mdash; &bogus; &amp</p><p>1 < 2</p>"
            ),
            "a < b && c > d ☺ ☃ — &bogus; &amp\n\n1 < 2"
        );
        assert_eq!(
            attribute(r#" data-x='1 > 2' HREF = "a&amp;b" alt=c"#, "href").as_deref(),
            Some("a&b")
        );
        assert_eq!(attribute(" alt=c", "alt").as_deref(), Some("c"));
        assert_eq!(attribute(" disabled", "href"), None);
    }

    #[test]
    fn unclosed_and_nested_markup_never_loses_the_rest() {
        assert_eq!(markdown("<p>open <span>never closed"), "open never closed");
        assert_eq!(markdown("<nav><nav>x</nav>still nav</nav>after"), "after");
        assert_eq!(markdown("<script>no end"), "");
        assert_eq!(markdown("<p a=\"unterminated>text"), "");
        assert_eq!(markdown("<SCRIPT>x</script >shown"), "shown");
    }

    #[test]
    fn a_spent_budget_cuts_the_page_between_words() {
        let html = "<p>one two three four five six seven eight nine ten</p>";
        let whole = extract(html, Some(BASE), Format::Text, BUDGET);
        assert!(!whole.cut);
        let cut = extract(html, Some(BASE), Format::Text, TAG_COST + 20 * BYTE_COST);
        assert!(cut.cut);
        assert!(
            !cut.text.is_empty()
                && whole.text.starts_with(&cut.text)
                && cut.text.len() < whole.text.len(),
            "{cut:?}"
        );
        assert!(!cut.text.ends_with(' '));
        assert!(extract(html, Some(BASE), Format::Text, 0).cut);
    }

    #[test]
    fn relative_links_resolve_against_the_page_and_need_its_address() {
        let link = |href: &str, base: Option<&str>| {
            extract(
                &format!("<a href=\"{href}\">t</a>"),
                base,
                Format::Markdown,
                BUDGET,
            )
            .text
        };
        for (href, expected) in [
            ("chapter.html", "https://site.example/docs/chapter.html"),
            ("./a/../b?c=1#d", "https://site.example/docs/b?c=1#d"),
            ("../../../up", "https://site.example/up"),
            ("..", "https://site.example/"),
            ("?page=2", "https://site.example/docs/page?page=2"),
            ("/x/./y/..", "https://site.example/x/"),
            (
                "next page (2)",
                "https://site.example/docs/next%20page%20%282%29",
            ),
            (
                "https://x.example/a&quot;b<c>",
                "https://x.example/a%22b%3Cc%3E",
            ),
        ] {
            assert_eq!(link(href, Some(BASE)), format!("[t]({expected})"), "{href}");
        }
        assert_eq!(
            link("?q", Some("https://site.example")),
            "[t](https://site.example/?q)"
        );
        for href in ["#top", "mailto:a@site.example", "", "data:text/plain,x"] {
            assert_eq!(link(href, Some(BASE)), "t", "{href}");
        }
        // Without the page's address (after a redirect), a relative link
        // cannot be resolved correctly, so only its text is kept.
        assert_eq!(link("/x", None), "t");
        assert_eq!(link("https://x.example/", None), "[t](https://x.example/)");
    }

    #[test]
    fn dropped_content_changes_nothing_around_it() {
        let html = "<p>intro</p><nav><a href=\"/menu\">Menu</a><main>x</main><ul><li>m</li>\
<pre>p</pre><svg><title>icon</title></svg></nav><p><a href=\"/article\">Article</a></p>\
<title>Real</title>";
        let page = extract(html, Some(BASE), Format::Markdown, BUDGET);
        assert_eq!(
            page.text,
            "intro\n\n[Article](https://site.example/article)"
        );
        assert_eq!(page.title.as_deref(), Some("Real"));
        assert_eq!(
            extract("<TITLE>News</TITLE >", None, Format::Text, BUDGET)
                .title
                .as_deref(),
            Some("News")
        );
    }

    #[test]
    fn raw_text_ends_only_at_its_own_closing_tag() {
        assert_eq!(
            markdown("<script>a(); </scripture>hidden()</script><p>Article</p>"),
            "Article"
        );
        assert_eq!(
            markdown("<style></styles>b{}</style/>shown<script></script"),
            "shown"
        );
    }
}
