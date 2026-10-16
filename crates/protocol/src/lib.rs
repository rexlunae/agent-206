//! Bounded MessagePack framing over reliable byte streams (SSH, pipes, WebSocket).
#![forbid(unsafe_code)]
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use uuid::Uuid;
pub const VERSION: u16 = 1;
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
const HEADER: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request<T> {
    pub id: Uuid,
    pub body: T,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response<T> {
    pub id: Uuid,
    pub body: T,
}
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported protocol or frame header")]
    Header,
    #[error("frame exceeds the 4 MiB limit")]
    TooLarge,
    #[error("invalid MessagePack: {0}")]
    Decode(#[from] rmp_serde::decode::Error),
    #[error("MessagePack encoding failed: {0}")]
    Encode(#[from] rmp_serde::encode::Error),
    #[error("truncated frame")]
    Truncated,
    #[error("trailing MessagePack bytes")]
    Trailing,
}
pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    let payload = rmp_serde::to_vec_named(value)?;
    if payload.len() > MAX_FRAME {
        return Err(Error::TooLarge);
    }
    let mut frame = Vec::with_capacity(HEADER + payload.len());
    frame.extend_from_slice(b"A206");
    frame.extend_from_slice(&VERSION.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}
pub fn payload_size(header: &[u8; HEADER]) -> Result<usize, Error> {
    if &header[..4] != b"A206" || u16::from_be_bytes([header[4], header[5]]) != VERSION {
        return Err(Error::Header);
    }
    let len = u32::from_be_bytes([header[6], header[7], header[8], header[9]]) as usize;
    if len > MAX_FRAME {
        return Err(Error::TooLarge);
    }
    Ok(len)
}
pub fn decode_payload<T: DeserializeOwned>(payload: &[u8]) -> Result<T, Error> {
    if payload.len() > MAX_FRAME {
        return Err(Error::TooLarge);
    }
    let mut cursor = std::io::Cursor::new(payload);
    let value = T::deserialize(&mut rmp_serde::Deserializer::new(&mut cursor))?;
    if cursor.position() != payload.len() as u64 {
        return Err(Error::Trailing);
    }
    Ok(value)
}
/// Handles arbitrary fragmentation/coalescing; never allocates from an unchecked length.
#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}
impl Decoder {
    pub fn feed<T: DeserializeOwned>(&mut self, bytes: &[u8]) -> Result<Vec<T>, Error> {
        let mut frames = Vec::new();
        // Incremental append bounds memory even if a caller supplies many frames at once.
        for chunk in bytes.chunks(8192) {
            self.buffer.extend_from_slice(chunk);
            while let Some(header) = self.buffer.get(..HEADER) {
                let header: &[u8; HEADER] = header.try_into().map_err(|_| Error::Header)?;
                let len = payload_size(header)?;
                if self.buffer.len() < HEADER + len {
                    break;
                }
                frames.push(decode_payload(&self.buffer[HEADER..HEADER + len])?);
                self.buffer.drain(..HEADER + len);
            }
        }
        Ok(frames)
    }
    pub fn finish(&self) -> Result<(), Error> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(Error::Truncated)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arbitrary_packet_boundaries_and_coalescing() {
        let bytes = [encode(&"first").unwrap(), encode(&"second").unwrap()].concat();
        for chunk_size in 1..=bytes.len() {
            let mut decoder = Decoder::default();
            let mut messages = vec![];
            for chunk in bytes.chunks(chunk_size) {
                messages.extend(decoder.feed::<String>(chunk).unwrap());
            }
            assert_eq!(messages, ["first", "second"]);
            decoder.finish().unwrap();
        }
    }
    #[test]
    fn rejects_large_headers_truncation_and_trailing_data() {
        let mut header = *b"A206\0\x01\0\0\0\0";
        header[6..].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            Decoder::default().feed::<String>(&header),
            Err(Error::TooLarge)
        ));
        let mut decoder = Decoder::default();
        decoder.feed::<String>(b"A2").unwrap();
        assert!(matches!(decoder.finish(), Err(Error::Truncated)));
        assert!(matches!(
            decode_payload::<u8>(&[1, 2]),
            Err(Error::Trailing)
        ));
    }
}
