//! The message a Kick channel wants everybody to have read.
//!
//! kick.com pins one line above the chat - the link, the rules, the thing
//! being talked about - and it stays there until it is replaced or taken down.
//! This client ignored it entirely, so the one line a channel deliberately put
//! in front of everybody was the one line moho did not show.
//!
//! It arrives two ways, and both are needed for the obvious reason: the event
//! only tells you about a pin made *while you are watching*, and a pin set
//! before you arrived is the ordinary case.
//!
//! - **The Pusher event**, on the same socket the chat arrives on. No auth, no
//!   extra transport - a new arm beside the seventeen already handled.
//! - **`/api/v2/channels/{slug}/pinned-message`** on joining, which needs a
//!   token and is therefore best-effort: an account with none, or with one
//!   Kick has since expired, simply learns about the pin when it next changes.
//!
//! Nothing here is documented by Kick. The event names come from the wrappers
//! that have reverse-engineered this socket and agree on them, and the parse
//! is deliberately lenient for the same reason `cards` is: a payload whose
//! shape has moved on should show nothing rather than something wrong.

use super::*;
use serde_json::Value;

/// What is pinned in a channel right now.
#[derive(Clone, Debug, PartialEq)]
pub struct Pin {
    /// The pinned message's own id, which is what a client keys a dismissal
    /// on - so dismissing this pin does not dismiss the next one.
    pub id: String,
    pub from: String,
    pub body: String,
}

impl Pin {
    pub(super) fn to_json(&self, buffer_id: &str) -> Value {
        serde_json::json!({
            "bufferId": buffer_id,
            "id": self.id,
            "from": self.from,
            "body": self.body,
        })
    }
}

/// Whether an event is about a pinned message at all.
///
/// Deliberately loose. Kick documents none of this; the names come from the
/// third-party wrappers that have reverse-engineered the socket, and matching
/// one exactly means a rename turns the feature off silently - which is the
/// failure mode that is hardest to notice and hardest to attribute.
pub fn is_pin_event(event: &str) -> bool {
    let name = event.rsplit('\\').next().unwrap_or(event);
    // Anchored rather than a bare search for "pin". A plain `contains` would
    // sweep up anything with the letters in it - a `SpinWheelEvent` would be
    // read as a pin - so the test is a name that *begins* with pin or unpin,
    // or that carries "Pinned" as its own word.
    name.contains("Pinned") || name.starts_with("Pin") || name.starts_with("Unpin")
}

/// Whether that event is taking the pin down rather than putting one up.
///
/// Read from the name rather than from the payload's emptiness, because an
/// unpin may well echo the message being unpinned - and read as a pin, that
/// would put back the thing it was announcing the removal of.
pub fn is_unpin(event: &str) -> bool {
    let name = event.rsplit('\\').next().unwrap_or(event);
    name.contains("Delete") || name.contains("Remove") || name.contains("Unpin")
}

/// Reads a pin out of whatever shape it arrived in.
///
/// Three of them, and all three are the same message object at a different
/// depth: the Pusher event wraps it in `message`, the HTTP endpoint wraps it
/// in `data`, and one of them occasionally hands it over bare. Rather than
/// guess which, this looks in all three and takes the first that has an id and
/// something to say.
pub fn read(payload: &Value) -> Option<Pin> {
    for candidate in [payload.get("message"), payload.get("data").and_then(|d| d.get("message")), payload.get("data"), Some(payload)] {
        let Some(message) = candidate else { continue };
        let id = message.get("id").and_then(id_text);
        let body = message.get("content").and_then(|v| v.as_str()).unwrap_or("").trim();
        let Some(id) = id else { continue };
        if body.is_empty() {
            continue;
        }
        let from = message
            .get("sender")
            .and_then(|s| s.get("username"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("someone")
            .to_string();
        return Some(Pin { id, from, body: body.to_string() });
    }
    None
}

/// Kick's message ids are strings in the socket payloads and numbers in some
/// HTTP ones, and a pin keyed on `"42"` in one and `42` in the other is a pin
/// that un-dismisses itself depending on where it was read.
fn id_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Records what is pinned and tells the client, or clears it.
pub fn announce(state: &AppState, buffer_id: &str, pin: Option<Pin>) {
    // Only on a change. The endpoint is read on every join and the event can
    // repeat, and re-announcing an identical pin would put it back on screen
    // for somebody who had just dismissed it.
    if state.runtime.kick_pin(buffer_id) == pin {
        return;
    }
    state.runtime.set_kick_pin(buffer_id, pin.clone());
    let payload = match &pin {
        Some(pin) => pin.to_json(buffer_id),
        // A cleared pin is an event carrying no pin rather than no event: the
        // client has one on screen and has to be told it is gone.
        None => serde_json::json!({ "bufferId": buffer_id, "id": Value::Null }),
    };
    state.events.emit("pinnedMessage", payload);
}

/// Sends the held pin to a window that has just subscribed.
///
/// Not `announce`, which deliberately says nothing when the pin has not
/// changed - that is right for the socket and wrong here: a second window
/// opening the same channel has never been told, and the pin has not changed
/// for anybody else.
pub fn replay(state: &AppState, buffer_id: &str) {
    if let Some(pin) = state.runtime.kick_pin(buffer_id) {
        state.events.emit("pinnedMessage", pin.to_json(buffer_id));
    }
}

/// Asks what is already pinned, for a channel just joined.
///
/// Best-effort by design. The endpoint answers 401 without a token that Kick
/// still accepts, and a channel whose pin cannot be read is a channel that
/// learns about the pin when it next changes - which is worth strictly more
/// than refusing to join.
pub async fn refresh_pin(state: &AppState, buffer_id: &str) {
    let Some(channel) = channel_when_ready(state, buffer_id).await else { return };
    let Ok(http) = api::client() else { return };
    let token = state
        .runtime
        .get_buffer(buffer_id)
        .and_then(|b| state.accounts.get_kick(&b.account_id))
        .and_then(|c| c.token)
        .filter(|t| !t.is_empty());
    match api::pinned_message(&http, token.as_deref(), &channel.slug).await {
        Ok(payload) => announce(state, buffer_id, read(&payload)),
        Err(e) => tracing::debug!("kick: reading what {} has pinned: {e:#}", channel.slug),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The socket wraps it in `message`, the endpoint in `data`, and one of
    /// them hands it over bare. All three are the same object at a different
    /// depth, and a reader that knew only one shape would show a pin on some
    /// channels and not others.
    #[test]
    fn a_pin_is_found_however_deep_it_was_buried() {
        let inner = serde_json::json!({
            "id": "abc", "content": "read the rules", "sender": { "username": "streamer" }
        });
        for payload in [
            serde_json::json!({ "message": inner.clone(), "duration": "120" }),
            serde_json::json!({ "data": { "message": inner.clone() } }),
            serde_json::json!({ "data": inner.clone() }),
            inner.clone(),
        ] {
            let pin = read(&payload).expect("a pin");
            assert_eq!(pin.id, "abc");
            assert_eq!(pin.from, "streamer");
            assert_eq!(pin.body, "read the rules");
        }
    }

    /// Kick writes an id as a string on the socket and as a number over HTTP.
    /// Keyed on one in one place and the other in the other, a dismissal
    /// would come undone depending on which path last read it.
    #[test]
    fn an_id_reads_the_same_whichever_type_it_arrived_as() {
        let text = serde_json::json!({ "message": { "id": "42", "content": "x", "sender": {} } });
        let number = serde_json::json!({ "message": { "id": 42, "content": "x", "sender": {} } });
        assert_eq!(read(&text).unwrap().id, "42");
        assert_eq!(read(&number).unwrap().id, "42");
    }

    /// Nothing rather than something wrong. A payload whose shape has moved
    /// on, or an unpin arriving as an empty object, must not draw an empty bar
    /// across the top of the chat.
    #[test]
    fn a_payload_with_nothing_in_it_is_no_pin() {
        assert!(read(&serde_json::json!({})).is_none());
        assert!(read(&serde_json::json!({ "message": {} })).is_none());
        // An id with no text is not a message anybody pinned.
        assert!(read(&serde_json::json!({ "message": { "id": "1", "content": "   " } })).is_none());
        // And text with no id cannot be dismissed, so it is not usable either.
        assert!(read(&serde_json::json!({ "message": { "content": "hello" } })).is_none());
    }

    /// The names are reverse-engineered and could change. Matching one
    /// exactly would turn the feature off silently on a rename, which is the
    /// hardest kind of failure to notice - so the test is the shape.
    #[test]
    fn a_pin_event_is_recognised_by_shape_and_its_direction_by_name() {
        for up in ["App\\Events\\PinnedMessageCreatedEvent", "PinnedMessageCreatedEvent", "App\\Events\\PinnedMessageUpdatedEvent"] {
            assert!(is_pin_event(up), "{up}");
            assert!(!is_unpin(up), "{up}");
        }
        for down in ["App\\Events\\PinnedMessageDeletedEvent", "PinnedMessageRemovedEvent", "UnpinMessageEvent"] {
            assert!(is_pin_event(down), "{down}");
            assert!(is_unpin(down), "{down}");
        }
        // And nothing else is swept up with them.
        for other in ["App\\Events\\ChatMessageEvent", "PollUpdateEvent", "MessageDeletedEvent", "SpinWheelEvent"] {
            assert!(!is_pin_event(other), "{other}");
        }
    }

    /// Whoever pinned it may not be named; the pin is still the point.
    #[test]
    fn a_pin_with_no_named_sender_is_still_a_pin() {
        let pin = read(&serde_json::json!({ "message": { "id": "7", "content": "hi", "sender": {} } })).expect("a pin");
        assert_eq!(pin.from, "someone");
    }
}
