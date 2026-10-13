//! CHATHISTORY: asking the server for what was said before we arrived.
//!
//! Only some networks have it, and the ones that do answer in batches. The
//! request is small enough to live here whole.

use super::*;

/// When the server says this message was sent, if it says at all.
///
/// The server-time tag is RFC3339 in UTC to millisecond precision
/// ("2026-08-29T12:34:56.789Z"). Absent unless the capability was granted,
/// which is the ordinary case on an older server - hence an Option rather
/// than a default, so the caller can fall back to the clock rather than to
/// the epoch.
/// How many messages to ask for at a time.
///
/// Enough to fill a screen and then some, and small enough that a server
/// which caps the request silently truncates rather than refuses. Servers
/// commonly limit this to 100 anyway.
pub const CHATHISTORY_PAGE: u32 = 100;

/// `CHATHISTORY LATEST <target> * <n>` - the most recent messages there are.
///
/// Built as a raw message because the crate has no command for it: this is an
/// IRCv3 extension rather than part of the protocol the crate models.
pub fn chathistory_latest(target: &str) -> Message {
    format!("CHATHISTORY LATEST {target} * {CHATHISTORY_PAGE}")
        .parse()
        .expect("a CHATHISTORY line built from a channel name is well formed")
}

/// `CHATHISTORY BEFORE <target> timestamp=<t> <n>` - what came before a point.
///
/// The timestamp is the server's own format, which is RFC 3339 with
/// milliseconds. Built from a unix second, which is what the store keeps.
pub fn chathistory_before(target: &str, before_unix: i64) -> Option<Message> {
    let at = chrono::DateTime::from_timestamp(before_unix, 0)?.format("%Y-%m-%dT%H:%M:%S%.3fZ");
    format!("CHATHISTORY BEFORE {target} timestamp={at} {CHATHISTORY_PAGE}").parse().ok()
}

/// Waits for a CHATHISTORY page to land, or gives up.
///
/// Polled rather than signalled. The alternative is threading a one-shot
/// channel from this call into the connection task so a batch ending can wake
/// it, which is more machinery than the problem deserves: the wait is bounded,
/// it ends the moment anything arrives, and the cost of being wrong is a page
/// that fills in a moment later as live messages rather than a page that is
/// missing.
pub async fn await_history(state: &AppState, buffer_id: &str, before: i64, limit: i64, had: usize) {
    const PATIENCE: Duration = Duration::from_secs(3);
    const CHECK: Duration = Duration::from_millis(100);
    let deadline = tokio::time::Instant::now() + PATIENCE;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(CHECK).await;
        if state.store.get_backlog(buffer_id, before, limit).map(|m| m.len()).unwrap_or(0) > had {
            return;
        }
    }
}
