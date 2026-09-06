//! Voice and video calls, as far as a daemon can carry them.
//!
//! The media is not here and cannot be. A call is WebRTC: two peers agreeing
//! on codecs and network paths and then sending each other encrypted RTP,
//! with echo cancellation, jitter buffers and hardware decode in the middle.
//! The client this daemon serves is Chromium, which has all of that; this
//! process has none of it, and building it in Rust to keep the media on this
//! side would be building a browser badly.
//!
//! So what lives here is the signalling, which genuinely is the daemon's: the
//! offer, the answer, the network candidates and the hangup are events in a
//! Matrix room, and rooms are what this backend already knows how to read and
//! write - encrypted ones included, which matters because a call in an
//! encrypted room signals through the same ciphertext everything else does.
//!
//! The division is the same one Discord's voice already draws here, from the
//! other side: there the daemon holds the audio because the connection is a
//! bespoke protocol no browser speaks. Where the protocol *is* the browser's,
//! the browser should hold it.
//!
//! One call at a time per account, which is what the events themselves assume:
//! `m.call.invite` names a call id, and every later event carries it.

use serde_json::{json, Value};

use crate::state::AppState;

/// Events a client has to see to be one end of a call.
///
/// Deliberately everything under `m.call.` rather than a list: the ones this
/// client does not act on are still worth passing through, because a client
/// that learns a new one later needs no change here, and because an event
/// that is silently dropped is the hardest kind to find missing.
pub fn handle(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    own_user_id: &str,
    event_type: &str,
    sender: &str,
    content: &Value,
) {
    // Our own signalling, echoed back by the server the way every event is.
    // Not silence, though: a call answered on another device has to end the
    // ringing here, and the only way this client learns that is by seeing its
    // own account answer somewhere else.
    let mine = sender == own_user_id;
    let call_id = content["call_id"].as_str().unwrap_or_default().to_string();
    // Which of this account's devices sent it. A call answered on a phone is
    // answered by the same user, and without this the desktop cannot tell
    // "somebody else took it" from its own echo.
    let device = content["party_id"].as_str().or_else(|| content["device_id"].as_str());

    state.events.emit(
        "matrixCall",
        json!({
            "accountId": account_id,
            "bufferId": buffer_id,
            "kind": event_type,
            "callId": call_id,
            "from": sender,
            "own": mine,
            "partyId": device,
            // The whole content, because what a client needs from it differs
            // per event - an offer carries SDP, candidates carry a list, a
            // hangup carries a reason - and re-modelling each here would be
            // a second copy of the specification to keep in step.
            "content": content,
        }),
    );
}

/// Sends one call event into a room.
///
/// Every one of them is an ordinary room event, so this is a thin wrapper over
/// the send path the rest of the backend uses - including the encryption,
/// which a call in an encrypted room needs as much as a message does.
pub async fn send(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    event_type: &str,
    content: Value,
) -> anyhow::Result<()> {
    // The typed send, not the message-shaped one beside it: a call event sent
    // as a message arrives as a blank line rather than as a telephone.
    super::send_typed_event(state, account_id, buffer_id, event_type, content).await
}

/// Where to reach this homeserver's TURN servers, and with what.
///
/// Asked of the server rather than configured, because that is where the
/// answer is: a homeserver runs its own relay and hands out short-lived
/// credentials for it. Without one, a call only connects between two people
/// whose networks happen to allow it - which is most of the time on a LAN and
/// almost never between two homes.
pub async fn turn_servers(state: &AppState, account_id: &str) -> anyhow::Result<Value> {
    let account = state
        .accounts
        .get_matrix(account_id)
        .ok_or_else(|| anyhow::anyhow!("account not connected"))?;
    let base = account.homeserver_url.trim_end_matches('/');
    match super::http::get_json(&format!("{base}/_matrix/client/v3/voip/turnServer"), &account.access_token).await {
        Ok(answer) => Ok(answer),
        // A server with no relay configured answers 404, and that is not an
        // error: it means "there is no relay", which a client can still make
        // a call without.
        Err(e) => {
            tracing::debug!("matrix[{account_id}]: no TURN server offered: {e:#}");
            Ok(json!({ "uris": [] }))
        }
    }
}
