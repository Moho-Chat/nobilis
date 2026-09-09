//! Who has read how far, in both directions.
//!
//! Matrix carries read receipts as first-class events, which means this
//! client both reports its own and shows everybody else's - and that a
//! receipt can arrive for a message this client has not loaded yet.

use super::*;

pub(super) async fn fetch_read_receipts(
    state: &AppState,
    account_id: &str,
    own_user_id: &str,
    homeserver_url: &str,
    access_token: &str,
) -> Result<()> {
    let filter = serde_json::json!({
        "room": { "timeline": { "limit": 1 }, "state": { "types": [] }, "ephemeral": { "limit": 100 } },
        "presence": { "types": [] }
    })
    .to_string();
    let url = format!(
        "{}/_matrix/client/v3/sync?timeout=0&filter={}",
        homeserver_url.trim_end_matches('/'),
        url::form_urlencoded::byte_serialize(filter.as_bytes()).collect::<String>()
    );
    let resp = http::get_json(&url, access_token).await.context("receipt sync")?;
    let Some(joined) = resp["rooms"]["join"].as_object() else { return Ok(()) };
    for (room_id, room) in joined {
        for event in room["ephemeral"]["events"].as_array().into_iter().flatten() {
            if event["type"].as_str() == Some("m.receipt") {
                take_read_receipts(state, account_id, room_id, own_user_id, &event["content"]);
            }
        }
    }
    Ok(())
}

/// Takes in who else has read how far, from a room's `m.receipt`.
///
/// Matrix says this as a map of event id -> receipt type -> user, which is
/// the inverse of what a client draws: the marker belongs beside the message
/// somebody has read up to, and a person only ever has one of those. So this
/// flattens to one entry per person, newest wins, and emits the room's whole
/// set rather than a delta - a set is what the frontend draws, and rebuilding
/// one from deltas is how a marker gets stuck against a message somebody has
/// long since read past.
///
/// Only `m.read` appears here. `m.read.private` is by definition never
/// federated to anybody else, so a receipt reaching this function is one its
/// sender chose to publish; see mark_read for the other side of that choice.
///
/// Our own receipts are dropped - every client hides your own marker, because
/// the message you have read up to is the one you are looking at.
pub(super) fn take_read_receipts(state: &AppState, account_id: &str, room_id: &str, own_user_id: &str, content: &Value) {
    let Some(by_event) = content.as_object() else { return };
    let mut changed = false;
    for (event_id, receipts) in by_event {
        let Some(readers) = receipts["m.read"].as_object() else { continue };
        for (user_id, receipt) in readers {
            if user_id == own_user_id {
                continue;
            }
            // Threads are not drawn as their own place yet, so a receipt
            // against one would put somebody's marker beside a message they
            // have not necessarily reached in the room itself.
            match receipt["thread_id"].as_str() {
                None | Some("main") => {}
                Some(_) => continue,
            }
            changed |= state.runtime.set_matrix_read_receipt(account_id, room_id, user_id, event_id);
        }
    }
    if changed {
        emit_read_receipts(state, account_id, room_id);
    }
}

/// Re-sends a buffer's read markers, for a frontend that has just started
/// caring about it. A no-op for anything that is not a Matrix room, so the
/// caller need not first ask which protocol a buffer is.
pub fn replay_read_receipts(state: &AppState, buffer_id: &str) {
    let Some(buffer) = state.runtime.get_buffer(buffer_id) else { return };
    if !buffer.account_id.starts_with("matrix:") {
        return;
    }
    let Some(room_id) = state.runtime.get_matrix_room(buffer_id) else { return };
    emit_read_receipts(state, &buffer.account_id, &room_id);
}

/// Sends the room's whole set of read markers to the frontend.
pub(super) fn emit_read_receipts(state: &AppState, account_id: &str, room_id: &str) {
    let Some((buffer_name, _)) = state.runtime.get_matrix_room_name(account_id, room_id) else { return };
    let members = state.runtime.get_matrix_room_members(account_id, room_id);
    let readers: Vec<Value> = state
        .runtime
        .get_matrix_read_receipts(account_id, room_id)
        .into_iter()
        // Somebody who has left still has a receipt on the server. Drawing it
        // would claim a person is in a room they are not in.
        .filter(|(user_id, _)| members.contains_key(user_id))
        .map(|(user_id, event_id)| {
            serde_json::json!({
                "userId": user_id,
                "nick": members.get(&user_id).cloned().unwrap_or_else(|| protocol::mxid_localpart(&user_id)),
                "avatarUrl": state.runtime.get_matrix_member_avatar(account_id, &user_id),
                "messageId": event_id,
            })
        })
        .collect();
    state.events.emit(
        "readReceipts",
        serde_json::json!({
            "accountId": account_id,
            "bufferId": crate::model::buffer_id(account_id, &buffer_name),
            "readers": readers,
        }),
    );
}

/// Says this room has been read, as far as its newest message.
///
/// Two markers, because Matrix has two and they answer different questions.
/// `m.read` is public - it is what puts your avatar against a message in
/// somebody else's client - and `m.fully_read` is private, and is what your
/// own other clients use to stop showing the room as unread.
///
/// Sent together through the one endpoint that takes both, so reading a room
/// here stops it being bold on your phone. Before this, `markBufferRead` was
/// a no-op for Matrix and the two never agreed.
///
/// `publicly` is what the privacy toggle turns off, and turning it off swaps
/// `m.read` for `m.read.private` rather than dropping it. Both settle the
/// room's unread count on the server; only the public one is federated, so
/// the difference is whether other people are told - not whether your own
/// devices agree. Sending nothing at all would have bought the same privacy
/// by making the room unread again on every other device you own.
pub async fn mark_read(state: &AppState, account_id: &str, buffer_id: &str, publicly: bool) -> Result<()> {
    let Some(config) = state.accounts.get_matrix(account_id) else { anyhow::bail!("no such account") };
    let Some(room_id) = state.runtime.get_matrix_room(buffer_id) else {
        anyhow::bail!("no known Matrix room for this buffer")
    };
    // The newest message this client actually holds. Nothing to say if the
    // room has never had one - and claiming to have read a room that is empty
    // would be a receipt pointing at nothing.
    let Some(event_id) = state.store.newest_message_id(buffer_id).ok().flatten().filter(|id| id.starts_with('$')) else {
        return Ok(());
    };
    let base = config.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/rooms/{}/read_markers",
        url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>()
    );
    let mut body = serde_json::json!({ "m.fully_read": event_id });
    body[if publicly { "m.read" } else { "m.read.private" }] = Value::String(event_id);
    http::post_json(&url, Some(&config.access_token), body).await.context("sending read markers")?;
    Ok(())
}

pub async fn send_typing(state: &AppState, account_id: &str, room_id: &str, typing: bool) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let room = url::form_urlencoded::byte_serialize(room_id.trim().as_bytes()).collect::<String>();
    let user = url::form_urlencoded::byte_serialize(account.user_id.trim().as_bytes()).collect::<String>();
    let body = if typing {
        serde_json::json!({ "typing": true, "timeout": crate::backend::discord::TYPING_TTL_MS })
    } else {
        serde_json::json!({ "typing": false })
    };
    http::put_json(&format!("{base}/_matrix/client/v3/rooms/{room}/typing/{user}"), &account.access_token, body).await?;
    Ok(())
}
