//! Sending a message on into another room, the way Matrix does it.
//!
//! Matrix has no forward event. What Element and every client after it do is
//! send the original's content again, as a new event in the other room - and
//! that is a real forward rather than an imitation, because the content *is*
//! the message: the formatting, and for a picture or a file the very same
//! `mxc://` upload, or in an encrypted room the same `file` with its key. The
//! receiving side gets the original, not a description of it, and nothing is
//! downloaded or uploaded again.
//!
//! What does not travel is everything that tied the message to where it was:
//! the reply or thread it sat in, and who it pinged.

use anyhow::{bail, Context, Result};
use serde_json::{Map, Value};

use super::{crypto, http, protocol, send_typed_event};
use crate::state::AppState;

/// The event types that are a message somebody could hand on. Polls are left
/// out on purpose: a poll's votes refer to the poll event they answer, so a
/// copy would be a second, empty poll rather than the one being shown.
const FORWARDABLE: [&str; 2] = [protocol::EVENT_ROOM_MESSAGE, protocol::EVENT_STICKER];

/// Forwards one event from a room on one Matrix account into a room on
/// another, or the same.
///
/// Fetched from the server rather than taken from the store: what the store
/// holds is a body and a sender, and the forward is the content - the HTML,
/// the attachment, its dimensions and its key.
pub async fn forward_message(
    state: &AppState,
    from_account: &str,
    from_buffer: &str,
    event_id: &str,
    to_account: &str,
    to_buffer: &str,
) -> Result<()> {
    if !event_id.starts_with('$') {
        bail!("that message has not reached the server yet");
    }
    let room_id = state.runtime.get_matrix_room(from_buffer).context("no known Matrix room for that message")?;
    let account = state.accounts.get_matrix(from_account).context("account not connected")?;
    let url = format!(
        "{}/_matrix/client/v3/rooms/{}/event/{}",
        account.homeserver_url.trim_end_matches('/'),
        escape(&room_id),
        escape(event_id)
    );
    let event = http::get_json(&url, &account.access_token).await.context("reading the message")?;
    let event = readable(state, from_account, &room_id, event).await?;

    // An edited message is forwarded as it reads now. The server bundles the
    // latest edit with the event; only one from the original sender counts,
    // which is the rule every client applies to edits.
    let edit = match event.pointer("/unsigned/m.relations/m.replace") {
        Some(edit) if edit["sender"] == event["sender"] && edit.get("content").is_some() => {
            Some(readable(state, from_account, &room_id, edit.clone()).await?)
        }
        _ => None,
    };

    let (event_type, content) = forwardable(&event, edit.as_ref())?;
    send_typed_event(state, to_account, to_buffer, &event_type, content).await
}

/// The event in plaintext, or a reason it cannot be forwarded.
async fn readable(state: &AppState, account_id: &str, room_id: &str, event: Value) -> Result<Value> {
    if event["type"].as_str() != Some(protocol::EVENT_ROOM_ENCRYPTED) {
        return Ok(event);
    }
    let session = state.runtime.get_matrix_machine(account_id).context("crypto session not ready yet")?;
    let room = ruma_common::RoomId::parse(room_id).context("invalid room id")?;
    crypto::decrypt_room_event(&session, &event, &room)
        .await
        .context("that message could not be decrypted here, so there is nothing to send on")
}

/// What gets sent: the original's type, and its content with the ties to its
/// old room cut.
fn forwardable(event: &Value, edit: Option<&Value>) -> Result<(String, Value)> {
    let event_type = event["type"].as_str().unwrap_or_default();
    if !FORWARDABLE.contains(&event_type) {
        bail!("only messages and stickers can be forwarded on Matrix");
    }
    // An edit carries the replacement under m.new_content; that is the
    // message as it now reads.
    let source = edit
        .and_then(|e| e["content"].get("m.new_content"))
        .unwrap_or(&event["content"]);
    let Some(original) = source.as_object().filter(|c| !c.is_empty()) else {
        bail!("that message was deleted");
    };

    let mut content: Map<String, Value> = original.clone();
    // The reply or thread it was part of, and the edit it may itself be.
    content.remove("m.relates_to");
    content.remove("m.new_content");
    // Nobody named in the original is pinged again by its being passed on.
    // Explicitly empty rather than absent: absent tells older clients to
    // fall back to searching the body for names, and they would.
    content.insert("m.mentions".into(), Value::Object(Map::new()));
    // A reply carries a quote of what it answered as a fallback, in the
    // older convention. Without the reply around it, that quote would be a
    // stranger's words with nothing to say whose.
    if original.contains_key("m.relates_to") {
        if let Some(body) = content.get("body").and_then(Value::as_str) {
            content.insert("body".into(), Value::String(strip_plain_fallback(body).to_string()));
        }
        if let Some(html) = content.get("formatted_body").and_then(Value::as_str) {
            content.insert("formatted_body".into(), Value::String(strip_html_fallback(html)));
        }
    }
    Ok((event_type.to_string(), Value::Object(content)))
}

/// The plain reply fallback is a run of `> ` lines and one blank line.
fn strip_plain_fallback(body: &str) -> &str {
    if !body.starts_with("> ") {
        return body;
    }
    match body.find("\n\n") {
        Some(end) if body[..end].lines().all(|l| l.starts_with('>')) => &body[end + 2..],
        _ => body,
    }
}

/// The HTML one is a single `<mx-reply>` element at the front.
fn strip_html_fallback(html: &str) -> String {
    match (html.find("<mx-reply>"), html.find("</mx-reply>")) {
        (Some(start), Some(end)) if start < end => format!("{}{}", &html[..start], &html[end + "</mx-reply>".len()..]),
        _ => html.to_string(),
    }
}

fn escape(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A picture is forwarded as the same upload, with everything that
    /// describes it, and not a word about where it came from.
    #[test]
    fn an_image_travels_as_the_same_upload() {
        let event = json!({
            "type": "m.room.message",
            "sender": "@a:x",
            "content": {
                "msgtype": "m.image", "body": "cat.png", "url": "mxc://x/abc",
                "info": { "w": 640, "h": 480, "mimetype": "image/png" },
                "m.relates_to": { "rel_type": "m.thread", "event_id": "$root" },
                "m.mentions": { "user_ids": ["@b:x"] }
            }
        });
        let (kind, content) = forwardable(&event, None).unwrap();
        assert_eq!(kind, "m.room.message");
        assert_eq!(content["url"], "mxc://x/abc");
        assert_eq!(content["info"]["w"], 640);
        assert!(content.get("m.relates_to").is_none());
        assert_eq!(content["m.mentions"], json!({}));
    }

    /// An encrypted attachment keeps its key: without it the upload is noise.
    #[test]
    fn an_encrypted_file_keeps_its_key() {
        let file = json!({ "url": "mxc://x/enc", "key": { "k": "secret" }, "iv": "iv", "hashes": { "sha256": "h" }, "v": "v2" });
        let event = json!({ "type": "m.room.message", "content": { "msgtype": "m.file", "body": "a.pdf", "file": file.clone() } });
        let (_, content) = forwardable(&event, None).unwrap();
        assert_eq!(content["file"], file);
    }

    /// A reply's quote of what it answered goes with the reply.
    #[test]
    fn a_reply_loses_its_fallback_quote() {
        let event = json!({
            "type": "m.room.message",
            "content": {
                "msgtype": "m.text",
                "body": "> <@b:x> what time?\n> later\n\nnine",
                "format": "org.matrix.custom.html",
                "formatted_body": "<mx-reply><blockquote>what time?</blockquote></mx-reply>nine",
                "m.relates_to": { "m.in_reply_to": { "event_id": "$q" } }
            }
        });
        let (_, content) = forwardable(&event, None).unwrap();
        assert_eq!(content["body"], "nine");
        assert_eq!(content["formatted_body"], "nine");
    }

    /// A message that merely starts with a quote, and answers nothing, keeps it.
    #[test]
    fn a_quote_that_is_not_a_reply_stays() {
        let event = json!({ "type": "m.room.message", "content": { "msgtype": "m.text", "body": "> said\n\nyes" } });
        let (_, content) = forwardable(&event, None).unwrap();
        assert_eq!(content["body"], "> said\n\nyes");
    }

    /// Forwarded as it reads now, not as it was first sent.
    #[test]
    fn an_edited_message_goes_as_edited() {
        let event = json!({ "type": "m.room.message", "content": { "msgtype": "m.text", "body": "teh" } });
        let edit = json!({ "content": { "m.new_content": { "msgtype": "m.text", "body": "the" } } });
        let (_, content) = forwardable(&event, Some(&edit)).unwrap();
        assert_eq!(content["body"], "the");
    }

    #[test]
    fn polls_and_deleted_messages_are_refused() {
        assert!(forwardable(&json!({ "type": "m.poll.start", "content": { "body": "?" } }), None).is_err());
        assert!(forwardable(&json!({ "type": "m.room.message", "content": {} }), None).is_err());
    }

    #[test]
    fn a_sticker_stays_a_sticker() {
        let event = json!({ "type": "m.sticker", "content": { "body": "wave", "url": "mxc://x/s", "info": {} } });
        assert_eq!(forwardable(&event, None).unwrap().0, "m.sticker");
    }
}
