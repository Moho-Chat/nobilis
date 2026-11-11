//! XMPP over WebSocket (RFC 7395), for servers that offer it (and for networks that let only web traffic through).
//!
//! The stream library speaks the TCP form: one `<stream:stream>` and its children as a run of text. WebSocket frames each hold one
//! whole element, and open and close the stream with `<open/>` and `<close/>` instead of a header and a footer. This module is the
//! translation in both directions, so that everything above - authentication, binding, stream management - is the same code on
//! either transport: [`Framer`] cuts what the library writes into frames (turning its stream header into `<open/>`), [`Unframer`]
//! turns a frame into what it expects to read, and [`WsIo`] is the byte stream the library is given over a connected socket.

use futures::{Sink, Stream};
use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

pub const FRAMING: &str = "urn:ietf:params:xml:ns:xmpp-framing";
const STREAMS: &str = "http://etherx.jabber.org/streams";

/// The attributes of a start tag, in order.
fn attributes(tag: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = tag.as_bytes();
    let mut i = tag.find(char::is_whitespace).unwrap_or(tag.len());
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/' || bytes[i] == b'>') {
            i += 1;
        }
        let start = i;
        while i < bytes.len() && bytes[i] != b'=' && !bytes[i].is_ascii_whitespace() && bytes[i] != b'>' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            break;
        }
        let name = tag[start..i].to_string();
        i += 1;
        let Some(&quote) = bytes.get(i) else { break };
        if quote != b'\'' && quote != b'"' {
            break;
        }
        i += 1;
        let vstart = i;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        out.push((name, tag[vstart..i.min(bytes.len())].to_string()));
        i += 1;
    }
    out
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

/// The end of the start tag that begins `s` (after its `>`), respecting quoted values.
fn tag_end(s: &str) -> Option<usize> {
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(c),
            (None, '>') => return Some(i + 1),
            _ => {}
        }
    }
    None
}

/// Cuts what the library writes into the frames a WebSocket carries.
#[derive(Default)]
pub struct Framer {
    buffer: String,
}

impl Framer {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));
        let mut frames = Vec::new();
        loop {
            let trimmed = self.buffer.trim_start();
            let skipped = self.buffer.len() - trimmed.len();
            if skipped > 0 {
                self.buffer.drain(..skipped);
            }
            if self.buffer.is_empty() {
                break;
            }
            if self.buffer.starts_with("<?") {
                match self.buffer.find("?>") {
                    Some(e) => {
                        self.buffer.drain(..e + 2);
                        continue;
                    }
                    None => break,
                }
            }
            if self.buffer.starts_with("</stream:stream") {
                match self.buffer.find('>') {
                    Some(e) => {
                        self.buffer.drain(..=e);
                        frames.push(format!("<close xmlns='{FRAMING}'/>"));
                        continue;
                    }
                    None => break,
                }
            }
            if self.buffer.starts_with("<stream:stream") {
                let Some(end) = tag_end(&self.buffer) else { break };
                let tag: String = self.buffer.drain(..end).collect();
                let a = attributes(&tag);
                let mut open = format!("<open xmlns='{FRAMING}'");
                for name in ["to", "from", "version", "xml:lang"] {
                    if let Some(v) = attr(&a, name) {
                        open.push_str(&format!(" {name}='{v}'"));
                    }
                }
                if attr(&a, "version").is_none() {
                    open.push_str(" version='1.0'");
                }
                open.push_str("/>");
                frames.push(open);
                continue;
            }
            match element_end(&self.buffer) {
                Some(e) => {
                    let frame: String = self.buffer.drain(..e).collect();
                    frames.push(with_namespace(frame));
                }
                None => break,
            }
        }
        frames
    }
}

/// A stanza in a frame names its namespace itself: over TCP it inherits the stream's, and a frame has no stream around it.
fn with_namespace(frame: String) -> String {
    for name in ["iq", "message", "presence"] {
        let Some(rest) = frame.strip_prefix(&format!("<{name}")) else { continue };
        if !rest.starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/') {
            continue;
        }
        let tag_end = tag_end(&frame).unwrap_or(frame.len());
        if attributes(&frame[..tag_end]).iter().any(|(n, _)| n == "xmlns") {
            return frame;
        }
        return format!("<{name} xmlns='jabber:client'{rest}");
    }
    frame
}

/// Where the first whole element in `s` ends, if it has ended.
fn element_end(s: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        if !rest.starts_with('<') {
            i += rest.find('<').unwrap_or(rest.len());
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            i += rest.find("]]>")? + 3;
            continue;
        }
        let end = tag_end(rest)?;
        let tag = &rest[..end];
        if tag.starts_with("</") {
            depth -= 1;
        } else if !tag.ends_with("/>") {
            depth += 1;
        }
        i += end;
        if depth <= 0 {
            return Some(i);
        }
    }
    None
}

/// Turns a frame into what the library reads: `<open/>` becomes the stream header, `<close/>` its footer, anything else as it is.
pub fn unframe(frame: &str) -> String {
    let f = frame.trim();
    if f.starts_with("<open") && f.contains(FRAMING) {
        let a = attributes(f);
        let mut header = format!("<stream:stream xmlns='jabber:client' xmlns:stream='{STREAMS}'");
        for name in ["from", "id", "version", "xml:lang"] {
            if let Some(v) = attr(&a, name) {
                header.push_str(&format!(" {name}='{v}'"));
            }
        }
        header.push('>');
        return header;
    }
    if f.starts_with("<close") && f.contains(FRAMING) {
        return "</stream:stream>".to_string();
    }
    f.to_string()
}

/// The byte stream the library is given over a connected WebSocket.
pub struct WsIo<S> {
    ws: WebSocketStream<S>,
    framer: Framer,
    readable: VecDeque<u8>,
    outgoing: VecDeque<String>,
    closed: bool,
}

impl<S> WsIo<S> {
    pub fn new(ws: WebSocketStream<S>) -> Self {
        Self { ws, framer: Framer::default(), readable: VecDeque::new(), outgoing: VecDeque::new(), closed: false }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> WsIo<S> {
    /// Sends what is waiting as far as the socket will take it.
    fn drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while let Some(frame) = self.outgoing.front() {
            match Pin::new(&mut self.ws).poll_ready(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(io::Error::other(e))),
                Poll::Ready(Ok(())) => {}
            }
            let frame = frame.clone();
            Pin::new(&mut self.ws).start_send(Message::Text(frame)).map_err(io::Error::other)?;
            self.outgoing.pop_front();
        }
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for WsIo<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        loop {
            if !self.readable.is_empty() {
                let n = buf.remaining().min(self.readable.len());
                let chunk: Vec<u8> = self.readable.drain(..n).collect();
                buf.put_slice(&chunk);
                return Poll::Ready(Ok(()));
            }
            if self.closed {
                return Poll::Ready(Ok(()));
            }
            // Writes that are waiting go out while the stream is read, so a request is never held behind a quiet connection.
            let _ = self.drain(cx);
            match Pin::new(&mut self.ws).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) | Poll::Ready(Some(Ok(Message::Close(_)))) => {
                    self.closed = true;
                    self.readable.extend(b"</stream:stream>");
                }
                Poll::Ready(Some(Ok(Message::Text(t)))) => self.readable.extend(unframe(&t).into_bytes()),
                Poll::Ready(Some(Ok(Message::Binary(b)))) => self.readable.extend(unframe(&String::from_utf8_lossy(&b)).into_bytes()),
                Poll::Ready(Some(Ok(_))) => {}
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(io::Error::other(e))),
            }
        }
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for WsIo<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        let frames = self.framer.push(buf);
        self.outgoing.extend(frames);
        match self.drain(cx) {
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            _ => Poll::Ready(Ok(buf.len())),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.drain(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.ws).poll_flush(cx).map_err(io::Error::other),
            other => other,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.drain(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.ws).poll_close(cx).map_err(io::Error::other),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(chunks: &[&str]) -> Vec<String> {
        let mut f = Framer::default();
        chunks.iter().flat_map(|c| f.push(c.as_bytes())).collect()
    }

    #[test]
    fn the_stream_header_the_library_writes_is_an_open_frame() {
        let out = frames(&["<?xml version='1.0'?><stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' to='example.org' version='1.0'>"]);
        assert_eq!(out, vec!["<open xmlns='urn:ietf:params:xml:ns:xmpp-framing' to='example.org' version='1.0'/>"]);
    }

    #[test]
    fn each_element_is_its_own_frame_however_the_bytes_arrive() {
        let out = frames(&["<iq type='get' id='1'><query xmlns='jabber:iq:roster'/>", "</iq><message to='a@b'><body>a &lt; b</body>", "</message><presence/>"]);
        assert_eq!(out, vec!["<iq xmlns='jabber:client' type='get' id='1'><query xmlns='jabber:iq:roster'/></iq>", "<message xmlns='jabber:client' to='a@b'><body>a &lt; b</body></message>", "<presence xmlns='jabber:client'/>"]);
    }

    #[test]
    fn a_stanza_names_its_namespace_in_its_frame_and_one_that_has_is_left_alone() {
        assert_eq!(frames(&["<iq type='set' id='b'><bind xmlns='urn:xmpp:bind'/></iq>"]), vec!["<iq xmlns='jabber:client' type='set' id='b'><bind xmlns='urn:xmpp:bind'/></iq>"]);
        assert_eq!(frames(&["<presence/>"]), vec!["<presence xmlns='jabber:client'/>"]);
        assert_eq!(frames(&["<message xmlns='jabber:client' to='a'/>"]), vec!["<message xmlns='jabber:client' to='a'/>"]);
        assert_eq!(frames(&["<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='X'/>"]), vec!["<auth xmlns='urn:ietf:params:xml:ns:xmpp-sasl' mechanism='X'/>"]);
    }

    #[test]
    fn a_value_with_an_angle_bracket_in_it_does_not_end_a_tag_early() {
        let out = frames(&["<message to='a>b' id=\"x>y\"><body>hi</body></message>"]);
        assert_eq!(out, vec!["<message xmlns='jabber:client' to='a>b' id=\"x>y\"><body>hi</body></message>"]);
    }

    #[test]
    fn the_footer_is_a_close_frame_and_an_unfinished_element_waits() {
        assert_eq!(frames(&["</stream:stream>"]), vec!["<close xmlns='urn:ietf:params:xml:ns:xmpp-framing'/>"]);
        let mut f = Framer::default();
        assert!(f.push(b"<message><bo").is_empty());
        assert_eq!(f.push(b"dy>x</body></message>"), vec!["<message xmlns='jabber:client'><body>x</body></message>"]);
    }

    #[test]
    fn what_the_server_opens_is_read_as_a_stream_header_and_the_rest_unchanged() {
        assert_eq!(unframe("<open xmlns='urn:ietf:params:xml:ns:xmpp-framing' from='example.org' id='abc' version='1.0' xml:lang='en'/>"), "<stream:stream xmlns='jabber:client' xmlns:stream='http://etherx.jabber.org/streams' from='example.org' id='abc' version='1.0' xml:lang='en'>");
        assert_eq!(unframe("<close xmlns='urn:ietf:params:xml:ns:xmpp-framing'/>"), "</stream:stream>");
        assert_eq!(unframe("<message from='a'><body>x</body></message>"), "<message from='a'><body>x</body></message>");
    }
}
