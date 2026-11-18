//! Voice chats: a group's or channel's call, joined from the window.
//!
//! Telegram's voice chats are WebRTC with Telegram's own server as the only peer, as Steam's are with Valve's. The
//! window holds the media (`lib/telegramvoice.ts`): it makes an offer, and what Telegram wants from it (ICE ufrag and
//! pwd, the DTLS fingerprint, the audio source) goes up as `joinGroupCall`'s JSON; Telegram answers with the server's
//! transport as `updateGroupCallConnection`, from which the window builds the answer. Who else is in it, and their
//! audio sources, come as `updateGroupCallParticipants`, from which the window builds the next offer.
//!
//! The JSON shapes follow Telegram Web A (Ajaxy/telegram-tt, GPL-3.0, `src/lib/vibecalls`), which talks to the same
//! server from a browser.
//!
//! What goes up to the window is one event, `telegramVoice`, with a `kind`: `call` (the call's state), `connection`
//! (the server's transport), `participants` (who is in it).

use super::send::{explain, peer_for};
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::tl;
use grammers_session::types::PeerKind;
use grammers_session::Session;
use serde_json::{json, Value};
use std::collections::HashMap;

/// The call a group or channel has going, if any.
async fn call_of(handle: &Handle, peer: grammers_session::types::PeerRef) -> Result<Option<tl::enums::InputGroupCall>, String> {
    let full = match peer.id.kind() {
        PeerKind::Channel => handle.client.invoke(&tl::functions::channels::GetFullChannel { channel: peer.into() }).await,
        PeerKind::Chat => handle.client.invoke(&tl::functions::messages::GetFullChat { chat_id: peer.into() }).await,
        PeerKind::User => return Err("a voice chat belongs to a group or a channel".to_string()),
    }
    .map_err(|e| explain(&e))?;
    let tl::enums::messages::ChatFull::Full(full) = full;
    Ok(match full.full_chat {
        tl::enums::ChatFull::Full(c) => c.call,
        tl::enums::ChatFull::ChannelFull(c) => c.call,
        tl::enums::ChatFull::CommunityFull(_) => None,
    })
}

/// A call as text the window can hand back: `id:access_hash`.
fn call_key(call: &tl::enums::InputGroupCall) -> Option<String> {
    match call {
        tl::enums::InputGroupCall::Call(c) => Some(format!("{}:{}", c.id, c.access_hash)),
        _ => None,
    }
}

fn input_call(key: &str) -> Result<tl::enums::InputGroupCall, String> {
    let (id, hash) = key.split_once(':').ok_or("that is not a voice chat")?;
    Ok(tl::types::InputGroupCall { id: id.parse().map_err(|_| "that is not a voice chat")?, access_hash: hash.parse().map_err(|_| "that is not a voice chat")? }.into())
}

/// A person's name from Telegram's list of users.
fn names_of(users: &[tl::enums::User], chats: &[tl::enums::Chat]) -> HashMap<i64, String> {
    let mut out: HashMap<i64, String> = HashMap::new();
    for u in users {
        if let tl::enums::User::User(u) = u {
            let name = [u.first_name.as_deref(), u.last_name.as_deref()].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
            out.insert(u.id, if name.is_empty() { u.username.clone().unwrap_or_default() } else { name });
        }
    }
    for c in chats {
        match c {
            tl::enums::Chat::Chat(c) => {
                out.insert(c.id, c.title.clone());
            }
            tl::enums::Chat::Channel(c) => {
                out.insert(c.id, c.title.clone());
            }
            _ => {}
        }
    }
    out
}

fn peer_number(peer: &tl::enums::Peer) -> i64 {
    match peer {
        tl::enums::Peer::User(p) => p.user_id,
        tl::enums::Peer::Chat(p) => p.chat_id,
        tl::enums::Peer::Channel(p) => p.channel_id,
    }
}

/// One person in a call, as the window reads them. `source` is the audio source Telegram knows them by (signed, as
/// Telegram sends it; the window turns it into the unsigned SSRC WebRTC uses).
pub fn participant(p: &tl::enums::GroupCallParticipant, names: &HashMap<i64, String>) -> Value {
    let tl::enums::GroupCallParticipant::Participant(p) = p;
    let id = peer_number(&p.peer);
    json!({
        "id": id.to_string(),
        "name": names.get(&id).cloned().unwrap_or_default(),
        "self": p.is_self,
        "muted": p.muted,
        "canSelfUnmute": p.can_self_unmute,
        "mutedByYou": p.muted_by_you,
        "left": p.left,
        "source": p.source,
        "volume": p.volume,
        "activeDate": p.active_date,
        "raisedHand": p.raise_hand_rating.is_some(),
        "hasVideo": p.video.is_some(),
    })
}

/// What the call is like now, for the window: whether there is one, what it is called, how many are in it, who.
pub async fn info(state: &AppState, buffer: &Buffer) -> Result<Value, String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    let Some(call) = call_of(&handle, peer).await? else { return Ok(json!({ "active": false })) };
    let got = handle.client.invoke(&tl::functions::phone::GetGroupCall { call: call.clone(), limit: 100 }).await.map_err(|e| explain(&e))?;
    let tl::enums::phone::GroupCall::Call(got) = got;
    let names = names_of(&got.users, &got.chats);
    let (title, count) = match &got.call {
        tl::enums::GroupCall::Call(c) => (c.title.clone().unwrap_or_default(), c.participants_count),
        tl::enums::GroupCall::Discarded(_) => return Ok(json!({ "active": false })),
    };
    Ok(json!({
        "active": true,
        "call": call_key(&call),
        "title": title,
        "count": count,
        "participants": got.participants.iter().map(|p| participant(p, &names)).collect::<Vec<_>>(),
    }))
}

/// Starts a voice chat in a group or channel (an administrator's right).
pub async fn start(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    handle
        .client
        .invoke(&tl::functions::phone::CreateGroupCall { rtmp_stream: false, peer: peer.into(), random_id: rand::random(), title: None, schedule_date: None })
        .await
        .map(drop)
        .map_err(|e| match &e {
            grammers_client::InvocationError::Rpc(r) if r.name == "GROUPCALL_ALREADY_STARTED" => "There is a voice chat going already".to_string(),
            _ => explain(&e),
        })
}

/// Joins with what the window's offer says (its ICE credentials, fingerprint and sources, as Telegram's JSON).
/// Telegram's answer, the server's transport, comes back here when it is in the reply, and as an event otherwise.
pub async fn join(state: &AppState, buffer: &Buffer, call: &str, params: &str, muted: bool) -> Result<Value, String> {
    let (handle, _) = peer_for(state, buffer).await?;
    // Only well-formed JSON goes to Telegram.
    serde_json::from_str::<Value>(params).map_err(|_| "the window's description of the call is not JSON".to_string())?;
    let updates = handle
        .client
        .invoke(&tl::functions::phone::JoinGroupCall {
            muted,
            video_stopped: true,
            call: input_call(call)?,
            join_as: tl::enums::InputPeer::PeerSelf,
            invite_hash: None,
            public_key: None,
            block: None,
            params: tl::types::DataJson { data: params.to_string() }.into(),
        })
        .await
        .map_err(|e| match &e {
            grammers_client::InvocationError::Rpc(r) if r.name == "GROUPCALL_FORBIDDEN" => "That voice chat has ended".to_string(),
            grammers_client::InvocationError::Rpc(r) if r.name == "GROUPCALL_JOIN_MISSING" => "Telegram asks for this voice chat to be joined again".to_string(),
            _ => explain(&e),
        })?;
    let list: Vec<tl::enums::Update> = match updates {
        tl::enums::Updates::Updates(u) => u.updates,
        tl::enums::Updates::Combined(u) => u.updates,
        tl::enums::Updates::UpdateShort(u) => vec![u.update],
        _ => Vec::new(),
    };
    let connection = list.iter().find_map(|u| match u {
        tl::enums::Update::GroupCallConnection(c) if !c.presentation => {
            let tl::enums::DataJson::Json(d) = &c.params;
            Some(d.data.clone())
        }
        _ => None,
    });
    Ok(json!({ "connection": connection }))
}

/// Leaves, naming the audio source this side joined with.
pub async fn leave(state: &AppState, buffer: &Buffer, call: &str, source: i32) -> Result<(), String> {
    let (handle, _) = peer_for(state, buffer).await?;
    handle.client.invoke(&tl::functions::phone::LeaveGroupCall { call: input_call(call)?, source }).await.map(drop).map_err(|e| explain(&e))
}

/// Mutes or unmutes this account (or, for an administrator, somebody else: `user_id`).
pub async fn mute(state: &AppState, buffer: &Buffer, call: &str, muted: bool, user_id: Option<&str>) -> Result<(), String> {
    let (handle, _) = peer_for(state, buffer).await?;
    let participant = match user_id.and_then(|u| u.parse::<i64>().ok()) {
        Some(id) => handle
            .session
            .peer_ref(grammers_session::types::PeerId::user_unchecked(id))
            .await
            .ok()
            .flatten()
            .map(Into::into)
            .ok_or("Telegram has not told this account about that person yet")?,
        None => tl::enums::InputPeer::PeerSelf,
    };
    handle
        .client
        .invoke(&tl::functions::phone::EditGroupCallParticipant {
            call: input_call(call)?,
            participant,
            muted: Some(muted),
            volume: None,
            raise_hand: None,
            video_stopped: None,
            video_paused: None,
            presentation_paused: None,
        })
        .await
        .map(drop)
        .map_err(|e| explain(&e))
}

/// Ends the voice chat for everybody (an administrator's right).
pub async fn end(state: &AppState, buffer: &Buffer, call: &str) -> Result<(), String> {
    let (handle, _) = peer_for(state, buffer).await?;
    handle.client.invoke(&tl::functions::phone::DiscardGroupCall { call: input_call(call)? }).await.map(drop).map_err(|e| explain(&e))
}

/// Who is in it, asked of Telegram (the window asks after joining, and when a source it does not know speaks).
pub async fn participants(state: &AppState, buffer: &Buffer, call: &str) -> Result<Value, String> {
    let (handle, _) = peer_for(state, buffer).await?;
    let got = handle
        .client
        .invoke(&tl::functions::phone::GetGroupParticipants { call: input_call(call)?, ids: Vec::new(), sources: Vec::new(), offset: String::new(), limit: 200 })
        .await
        .map_err(|e| explain(&e))?;
    let tl::enums::phone::GroupParticipants::Participants(got) = got;
    let names = names_of(&got.users, &got.chats);
    Ok(json!({ "participants": got.participants.iter().map(|p| participant(p, &names)).collect::<Vec<_>>(), "count": got.count }))
}

/// Telegram's updates about calls, passed up as they come.
pub fn update(state: &AppState, account_id: &str, update: &tl::enums::Update) {
    use tl::enums::Update as U;
    match update {
        U::GroupCallConnection(c) => {
            let tl::enums::DataJson::Json(d) = &c.params;
            state.events.emit("telegramVoice", json!({ "accountId": account_id, "kind": "connection", "data": { "presentation": c.presentation, "params": d.data } }));
        }
        U::GroupCallParticipants(u) => {
            // Names are not in the update; the window has them from the list it read, and asks again for strangers.
            let names = HashMap::new();
            state.events.emit(
                "telegramVoice",
                json!({ "accountId": account_id, "kind": "participants", "data": { "call": call_key(&u.call), "version": u.version, "participants": u.participants.iter().map(|p| participant(p, &names)).collect::<Vec<_>>() } }),
            );
        }
        U::GroupCall(u) => {
            let (key, active, count, title) = match &u.call {
                tl::enums::GroupCall::Call(c) => (format!("{}:{}", c.id, c.access_hash), true, c.participants_count, c.title.clone().unwrap_or_default()),
                tl::enums::GroupCall::Discarded(c) => (format!("{}:{}", c.id, c.access_hash), false, 0, String::new()),
            };
            let remote = u.peer.as_ref().map(|p| super::peers::remote_id(grammers_session::types::PeerId::from(p.clone())));
            let buffer = remote.and_then(|r| state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(r.as_str())));
            state.events.emit(
                "telegramVoice",
                json!({ "accountId": account_id, "kind": "call", "data": { "call": key, "active": active, "count": count, "title": title, "bufferId": buffer.map(|b| b.id) } }),
            );
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_call_goes_to_the_window_and_comes_back_the_same() {
        let call: tl::enums::InputGroupCall = tl::types::InputGroupCall { id: 12, access_hash: -34 }.into();
        let key = call_key(&call).unwrap();
        assert_eq!(key, "12:-34");
        assert_eq!(call_key(&input_call(&key).unwrap()), Some(key));
        assert!(input_call("nonsense").is_err());
    }
}
