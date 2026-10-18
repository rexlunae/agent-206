//! git's pkt-line framing: a four-digit hexadecimal length that counts
//! itself, then the payload; `0000` is a flush packet.
use crate::WireError;

/// The largest pkt-line git sends or accepts, header included.
pub const MAX_PACKET: usize = 65520;

/// One data packet, or `None` for a flush packet.
pub type Packet<'a> = Option<&'a [u8]>;

/// Append `payload` as one data packet, refused if it does not fit in one.
pub fn data(out: &mut Vec<u8>, payload: &[u8]) -> Result<(), WireError> {
    if payload.len() + 4 > MAX_PACKET {
        return Err(WireError::TooLong(MAX_PACKET));
    }
    out.extend_from_slice(format!("{:04x}", payload.len() + 4).as_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

pub fn flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

/// Reads packets from a complete body.
pub struct Reader<'a> {
    rest: &'a [u8],
}
impl<'a> Reader<'a> {
    pub fn new(body: &'a [u8]) -> Self {
        Self { rest: body }
    }
    pub fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }
    /// What has not been read yet.
    pub fn rest(&self) -> &'a [u8] {
        self.rest
    }
    /// The next packet, or an error at the end of the body or on a malformed
    /// length.
    pub fn next(&mut self) -> Result<Packet<'a>, WireError> {
        let header = self.rest.get(..4).ok_or(WireError::Truncated)?;
        let length = std::str::from_utf8(header)
            .ok()
            .filter(|digits| digits.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|digits| usize::from_str_radix(digits, 16).ok())
            .ok_or(WireError::Malformed("a packet length is not hexadecimal"))?;
        match length {
            0 => {
                self.rest = &self.rest[4..];
                Ok(None)
            }
            // 0001 and 0002 are protocol v2 delimiters, never sent here.
            1..=4 => Err(WireError::Malformed("a packet length is too short")),
            _ if length > MAX_PACKET => Err(WireError::Malformed("a packet is too long")),
            _ => {
                let payload = self.rest.get(4..length).ok_or(WireError::Truncated)?;
                self.rest = &self.rest[length..];
                Ok(Some(payload))
            }
        }
    }
}

/// A packet's text without its trailing newline.
pub fn line(payload: &[u8]) -> &[u8] {
    payload.strip_suffix(b"\n").unwrap_or(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_round_trip_and_malformed_lengths_are_refused() {
        let payload = uuid::Uuid::new_v4().to_string();
        let mut body = vec![];
        data(&mut body, payload.as_bytes()).unwrap();
        flush(&mut body);
        let mut reader = Reader::new(&body);
        assert_eq!(reader.next().unwrap(), Some(payload.as_bytes()));
        assert_eq!(reader.next().unwrap(), None);
        assert!(reader.is_empty());
        assert!(matches!(reader.next(), Err(WireError::Truncated)));

        for bad in [
            &b"00"[..],
            b"zz10abc",
            b"0003",
            b"0001",
            b"0010abc",
            b"fff1",
        ] {
            assert!(Reader::new(bad).next().is_err(), "{bad:?}");
        }
    }
}
