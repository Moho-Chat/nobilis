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

use anyhow::Context;
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
///
/// The unstable name, deliberately: this is what Element writes and reads
/// today, and a call is only worth being in if the client most people use can
/// see that you are in it. `m.rtc.member` is where this is going - sticky
/// events, MSC4143 - and is read below as well, so a room where Element has
/// already moved on is still understood.
pub const EVENT_MEMBER: &str = "org.matrix.msc3401.call.member";

/// Where the membership event is going: MSC4143's own name for it.
pub const EVENT_MEMBER_NEXT: &str = "org.matrix.msc4143.rtc.member";

/// What moho puts in `foci_preferred` to mean "I am on the mesh".
///
/// Element's clients read the transports in a membership and connect to the
/// one they can use. This is not one of them, on purpose: a moho participant
/// says what it actually offers rather than claiming a LiveKit focus it
/// cannot serve, and a client that does not understand it ignores it rather
/// than dialling nothing.
pub const TRANSPORT_MESH: &str = "moho.mesh";

/// The state key Element uses for a per-device membership.
///
/// `_@user:server_DEVICEID_m.call`, with the leading underscore because an
/// ordinary room refuses a state key starting with `@` from anybody but its
/// owner - and the whole point of the key is that it is per device. Rooms on
/// the MSC3757 versions restrict it properly and take the bare form.
pub fn membership_state_key(user_id: &str, device_id: &str, room_version: &str) -> String {
    let key = format!("{user_id}_{device_id}_m.call");
    if room_version.starts_with("org.matrix.msc3757") || room_version.starts_with("org.matrix.msc3779") {
        key
    } else {
        format!("_{key}")
    }
}

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
    // The media server this end is on, where it is on one.
    focus: Option<Value>,
) -> anyhow::Result<()> {
    let account = state.accounts.get_matrix(account_id).ok_or_else(|| anyhow::anyhow!("account not connected"))?;
    let room_id = state
        .runtime
        .get_matrix_room(buffer_id)
        .ok_or_else(|| anyhow::anyhow!("no known Matrix room for this buffer"))?;
    let now = chrono::Utc::now().timestamp_millis();
    let version = state.runtime.matrix_room_version(account_id, &room_id).unwrap_or_default();
    let state_key = membership_state_key(&account.user_id, device_id, &version);

    // One event per device, so leaving on the desktop does not take the phone
    // out of the call with it - which is why the state key carries the device
    // and the content describes only this one.
    let content = if joined {
        // Everybody may join a call at all: a membership is a *state* event,
        // and a room's default is that only moderators may send those. Element
        // needs this as much as moho does - it is why a call in an ordinary
        // room is one nobody else can join.
        allow_everybody_to_join(state, account_id, &room_id).await;
        json!({
            "application": "m.call",
            "call_id": "",
            "scope": "m.room",
            "device_id": device_id,
            // What the SFU-based clients key their media participant on.
            "membershipID": format!("{}:{}", account.user_id, device_id),
            "created_ts": now,
            "expires": MEMBERSHIP_TTL_MS,
            // What this end is actually on. A media server where the call
            // has one - which is what makes this membership legible to
            // Element, since that is the only transport its clients speak -
            // and the mesh where there is none, which a client that does not
            // know it ignores rather than dialling nothing.
            "focus_active": focus.clone().unwrap_or_else(|| json!({ "type": TRANSPORT_MESH })),
            "foci_preferred": [focus.clone().unwrap_or_else(|| json!({ "type": TRANSPORT_MESH }))],
        })
    } else {
        // Leaving is an empty membership rather than a deleted event, because
        // Matrix has no delete - and empty is how every client reads "gone".
        json!({})
    };

    let result = super::put_room_state(state, account_id, &room_id, EVENT_MEMBER, &state_key, content).await;
    if let Err(e) = &result {
        // The refusal that has an explanation worth giving.
        if e.to_string().contains("M_FORBIDDEN") {
            anyhow::bail!(
                "this room does not let its members join calls - somebody who can change the room's \
                 permissions has to allow the {EVENT_MEMBER} event"
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

/// Reads one membership state event into what it says.
///
/// Two shapes, because Matrix is between them: the session shape Element
/// writes today (`org.matrix.msc3401.call.member`, one event per device,
/// `expires` as a duration from `created_ts`) and the MSC4143 shape it is
/// moving to (`m.rtc.member`, a `member` object and `transports`). Reading
/// both costs a branch and means a room where half the people are on a newer
/// Element is still a room where moho can see the call.
///
/// Expiry-aware either way: a membership past its stamp is somebody whose
/// client died, and treating it as a participant leaves a tile in the grid
/// for somebody who left hours ago.
pub fn read_membership(user_id: &str, content: &Value, now_ms: i64) -> Option<Value> {
    // Gone: an empty content is how every client spells "no longer here",
    // since Matrix state cannot be deleted.
    if content.as_object().is_none_or(|c| c.is_empty()) {
        return None;
    }
    // The MSC4143 shape.
    if let Some(member) = content.get("member").and_then(|m| m.as_object()) {
        let device = member.get("device_id").and_then(|d| d.as_str()).unwrap_or_default();
        let transports: Vec<String> = content["transports"]["published"]
            .as_array()
            .map(|list| list.iter().filter_map(|t| t["type"].as_str().map(String::from)).collect())
            .unwrap_or_default();
        return Some(json!({
            "user_id": member.get("user_id").and_then(|u| u.as_str()).unwrap_or(user_id),
            "membership": { "device_id": device, "expires_ts": now_ms + MEMBERSHIP_TTL_MS },
            "transports": transports,
        }));
    }
    // The session shape. `expires` is a duration and `created_ts` the moment
    // it started, which is not the same as a stamp and reads differently.
    if content["application"].as_str().unwrap_or("m.call") != "m.call" {
        return None;
    }
    let created = content["created_ts"].as_i64().unwrap_or(now_ms);
    let expires_ts = match content["expires"].as_i64() {
        Some(duration) => created + duration,
        // A membership with no expiry at all: believed for as long as one of
        // ours would be, rather than for ever.
        None => created + MEMBERSHIP_TTL_MS,
    };
    if expires_ts <= now_ms {
        return None;
    }
    let foci = content["foci_preferred"].as_array().cloned().unwrap_or_default();
    let transports: Vec<String> = foci.iter().filter_map(|t| t["type"].as_str().map(String::from)).collect();
    Some(json!({
        "user_id": user_id,
        "membership": {
            "device_id": content["device_id"].as_str().unwrap_or_default(),
            "expires_ts": expires_ts,
        },
        "transports": transports,
        // The media server this participant is on, where they are on one:
        // everybody in a call has to be where the media already is, so this
        // is what a client joining afterwards has to use.
        "focus": foci.iter().find(|f| f["type"].as_str() == Some("livekit")).cloned().unwrap_or(Value::Null),
    }))
}

/// Where the media server is, if there is one to use.
///
/// Three places, in the order that answers the question best:
///
/// 1. The call itself. A call already running names its focus in every
///    membership, and everybody has to be where the media already is - this
///    is what makes joining *Element's* call possible rather than starting a
///    second one beside it.
/// 2. This account's own setting, for a homeserver that publishes nothing.
///    Element has the same fallback for the same reason.
/// 3. The homeserver's `.well-known`, which is where a server that has been
///    set up for calls says so.
pub async fn find_focus(state: &AppState, account_id: &str, room_id: &str) -> Option<Value> {
    let members = state.runtime.matrix_call_members(account_id, room_id);
    if let Some(focus) = members.iter().find_map(|m| m["focus"].as_object()) {
        return Some(Value::Object(focus.clone()));
    }
    let account = state.accounts.get_matrix(account_id)?;
    if let Some(url) = account.rtc_focus_url.as_deref().filter(|u| !u.trim().is_empty()) {
        return Some(json!({ "type": "livekit", "livekit_service_url": url.trim() }));
    }
    // The homeserver's own answer. Fetched rather than remembered: it changes
    // when somebody sets a server up for calls, which is exactly when a
    // client that cached "none" would be wrong.
    let base = account.homeserver_url.trim_end_matches('/');
    let host = base.split("://").nth(1).unwrap_or(base).split('/').next().unwrap_or(base);
    let well_known = super::http::get_json(&format!("https://{host}/.well-known/matrix/client"), "").await.ok()?;
    well_known["org.matrix.msc4143.rtc_foci"]
        .as_array()?
        .iter()
        .find(|f| f["type"].as_str() == Some("livekit"))
        .cloned()
}

/// Trades this account's identity for a way into the media server.
///
/// The server beside a LiveKit SFU does not take a Matrix access token - it
/// would have no way to check one. It takes an OpenID token, which is the
/// homeserver saying "this really is who they claim to be" in a form a third
/// party can verify by asking the homeserver back. That is exchanged for a
/// LiveKit URL and a JWT good for one room.
pub async fn rtc_token(state: &AppState, account_id: &str, room_id: &str, focus: &Value) -> anyhow::Result<Value> {
    let account = state.accounts.get_matrix(account_id).ok_or_else(|| anyhow::anyhow!("account not connected"))?;
    let base = account.homeserver_url.trim_end_matches('/');
    let openid = super::http::post_json(
        &format!(
            "{base}/_matrix/client/v3/user/{}/openid/request_token",
            url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>()
        ),
        Some(&account.access_token),
        json!({}),
    )
    .await
    .context("asking the homeserver to vouch for this account")?;

    let service = focus["livekit_service_url"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("that call's media server has no address"))?
        .trim_end_matches('/');
    // The room as the media server knows it. The call names its own alias
    // where it has one, since everybody has to land in the same LiveKit room.
    let alias = focus["livekit_alias"].as_str().unwrap_or(room_id);
    let answer = super::http::post_json(
        &format!("{service}/sfu/get"),
        None,
        json!({ "room": alias, "openid_token": openid, "device_id": account.device_id }),
    )
    .await
    .context("asking the media server for a way in")?;
    if answer["jwt"].as_str().unwrap_or_default().is_empty() {
        anyhow::bail!("the media server would not let this account in");
    }
    Ok(json!({
        "url": answer["url"],
        "jwt": answer["jwt"],
        "identity": format!("{}:{}", account.user_id, account.device_id),
    }))
}

/// Who to call, and who to wait for./// Who to call, and who to wait for.
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

    /// Exactly what Element writes today, out of matrix-js-sdk's
    /// `SessionMembershipData`. If this stops being read, moho stops being
    /// able to see the calls most people are in.
    fn element_membership(created: i64, expires: i64) -> Value {
        json!({
            "application": "m.call",
            "call_id": "",
            "scope": "m.room",
            "device_id": "ELEMENTDEV",
            "membershipID": "@them:example.org:ELEMENTDEV",
            "created_ts": created,
            "expires": expires,
            "focus_active": { "type": "livekit", "focus_selection": "oldest_membership" },
            "foci_preferred": [{
                "type": "livekit",
                "livekit_service_url": "https://livekit-jwt.call.matrix.org",
                "livekit_alias": "!room:example.org"
            }],
        })
    }

    #[test]
    fn elements_own_membership_is_read() {
        let read = read_membership("@them:example.org", &element_membership(1_000, 4_000), 2_000).unwrap();
        assert_eq!(read["user_id"], "@them:example.org");
        assert_eq!(read["membership"]["device_id"], "ELEMENTDEV");
        // created_ts + expires, because `expires` is a duration rather than a
        // stamp - reading it as a stamp would expire everybody immediately.
        assert_eq!(read["membership"]["expires_ts"], 5_000);
        assert_eq!(read["transports"][0], "livekit");
    }

    #[test]
    fn a_membership_past_its_stamp_is_not_a_participant() {
        // Created long ago and short-lived: somebody whose client died.
        assert!(read_membership("@them:example.org", &element_membership(1_000, 500), 9_000).is_none());
        // And an emptied event, which is how leaving is spelled.
        assert!(read_membership("@them:example.org", &json!({}), 1_000).is_none());
    }

    #[test]
    fn a_membership_carries_the_media_server_it_is_on() {
        let read = read_membership("@them:example.org", &element_membership(1_000, 9_000), 2_000).unwrap();
        // Everybody in a call has to be where the media already is, so this
        // is what a client joining afterwards connects to.
        assert_eq!(read["focus"]["livekit_service_url"], "https://livekit-jwt.call.matrix.org");
        let mesh = json!({ "application": "m.call", "device_id": "D", "created_ts": 1_000, "expires": 9_000,
                           "foci_preferred": [{ "type": "moho.mesh" }] });
        assert!(read_membership("@a:example.org", &mesh, 2_000).unwrap()["focus"].is_null());
    }

    #[test]
    fn the_newer_shape_is_read_too() {
        let content = json!({
            "member": { "id": "abc", "user_id": "@them:example.org", "device_id": "NEWDEV" },
            "slot_id": "m.call#ROOM",
            "application": { "type": "m.call" },
            "transports": { "published": [{ "type": "m.livekit" }] },
        });
        let read = read_membership("@ignored:example.org", &content, 1_000).unwrap();
        assert_eq!(read["user_id"], "@them:example.org");
        assert_eq!(read["membership"]["device_id"], "NEWDEV");
    }

    #[test]
    fn something_that_is_not_a_call_is_not_read_as_one() {
        let content = json!({ "application": "m.whiteboard", "device_id": "X", "expires": 9_000 });
        assert!(read_membership("@a:example.org", &content, 1_000).is_none());
    }

    #[test]
    fn the_state_key_is_the_one_element_writes() {
        // Prefixed on an ordinary room, because a key starting with `@` is
        // reserved for its owner and this one has to be per device.
        assert_eq!(
            membership_state_key("@me:example.org", "DEV", "10"),
            "_@me:example.org_DEV_m.call"
        );
        // And bare where the room version restricts it properly.
        assert_eq!(
            membership_state_key("@me:example.org", "DEV", "org.matrix.msc3757.10"),
            "@me:example.org_DEV_m.call"
        );
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
