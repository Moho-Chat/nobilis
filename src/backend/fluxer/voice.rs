//! Calls: a guild's voice channels, and the calls in direct messages and groups (`voice/index.md`).
//!
//! Fluxer holds calls on LiveKit. The daemon asks for a place in one with a Voice State Update on the gateway (op 4) and
//! Fluxer answers this session alone with `VOICE_SERVER_UPDATE`: the LiveKit address, a token, the id of this voice
//! connection, and - where the channel's media is encrypted - a room key the server made. The window, which is a LiveKit
//! client already, joins with those. Leaving is the same command with no channel and this connection's id.
//!
//! Who is in each call comes as `VOICE_STATE_UPDATE` (and in `READY`'s guilds, and in a call's `CALL_CREATE`), so a
//! channel can say who is talking before anybody joins. A call starting in a direct message rings the others with
//! `POST /channels/{id}/call/ring`; one somebody else starts and rings this account for is said to the window as ringing.
//!
//! Peer-to-peer calls (small DM calls without a media server) are not joined: moho joins with `p2p` unset, which Fluxer
//! takes as a LiveKit join wherever the call is not already peer-to-peer, and a refusal is said in words.

use super::messages;
use super::media;
use super::Handle;
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

/// Somebody in a call.
#[derive(Debug, Clone, Default)]
pub struct VoiceMember {
    pub user: String,
    pub connection: String,
    pub muted: bool,
    pub camera: bool,
    pub screen: bool,
}

pub fn parse_state(v: &Value) -> Option<(Option<String>, VoiceMember)> {
    let user = v["user_id"].as_str()?.to_string();
    Some((
        v["channel_id"].as_str().map(str::to_string),
        VoiceMember {
            user,
            connection: v["connection_id"].as_str().unwrap_or("").to_string(),
            muted: v["self_mute"].as_bool().unwrap_or(false) || v["mute"].as_bool().unwrap_or(false) || v["suppress"].as_bool().unwrap_or(false),
            camera: v["self_video"].as_bool().unwrap_or(false),
            screen: v["self_stream"].as_bool().unwrap_or(false),
        },
    ))
}

/// Keeps one voice state: the person (by connection) is taken out of every call, and put in the one named. Answers the
/// channels touched.
pub fn keep_state(handle: &Handle, v: &Value) -> Vec<String> {
    let Some((channel, member)) = parse_state(v) else { return Vec::new() };
    let mut voice = handle.voice.lock().unwrap_or_else(|e| e.into_inner());
    let mut touched: Vec<String> = Vec::new();
    for (id, list) in voice.iter_mut() {
        let before = list.len();
        list.retain(|m| !(m.user == member.user && (m.connection == member.connection || member.connection.is_empty() || m.connection.is_empty())));
        if list.len() != before {
            touched.push(id.clone());
        }
    }
    if let Some(channel) = channel {
        voice.entry(channel.clone()).or_default().push(member);
        if !touched.contains(&channel) {
            touched.push(channel);
        }
    }
    voice.retain(|_, l| !l.is_empty());
    touched
}

/// The people in a channel's call, in the words the window draws (the same as Stoat's).
pub fn voice_members(handle: &Handle, channel: &str) -> Vec<Value> {
    let list = handle.voice.lock().unwrap_or_else(|e| e.into_inner()).get(channel).cloned().unwrap_or_default();
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let guild = world.channels.get(channel).and_then(|c| c.guild.clone());
    list.iter()
        .map(|m| {
            let avatar = world.users.get(&m.user).and_then(|u| u.avatar.clone()).and_then(|h| media::asset_url(&handle.instance, "avatars", &m.user, &h)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp")));
            json!({ "userId": m.user, "name": world.name_in(guild.as_deref(), &m.user), "avatarUrl": avatar, "camera": m.camera, "screen": m.screen, "muted": m.muted, "isSelf": m.user == world.own_id })
        })
        .collect()
}

/// Says who is in a channel's call (the window's `stoatVoice` event, which it draws for both services).
pub fn tell(state: &AppState, handle: &Handle, channel: &str) {
    let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) else { return };
    state.events.emit("stoatVoice", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "members": voice_members(handle, channel) }));
}

/// A voice state from the gateway, kept and said.
pub fn state_changed(state: &AppState, handle: &Handle, v: &Value) {
    for channel in keep_state(handle, v) {
        tell(state, handle, &channel);
    }
}

/// A call in a direct message or a group: who is in it, and whether it is ringing this account.
pub fn call_changed(state: &AppState, handle: &Handle, kind: &str, v: &Value) {
    let Some(channel) = v["channel_id"].as_str() else { return };
    let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
    if kind == "CALL_DELETE" {
        handle.voice.lock().unwrap_or_else(|e| e.into_inner()).remove(channel);
    } else {
        handle.voice.lock().unwrap_or_else(|e| e.into_inner()).remove(channel);
        for s in v["voice_states"].as_array().into_iter().flatten() {
            keep_state(handle, s);
        }
    }
    tell(state, handle, channel);
    let ringing = kind != "CALL_DELETE" && v["ringing"].as_array().is_some_and(|r| r.iter().any(|u| u.as_str() == Some(own.as_str())));
    let was = handle.ringing.lock().unwrap_or_else(|e| e.into_inner()).contains(channel);
    if ringing != was {
        if ringing {
            handle.ringing.lock().unwrap_or_else(|e| e.into_inner()).insert(channel.to_string());
        } else {
            handle.ringing.lock().unwrap_or_else(|e| e.into_inner()).remove(channel);
        }
        if let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) {
            state.events.emit("incomingCall", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "channelId": channel, "ringing": ringing }));
        }
    }
}

/// The conversations ringing now, for a window that opens in the middle of it.
pub fn ringing(state: &AppState, handle: &Handle) -> Vec<Value> {
    let channels: Vec<String> = handle.ringing.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect();
    channels.into_iter().filter_map(|c| messages::buffer_of(state, &handle.account_id, &c).map(|b| json!({ "accountId": handle.account_id, "bufferId": b.id, "channelId": c, "ringing": true }))).collect()
}

/// A grant the gateway sent: handed to whoever is waiting for one.
pub fn grant(handle: &Handle, v: &Value) {
    if let Some(sender) = handle.voice_wait.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = sender.send(Ok(v.clone()));
    }
}

/// A refusal the gateway sent (op 12): handed to whoever is waiting, in words.
pub fn refused(handle: &Handle, v: &Value) -> bool {
    let words = match v["code"].as_str().unwrap_or("") {
        "VOICE_CHANNEL_FULL" => "That call is full".to_string(),
        "VOICE_E2EE_REQUIRED" => "This call requires encrypted call media".to_string(),
        "VOICE_P2P_CONSENT_REQUIRED" => "That call is peer-to-peer (the people in it connect to each other directly), which moho does not join. Join it from Fluxer's own app".to_string(),
        "VOICE_P2P_UNAVAILABLE" => "Fluxer could not make that call peer-to-peer".to_string(),
        other => v["message"].as_str().map(str::to_string).unwrap_or_else(|| format!("Fluxer refused the call ({other})")),
    };
    match handle.voice_wait.lock().unwrap_or_else(|e| e.into_inner()).take() {
        Some(sender) => {
            let _ = sender.send(Err(words));
            true
        }
        None => false,
    }
}

fn place_of(handle: &Handle, buffer: &Buffer) -> Result<(String, Option<String>, bool)> {
    let channel = buffer.remote_id.clone().ok_or_else(|| anyhow!("this is not a Fluxer conversation"))?;
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let c = world.channels.get(&channel).ok_or_else(|| anyhow!("Fluxer has no such channel"))?;
    let private = c.guild.is_none();
    if !private && c.kind != super::world::Kind::Voice {
        bail!("Only a voice channel has a call");
    }
    if !private && world.permissions(c) & super::world::CONNECT == 0 {
        bail!("You do not have permission to join this call");
    }
    Ok((channel.clone(), c.guild.clone(), private))
}

/// Asks for a place in a call and answers what the window joins with: the LiveKit address, the token, and the room
/// key where there is one. Starting a call in a direct message or a group rings the others. It joins muted or deafened
/// as the voice buttons stand, so the others never see a microphone that is not open.
pub async fn join(handle: &Handle, buffer: &Buffer, video: bool, muted: bool, deafened: bool) -> Result<Value> {
    if let Some(why) = crate::net::route::router().call_refusal(&handle.account_id) {
        bail!("{why}");
    }
    if !handle.instance.voice {
        bail!("This instance has calls turned off");
    }
    let (channel, guild, private) = place_of(handle, buffer)?;
    let (tx, rx) = tokio::sync::oneshot::channel();
    *handle.voice_wait.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    let started_empty = !handle.voice.lock().unwrap_or_else(|e| e.into_inner()).contains_key(&channel);
    let command = json!({ "op": 4, "d": { "guild_id": guild, "channel_id": channel, "self_mute": muted || deafened, "self_deaf": deafened, "self_video": video, "self_stream": false } });
    *handle.voice_media.lock().unwrap_or_else(|e| e.into_inner()) = (video, false);
    handle.gateway.send(command.to_string()).map_err(|_| anyhow!("the connection to Fluxer is not up"))?;
    let answer = tokio::time::timeout(Duration::from_secs(20), rx).await.map_err(|_| anyhow!("Fluxer did not answer with a way into the call"))?.map_err(|_| anyhow!("the connection to Fluxer went away"))?;
    let grant = answer.map_err(|words| anyhow!(words))?;
    if grant["p2p"].as_bool() == Some(true) {
        let connection = grant["connection_id"].as_str().unwrap_or("").to_string();
        let _ = handle.gateway.send(json!({ "op": 4, "d": { "guild_id": guild, "channel_id": null, "connection_id": connection } }).to_string());
        bail!("That call is peer-to-peer (the people in it connect to each other directly), which moho does not join. Join it from Fluxer's own app");
    }
    let (Some(token), Some(endpoint)) = (grant["token"].as_str(), grant["endpoint"].as_str()) else { bail!("Fluxer did not give a way into the call") };
    *handle.voice_here.lock().unwrap_or_else(|e| e.into_inner()) = Some((guild.clone(), channel.clone(), grant["connection_id"].as_str().unwrap_or("").to_string()));
    if private {
        handle.ringing.lock().unwrap_or_else(|e| e.into_inner()).remove(&channel);
        if started_empty {
            let _ = handle.api.post(&format!("/channels/{channel}/call/ring"), &json!({})).await;
        }
    }
    Ok(json!({ "token": token, "url": endpoint, "e2eeKey": grant["e2ee_key"] }))
}

/// Leaves the call this account is in from here.
pub fn leave(handle: &Handle) {
    if let Some((guild, _, connection)) = handle.voice_here.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = handle.gateway.send(json!({ "op": 4, "d": { "guild_id": guild, "channel_id": null, "connection_id": connection } }).to_string());
    }
}

/// Says the camera and shared screen as they are now, with the microphone and hearing as the voice buttons stand, so
/// others see them.
pub fn set_state(handle: &Handle, muted: bool, deafened: bool, camera: bool, screen: bool) {
    *handle.voice_media.lock().unwrap_or_else(|e| e.into_inner()) = (camera, screen);
    say_state(handle, muted, deafened);
}

/// Says a change of the voice buttons in the call this account is in, if it is in one; camera and screen stay as
/// they were last said.
pub fn say_state(handle: &Handle, muted: bool, deafened: bool) {
    let (camera, screen) = *handle.voice_media.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((guild, channel, connection)) = handle.voice_here.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        let _ = handle.gateway.send(json!({ "op": 4, "d": { "guild_id": guild, "channel_id": channel, "connection_id": connection, "self_mute": muted || deafened, "self_deaf": deafened, "self_video": camera, "self_stream": screen } }).to_string());
    }
}

/// The picture Fluxer keeps of somebody's shared screen, as a data URL; `None` where they share none, or the sharer's
/// app has not sent one yet. A stream is named by where it is and by the sharer's voice connection
/// (`{guild}:{channel}:{connection}`, `dm` in place of a guild), as Fluxer's own app names it.
pub async fn stream_preview(handle: &Handle, buffer: &Buffer, user: &str) -> Result<Option<String>> {
    use base64::Engine;
    let (channel, guild, _) = place_of(handle, buffer)?;
    let connection = handle.voice.lock().unwrap_or_else(|e| e.into_inner()).get(&channel).and_then(|list| list.iter().find(|m| m.user == user && m.screen).map(|m| m.connection.clone()));
    let Some(connection) = connection.filter(|c| !c.is_empty()) else { return Ok(None) };
    let key = stream_key(guild.as_deref(), &channel, &connection);
    let Some((content_type, bytes)) = handle.api.get_bytes(&format!("/streams/{key}/preview")).await.map_err(|e| anyhow!("{e}"))? else { return Ok(None) };
    if bytes.is_empty() {
        return Ok(None);
    }
    Ok(Some(format!("data:{content_type};base64,{}", base64::engine::general_purpose::STANDARD.encode(bytes))))
}

fn stream_key(guild: Option<&str>, channel: &str, connection: &str) -> String {
    format!("{}:{channel}:{connection}", guild.unwrap_or("dm"))
}

/// Stops a call ringing this account.
pub async fn decline(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<()> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Fluxer conversation"))?;
    let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
    handle.ringing.lock().unwrap_or_else(|e| e.into_inner()).remove(channel);
    state.events.emit("incomingCall", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "channelId": channel, "ringing": false }));
    let _ = handle.api.post(&format!("/channels/{channel}/call/stop-ringing"), &json!({ "recipients": [own] })).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_voice_state_says_who_and_where_and_what_they_send() {
        let (channel, m) = parse_state(&json!({ "user_id": "5", "channel_id": "9", "connection_id": "c1", "self_mute": true, "self_video": true, "self_stream": false })).unwrap();
        assert_eq!(channel.as_deref(), Some("9"));
        assert_eq!((m.user.as_str(), m.connection.as_str(), m.muted, m.camera, m.screen), ("5", "c1", true, true, false));
        let (gone, _) = parse_state(&json!({ "user_id": "5", "channel_id": null, "connection_id": "c1" })).unwrap();
        assert!(gone.is_none());
        assert!(parse_state(&json!({ "channel_id": "9" })).is_none());
    }

    #[test]
    fn a_stream_is_named_as_fluxer_names_it() {
        assert_eq!(stream_key(Some("1"), "2", "c3"), "1:2:c3");
        assert_eq!(stream_key(None, "2", "c3"), "dm:2:c3");
    }
}
