//! Tool-call markup that some models write into their reply text instead of
//! the provider's tool-call channel. A block between a matched open and close
//! tag is a call that never parsed and will never run, so it is removed from
//! the reply; code blocks and inline code are left alone, because a reply
//! about tool calls quotes their markup there.
//!
//! Adapted from RustyClaw's `StreamSanitizer` (`tool_healing.rs`, MIT).

/// How far past a tag the filter looks for the backtick run that would make
/// it inline code, so a long line is not copied again for every chunk. The
/// limit is on the text, not on the chunks, so every chunking of a reply is
/// filtered the same way.
const MAX_HOLD: usize = 4096;

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
/// Chunks split tags anywhere, so text that could still become a tag or a
/// backtick run is held until a later chunk decides it; a tag after an
/// unclosed backtick run waits for the run's close, the line's end or
/// [`MAX_HOLD`] bytes. Fence lines are passed on as they arrive, with only
/// their marker run and whether a closing line stays blank kept as state.
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
    /// The start of the current line, while it may still be a fence.
    lead: Option<Lead>,
    /// The fenced code block being passed through.
    fence: Option<Fence>,
    /// The current line began with a marker that closes the fence if nothing
    /// but whitespace follows it.
    closer: bool,
    /// Length of the backtick run that opened the inline code on this line.
    code: Option<usize>,
    removed: usize,
    /// The stream ended: nothing more can complete held text.
    ended: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Fence {
    mark: u8,
    len: usize,
}

/// Indentation and marker run seen at the start of a line so far.
#[derive(Clone, Copy, Debug, Default)]
struct Lead {
    indent: usize,
    /// The marker byte, or 0 before one.
    mark: u8,
    len: usize,
}

enum Step {
    /// Continue from this byte offset.
    At(usize),
    /// Hold the text from this byte offset for the next chunk.
    Hold(usize),
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
        self.ended = true;
        let out = self.feed("");
        // Only the tail of an unterminated block can remain.
        self.held.clear();
        self.closing = None;
        out
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
        if self.mid_line && self.lead.is_none() {
            return match self.fence {
                Some(_) => self.pass_line(text, at, out),
                None => self.prose(text, at, out, false),
            };
        }
        // The line's indentation and marker run hold no tags, so they are
        // passed on while counted.
        let mut lead = self.lead.take().unwrap_or_default();
        let bytes = text.as_bytes();
        let mut i = at;
        while let Some(&b) = bytes.get(i) {
            let counted = if lead.mark == 0 {
                if b == b' ' && lead.indent < 4 {
                    lead.indent += 1;
                    true
                } else if lead.indent < 4 && (b == b'`' || b == b'~') {
                    lead.mark = b;
                    lead.len = 1;
                    true
                } else {
                    false
                }
            } else if b == lead.mark {
                lead.len += 1;
                true
            } else {
                false
            };
            if !counted {
                break;
            }
            i += 1;
        }
        out.push_str(&text[at..i]);
        self.mid_line = true;
        if i == bytes.len() && !self.ended {
            self.lead = Some(lead);
            return Step::At(i);
        }
        let fence = (lead.mark != 0 && lead.len >= 3).then_some(Fence {
            mark: lead.mark,
            len: lead.len,
        });
        match (self.fence, fence) {
            (None, Some(fence)) => {
                self.fence = Some(fence);
                self.pass_line(text, i, out)
            }
            (Some(open), fence) => {
                self.closer = fence.is_some_and(|f| f.mark == open.mark && f.len >= open.len);
                self.pass_line(text, i, out)
            }
            (None, None) => {
                if lead.mark == b'`' {
                    self.code = Some(lead.len);
                }
                let line_start = lead.indent == 0 && lead.mark == 0;
                self.prose(text, i, out, line_start)
            }
        }
    }

    /// Ordinary text: remove leaked blocks outside inline code.
    fn prose(&mut self, text: &str, at: usize, out: &mut String, line_start: bool) -> Step {
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
                    if i + run == bytes.len() && !self.decide(text, i) {
                        out.push_str(&text[at..i]);
                        return self.hold(line_start && i == at, i);
                    }
                    self.code = match self.code {
                        None => Some(run),
                        Some(open) if open == run => None,
                        open => open,
                    };
                    i += run;
                }
                b'<' => {
                    let tail = &text[i..];
                    if let Some((open, close)) =
                        TAGS.iter().find(|(open, _)| tail.starts_with(open))
                    {
                        // An opening backtick run is code only once a run of
                        // the same length closes it on this line.
                        if let Some(run) = self.code {
                            match closes(tail, run, self.ended) {
                                Some(true) => {
                                    i += 1;
                                    continue;
                                }
                                Some(false) => self.code = None,
                                None => {
                                    out.push_str(&text[at..i]);
                                    return self.hold(line_start && i == at, i);
                                }
                            }
                        }
                        out.push_str(&text[at..i]);
                        self.closing = Some(close);
                        self.removed += 1;
                        // What follows the block continues the line the
                        // block started on.
                        self.mid_line = !(line_start && i == at);
                        return Step::At(i + open.len());
                    }
                    if !self.decide(text, i)
                        && TAGS
                            .iter()
                            .any(|(open, _)| open.len() > tail.len() && open.starts_with(tail))
                    {
                        out.push_str(&text[at..i]);
                        return self.hold(line_start && i == at, i);
                    }
                    i += 1;
                }
                _ => i += 1,
            }
        }
        out.push_str(&text[at..]);
        Step::At(text.len())
    }

    /// Whether text from `from` must be decided now rather than held.
    fn decide(&self, text: &str, from: usize) -> bool {
        self.ended || text.len() - from > MAX_HOLD
    }

    /// Hold the text from `from`, which starts a line when `line_start`.
    fn hold(&mut self, line_start: bool, from: usize) -> Step {
        self.mid_line = !line_start;
        Step::Hold(from)
    }

    /// Pass the rest of a fence line through unchanged; a closing marker
    /// followed only by whitespace ends the fence with its line.
    fn pass_line(&mut self, text: &str, at: usize, out: &mut String) -> Step {
        let rest = &text[at..];
        let end = rest.find('\n');
        let line = &rest[..end.unwrap_or(rest.len())];
        self.closer &= line.bytes().all(|b| matches!(b, b' ' | b'\t' | b'\r'));
        match end {
            Some(pos) => {
                out.push_str(&rest[..=pos]);
                if std::mem::take(&mut self.closer) {
                    self.fence = None;
                }
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

/// Whether a backtick run of `run` closes inline code later on this line
/// (`None` when the text ends first, unless `complete`).
fn closes(text: &str, run: usize, complete: bool) -> Option<bool> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i > MAX_HOLD {
            return Some(false);
        }
        match bytes[i] {
            b'\n' => return Some(false),
            b'`' => {
                let len = bytes[i..].iter().take_while(|&&b| b == b'`').count();
                if i + len == bytes.len() && !complete {
                    return None;
                }
                if len == run {
                    return Some(true);
                }
                i += len;
            }
            _ => i += 1,
        }
    }
    complete.then_some(false)
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
        // A fence closes only on a line of nothing but its marker.
        let text = "```\n```rust\n<tool_call>example</tool_call>\n```  \n<tool_call>x</tool_call>";
        assert_eq!(
            every_split(text),
            (
                "```\n```rust\n<tool_call>example</tool_call>\n```  \n".to_owned(),
                1
            )
        );
        // A removed block that began a line leaves the next text at a line start.
        let text = "Intro\n<tool_call>x\n</tool_call>```xml\n<tool_call>example</tool_call>\n```\n";
        assert_eq!(
            every_split(text),
            (
                "Intro\n```xml\n<tool_call>example</tool_call>\n```\n".to_owned(),
                1
            )
        );
        assert_eq!(
            every_split("a <tool_call>x</tool_call>```\n<tool_call>y</tool_call>"),
            ("a ```\n".to_owned(), 2)
        );
        // A closing fence may end in CRLF.
        assert_eq!(
            every_split("```\r\n<tool_call>e</tool_call>\r\n```\r\n<tool_call>x</tool_call>"),
            ("```\r\n<tool_call>e</tool_call>\r\n```\r\n".to_owned(), 1)
        );
        // Inline code may close on the reply's last byte.
        let text = "Use `<tool_call>x</tool_call>`";
        assert_eq!(every_split(text), (text.to_owned(), 0));
        // An unmatched backtick is literal text.
        assert_eq!(
            every_split("Use `oops <tool_call>{}</tool_call> here\nnext"),
            ("Use `oops  here\nnext".to_owned(), 1)
        );
        assert_eq!(
            every_split("``a` <tool_call>{}</tool_call>`"),
            ("``a` `".to_owned(), 1)
        );
        // Four spaces is indented text, not a fence.
        assert_eq!(
            every_split("    ```\n<tool_call>x</tool_call>"),
            ("    ```\n".to_owned(), 1)
        );
    }

    #[test]
    fn a_long_undecided_line_is_decided_without_waiting_for_its_end() {
        let mut filter = Filter::default();
        assert_eq!(filter.feed("`a <tool_call>"), "`a ");
        let mut out = String::new();
        for _ in 0..1000 {
            out.push_str(&filter.feed(&uuid::Uuid::new_v4().to_string()));
            assert!(filter.held.len() <= MAX_HOLD + 64, "{}", filter.held.len());
        }
        // Decided as an unmatched backtick: the block is removed.
        assert_eq!(out, "");
        assert_eq!(filter.feed("</tool_call>b\n"), "b\n");
        assert_eq!(filter.removed(), 1);
    }

    #[test]
    fn lookahead_limits_do_not_depend_on_chunking() {
        let long = "a".repeat(MAX_HOLD);
        // Inline code closing too far after the tag is not code.
        let text = format!("` <tool_call>x</tool_call> {long}`");
        assert_eq!(every_split(&text), (format!("`  {long}`"), 1));
        let text = format!("`<tool_call>x</tool_call>{}`", &long[..MAX_HOLD - 40]);
        assert_eq!(every_split(&text), (text.clone(), 0));
        // Fence lines of any length are decided by their whole line.
        let spaces = " ".repeat(MAX_HOLD + 1);
        let text = format!(
            "```\n```{spaces}x\n<tool_call>q</tool_call>\n```{spaces}\n<tool_call>l</tool_call>"
        );
        let kept = format!("```\n```{spaces}x\n<tool_call>q</tool_call>\n```{spaces}\n");
        assert_eq!(every_split(&text), (kept, 1));
        let fence = "`".repeat(MAX_HOLD + 1);
        let text = format!("{fence}\n<tool_call>e</tool_call>\n{fence}\n<tool_call>x</tool_call>");
        let kept = format!("{fence}\n<tool_call>e</tool_call>\n{fence}\n");
        assert_eq!(every_split(&text), (kept, 1));
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
