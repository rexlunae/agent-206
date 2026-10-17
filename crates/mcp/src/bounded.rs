//! The MCP stdio transport is newline-delimited JSON with no length limit of
//! its own, so a server could make the client buffer an endless line. This
//! reader fails the session once a line grows past its bound.
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::io::{AsyncRead, ReadBuf};

pub struct LineLimited<R> {
    inner: R,
    limit: usize,
    /// Bytes since the last newline.
    current: usize,
}
impl<R> LineLimited<R> {
    pub fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            limit,
            current: 0,
        }
    }
}
impl<R: AsyncRead + Unpin> AsyncRead for LineLimited<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let start = buf.filled().len();
        let this = &mut *self;
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = result {
            for &byte in &buf.filled()[start..] {
                if byte == b'\n' {
                    this.current = 0;
                } else {
                    this.current += 1;
                    if this.current > this.limit {
                        // A failed read must not hand back the bytes it read.
                        buf.set_filled(start);
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "MCP server message exceeds the line limit",
                        )));
                    }
                }
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    #[tokio::test]
    async fn passes_short_lines_and_stops_a_long_one() {
        let mut reader = LineLimited::new(&b"abc\ndef\n"[..], 3);
        let mut out = String::new();
        reader.read_to_string(&mut out).await.unwrap();
        assert_eq!(out, "abc\ndef\n");
        let mut reader = LineLimited::new(&b"ab\nabcd\n"[..], 3);
        let error = reader.read_to_end(&mut vec![]).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
