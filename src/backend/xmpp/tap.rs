//! A way to put one stream-level element on the wire that the stanza stream cannot send: client state indication (XEP-0352),
//! `<active/>` and `<inactive/>`, which are not stanzas.
//!
//! The stream is wrapped ([`Tapped`]) at the very end of the chain, where the bytes the library writes are watched just enough to
//! know when it is between two elements - never in the middle of one - and what was queued with [`Tap::push`] is written then. Nothing
//! the library writes is changed or reordered; it is only ever preceded by a complete element.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};

/// What is waiting to be written.
#[derive(Default)]
pub struct Tap {
    queue: Mutex<VecDeque<Vec<u8>>>,
}

impl Tap {
    pub fn push(&self, bytes: Vec<u8>) {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).push_back(bytes);
    }
}

/// Follows the library's output to know whether it is between elements.
#[derive(Default)]
pub(super) struct Boundary {
    depth: i32,
    in_tag: bool,
    quote: Option<u8>,
    /// The tag being read: its name's first bytes, whether it closes, and whether it ended with `/>`.
    name: Vec<u8>,
    closing: bool,
    last: u8,
    started: bool,
}

impl Boundary {
    pub(super) fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            if let Some(q) = self.quote {
                if b == q {
                    self.quote = None;
                }
                self.last = b;
                continue;
            }
            if !self.in_tag {
                if b == b'<' {
                    self.in_tag = true;
                    self.name.clear();
                    self.closing = false;
                    self.started = false;
                    self.last = b;
                }
                continue;
            }
            match b {
                b'\'' | b'"' => self.quote = Some(b),
                b'/' if self.name.is_empty() && !self.started => self.closing = true,
                b'>' => {
                    self.in_tag = false;
                    let selfclosing = self.last == b'/';
                    let name = String::from_utf8_lossy(&self.name).to_string();
                    if name == "stream:stream" {
                        // The stream's own header opens the level everything else sits in; its footer closes it.
                        self.depth = if self.closing { 0 } else { 1 };
                    } else if name.starts_with('?') || name.starts_with('!') {
                        // A declaration: not an element.
                    } else if self.closing {
                        self.depth -= 1;
                    } else if !selfclosing {
                        self.depth += 1;
                    }
                }
                b if !self.started && !b.is_ascii_whitespace() && b != b'/' => self.name.push(b),
                b if b.is_ascii_whitespace() && !self.name.is_empty() => self.started = true,
                _ => {}
            }
            self.last = b;
        }
    }

    /// Between two elements of the stream, with nothing half written.
    pub(super) fn between(&self) -> bool {
        !self.in_tag && self.depth == 1
    }
}

pub struct Tapped<S> {
    inner: S,
    tap: Arc<Tap>,
    boundary: Boundary,
    /// Bytes taken from the queue and not yet all written.
    injecting: Vec<u8>,
}

impl<S> Tapped<S> {
    pub fn new(inner: S, tap: Arc<Tap>) -> Self {
        Self { inner, tap, boundary: Boundary::default(), injecting: Vec::new() }
    }
}

impl<S: AsyncWrite + Unpin> Tapped<S> {
    /// Writes what is queued, if the library is between elements. `Pending` while it is being written.
    fn inject(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            if self.injecting.is_empty() {
                if !self.boundary.between() {
                    return Poll::Ready(Ok(()));
                }
                match self.tap.queue.lock().unwrap_or_else(|e| e.into_inner()).pop_front() {
                    Some(next) => self.injecting = next,
                    None => return Poll::Ready(Ok(())),
                }
            }
            match Pin::new(&mut self.inner).poll_write(cx, &self.injecting) {
                Poll::Ready(Ok(n)) => {
                    self.injecting.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for Tapped<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncBufRead + Unpin> AsyncBufRead for Tapped<S> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        // The inner stream's own buffer: reborrowed through the pin.
        let this = self.get_mut();
        Pin::new(&mut this.inner).poll_fill_buf(cx)
    }

    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        Pin::new(&mut self.inner).consume(amt)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tapped<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.inject(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }
        match Pin::new(&mut self.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                self.boundary.feed(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Poll::Ready(Err(e)) = self.inject(cx) {
            return Poll::Ready(Err(e));
        }
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(chunks: &[&str]) -> Boundary {
        let mut b = Boundary::default();
        for c in chunks {
            b.feed(c.as_bytes());
        }
        b
    }

    #[test]
    fn inside_the_stream_between_two_elements_is_a_boundary_and_inside_one_is_not() {
        assert!(!after(&[]).between(), "before the stream has been opened");
        let header = "<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' to='example.org' version='1.0'>";
        assert!(after(&[header]).between());
        assert!(after(&[header, "<iq type='get' id='1'><query xmlns='x'/></iq>"]).between());
        assert!(!after(&[header, "<iq type='get'><qu"]).between());
        assert!(!after(&[header, "<message><body>hi"]).between());
        assert!(after(&[header, "<message><body>hi</body></message><presence/>"]).between());
        assert!(!after(&[header, "<message to='a>b"]).between(), "a bracket inside a value is not the end of the tag");
    }

    #[test]
    fn a_restart_of_the_stream_is_the_same_level_again() {
        let h = "<stream:stream to='x'>";
        assert!(after(&[h, "<auth xmlns='s'>abc</auth>", h]).between());
        assert!(!after(&[h, "</stream:stream>"]).between());
    }

    #[test]
    fn a_declaration_and_a_quote_do_not_confuse_it() {
        assert!(after(&["<?xml version='1.0'?><stream:stream to='x'>", "<message body=\"a'b\"/>"]).between());
    }
}
