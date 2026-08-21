//! Wire format of server frames.
//!
//! Ported from sockchat-rs's `chat/json.rs` + `chat/msg.rs`
//! (<https://gitgud.io/jcmoon/sockchat-rs>). Frames are usually a JSON
//! object, but the server also sends bare plaintext - typically errors such
//! as "You cannot join this room." Anything that doesn't start with `{` is
//! treated as plaintext rather than being dropped.

use serde::Deserialize;

/// The full permission set the server sends. Only `can_view` matters today;
/// the rest is modelled so the shape stays faithful to the wire.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct Perms {
    #[serde(default)]
    pub can_view: bool,
    #[serde(default)]
    pub can_send: bool,
}

#[derive(Debug, Deserialize)]
pub struct WireMessage {
    pub author: WireUser,
    #[serde(default)]
    pub message_raw: String,
    #[serde(default)]
    pub message_uuid: String,
    #[serde(default)]
    pub message_date: i64,
    /// Nonzero once this message has been edited at least once - the wire
    /// has no separate "edit" event, the server just re-sends the same
    /// `message_uuid` with this bumped. Whether a given re-arrival is a
    /// *new* edit we haven't seen (vs. the message merely showing up for
    /// the first time already carrying prior edit history) is for the
    /// caller to decide by checking what's already stored locally.
    #[serde(default)]
    pub message_edit_date: i64,
    /// Two different flags observed for the same thing on the wire -
    /// both checked via `is_deleted()` rather than assuming only one is
    /// ever sent.
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub is_deleted: bool,
    /// The room the message was posted in. The server always sends this for
    /// a room message; its absence is what marks a whisper instead.
    #[serde(default)]
    pub room_id: Option<u32>,
}

impl WireMessage {
    pub fn is_deleted(&self) -> bool {
        self.deleted || self.is_deleted
    }
}

#[derive(Debug, Deserialize)]
pub struct WireWhisper {
    pub author: WireUser,
    #[serde(default)]
    pub message_raw: String,
    #[serde(default)]
    pub message_uuid: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WireUser {
    #[serde(deserialize_with = "id_as_string")]
    pub id: String,
    pub username: String,
    /// A relative path (e.g. `/data/avatars/l/.../123.jpg`, needing the
    /// account's own host prefixed) or an already-absolute URL - resolving
    /// that, and fetching+caching the actual image, is backend/sockchat/
    /// mod.rs's job (this module only carries the wire shape faithfully).
    #[serde(default)]
    pub avatar_url: Option<String>,
}

/// The server sends user ids as either a JSON number or a numeric string
/// depending on context (a plain user object vs. one keyed into the `users`
/// map) - accept both rather than failing to decode one of them.
fn id_as_string<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IdOrString {
        Id(i64),
        S(String),
    }
    Ok(match IdOrString::deserialize(d)? {
        IdOrString::Id(n) => n.to_string(),
        IdOrString::S(s) => s,
    })
}

#[derive(Debug, Default, Deserialize)]
struct RawResponse {
    #[serde(default)]
    messages: Option<Vec<serde_json::Value>>,
    #[serde(rename = "Whisper", default)]
    whisper: Option<serde_json::Value>,
    #[serde(default)]
    permissions: Option<Perms>,
    /// A batch delete notification: message uuids removed elsewhere (e.g.
    /// a moderator deleting someone else's post) rather than arriving
    /// tagged on a message object's own `deleted`/`is_deleted` flag.
    #[serde(default)]
    delete: Vec<String>,
}

#[derive(Debug, Default)]
pub struct ServerResponse {
    pub messages: Vec<WireMessage>,
    pub whisper: Option<WireWhisper>,
    pub perms: Option<Perms>,
    pub deleted_uuids: Vec<String>,
    /// Non-JSON payload, if the frame wasn't an object.
    pub plaintext: Option<String>,
}

impl ServerResponse {
    pub fn parse(frame: &str) -> Self {
        let trimmed = frame.trim_start();
        if !trimmed.starts_with('{') {
            let text = frame.trim();
            return ServerResponse { plaintext: (!text.is_empty()).then(|| text.to_string()), ..Default::default() };
        }

        let raw: RawResponse = match serde_json::from_str(trimmed) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("failed to decode server frame: {e}");
                return ServerResponse::default();
            }
        };

        // Decode elements individually so one malformed record doesn't
        // discard the whole batch.
        let messages = raw
            .messages
            .unwrap_or_default()
            .into_iter()
            .filter_map(|v| match serde_json::from_value::<WireMessage>(v) {
                Ok(m) => Some(m),
                Err(e) => {
                    tracing::debug!("skipping undecodable message: {e}");
                    None
                }
            })
            .collect();

        let whisper = raw.whisper.and_then(|v| match serde_json::from_value(v) {
            Ok(w) => Some(w),
            Err(e) => {
                tracing::debug!("skipping undecodable whisper: {e}");
                None
            }
        });

        ServerResponse { messages, whisper, perms: raw.permissions, deleted_uuids: raw.delete, plaintext: None }
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
            && self.whisper.is_none()
            && self.perms.is_none()
            && self.deleted_uuids.is_empty()
            && self.plaintext.is_none()
    }
}

/// Decode the HTML entities the server escapes in `message_raw`.
///
/// Numeric forms matter as much as the named ones: the site emits
/// apostrophes as `&#x27;`, so handling only `&#039;` leaves visible
/// mojibake in ordinary messages.
pub fn unescape_html(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];

        let named = [("&quot;", "\""), ("&apos;", "'"), ("&amp;", "&"), ("&lt;", "<"), ("&gt;", ">"), ("&nbsp;", " ")]
            .iter()
            .find_map(|(ent, rep)| tail.strip_prefix(ent).map(|r| ((*rep).to_string(), r)));

        match named.or_else(|| numeric_entity(tail)) {
            Some((rep, r)) => {
                out.push_str(&rep);
                rest = r;
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Decode `&#NN;` and `&#xHH;`, returning the text and the remaining input.
fn numeric_entity(tail: &str) -> Option<(String, &str)> {
    let body = tail.strip_prefix("&#")?;
    let (digits, radix) = match body.strip_prefix(['x', 'X']) {
        Some(hex) => (hex, 16),
        None => (body, 10),
    };
    let end = digits.find(';')?;
    if end == 0 || end > 7 {
        return None;
    }
    let code = u32::from_str_radix(&digits[..end], radix).ok()?;
    let ch = char::from_u32(code)?;
    Some((ch.to_string(), &digits[end + 1..]))
}

/// A leading `>` (not `>>`, which is a post-quote reference) gets greentext
/// colouring, matching the site's own rendering.
pub fn prepare_outgoing(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with('>') && !text.starts_with(">>") {
        return Some(format!("[color=#72ff72]{text}"));
    }
    Some(text.to_string())
}

/// `/edit {"uuid": "...", "message": "..."}` - the site's own client sends
/// the new body as a JSON payload following the command, not as plain text
/// the way an ordinary chat message or `/join` is.
pub fn prepare_edit(uuid: &str, new_body: &str) -> String {
    let payload = serde_json::json!({ "uuid": uuid, "message": new_body });
    format!("/edit {payload}")
}

/// `/delete <uuid>` - unlike `/edit`, just the bare uuid, no JSON.
pub fn prepare_delete(uuid: &str) -> String {
    format!("/delete {uuid}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_message_batches() {
        let r = ServerResponse::parse(
            r#"{"messages":[
                {"author":{"id":1,"username":"a"},"message_raw":"hi","message_uuid":"u1","message_date":100},
                {"author":{"id":2,"username":"b"},"message_raw":"yo","message_uuid":"u2","message_date":101}
            ]}"#,
        );
        assert_eq!(r.messages.len(), 2);
        assert_eq!(r.messages[0].message_raw, "hi");
    }

    #[test]
    fn messages_carry_their_room() {
        let r = ServerResponse::parse(r#"{"messages":[{"author":{"id":1,"username":"a"},"message_raw":"hi","message_uuid":"u1","room_id":20}]}"#);
        assert_eq!(r.messages[0].room_id, Some(20));
    }

    #[test]
    fn accepts_string_or_numeric_author_ids() {
        let r = ServerResponse::parse(r#"{"messages":[{"author":{"id":"160024","username":"y a t s"},"message_raw":"hi","message_uuid":"u1"}]}"#);
        assert_eq!(r.messages[0].author.id, "160024");
        let r2 = ServerResponse::parse(r#"{"messages":[{"author":{"id":7,"username":"x"},"message_raw":"hi","message_uuid":"u2"}]}"#);
        assert_eq!(r2.messages[0].author.id, "7");
    }

    #[test]
    fn one_bad_message_does_not_discard_the_batch() {
        let r = ServerResponse::parse(
            r#"{"messages":[
                {"author":{"id":1,"username":"a"},"message_raw":"good","message_uuid":"u1"},
                {"author":"not-an-object","message_raw":"bad"}
            ]}"#,
        );
        assert_eq!(r.messages.len(), 1);
        assert_eq!(r.messages[0].message_raw, "good");
    }

    #[test]
    fn parses_whispers_and_perms() {
        let r = ServerResponse::parse(r#"{"Whisper":{"author":{"id":9,"username":"w"},"message_raw":"psst","message_uuid":"u9"}}"#);
        let w = r.whisper.unwrap();
        assert_eq!(w.author.id, "9");
        assert_eq!(w.message_raw, "psst");

        let r = ServerResponse::parse(r#"{"permissions":{"can_view":true,"can_send":false}}"#);
        let p = r.perms.unwrap();
        assert!(p.can_view);
        assert!(!p.can_send);
    }

    #[test]
    fn treats_non_json_frames_as_plaintext() {
        let r = ServerResponse::parse("You cannot join this room.");
        assert_eq!(r.plaintext.as_deref(), Some("You cannot join this room."));
        assert!(r.messages.is_empty());
        assert!(ServerResponse::parse("   ").is_empty());
    }

    #[test]
    fn malformed_json_object_yields_empty_response() {
        assert!(ServerResponse::parse(r#"{"messages": [ broken"#).is_empty());
    }

    #[test]
    fn unescapes_entities() {
        assert_eq!(unescape_html("a &amp; b"), "a & b");
        assert_eq!(unescape_html("&lt;tag&gt;"), "<tag>");
        assert_eq!(unescape_html("it&#039;s"), "it's");
        assert_eq!(unescape_html("There&#x27;s so much evil"), "There's so much evil");
        assert_eq!(unescape_html("&#8212;"), "—");
    }

    #[test]
    fn malformed_numeric_entities_are_left_alone() {
        assert_eq!(unescape_html("&#;"), "&#;");
        assert_eq!(unescape_html("costs &#50 or so"), "costs &#50 or so");
    }

    #[test]
    fn greentext_gets_colored() {
        assert_eq!(prepare_outgoing(">implying").unwrap(), "[color=#72ff72]>implying");
        assert_eq!(prepare_outgoing(">>123").unwrap(), ">>123");
        assert_eq!(prepare_outgoing("normal").unwrap(), "normal");
    }

    #[test]
    fn blank_outgoing_messages_are_dropped() {
        assert!(prepare_outgoing("   ").is_none());
        assert!(prepare_outgoing("").is_none());
        assert_eq!(prepare_outgoing("  hi  ").unwrap(), "hi");
    }

    #[test]
    fn parses_edit_dates_and_avatar_urls() {
        let r = ServerResponse::parse(
            r#"{"messages":[{"author":{"id":1,"username":"a","avatar_url":"/data/avatars/l/0/1.jpg"},"message_raw":"hi","message_uuid":"u1","message_edit_date":12345}]}"#,
        );
        assert_eq!(r.messages[0].message_edit_date, 12345);
        assert_eq!(r.messages[0].author.avatar_url.as_deref(), Some("/data/avatars/l/0/1.jpg"));
        assert!(!r.messages[0].is_deleted());
    }

    #[test]
    fn a_message_with_no_edit_date_or_avatar_defaults_sanely() {
        let r = ServerResponse::parse(r#"{"messages":[{"author":{"id":1,"username":"a"},"message_raw":"hi","message_uuid":"u1"}]}"#);
        assert_eq!(r.messages[0].message_edit_date, 0);
        assert!(r.messages[0].author.avatar_url.is_none());
    }

    #[test]
    fn parses_per_message_delete_flags() {
        let deleted = ServerResponse::parse(r#"{"messages":[{"author":{"id":1,"username":"a"},"message_uuid":"u1","deleted":true}]}"#);
        assert!(deleted.messages[0].is_deleted());

        let is_deleted = ServerResponse::parse(r#"{"messages":[{"author":{"id":1,"username":"a"},"message_uuid":"u1","is_deleted":true}]}"#);
        assert!(is_deleted.messages[0].is_deleted());
    }

    #[test]
    fn parses_the_top_level_delete_batch() {
        let r = ServerResponse::parse(r#"{"delete":["u1","u2"]}"#);
        assert_eq!(r.deleted_uuids, vec!["u1".to_string(), "u2".to_string()]);
        assert!(!r.is_empty());

        assert!(ServerResponse::parse(r#"{"delete":[]}"#).is_empty());
    }

    #[test]
    fn builds_the_edit_and_delete_commands() {
        assert_eq!(prepare_edit("abc-123", "new text"), r#"/edit {"message":"new text","uuid":"abc-123"}"#);
        assert_eq!(prepare_delete("abc-123"), "/delete abc-123");
    }
}
