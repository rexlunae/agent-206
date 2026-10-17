//! Tool-call markup that some models write into their reply text instead of
//! the provider's tool-call channel. A block between a matched open and close
//! tag is a call that never parsed and will never run, so it is removed from
//! the reply; code blocks and inline code are left alone, because a reply
//! about tool calls quotes their markup there.
//!
//! Adapted from RustyClaw's `StreamSanitizer` (`tool_healing.rs`, MIT).

/// Open and close tags of leaked tool-call blocks.
const TAGS: [(&str, &str); 6] = [
    ("<tool_call>", "</tool_call>"),
    ("<tool_calls>", "</tool_calls>"),
    ("<function_call>", "</function_call>"),
    ("<function_calls>", "</function_calls>"),
    ("<tool_use>", "</tool_use>"),
    ("<|tool_call|>", "<|/tool_call|>"),
];

/// Removes leaked tool-call blocks from streamed text.
///
/// Chunks split tags anywhere, so text that could still become a tag, a code
/// fence or a backtick run is held until the next chunk decides it; at most a
/// tag's length is held, and only when a chunk ends in something tag-shaped.
/// [`Filter::finish`] releases held text and drops an unterminated block.
#[derive(Debug, Default)]
pub struct Filter {
    /// Text not decided yet, or inside a block the tail that could begin its
    /// close tag.
    held: String,
    /// The close tag awaited inside a leaked block.
    closing: Option<&'static str>,
    /// The next byte does not start a line.
    mid_line: bool,
    /// The fenced code block being passed through.
    fence: Option<Fence>,
    /// Length of the backtick run that opened the inline code on this line.
    code: Option<usize>,
    removed: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fence {
    mark: u8,
    len: usize,
}

enum Step {
    /// Continue from this byte offset.
    At(usize),
    /// Hold the text from this byte offset for the next chunk.
    Hold(usize),
}

enum Mark {
    Fence(Fence),
    None,
    /// The chunk ended before the line showed whether it is a fence.
    Undecided,
}

impl Filter {
    /// Filter one chunk; returns the text that can be shown now.
    pub fn feed(&mut self, chunk: &str) -> String {
        let mut text = std::mem::take(&mut self.held);
        text.push_str(chunk);
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        while at < text.len() {
            match self.step(&text, at, &mut out) {
                Step::At(next) => at = next,
                Step::Hold(from) => {
                    self.held = text[from..].to_owned();
                    break;
                }
            }
        }
        out
    }

    /// The stream ended: held text was ordinary text, an unterminated block
    /// was markup.
    pub fn finish(&mut self) -> String {
        let held = std::mem::take(&mut self.held);
        if self.closing.take().is_some() {
            String::new()
        } else {
            held
        }
    }

    /// Leaked blocks removed so far.
    pub fn removed(&self) -> usize {
        self.removed
    }

    fn step(&mut self, text: &str, at: usize, out: &mut String) -> Step {
        let rest = &text[at..];
        if let Some(close) = self.closing {
            if let Some(pos) = rest.find(close) {
                self.closing = None;
                return Step::At(at + pos + close.len());
            }
            // Only a tail shorter than the close tag can still begin it.
            let mut from = text.len() - (close.len() - 1).min(rest.len());
            while !text.is_char_boundary(from) {
                from -= 1;
            }
            return Step::Hold(from);
        }
        if !self.mid_line {
            match fence_mark(rest) {
                Mark::Undecided => return Step::Hold(at),
                Mark::Fence(mark) => {
                    self.fence = match self.fence {
                        None => Some(mark),
                        Some(open) if mark.mark == open.mark && mark.len >= open.len => None,
                        open => open,
                    };
                    return self.pass_line(text, at, out);
                }
                Mark::None => {}
            }
        }
        if self.fence.is_some() {
            return self.pass_line(text, at, out);
        }
        self.mid_line = true;
        let bytes = text.as_bytes();
        let mut i = at;
        // Every byte compared is ASCII, so each offset is a char boundary.
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => {
                    out.push_str(&text[at..=i]);
                    self.mid_line = false;
                    self.code = None;
                    return Step::At(i + 1);
                }
                b'`' => {
                    let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                    if i + run == bytes.len() {
                        out.push_str(&text[at..i]);
                        return Step::Hold(i);
                    }
                    self.code = match self.code {
                        None => Some(run),
                        Some(open) if open == run => None,
                        open => open,
                    };
                    i += run;
                }
                b'<' if self.code.is_none() => {
                    let tail = &text[i..];
                    if let Some((open, close)) =
                        TAGS.iter().find(|(open, _)| tail.starts_with(open))
                    {
                        out.push_str(&text[at..i]);
                        self.closing = Some(close);
                        self.removed += 1;
                        return Step::At(i + open.len());
                    }
                    if TAGS
                        .iter()
                        .any(|(open, _)| open.len() > tail.len() && open.starts_with(tail))
                    {
                        out.push_str(&text[at..i]);
                        return Step::Hold(i);
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        out.push_str(rest);
        Step::At(text.len())
    }

    /// Pass the rest of a line through unchanged.
    fn pass_line(&mut self, text: &str, at: usize, out: &mut String) -> Step {
        let rest = &text[at..];
        match rest.find('\n') {
            Some(pos) => {
                out.push_str(&rest[..=pos]);
                self.mid_line = false;
                Step::At(at + pos + 1)
            }
            None => {
                out.push_str(rest);
                self.mid_line = true;
                Step::At(text.len())
            }
        }
    }
}

/// Whether a line opens or closes a fenced code block: up to three spaces,
/// then three or more backticks or tildes.
fn fence_mark(line: &str) -> Mark {
    let bytes = line.as_bytes();
    let indent = bytes.iter().take(4).take_while(|&&b| b == b' ').count();
    if indent > 3 {
        return Mark::None;
    }
    let Some(&mark) = bytes.get(indent) else {
        return Mark::Undecided;
    };
    if mark != b'`' && mark != b'~' {
        return Mark::None;
    }
    let len = bytes[indent..].iter().take_while(|&&b| b == mark).count();
    if indent + len == bytes.len() {
        Mark::Undecided
    } else if len < 3 {
        Mark::None
    } else {
        Mark::Fence(Fence { mark, len })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Filter `chunks` as one stream.
    fn filter(chunks: &[&str]) -> (String, usize) {
        let mut filter = Filter::default();
        let mut out: String = chunks.iter().map(|chunk| filter.feed(chunk)).collect();
        out.push_str(&filter.finish());
        (out, filter.removed())
    }

    /// Every split of `text` into two chunks gives the same result.
    fn every_split(text: &str) -> (String, usize) {
        let whole = filter(&[text]);
        for (at, _) in text.char_indices().skip(1) {
            assert_eq!(filter(&[&text[..at], &text[at..]]), whole, "split at {at}");
        }
        let bytes: Vec<String> = text.chars().map(String::from).collect();
        let refs: Vec<&str> = bytes.iter().map(String::as_str).collect();
        assert_eq!(filter(&refs), whole, "one char per chunk");
        whole
    }

    #[test]
    fn ordinary_text_is_unchanged() {
        for text in [
            "compare a < b and use <em>this</em>, or <tool>",
            "the <tool_calls are counted",
            "émojis 🦀 end with <",
            "ends in backticks ``",
            "  ",
        ] {
            assert_eq!(every_split(text), (text.to_owned(), 0));
        }
    }

    #[test]
    fn leaked_blocks_are_removed_wherever_chunks_split_them() {
        for (open, close) in TAGS {
            let call = uuid::Uuid::new_v4();
            let text = format!("Reading it.{open}{{\"name\": \"{call}\"}}{close} Done 🦀.");
            assert_eq!(every_split(&text), ("Reading it. Done 🦀.".to_owned(), 1));
        }
        let text = "a<tool_use>x</tool_use>b<function_calls>y</function_calls>c";
        assert_eq!(every_split(text), ("abc".to_owned(), 2));
    }

    #[test]
    fn an_unterminated_block_is_dropped() {
        assert_eq!(
            every_split("Okay. <tool_call>{\"name\": \"x\""),
            ("Okay. ".to_owned(), 1)
        );
    }

    #[test]
    fn code_blocks_and_inline_code_keep_their_markup() {
        let quoted = [
            "Use `<tool_call>` tags.",
            "Use ``a ` <tool_call>`` tags.",
            "```xml\n<tool_call>{}</tool_call>\n```\n",
            "~~~~\n<tool_call>\n~~~\nstill code </tool_call>\n~~~~\n",
            "   ```\n<tool_use>\n   ```\n",
        ];
        for text in quoted {
            assert_eq!(every_split(text), (text.to_owned(), 0));
        }
        // Inline code ends with its line; a fence ends at its closing line.
        assert_eq!(
            every_split("`open\n<tool_call>x</tool_call>after"),
            ("`open\nafter".to_owned(), 1)
        );
        assert_eq!(
            every_split("```\ncode\n```\nprose<tool_call>x</tool_call>!"),
            ("```\ncode\n```\nprose!".to_owned(), 1)
        );
        // Four spaces is indented text, not a fence.
        assert_eq!(
            every_split("    ```\n<tool_call>x</tool_call>"),
            ("    ```\n".to_owned(), 1)
        );
    }

    #[test]
    fn a_long_block_is_not_kept() {
        let mut filter = Filter::default();
        assert_eq!(filter.feed("a<tool_call>"), "a");
        for _ in 0..1000 {
            assert_eq!(filter.feed(&uuid::Uuid::new_v4().to_string()), "");
            assert!(filter.held.len() < "</tool_call>".len());
        }
        assert_eq!(filter.feed("</tool_call>b"), "b");
        assert_eq!(filter.finish(), "");
    }
}
