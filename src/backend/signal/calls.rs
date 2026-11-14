//! Calls. The call itself (its keys, its media) is RingRTC's, Signal's own WebRTC, which runs in moho's app beside the
//! window (see the app's `signal-calls`), not here: this daemon carries its messages. Every call message is handed to the
//! app as it arrived (`signalCallMessage`, with the identity keys RingRTC binds a call to), and what RingRTC says goes
//! out through `send` - the daemon is the one connected to Signal.
//!
//! What the daemon reads itself, so a window without RingRTC still knows: the offer (who, voice or video, its id), and
//! the hangups and busy signals that end it. So an incoming call rings the window, stops ringing when the caller gives up
//! or one of the account's devices picks up or turns it down, and a call nobody answered is said as a missed call - under
//! the same id as the phone's own call log line for it, so the two never show twice.

use super::people::{self, Ctx};
use crate::state::AppState;
use presage::libsignal_service::protocol::ServiceId;
use presage::proto::call_message::{hangup, offer};
use presage::proto::CallMessage;
use presage::store::Thread;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

/// How long a call rings before it is taken for missed, as Signal's clients give up on one.
pub const RINGS_FOR_MS: u64 = 60_000;

#[derive(Clone)]
struct Ring {
    account_id: String,
    buffer_id: String,
    video: bool,
    started_ms: u64,
}

/// The calls ringing now, by call id.
fn ringing() -> &'static Mutex<HashMap<u64, Ring>> {
    static RINGING: OnceLock<Mutex<HashMap<u64, Ring>>> = OnceLock::new();
    RINGING.get_or_init(Default::default)
}

fn tell(state: &AppState, ring: &Ring, call_id: u64, on: bool) {
    state.events.emit(
        "incomingCall",
        serde_json::json!({
            "accountId": ring.account_id,
            "bufferId": ring.buffer_id,
            "channelId": call_id.to_string(),
            "ringing": on,
            "video": ring.video,
            // The app answers it with RingRTC where it has it (it knows the call by this id); without, it is answered on
            // the phone and only silenced here.
            "answerElsewhere": true,
            "callId": call_id.to_string(),
        }),
    );
}

/// The missed-call line, as the phone's call log writes it (see `inbox::call_logged`), so whichever comes first is kept.
fn say_missed(state: &AppState, ring: &Ring, call_id: u64) {
    use presage::proto::sync_message::call_event::{Direction, Event, Type};
    let kind = if ring.video { Type::VideoCall } else { Type::AudioCall };
    let Some(words) = super::inbox::call_words(kind as i32, Direction::Incoming as i32, Event::NotAccepted as i32) else { return };
    let Some(buffer) = state.runtime.get_buffer(&ring.buffer_id) else { return };
    let id = format!("call-{call_id}-{}", Event::NotAccepted as i32);
    state.runtime.record_message_at(
        state, &ring.account_id, &buffer.name, &buffer.kind, "Signal", &format!("{words}. Calls are answered in Signal on your phone"), false, "system", None, Some(id), false, None, Vec::new(), Vec::new(), None, Some((ring.started_ms / 1000) as i64), None, None,
    );
}

/// Stops a call ringing. `missed` says it as missed.
fn stop(state: &AppState, call_id: u64, missed: bool) {
    let Some(ring) = ringing().lock().unwrap_or_else(|e| e.into_inner()).remove(&call_id) else { return };
    tell(state, &ring, call_id, false);
    if missed {
        say_missed(state, &ring, call_id);
    }
}

/// Silences a ringing call here (the window's button). Nothing is sent: the caller still hears it ring, and the phone
/// still rings, as when a ringing device is put face down.
pub fn silence(state: &AppState, buffer_id: &str) -> bool {
    let id = ringing().lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(_, r)| r.buffer_id == buffer_id).map(|(id, _)| *id);
    match id {
        Some(id) => {
            stop(state, id, false);
            true
        }
        None => false,
    }
}

/// A call message as RingRTC's `CallingMessage` takes it (ids as text: they are 64-bit), for the app.
pub fn to_wire(call: &CallMessage) -> serde_json::Value {
    use base64::Engine;
    let b64 = |b: &Option<Vec<u8>>| b.as_ref().map(|b| base64::engine::general_purpose::STANDARD.encode(b));
    let mut out = serde_json::Map::new();
    if let Some(o) = &call.offer {
        out.insert("offer".into(), serde_json::json!({ "callId": o.id.unwrap_or(0).to_string(), "type": o.r#type.unwrap_or(0), "opaque": b64(&o.opaque) }));
    }
    if let Some(a) = &call.answer {
        out.insert("answer".into(), serde_json::json!({ "callId": a.id.unwrap_or(0).to_string(), "opaque": b64(&a.opaque) }));
    }
    if !call.ice_update.is_empty() {
        out.insert("iceCandidates".into(), call.ice_update.iter().map(|i| serde_json::json!({ "callId": i.id.unwrap_or(0).to_string(), "opaque": b64(&i.opaque) })).collect());
    }
    if let Some(b) = &call.busy {
        out.insert("busy".into(), serde_json::json!({ "callId": b.id.unwrap_or(0).to_string() }));
    }
    if let Some(h) = &call.hangup {
        out.insert("hangup".into(), serde_json::json!({ "callId": h.id.unwrap_or(0).to_string(), "type": h.r#type.unwrap_or(0), "deviceId": h.device_id.unwrap_or(0) }));
    }
    if let Some(o) = &call.opaque {
        out.insert("opaque".into(), serde_json::json!({ "data": b64(&o.data), "urgency": o.urgency.unwrap_or(0) }));
    }
    if let Some(d) = call.destination_device_id {
        out.insert("destinationDeviceId".into(), d.into());
    }
    serde_json::Value::Object(out)
}

/// The other way: what RingRTC asked to send, as a call message.
pub fn from_wire(value: &serde_json::Value) -> Result<CallMessage, String> {
    use base64::Engine;
    use presage::proto::call_message::{Answer, Busy, Hangup, IceUpdate, Offer, Opaque};
    let id = |v: &serde_json::Value| -> Result<Option<u64>, String> {
        match v.get("callId") {
            None => Ok(None),
            Some(serde_json::Value::String(s)) => s.parse().map(Some).map_err(|_| "a call id is a number".to_string()),
            Some(serde_json::Value::Number(n)) => Ok(n.as_u64()),
            Some(_) => Err("a call id is a number".to_string()),
        }
    };
    let bytes = |v: &serde_json::Value, name: &str| -> Result<Option<Vec<u8>>, String> {
        match v.get(name).and_then(|b| b.as_str()) {
            None => Ok(None),
            Some(text) => base64::engine::general_purpose::STANDARD.decode(text).map(Some).map_err(|_| format!("\"{name}\" is base64")),
        }
    };
    let int = |v: &serde_json::Value, name: &str| v.get(name).and_then(|n| n.as_i64());
    let mut call = CallMessage::default();
    if let Some(o) = value.get("offer") {
        call.offer = Some(Offer { id: id(o)?, r#type: int(o, "type").map(|t| t as i32), opaque: bytes(o, "opaque")? });
    }
    if let Some(a) = value.get("answer") {
        call.answer = Some(Answer { id: id(a)?, opaque: bytes(a, "opaque")? });
    }
    if let Some(list) = value.get("iceCandidates").and_then(|l| l.as_array()) {
        for i in list {
            call.ice_update.push(IceUpdate { id: id(i)?, opaque: bytes(i, "opaque")? });
        }
    }
    if let Some(b) = value.get("busy") {
        call.busy = Some(Busy { id: id(b)? });
    }
    if let Some(h) = value.get("hangup") {
        call.hangup = Some(Hangup { id: id(h)?, r#type: int(h, "type").map(|t| t as i32), device_id: int(h, "deviceId").map(|d| d as u32) });
    }
    if let Some(o) = value.get("opaque") {
        call.opaque = Some(Opaque { data: bytes(o, "data")?, urgency: int(o, "urgency").map(|u| u as i32) });
    }
    call.destination_device_id = value.get("destinationDeviceId").and_then(|d| d.as_u64()).map(|d| d as u32);
    if call == CallMessage::default() {
        return Err("that call message says nothing".to_string());
    }
    Ok(call)
}

/// Hands a call message to the app (RingRTC), with what it needs to take it: who and which device sent it, how old it
/// is, this device, and both identity keys (RingRTC binds the call to them; without the key type byte, as Signal's
/// clients give them).
pub fn hand_to_app(ctx: &Ctx<'_>, sender: Uuid, sender_device: u32, server_ms: u64, call: &CallMessage) {
    use base64::Engine;
    let key = |k: Option<presage::libsignal_service::protocol::IdentityKey>| k.map(|k| base64::engine::general_purpose::STANDARD.encode(&k.serialize()[1..]));
    let theirs = key(ctx.store.identity_of(&sender.to_string()).ok().flatten());
    let mine = key(ctx.store.own_identity().ok().flatten());
    let (Some(theirs), Some(mine)) = (theirs, mine) else {
        tracing::info!("signal[{}]: a call message without the keys to take it", ctx.account_id);
        return;
    };
    let local_device = ctx.state.accounts.get_signal(ctx.account_id).map(|c| c.device_id).unwrap_or(0);
    let age = super::commands::now_ms().saturating_sub(server_ms) / 1000;
    let buffer_id = if ctx.is_own(&sender) {
        None
    } else {
        let remote = format!("p:{sender}");
        ctx.state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id && b.remote_id.as_deref() == Some(remote.as_str())).map(|b| b.id)
    };
    ctx.state.events.emit(
        "signalCallMessage",
        serde_json::json!({
            "accountId": ctx.account_id,
            "ownAci": ctx.own.to_string(),
            "localDeviceId": local_device,
            "sender": sender.to_string(),
            "senderDeviceId": sender_device,
            "ageSec": age,
            "receivedAtMs": server_ms,
            "senderIdentityKey": theirs,
            "receiverIdentityKey": mine,
            "bufferId": buffer_id,
            "message": to_wire(call),
        }),
    );
}

/// What the app needs to place or take a call on an account: who it is, this device, and Signal's relays. A routed
/// account is refused: a call's media is UDP to Signal's relays and the other person, which Tor cannot carry.
pub async fn settings(state: &AppState, account_id: &str) -> Result<serde_json::Value, String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    if crate::net::route::router().routed(account_id) {
        return Err("Calls are not made on an account that goes through Tor: a call's sound and picture would leave Tor".to_string());
    }
    let (done, answer) = tokio::sync::oneshot::channel();
    handle.commands.send(super::Command::CallRelays { done }).map_err(|_| "that Signal account is not connected".to_string())?;
    let relays = match tokio::time::timeout(std::time::Duration::from_secs(30), answer).await {
        Ok(Ok(r)) => r?,
        Ok(Err(_)) => return Err("the connection ended before Signal answered".to_string()),
        Err(_) => return Err("Signal did not answer in time".to_string()),
    };
    let device = state.accounts.get_signal(account_id).map(|c| c.device_id).unwrap_or(0);
    Ok(serde_json::json!({ "ownAci": handle.own.to_string(), "deviceId": device, "iceServers": relays }))
}

/// Where Signal's group calls are held.
pub const SFU_URL: &str = "https://sfu.voip.signal.org";

/// What the app needs to join a group's call: the group's id as RingRTC knows it (the server's identifier), the
/// membership proof, every member with their id as the group encrypts it, and the call server.
pub async fn group_info(state: &AppState, buffer_id: &str) -> Result<serde_json::Value, String> {
    use base64::Engine;
    use presage::libsignal_service::groups_v2::GroupOperations;
    use presage::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};
    let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
    let buffer = state.runtime.get_buffer(buffer_id).ok_or("no such conversation")?;
    let handle = state.runtime.signal_handle(&buffer.account_id).ok_or("that Signal account is not connected")?;
    if crate::net::route::router().routed(&buffer.account_id) {
        return Err("Calls are not made on an account that goes through Tor: a call's sound and picture would leave Tor".to_string());
    }
    let Some(Thread::Group(key)) = buffer.remote_id.as_deref().and_then(super::ids::parse_remote_id) else { return Err("that is not a group".to_string()) };
    if handle.store.has_left(&key) {
        return Err("You are no longer in this group".to_string());
    }
    let group = presage::store::ContentsStore::group(&handle.store, key).await.ok().flatten().ok_or("that group is not known to this account")?;
    let operations = GroupOperations::new(GroupSecretParams::derive_from_master_key(GroupMasterKey::new(key)));
    let mut members = Vec::new();
    for m in &group.members {
        let cipher = operations.encrypt_aci(m.aci).map_err(|e| format!("a member could not be named: {e:?}"))?;
        members.push(serde_json::json!({ "userId": b64(Uuid::from(m.aci).as_bytes()), "cipher": b64(&cipher) }));
    }
    let (done, answer) = tokio::sync::oneshot::channel();
    handle.commands.send(super::Command::GroupCallProof { key, done }).map_err(|_| "that Signal account is not connected".to_string())?;
    let proof = match tokio::time::timeout(std::time::Duration::from_secs(30), answer).await {
        Ok(Ok(r)) => r?,
        Ok(Err(_)) => return Err("the connection ended before Signal answered".to_string()),
        Err(_) => return Err("Signal did not answer in time".to_string()),
    };
    let device = state.accounts.get_signal(&buffer.account_id).map(|c| c.device_id).unwrap_or(0);
    Ok(serde_json::json!({
        "accountId": buffer.account_id,
        "bufferId": buffer.id,
        "groupId": b64(&super::commands::group_identifier(&key)),
        "proof": b64(&proof),
        "members": members,
        "sfuUrl": SFU_URL,
        "ownAci": handle.own.to_string(),
        "deviceId": device,
    }))
}

/// The group a RingRTC group id (the server's identifier, base64) is, as its conversation.
pub async fn group_buffer(state: &AppState, account_id: &str, group_id: &str) -> Result<Option<String>, String> {
    use base64::Engine;
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let id = base64::engine::general_purpose::STANDARD.decode(group_id).map_err(|_| "that is not a group id".to_string())?;
    let groups = presage::store::ContentsStore::groups(&handle.store).await.map_err(|e| e.to_string())?;
    let Some(key) = groups.filter_map(Result::ok).map(|(key, _)| key).find(|key| super::commands::group_identifier(key) == id) else { return Ok(None) };
    let remote = super::ids::remote_id(&Thread::Group(key));
    Ok(state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote.as_str())).map(|b| b.id))
}

/// Sends a group call's message (RingRTC's ring, as `opaque` bytes) to the group's members, or to `recipients` when it
/// names them.
pub async fn send_to_group(state: &AppState, account_id: &str, group_id: &str, message: &serde_json::Value, recipients: &[String]) -> Result<(), String> {
    use base64::Engine;
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let id = base64::engine::general_purpose::STANDARD.decode(group_id).map_err(|_| "that is not a group id".to_string())?;
    let groups = presage::store::ContentsStore::groups(&handle.store).await.map_err(|e| e.to_string())?;
    let group = groups.filter_map(Result::ok).find(|(key, _)| super::commands::group_identifier(key) == id).map(|(_, g)| g).ok_or("that group is not known to this account")?;
    let people: Vec<Uuid> = if recipients.is_empty() {
        group.members.iter().map(|m| Uuid::from(m.aci)).filter(|u| *u != handle.own).collect()
    } else {
        recipients.iter().filter_map(|r| Uuid::parse_str(r).ok()).filter(|u| *u != handle.own).collect()
    };
    if people.is_empty() {
        return Ok(());
    }
    let call = from_wire(message)?;
    let (done, answer) = tokio::sync::oneshot::channel();
    handle.commands.send(super::Command::CallMessageToPeople { people, call: Box::new(call), done }).map_err(|_| "that Signal account is not connected".to_string())?;
    match tokio::time::timeout(std::time::Duration::from_secs(30), answer).await {
        Ok(Ok(r)) => r.map(|_| ()),
        Ok(Err(_)) => Err("the connection ended before Signal answered".to_string()),
        Err(_) => Err("Signal did not answer in time".to_string()),
    }
}

/// Sends what RingRTC asked to send to `to`. Answers once Signal took it, which is what RingRTC waits on.
pub async fn send(state: &AppState, account_id: &str, to: &str, message: &serde_json::Value) -> Result<(), String> {
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let to = Uuid::parse_str(to).map_err(|_| "a call goes to a Signal person's id".to_string())?;
    let call = from_wire(message)?;
    let (done, answer) = tokio::sync::oneshot::channel();
    handle.commands.send(super::Command::CallMessage { to, call: Box::new(call), done }).map_err(|_| "that Signal account is not connected".to_string())?;
    match tokio::time::timeout(std::time::Duration::from_secs(30), answer).await {
        Ok(Ok(r)) => r.map(|_| ()),
        Ok(Err(_)) => Err("the connection ended before Signal answered".to_string()),
        Err(_) => Err("Signal did not answer in time".to_string()),
    }
}

/// A call RingRTC took or placed here ended: said in the conversation as the phone's call log says it, under the same
/// id, and told to the account's other devices so their call logs have it too.
pub fn ended(state: &AppState, account_id: &str, remote: &str, call_id: u64, video: bool, incoming: bool, accepted: bool) -> Result<(), String> {
    use presage::proto::sync_message::call_event::{Direction, Event, Type};
    let handle = state.runtime.signal_handle(account_id).ok_or("that Signal account is not connected")?;
    let remote = Uuid::parse_str(remote).map_err(|_| "a call is with a Signal person's id".to_string())?;
    let at_ms = super::commands::now_ms();
    let kind = if video { Type::VideoCall } else { Type::AudioCall };
    let direction = if incoming { Direction::Incoming } else { Direction::Outgoing };
    let event = if accepted { Event::Accepted } else { Event::NotAccepted };
    if let Some(words) = super::inbox::call_words(kind as i32, direction as i32, event as i32) {
        let remote_id = format!("p:{remote}");
        if let Some(buffer) = state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote_id.as_str())) {
            let id = format!("call-{call_id}-{}", event as i32);
            state.runtime.record_message_at(state, account_id, &buffer.name, &buffer.kind, "Signal", &words, false, "system", None, Some(id), false, None, Vec::new(), Vec::new(), None, Some((at_ms / 1000) as i64), None, None);
        }
    }
    // Not ringing here any more either.
    stop(state, call_id, false);
    handle.commands.send(super::Command::CallEventSync { remote, call_id, at_ms, video, incoming, accepted }).map_err(|_| "that Signal account is not connected".to_string())
}

/// A call message from `sender`, as the server stamped it at `server_ms`.
pub async fn arrived(ctx: &Ctx<'_>, sender: Uuid, server_ms: u64, call: &CallMessage) {
    if let Some(offer) = &call.offer {
        if ctx.is_own(&sender) {
            return;
        }
        let Some(call_id) = offer.id else { return };
        // An offer that waited on the server while moho was away is a call already over.
        if super::commands::now_ms().saturating_sub(server_ms) > RINGS_FOR_MS {
            return;
        }
        let thread = Thread::Contact(ServiceId::Aci(presage::libsignal_service::protocol::Aci::from(sender)));
        let buffer = people::buffer_for(ctx, &thread).await;
        let video = offer.r#type == Some(offer::Type::OfferVideoCall as i32);
        let ring = Ring { account_id: ctx.account_id.to_string(), buffer_id: buffer.id.clone(), video, started_ms: server_ms };
        ringing().lock().unwrap_or_else(|e| e.into_inner()).insert(call_id, ring.clone());
        tell(ctx.state, &ring, call_id, true);
        tracing::info!("signal[{}]: a {} call is ringing", ctx.account_id, if video { "video" } else { "voice" });
        // Nobody answered in time: missed, unless something else ended it first.
        let state = ctx.state.clone();
        let wait = RINGS_FOR_MS.saturating_sub(super::commands::now_ms().saturating_sub(server_ms));
        tokio::task::spawn_local(async move {
            tokio::time::sleep(std::time::Duration::from_millis(wait)).await;
            stop(&state, call_id, true);
        });
        return;
    }
    if let Some(hangup) = &call.hangup {
        let Some(call_id) = hangup.id else { return };
        // From another of the account's devices: picked up or turned down there; its call log says which.
        if ctx.is_own(&sender) {
            stop(ctx.state, call_id, false);
            return;
        }
        // From the caller: a plain hangup is giving up before anybody answered; the others say one of the account's
        // devices picked up, turned it down or was busy, and that device's call log says which.
        let gave_up = hangup.r#type.is_none_or(|t| t == hangup::Type::HangupNormal as i32);
        stop(ctx.state, call_id, gave_up);
        return;
    }
    // The account's other device was busy: it is not ringing there. Answers and connection details are RingRTC's, for a
    // device in the call.
    if let Some(call_id) = call.busy.as_ref().and_then(|b| b.id) {
        stop(ctx.state, call_id, false);
    }
}

#[cfg(test)]
pub(super) fn is_ringing(call_id: u64) -> bool {
    ringing().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&call_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::proto::call_message::{Busy, Hangup, Offer};

    #[test]
    fn a_call_message_goes_to_ringrtc_and_back_unchanged() {
        use presage::proto::call_message::{IceUpdate, Opaque};
        let call = CallMessage {
            offer: Some(Offer { id: Some(u64::MAX - 3), r#type: Some(1), opaque: Some(vec![1, 2, 3]) }),
            ice_update: vec![IceUpdate { id: Some(9), opaque: Some(vec![4]) }, IceUpdate { id: Some(9), opaque: Some(vec![5]) }],
            hangup: Some(Hangup { id: Some(9), r#type: Some(1), device_id: Some(3) }),
            opaque: Some(Opaque { data: Some(vec![7, 7]), urgency: Some(1) }),
            destination_device_id: Some(2),
            ..Default::default()
        };
        let wire = to_wire(&call);
        assert_eq!(wire["offer"]["callId"], (u64::MAX - 3).to_string(), "64-bit ids go as text");
        assert_eq!(from_wire(&wire).unwrap(), call);
        assert!(from_wire(&serde_json::json!({})).is_err());
        assert!(from_wire(&serde_json::json!({ "busy": { "callId": "x" } })).is_err());
    }

    #[tokio::test]
    async fn a_call_rings_and_a_caller_who_gives_up_leaves_a_missed_call() {
        let local = tokio::task::LocalSet::new();
        local
            .run_until(async {
                let (state, store) = super::super::inbox::tests::world_for_calls("calls").await;
                let me = Uuid::parse_str(super::super::inbox::tests::ME_FOR_CALLS).unwrap();
                let ctx = Ctx::new(&state, super::super::inbox::tests::ACCOUNT_FOR_CALLS, store, me);
                let alice = Uuid::from_u128(2);
                let mut events = state.events.subscribe();
                let now = super::super::commands::now_ms();
                let offer = CallMessage { offer: Some(Offer { id: Some(77), r#type: Some(offer::Type::OfferVideoCall as i32), ..Default::default() }), ..Default::default() };
                arrived(&ctx, alice, now, &offer).await;
                assert!(is_ringing(77));
                let rang = std::iter::from_fn(|| events.try_recv().ok()).find(|e| e.name == "incomingCall").expect("a ring");
                assert_eq!((rang.data["ringing"].as_bool(), rang.data["video"].as_bool()), (Some(true), Some(true)));
                // The caller hangs up: no longer ringing, and said as missed.
                let hangup = CallMessage { hangup: Some(Hangup { id: Some(77), r#type: Some(hangup::Type::HangupNormal as i32), ..Default::default() }), ..Default::default() };
                arrived(&ctx, alice, now, &hangup).await;
                assert!(!is_ringing(77));
                let buffer = state.runtime.list_buffers().into_iter().find(|b| b.account_id == ctx.account_id).unwrap();
                let rows = state.store.get_backlog(&buffer.id, i64::MAX, 10).unwrap();
                assert!(rows.iter().any(|m| m.body.starts_with("Missed video call")), "{:?}", rows.iter().map(|m| &m.body).collect::<Vec<_>>());
                // An offer from long ago does not ring; one taken on another device stops without a missed line.
                arrived(&ctx, alice, now - RINGS_FOR_MS - 1, &CallMessage { offer: Some(Offer { id: Some(78), ..Default::default() }), ..Default::default() }).await;
                assert!(!is_ringing(78));
                arrived(&ctx, alice, now, &CallMessage { offer: Some(Offer { id: Some(79), ..Default::default() }), ..Default::default() }).await;
                arrived(&ctx, me, now, &CallMessage { hangup: Some(Hangup { id: Some(79), r#type: Some(hangup::Type::HangupAccepted as i32), ..Default::default() }), ..Default::default() }).await;
                assert!(!is_ringing(79));
                // Turned down on another device (the caller says so): stops, and is not missed.
                arrived(&ctx, alice, now, &CallMessage { offer: Some(Offer { id: Some(81), ..Default::default() }), ..Default::default() }).await;
                let lines = state.store.get_backlog(&buffer.id, i64::MAX, 50).unwrap().len();
                arrived(&ctx, alice, now, &CallMessage { hangup: Some(Hangup { id: Some(81), r#type: Some(hangup::Type::HangupDeclined as i32), device_id: Some(2) }), ..Default::default() }).await;
                assert!(!is_ringing(81));
                assert_eq!(state.store.get_backlog(&buffer.id, i64::MAX, 50).unwrap().len(), lines);
                arrived(&ctx, alice, now, &CallMessage { offer: Some(Offer { id: Some(80), ..Default::default() }), ..Default::default() }).await;
                assert!(silence(&state, &buffer.id));
                assert!(!is_ringing(80));
                let _ = CallMessage { busy: Some(Busy { id: Some(1) }), ..Default::default() };
            })
            .await;
    }
}
