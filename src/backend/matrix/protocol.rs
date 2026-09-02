//! Client-Server API event-type constants + small per-event extraction
//! helpers. Plain `serde_json::Value` + manual field access throughout
//! (matching backend/discord.rs's own style for gateway payloads) rather
//! than fully-typed structs - `/sync`'s per-room maps are keyed by dynamic
//! room ids and its timeline mixes a couple dozen distinct event types, so
//! a hand-rolled typed model would need to enumerate exhaustively for no
//! real benefit over targeted extraction at the few event types this
//! backend actually acts on.

use serde_json::Value;

pub const EVENT_ROOM_MESSAGE: &str = "m.room.message";
pub const EVENT_ROOM_NAME: &str = "m.room.name";
pub const EVENT_ROOM_MEMBER: &str = "m.room.member";
pub const EVENT_ROOM_ENCRYPTION: &str = "m.room.encryption";
pub const EVENT_ROOM_ENCRYPTED: &str = "m.room.encrypted";
pub const EVENT_REACTION: &str = "m.reaction";
pub const EVENT_REDACTION: &str = "m.room.redaction";

pub fn event_type(event: &Value) -> &str {
    event["type"].as_str().unwrap_or("")
}

pub fn sender(event: &Value) -> &str {
    event["sender"].as_str().unwrap_or("unknown")
}

pub fn event_id(event: &Value) -> Option<&str> {
    event["event_id"].as_str()
}

/// Strips a full Matrix ID (`@alice:matrix.org`) down to its bare
/// localpart (`alice`) - used as the display name until a later pass
/// pulls in `m.room.member`'s own `displayname`; a bare MXID localpart is
/// what every other backend's "no display name resolved yet" fallback
/// already looks like, e.g. IRC's nick, so this matches the existing
/// convention rather than inventing a new placeholder shape.
pub fn mxid_localpart(mxid: &str) -> String {
    mxid.trim_start_matches('@').split(':').next().unwrap_or(mxid).to_string()
}

/// A sender's bare Matrix ID, stripped the same way mxid_localpart does -
/// see that function's doc comment.
pub fn short_sender(event: &Value) -> String {
    mxid_localpart(sender(event))
}

/// The plain-text body of an `m.room.message`/`m.room.encrypted`'s
/// decrypted content, plus whether it's an emote (`/me`-style,
/// `msgtype: "m.emote"` - IRC's own `is_action` concept). Media messages
/// (`m.image`/`m.video`/etc.) fall back to their filename `body` here -
/// see media_mxc_uri/is_media for real content resolution.
pub fn message_body(content: &Value) -> (String, bool) {
    let body = content["body"].as_str().unwrap_or("").to_string();
    let is_action = content["msgtype"].as_str() == Some("m.emote");
    (body, is_action)
}

/// The sender's formatted version of the body, if there is one.
///
/// Only `org.matrix.custom.html` counts: the spec allows other formats and a
/// client that rendered an unknown one as HTML would be trusting markup it
/// has no reason to believe is HTML at all. Still untrusted either way -
/// this is somebody else's markup, and the frontend sanitises it.
pub fn formatted_body(content: &Value) -> Option<&str> {
    if content["format"].as_str() != Some("org.matrix.custom.html") {
        return None;
    }
    content["formatted_body"].as_str().filter(|h| !h.is_empty())
}

/// `redacts`, the target event id of an `m.room.redaction` - used for both
/// real message deletes and reaction removal (Matrix has no dedicated "remove
/// reaction" event; un-reacting is redacting the `m.reaction` event you sent -
/// see Runtime::take_matrix_reaction_target).
pub fn redaction_target(event: &Value) -> Option<&str> {
    event["redacts"].as_str().or_else(|| event["content"]["redacts"].as_str())
}

/// The target event id of an edit (`m.relates_to.rel_type == "m.replace"`),
/// if `content` is one. The edit's *new* content lives under
/// `m.new_content` (see edit_new_content) - `content` itself is a
/// best-effort fallback body for clients that don't understand edits.
pub fn edit_target(content: &Value) -> Option<&str> {
    let relates_to = &content["m.relates_to"];
    if relates_to["rel_type"].as_str() == Some("m.replace") { relates_to["event_id"].as_str() } else { None }
}

/// The edit's real replacement content - pass this to message_body
/// instead of the outer content when edit_target returned Some.
pub fn edit_new_content(content: &Value) -> &Value {
    &content["m.new_content"]
}

/// The target event id of a reply (`m.relates_to["m.in_reply_to"]`).
pub fn reply_target(content: &Value) -> Option<&str> {
    content["m.relates_to"]["m.in_reply_to"]["event_id"].as_str()
}

/// The thread a message belongs to, if it is in one.
///
/// A threaded message carries `rel_type: m.thread` and the id of the message
/// the thread grew from. It usually *also* carries an `m.in_reply_to`, which
/// is a fallback for clients that do not understand threads and which points
/// at the previous message in the thread rather than at the thread itself -
/// so reading only that answers a different question and gets a chain of
/// one-line replies where a conversation was.
///
/// Threads are not buffers here. What this buys is that a threaded message
/// says which conversation it belongs to instead of arriving in the timeline
/// with no context at all, which is what a flat client shows today.
pub fn thread_root(content: &Value) -> Option<&str> {
    let relates_to = &content["m.relates_to"];
    if relates_to["rel_type"].as_str() != Some("m.thread") {
        return None;
    }
    relates_to["event_id"].as_str()
}

/// The target event id + emoji key of an `m.reaction` event
/// (`m.relates_to.rel_type == "m.annotation"`).
pub fn reaction_target(content: &Value) -> Option<(&str, &str)> {
    let relates_to = &content["m.relates_to"];
    if relates_to["rel_type"].as_str() != Some("m.annotation") {
        return None;
    }
    Some((relates_to["event_id"].as_str()?, relates_to["key"].as_str()?))
}

#[cfg(test)]
mod thread_tests {
    use super::*;

    #[test]
    fn finds_the_thread_a_message_belongs_to() {
        let threaded = serde_json::json!({
            "m.relates_to": {
                "rel_type": "m.thread",
                "event_id": "$root",
                // The fallback for clients that do not understand threads.
                // It names the previous message in the thread, not the thread,
                // which is why reading it instead would give a chain of
                // one-line replies where a conversation was.
                "m.in_reply_to": { "event_id": "$previous" },
                "is_falling_back": true
            }
        });
        assert_eq!(thread_root(&threaded), Some("$root"));
        assert_eq!(reply_target(&threaded), Some("$previous"));
    }

    #[test]
    fn an_ordinary_reply_is_not_a_thread() {
        let reply = serde_json::json!({ "m.relates_to": { "m.in_reply_to": { "event_id": "$x" } } });
        assert_eq!(thread_root(&reply), None);
        assert_eq!(reply_target(&reply), Some("$x"));
    }

    #[test]
    fn an_edit_is_not_a_thread_either() {
        let edit = serde_json::json!({ "m.relates_to": { "rel_type": "m.replace", "event_id": "$x" } });
        assert_eq!(thread_root(&edit), None);
    }

    #[test]
    fn a_plain_message_relates_to_nothing() {
        assert_eq!(thread_root(&serde_json::json!({ "body": "hi" })), None);
    }
}

const MEDIA_MSGTYPES: &[&str] = &["m.image", "m.video", "m.audio", "m.file"];

/// The `mxc://` URI of an *unencrypted* media message (`m.image`/
/// `m.video`/`m.audio`/`m.file`), if `content` is one and isn't itself
/// Matrix-encrypted-file media (`content.file` present instead of a plain
/// `content.url` - see encrypted_media_file for that case).
pub fn media_mxc_uri(content: &Value) -> Option<&str> {
    if !MEDIA_MSGTYPES.contains(&content["msgtype"].as_str().unwrap_or("")) {
        return None;
    }
    if !content["file"].is_null() {
        return None;
    }
    content["url"].as_str()
}

/// The `content.file` object (an `EncryptedFile` - mxc URI plus the
/// per-file AES key/iv/hash needed to decrypt it) of an *E2EE* media
/// message, if `content` is one - see mod.rs's cached_encrypted_media_path
/// for the actual download+decrypt.
pub fn encrypted_media_file(content: &Value) -> Option<&Value> {
    if !MEDIA_MSGTYPES.contains(&content["msgtype"].as_str().unwrap_or("")) {
        return None;
    }
    content["file"].is_object().then(|| &content["file"])
}

#[cfg(test)]
mod tests {
    use super::formatted_body;
    use serde_json::json;

    /// Only Matrix's own HTML format counts. A client that rendered an
    /// unknown format as HTML would be trusting markup it has no reason to
    /// believe is HTML at all.
    #[test]
    fn only_the_html_format_is_treated_as_html() {
        let html = json!({ "format": "org.matrix.custom.html", "formatted_body": "<b>hi</b>", "body": "hi" });
        assert_eq!(formatted_body(&html), Some("<b>hi</b>"));

        let other = json!({ "format": "org.example.markdown", "formatted_body": "**hi**", "body": "hi" });
        assert_eq!(formatted_body(&other), None);

        // A plain message, and an empty formatted body, both fall back.
        assert_eq!(formatted_body(&json!({ "body": "hi" })), None);
        assert_eq!(formatted_body(&json!({ "format": "org.matrix.custom.html", "formatted_body": "" })), None);
    }
}
