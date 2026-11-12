//! The byte stream between the TLS (or WebSocket) connection and the stream library, and what it does besides carrying bytes.
//!
//! Signing in is done here, not by the library (see [`super::login`]): the library cannot send or read SASL2's elements, so
//! the whole exchange up to a bound session is ours. What it is then handed has to look, to it, like a stream it opened
//! itself:
//!
//! - **After SASL2 there is no stream restart** (XEP-0388), but the library always sends a stream header and waits for the
//!   server's header and features. [`Wire`] swallows the header the library writes and plays it a header and the features
//!   the server sent (or, when the session was bound inline, features made from what Bind2 said).
//! - **A session bound by Bind2** (XEP-0386) is bound already, but the library binds a resource whenever it does not resume.
//!   That one request is answered here, with the address the server bound, and never reaches the server.
//!
//! Everything else passes through unchanged. The read side also watches the top-level elements the server sends for the few
//! that say something about the connection itself: a stream error (and its words), and whether stream management made the
//! session resumable - which decides, at the next connection, whether to resume or to bind inline.

use super::tap::Boundary;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncBufRead, AsyncRead, AsyncWrite, ReadBuf};

/// What the connection has told about itself, shared with the account's task.
#[derive(Debug, Default, Clone)]
pub struct Observed {
    /// The last stream error the server sent: its condition, its text, and for `see-other-host` where to go.
    pub stream_error: Option<StreamError>,
    /// Whether the library holds a session it can resume (XEP-0198 `<enabled resume='true'/>` and not since lost).
    pub resumable: bool,
    /// The connection is to be treated as dead (it stopped answering after the computer woke up): the next read fails.
    cut: bool,
    /// Whoever is waiting to read, so a cut is noticed at once.
    reader: Option<std::task::Waker>,
}

/// Ends the current connection from this side: the library sees it fail and reconnects (resuming, if it can).
pub fn cut(observed: &ObservedSlot) {
    let mut o = observed.lock().unwrap_or_else(|e| e.into_inner());
    o.cut = true;
    if let Some(w) = o.reader.take() {
        w.wake();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StreamError {
    pub condition: String,
    pub text: String,
    /// `see-other-host`'s content.
    pub other_host: String,
}

pub type ObservedSlot = Arc<Mutex<Observed>>;

/// Reads the condition, text and other host out of a `<stream:error>` element.
pub fn parse_stream_error(xml: &str) -> StreamError {
    let el = super::login::parse_fragment(xml);
    let mut e = StreamError { condition: String::new(), text: String::new(), other_host: String::new() };
    if let Some(el) = el {
        for c in el.children() {
            if c.ns() != "urn:ietf:params:xml:ns:xmpp-streams" {
                continue;
            }
            match c.name() {
                "text" => e.text = c.text().trim().to_string(),
                name => {
                    e.condition = name.to_string();
                    if name == "see-other-host" {
                        e.other_host = c.text().trim().to_string();
                    }
                }
            }
        }
    }
    e
}

/// Follows the top-level elements going one way, keeping the start of each one that matters.
#[derive(Default)]
struct Watcher {
    boundary: Boundary,
    current: Vec<u8>,
    capturing: bool,
}

/// How much of an element is kept to look at: enough for any stream error or stream-management element.
const WATCH_CAP: usize = 8192;

impl Watcher {
    /// Feeds bytes; returns the elements that ended in them (only their first [`WATCH_CAP`] bytes).
    fn feed(&mut self, bytes: &[u8]) -> Vec<Vec<u8>> {
        let mut done = Vec::new();
        for &b in bytes {
            let before = self.boundary.between();
            self.boundary.feed(&[b]);
            let after = self.boundary.between();
            if before && b == b'<' {
                self.capturing = true;
                self.current.clear();
            }
            if self.capturing && self.current.len() < WATCH_CAP {
                self.current.push(b);
            }
            if self.capturing && !before && after {
                self.capturing = false;
                done.push(std::mem::take(&mut self.current));
            }
        }
        done
    }
}

/// What to do to the next things the library writes.
#[derive(Default, Debug)]
struct Plan {
    /// Swallow the stream header (a restart the server must not see).
    swallow_header: bool,
    /// Answer the resource-binding request with this full address instead of sending it.
    fake_bind: Option<String>,
}

pub struct Wire<S> {
    inner: S,
    /// What has been read and not consumed (and, in front of it, anything being played to the library).
    buf: Vec<u8>,
    pos: usize,
    /// Bytes waiting to be written to `inner`.
    out: Vec<u8>,
    /// What the library is writing, held until it is known whether to pass it on.
    hold: Vec<u8>,
    reads: Watcher,
    /// How many bytes at the front of `buf` were played rather than read.
    played: usize,
    /// An answer to play once the reader is between two elements.
    pending_answer: Option<Vec<u8>>,
    plan: Plan,
    observed: ObservedSlot,
}

impl<S> Wire<S> {
    pub fn new(inner: S, observed: ObservedSlot) -> Self {
        {
            let mut o = observed.lock().unwrap_or_else(|e| e.into_inner());
            o.cut = false;
            o.reader = None;
        }
        Self { inner, buf: Vec::new(), pos: 0, out: Vec::new(), hold: Vec::new(), reads: Watcher::default(), played: 0, pending_answer: None, plan: Plan::default(), observed }
    }

    /// What has been read and not consumed.
    pub fn buffered(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    /// Marks bytes read (by the sign-in) as used.
    pub fn take(&mut self, n: usize) {
        let n = n.min(self.buf.len() - self.pos);
        let used = self.buf[self.pos..self.pos + n].to_vec();
        self.pos += n;
        self.watch(&used);
        if self.pos == self.buf.len() {
            self.buf.clear();
            self.pos = 0;
        }
    }

    /// Plays `bytes` to the reader before anything else that has been read. They are not watched (they did not come from the
    /// server), but they are followed, so the reader's place in the stream is known.
    fn play(&mut self, bytes: &[u8]) {
        let mut b = bytes.to_vec();
        b.extend_from_slice(&self.buf[self.pos..]);
        self.buf = b;
        self.pos = 0;
        self.played = bytes.len();
    }

    /// Prepares the hand-over to the library after SASL2: its stream header is not sent, and it is played `header_and_features`
    /// in place of the server's answer; with `bound`, its resource binding is answered with that address.
    pub fn hand_over(&mut self, header_and_features: &[u8], bound: Option<String>) {
        self.plan = Plan { swallow_header: true, fake_bind: bound };
        self.play(header_and_features);
    }

    fn watch(&mut self, bytes: &[u8]) {
        for el in self.reads.feed(bytes) {
            let text = String::from_utf8_lossy(&el);
            let mut o = self.observed.lock().unwrap_or_else(|e| e.into_inner());
            if text.starts_with("<stream:error") {
                o.stream_error = Some(parse_stream_error(&text));
                o.resumable = false;
            } else if text.starts_with("<enabled") && text.contains("urn:xmpp:sm:3") {
                o.resumable = ["resume='true'", "resume=\"true\"", "resume='1'", "resume=\"1\""].iter().any(|r| text.contains(r));
            } else if text.starts_with("<failed") && text.contains("urn:xmpp:sm:3") {
                o.resumable = false;
            }
        }
    }
}

impl<S: AsyncRead + Unpin> Wire<S> {
    /// Reads more from the connection into the buffer. `Ok(0)` at its end.
    pub fn poll_more(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<usize>> {
        {
            let mut o = self.observed.lock().unwrap_or_else(|e| e.into_inner());
            if o.cut {
                return Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "the connection stopped answering")));
            }
            o.reader = Some(cx.waker().clone());
        }
        let mut chunk = [0u8; 16384];
        let mut rb = ReadBuf::new(&mut chunk);
        match Pin::new(&mut self.inner).poll_read(cx, &mut rb) {
            Poll::Ready(Ok(())) => {
                let n = rb.filled().len();
                if self.pos > 0 && self.pos == self.buf.len() {
                    self.buf.clear();
                    self.pos = 0;
                }
                self.buf.extend_from_slice(rb.filled());
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }

    pub async fn more(&mut self) -> io::Result<usize> {
        std::future::poll_fn(|cx| self.poll_more(cx)).await
    }
}

impl<S: AsyncWrite + Unpin> Wire<S> {
    /// Writes what is waiting. `Ready(Ok)` once all of it is written.
    fn drain_out(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while !self.out.is_empty() {
            match Pin::new(&mut self.inner).poll_write(cx, &self.out) {
                Poll::Ready(Ok(0)) => return Poll::Ready(Err(io::ErrorKind::WriteZero.into())),
                Poll::Ready(Ok(n)) => {
                    self.out.drain(..n);
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        Poll::Ready(Ok(()))
    }

    /// Decides about what the library wrote: passes it on, or keeps it back while the plan says to look at it first.
    fn sort(&mut self, bytes: &[u8]) {
        if self.plan.swallow_header {
            self.hold.extend_from_slice(bytes);
            let text = String::from_utf8_lossy(&self.hold).to_string();
            if let Some(at) = text.find("<stream:stream") {
                if let Some(end) = super::websocket::tag_end(&text[at..]) {
                    // The header (and the declaration before it) goes nowhere; whatever follows it goes on being sorted.
                    let rest = self.hold[at + end..].to_vec();
                    self.hold.clear();
                    self.plan.swallow_header = false;
                    self.sort(&rest);
                }
            }
            return;
        }
        if self.plan.fake_bind.is_some() {
            self.hold.extend_from_slice(bytes);
            let text = String::from_utf8_lossy(&self.hold).to_string();
            let start = text.find('<').unwrap_or(text.len());
            let Some(end) = super::websocket::element_end(&text[start..]) else { return };
            let element = text[start..start + end].to_string();
            let rest = self.hold[start + end..].to_vec();
            self.hold.clear();
            if element.starts_with("<iq") && element.contains("urn:ietf:params:xml:ns:xmpp-bind") {
                let jid = self.plan.fake_bind.take().unwrap_or_default();
                let tag = &element[..super::websocket::tag_end(&element).unwrap_or(element.len())];
                let id = super::websocket::attr(&super::websocket::attributes(tag), "id").unwrap_or("").to_string();
                let answer = format!("<iq type='result' id='{}'><bind xmlns='urn:ietf:params:xml:ns:xmpp-bind'><jid>{}</jid></bind></iq>", super::xml::escape(&id), super::xml::escape(&jid));
                self.pending_answer = Some(answer.into_bytes());
            } else {
                // Something before the binding (the library trying to resume a session first, say): it goes to the server as
                // written, and the binding, when it comes, is still answered here.
                self.out.extend_from_slice(element.as_bytes());
            }
            self.sort(&rest);
            return;
        }
        self.out.extend_from_slice(bytes);
    }
}

enum Mended {
    /// Nothing to change, or changed: the buffer can be read as it is.
    Ready,
    /// A start tag that may need changing begins here and has not all arrived.
    HoldFrom(usize),
}

/// XEP-0198 makes `h` on `<failed/>` optional, and servers leave it out (ejabberd does, refusing a resumption); the stream
/// library's parser requires it and kills the stream when it is missing - and keeps the session it failed to resume, so it
/// tries again forever. A `<failed>` of stream management without `h` is given `h='0'` (which the library only uses to drop
/// stanzas the server says it had; 0 drops none).
fn mend_failed(buf: &mut Vec<u8>, from: usize) -> Mended {
    let mut search = from;
    while let Some(off) = buf[search..].windows(7).position(|w| w == b"<failed") {
        let at = search + off;
        let Some(end_rel) = buf[at..].iter().position(|&b| b == b'>') else { return Mended::HoldFrom(at) };
        let tag = String::from_utf8_lossy(&buf[at..at + end_rel + 1]).to_string();
        let attrs = super::websocket::attributes(&tag);
        let is_sm = super::websocket::attr(&attrs, "xmlns") == Some("urn:xmpp:sm:3");
        if is_sm && super::websocket::attr(&attrs, "h").is_none() {
            let insert = b" h='0'";
            let pos = at + "<failed".len();
            buf.splice(pos..pos, insert.iter().copied());
        }
        search = at + 7;
    }
    // A buffer that ends part way into "<failed" is held from there too.
    let tail_from = buf.len().saturating_sub(6).max(from);
    if let Some(i) = (tail_from..buf.len()).find(|&i| b"<failed".starts_with(&buf[i..])) {
        return Mended::HoldFrom(i);
    }
    Mended::Ready
}

impl<S: AsyncRead + Unpin> AsyncRead for Wire<S> {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let available = match self.as_mut().poll_fill_buf(cx) {
            Poll::Ready(Ok(b)) => b.len().min(buf.remaining()),
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        };
        let pos = self.pos;
        buf.put_slice(&self.buf[pos..pos + available]);
        self.consume(available);
        Poll::Ready(Ok(()))
    }
}

impl<S: AsyncRead + Unpin> AsyncBufRead for Wire<S> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<&[u8]>> {
        let this = self.get_mut();
        if this.pending_answer.is_some() && this.reads.boundary.between() && this.played == 0 {
            let answer = this.pending_answer.take().unwrap_or_default();
            this.play(&answer);
        }
        loop {
            if this.pos >= this.buf.len() {
                match this.poll_more(cx) {
                    Poll::Ready(Ok(_)) => {}
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            }
            match mend_failed(&mut this.buf, this.pos + this.played) {
                Mended::Ready => break,
                Mended::HoldFrom(at) if at > this.pos => return Poll::Ready(Ok(&this.buf[this.pos..at])),
                // The start of a tag that needs looking at, and nothing before it: more is read first.
                Mended::HoldFrom(_) => match this.poll_more(cx) {
                    Poll::Ready(Ok(0)) => break,
                    Poll::Ready(Ok(_)) => {}
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                },
            }
        }
        Poll::Ready(Ok(&this.buf[this.pos..]))
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        let this = self.get_mut();
        let amt = amt.min(this.buf.len() - this.pos);
        // What was played came from here, not the server: followed for position, not watched.
        let played = amt.min(this.played);
        if played > 0 {
            let bytes = this.buf[this.pos..this.pos + played].to_vec();
            this.reads.boundary.feed(&bytes);
            this.played -= played;
        }
        let from_server = this.buf[this.pos + played..this.pos + amt].to_vec();
        this.pos += amt;
        this.watch(&from_server);
        if this.pos == this.buf.len() {
            this.buf.clear();
            this.pos = 0;
        }
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Wire<S> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        // Earlier bytes go first; and a writer is not let run far ahead of the connection.
        if let Poll::Ready(Err(e)) = self.drain_out(cx) {
            return Poll::Ready(Err(e));
        }
        if self.out.len() > 65536 {
            return Poll::Pending;
        }
        self.sort(buf);
        let _ = self.drain_out(cx);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.drain_out(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.inner).poll_flush(cx),
            other => other,
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.drain_out(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut self.inner).poll_shutdown(cx),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_without_h_is_given_one_and_others_are_left_alone() {
        let mut b = b"<failed xmlns='urn:xmpp:sm:3'><item-not-found/></failed>".to_vec();
        assert!(matches!(mend_failed(&mut b, 0), Mended::Ready));
        assert_eq!(String::from_utf8(b).unwrap(), "<failed h='0' xmlns='urn:xmpp:sm:3'><item-not-found/></failed>");
        let mut with_h = b"<failed xmlns='urn:xmpp:sm:3' h='5'/>".to_vec();
        mend_failed(&mut with_h, 0);
        assert_eq!(with_h, b"<failed xmlns='urn:xmpp:sm:3' h='5'/>".to_vec());
        let mut other = b"<failed xmlns='urn:ietf:params:xml:ns:xmpp-sasl'><not-authorized/></failed>".to_vec();
        mend_failed(&mut other, 0);
        assert!(!String::from_utf8(other).unwrap().contains("h='0'"));
    }

    #[test]
    fn a_tag_that_has_not_all_arrived_is_held_back() {
        let mut b = b"<a/><failed xmlns='urn:xm".to_vec();
        assert!(matches!(mend_failed(&mut b, 0), Mended::HoldFrom(4)));
        let mut c = b"<a/><fai".to_vec();
        assert!(matches!(mend_failed(&mut c, 0), Mended::HoldFrom(4)));
        let mut d = b"<a/>".to_vec();
        assert!(matches!(mend_failed(&mut d, 0), Mended::Ready));
    }

    #[test]
    fn the_watcher_sees_whole_top_level_elements() {
        let mut w = Watcher::default();
        let mut done = w.feed(b"<stream:stream xmlns='jabber:client'><enabled xmlns='urn:xmpp:sm:3' res");
        assert!(done.is_empty());
        done = w.feed(b"ume='true' id='x'/><message><body>a</body></message>");
        assert_eq!(done.len(), 2);
        assert!(String::from_utf8_lossy(&done[0]).starts_with("<enabled"));
    }
}
