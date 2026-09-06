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

/// How long a published membership stands before it is treated as stale.
///
/// A client that is killed mid-call never withdraws its membership, so the
/// state event would say somebody is in a call for ever. Every client that
/// implements MSC3401 stamps an expiry and refreshes it while it is still
/// there; anything past its expiry is somebody's crash, not a participant.
pub const MEMBERSHIP_TTL_MS: i64 = 90_000;

/// The event that says who is in a room's call.
pub const EVENT_MEMBER: &str = "m.call.member";

/// Says this account is in the room's call, or is no longer.
///
/// One state event per user, keyed by user id, holding a list of memberships -
/// one per device, since the same person may be in a call from a phone and a
/// desktop and only one of those should stop when the other leaves.
///
/// Withdrawing sends the remaining memberships rather than empty content, for
/// the same reason: another device of this account may still be in the call.
pub async fn set_membership(
    state: &AppState,
    account_id: &str,
    buffer_id: &str,
    device_id: &str,
    joined: bool,
) -> anyhow::Result<()> {
    let account = state.accounts.get_matrix(account_id).ok_or_else(|| anyhow::anyhow!("account not connected"))?;
    let room_id = state
        .runtime
        .get_matrix_room(buffer_id)
        .ok_or_else(|| anyhow::anyhow!("no known Matrix room for this buffer"))?;
    let now = chrono::Utc::now().timestamp_millis();
    let mut memberships: Vec<Value> = state
        .runtime
        .matrix_call_members(account_id, &room_id)
        .into_iter()
        .filter(|m| m["user_id"].as_str() == Some(account.user_id.as_str()))
        .filter_map(|m| m["membership"].clone().into())
        // This device's own entry is rewritten below either way.
        .filter(|m: &Value| m["device_id"].as_str() != Some(device_id))
        .filter(|m: &Value| m["expires_ts"].as_i64().unwrap_or(0) > now)
        .collect();
    if joined {
        memberships.push(json!({
            "application": "m.call",
            "call_id": "",
            "scope": "m.room",
            "device_id": device_id,
            "expires_ts": now + MEMBERSHIP_TTL_MS,
            // No focus: this is a mesh between the people in the room rather
            // than a conference on somebody's server. See the module note.
            "foci_active": [],
        }));
    }
    // Whether everybody in the room may join a call at all.
    //
    // A membership is a *state* event, and a room's default is that only
    // moderators may send those - so a call started in an ordinary room is
    // one nobody else can join, and they find out with a bare "not
    // authorized". Where this account can change the permissions, it lowers
    // that one event to what everybody has; where it cannot, the call still
    // works for whoever may already send it.
    if joined {
        allow_everybody_to_join(state, account_id, &room_id).await;
    }

    let result = super::put_room_state(
        state,
        account_id,
        &room_id,
        EVENT_MEMBER,
        &account.user_id,
        json!({ "memberships": memberships }),
    )
    .await;
    if let Err(e) = &result {
        // The refusal that has an explanation worth giving.
        if e.to_string().contains("M_FORBIDDEN") {
            anyhow::bail!(
                "this room does not let its members join calls - somebody who can change the room's \
                 permissions has to allow the m.call.member event"
            );
        }
    }
    result
}

/// Lets ordinary members join this room's calls, where we may say so.
///
/// One key in the power levels: `events["m.call.member"] = 0`. Without it the
/// room's `state_default` applies, which is 50 - a moderator - and a call in
/// an ordinary room is a call of one.
///
/// Best-effort and silent on refusal: somebody without the power to change
/// permissions can still be in a call, and failing to start one because the
/// room could not be reconfigured would be worse than a call only some people
/// can join.
async fn allow_everybody_to_join(state: &AppState, account_id: &str, room_id: &str) {
    let Some(levels) = state.runtime.matrix_power_levels(account_id, room_id) else { return };
    if levels["events"][EVENT_MEMBER].as_i64() == Some(0) {
        return;
    }
    let mut next = levels.clone();
    next["events"][EVENT_MEMBER] = json!(0);
    match super::put_room_state(state, account_id, room_id, "m.room.power_levels", "", next).await {
        Ok(()) => tracing::info!("matrix[{account_id}]: {room_id} now lets its members join calls"),
        Err(e) => tracing::debug!("matrix[{account_id}]: cannot open {room_id} to calls: {e:#}"),
    }
}

/// Reads one `m.call.member` state event into the memberships it carries.
///
/// Tolerant, and expiry-aware: a membership past its stamp is somebody whose
/// client died, and treating it as a participant would leave a tile in the
/// grid for somebody who left hours ago.
pub fn read_memberships(user_id: &str, content: &Value, now_ms: i64) -> Vec<Value> {
    content["memberships"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter(|m| m["application"].as_str().unwrap_or("m.call") == "m.call")
                .filter(|m| m["expires_ts"].as_i64().unwrap_or(0) > now_ms)
                .map(|m| json!({ "user_id": user_id, "membership": m }))
                .collect()
        })
        .unwrap_or_default()
}

/// Who to call, and who to wait for.
///
/// Both ends of every pair see each other arrive, and if both call, both
/// answer, and the call collides with itself. So the rule is one line and the
/// same on both sides: the smaller id calls the larger. It has to be a
/// property of the pair rather than of who arrived first, because "first" is
/// not something two clients can agree on.
pub fn should_offer(own_key: &str, their_key: &str) -> bool {
    own_key < their_key
}

#[cfg(test)]
mod member_tests {
    use super::*;

    #[test]
    fn a_membership_past_its_stamp_is_not_a_participant() {
        let content = json!({ "memberships": [
            { "application": "m.call", "device_id": "HERE", "expires_ts": 2_000 },
            { "application": "m.call", "device_id": "GONE", "expires_ts": 500 },
        ]});
        let live = read_memberships("@a:example.org", &content, 1_000);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0]["membership"]["device_id"], "HERE");
        assert_eq!(live[0]["user_id"], "@a:example.org");
    }

    #[test]
    fn something_that_is_not_a_call_is_not_read_as_one() {
        let content = json!({ "memberships": [
            { "application": "m.whiteboard", "device_id": "X", "expires_ts": 9_000 },
        ]});
        assert!(read_memberships("@a:example.org", &content, 1_000).is_empty());
        assert!(read_memberships("@a:example.org", &json!({}), 1_000).is_empty());
    }

    #[test]
    fn exactly_one_end_of_every_pair_offers() {
        // Whichever way round the two clients ask, they must not agree.
        assert!(should_offer("@a:example.org|AAA", "@b:example.org|BBB"));
        assert!(!should_offer("@b:example.org|BBB", "@a:example.org|AAA"));
        // Including two devices of the same person, which is the case that
        // makes a user-id comparison alone wrong.
        assert!(should_offer("@a:example.org|AAA", "@a:example.org|ZZZ"));
        assert!(!should_offer("@a:example.org|ZZZ", "@a:example.org|AAA"));
    }
}
