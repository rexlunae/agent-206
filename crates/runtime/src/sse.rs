//! Incremental, bounded `text/event-stream` decoding (the WHATWG
//! server-sent events format) for provider responses.
//!
//! Bytes may arrive split anywhere, including inside a line terminator or a
//! UTF-8 sequence. Lines and events are bounded so a hostile or broken
//! upstream cannot grow memory without limit.

/// The largest line or event payload accepted. Provider events carry one
/// delta each; anything this large is not a stream we can relay.
pub const MAX_EVENT: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SseError {
    #[error("a server-sent event exceeds {MAX_EVENT} bytes")]
    TooLarge,
    #[error("a server-sent event is not valid UTF-8")]
    Utf8,
}

/// One dispatched event: its `event:` name, if any, and its joined `data:`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub name: Option<String>,
    pub data: String,
}

#[derive(Default)]
pub struct Decoder {
    line: Vec<u8>,
    name: Option<String>,
    data: String,
    has_data: bool,
    /// The previous byte was a CR, so a following LF ends nothing new.
    after_cr: bool,
    /// Past the stream start, where a byte-order mark may appear.
    started: bool,
    /// Bytes of a leading byte-order mark matched so far.
    bom: usize,
}

impl Decoder {
    /// Feed the next bytes and return every event they complete, in order.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Event>, SseError> {
        let mut events = vec![];
        self.feed_into(bytes, &mut events)?;
        Ok(events)
    }

    /// Feed the next bytes, appending every event they complete to `events`.
    /// Events completed before a decoding error stay in `events`, so a caller
    /// can keep what arrived intact before the stream went bad.
    pub fn feed_into(&mut self, bytes: &[u8], events: &mut Vec<Event>) -> Result<(), SseError> {
        const BOM: &[u8] = b"\xEF\xBB\xBF";
        for &byte in bytes {
            if !self.started {
                // A leading byte-order mark is skipped, even when split.
                if byte == BOM[self.bom] {
                    self.bom += 1;
                    self.started = self.bom == BOM.len();
                    continue;
                }
                self.started = true;
                for &held in &BOM[..self.bom] {
                    self.byte(held, events)?;
                }
            }
            self.byte(byte, events)?;
        }
        Ok(())
    }

    fn byte(&mut self, byte: u8, events: &mut Vec<Event>) -> Result<(), SseError> {
        let after_cr = std::mem::replace(&mut self.after_cr, byte == b'\r');
        match byte {
            b'\n' if after_cr => {}
            b'\n' | b'\r' => {
                let line = std::mem::take(&mut self.line);
                if let Some(event) = self.line_done(&line)? {
                    events.push(event);
                }
            }
            _ => {
                if self.line.len() >= MAX_EVENT {
                    return Err(SseError::TooLarge);
                }
                self.line.push(byte);
            }
        }
        Ok(())
    }

    fn line_done(&mut self, line: &[u8]) -> Result<Option<Event>, SseError> {
        if line.is_empty() {
            let data = std::mem::take(&mut self.data);
            let name = self.name.take();
            return Ok(std::mem::take(&mut self.has_data).then_some(Event { name, data }));
        }
        let line = std::str::from_utf8(line).map_err(|_| SseError::Utf8)?;
        if line.starts_with(':') {
            return Ok(None);
        }
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                if self.data.len() + value.len() > MAX_EVENT {
                    return Err(SseError::TooLarge);
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.name = Some(value.to_owned()),
            // `id` and `retry` steer browser reconnection; a provider call is
            // never resumed, so they carry nothing here.
            _ => {}
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn random() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[test]
    fn events_before_a_decoding_error_are_kept() {
        let text = random();
        let mut decoder = Decoder::default();
        let mut events = vec![];
        let bytes = [
            format!("data: {text}\n\n").into_bytes(),
            b"data: \xff\n".to_vec(),
        ]
        .concat();
        assert_eq!(decoder.feed_into(&bytes, &mut events), Err(SseError::Utf8));
        assert_eq!(
            events,
            [Event {
                name: None,
                data: text
            }]
        );
    }

    #[test]
    fn events_survive_any_split_and_every_line_terminator() {
        let (first, second, name) = (random(), random(), random());
        for newline in ["\n", "\r\n", "\r"] {
            let text = format!(
                "\u{feff}: comment{newline}event: {name}{newline}data: {first}{newline}data:{second}{newline}{newline}id: 1{newline}retry: 5{newline}{newline}data{newline}{newline}"
            );
            let bytes = text.as_bytes();
            for size in 1..=bytes.len() {
                let mut decoder = Decoder::default();
                let mut events = vec![];
                for piece in bytes.chunks(size) {
                    events.extend(decoder.feed(piece).unwrap());
                }
                assert_eq!(
                    events,
                    [
                        Event {
                            name: Some(name.clone()),
                            data: format!("{first}\n{second}"),
                        },
                        Event {
                            name: None,
                            data: String::new(),
                        },
                    ],
                    "split every {size} bytes with {newline:?}"
                );
            }
        }
    }

    #[test]
    fn a_split_multibyte_character_is_reassembled() {
        let text = format!("data: {}\u{00e9}\u{1f600}\n\n", random());
        let bytes = text.as_bytes();
        for size in 1..=bytes.len() {
            let mut decoder = Decoder::default();
            let events: Vec<_> = bytes
                .chunks(size)
                .flat_map(|piece| decoder.feed(piece).unwrap())
                .collect();
            assert_eq!(events.len(), 1);
            assert_eq!(format!("data: {}\n\n", events[0].data), text);
        }
    }

    #[test]
    fn invalid_utf8_and_oversized_lines_are_typed_errors() {
        let mut decoder = Decoder::default();
        assert_eq!(decoder.feed(b"data: \xff\xfe\n"), Err(SseError::Utf8));
        let mut decoder = Decoder::default();
        let mut line = b"data: ".to_vec();
        line.resize(MAX_EVENT + 1, b'x');
        assert_eq!(decoder.feed(&line), Err(SseError::TooLarge));
        // Many short data lines cannot add up past the bound either.
        let mut decoder = Decoder::default();
        let piece = format!("data: {}\n", "y".repeat(1024));
        let result =
            (0..=MAX_EVENT / 1024).try_for_each(|_| decoder.feed(piece.as_bytes()).map(drop));
        assert_eq!(result, Err(SseError::TooLarge));
    }

    #[test]
    fn an_unterminated_event_is_not_dispatched() {
        let mut decoder = Decoder::default();
        let events = decoder.feed(format!("data: {}\n", random()).as_bytes());
        assert_eq!(events, Ok(vec![]));
    }
}
