//! The Calls plugin (`com.mattermost.calls`): who is in a call in each channel, and the window's call joined through the
//! plugin's own signalling.
//!
//! The media is the window's: a call is WebRTC against the plugin's media server (rtcd), and the window is a browser, so
//! what crosses this process is signalling only. A call has a websocket of its own (as Mattermost's app opens one), on
//! which this client says `join` with the channel; the plugin acknowledges it (`join`), then the window's peer connection
//! offers, and offers, answers and network candidates go back and forth (`signal` from the server, `sdp` and `ice` to it).
//! An `sdp` goes as the plugin reads it: the description as JSON, zlib-compressed, as the binary `data` of a msgpack frame.
//! Mute, screen sharing, a raised hand and a reaction are said on the same websocket; the plugin tells everybody in the
//! channel, on their ordinary websockets, and that is what fills in who is in a call here (`call_start`, `user_joined`,
//! `user_muted`...).
//!
//! What a server allows is its licence's: an unlicensed one holds calls in direct messages only, with no video and no
//! host controls, and refuses the rest in its own words.

use super::api::ApiError;
use super::{media, messages, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as Ws;

const PLUGIN: &str = "com.mattermost.calls";
const PREFIX: &str = "custom_com.mattermost.calls_";

/// Somebody in a call: their session (a call can have the same person twice, from two apps) and what they are doing.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Session {
    pub user: String,
    pub unmuted: bool,
    pub raised_hand: bool,
    pub screen: bool,
}

/// A call going on in a channel.
#[derive(Debug, Clone, Default)]
pub struct Call {
    pub id: String,
    pub host: String,
    pub sessions: HashMap<String, Session>,
}

/// What the window asks to say on a call's websocket.
pub enum Say {
    Sdp(String),
    Ice(String),
    Action(&'static str, Value),
    Leave,
}

/// The calls this process is in, by buffer: where to send what the window says.
fn joined() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<Say>>> {
    static J: OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<Say>>>> = OnceLock::new();
    J.get_or_init(Default::default)
}

/// The calls going on, by account and channel.
fn calls() -> &'static Mutex<HashMap<String, HashMap<String, Call>>> {
    static C: OnceLock<Mutex<HashMap<String, HashMap<String, Call>>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The channels ringing for an account: a call in a direct or group message this account is not in.
fn ringing_set() -> &'static Mutex<HashMap<String, std::collections::HashSet<String>>> {
    static R: OnceLock<Mutex<HashMap<String, std::collections::HashSet<String>>>> = OnceLock::new();
    R.get_or_init(Default::default)
}

pub fn parse_call(v: &Value) -> Option<Call> {
    let id = v["id"].as_str()?.to_string();
    let screen = v["screen_sharing_session_id"].as_str().unwrap_or("");
    let sessions = v["sessions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some((s["session_id"].as_str()?.to_string(), Session { user: s["user_id"].as_str()?.to_string(), unmuted: s["unmuted"].as_bool().unwrap_or(false), raised_hand: s["raised_hand"].as_i64().unwrap_or(0) > 0, screen: s["session_id"].as_str() == Some(screen) })))
        .collect();
    Some(Call { id, host: v["host_id"].as_str().unwrap_or("").to_string(), sessions })
}

/// Who is in a call going on, and whether this account rings for it, read when the account connects.
pub async fn load(state: &AppState, handle: &Handle) {
    let Ok(list) = handle.api.get(&format!("/plugins/{PLUGIN}/channels"), &[]).await else { return };
    let mut found = HashMap::new();
    for c in list.as_array().into_iter().flatten() {
        if let (Some(channel), Some(call)) = (c["channel_id"].as_str(), parse_call(&c["call"])) {
            found.insert(channel.to_string(), call);
        }
    }
    let channels: Vec<String> = found.keys().cloned().collect();
    calls().lock().unwrap_or_else(|e| e.into_inner()).insert(handle.account_id.clone(), found);
    for channel in channels {
        tell(state, handle, &channel);
        ring_if_wanted(state, handle, &channel);
    }
}

/// The people in a channel's call, in the words the window draws (the same as Stoat's).
pub fn members(handle: &Handle, channel: &str) -> Vec<Value> {
    let call = calls().lock().unwrap_or_else(|e| e.into_inner()).get(&handle.account_id).and_then(|c| c.get(channel).cloned());
    let Some(call) = call else { return Vec::new() };
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<Value> = call
        .sessions
        .iter()
        .map(|(session, s)| {
            let picture = world.users.get(&s.user).map(|u| u.picture).unwrap_or(0);
            json!({
                "userId": s.user,
                "sessionId": session,
                "name": world.name_of(&s.user, &handle.server.teammate_name),
                "avatarUrl": media::avatar(&handle.account_id, &s.user, picture),
                "camera": false,
                "screen": s.screen,
                "muted": !s.unmuted,
                "raisedHand": s.raised_hand,
                "host": s.user == call.host,
                "isSelf": s.user == world.own_id,
            })
        })
        .collect();
    out.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    out
}

/// Tells the window who is in a channel's call.
fn tell(state: &AppState, handle: &Handle, channel: &str) {
    let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) else { return };
    state.events.emit("stoatVoice", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "members": members(handle, channel) }));
}

/// Every conversation of an account with a call in it, and who is there, for a window that opens in the middle.
pub fn all_members(state: &AppState, handle: &Handle) -> Vec<(String, Value)> {
    let channels: Vec<String> = calls().lock().unwrap_or_else(|e| e.into_inner()).get(&handle.account_id).map(|c| c.keys().cloned().collect()).unwrap_or_default();
    channels.into_iter().filter_map(|c| messages::buffer_of(state, &handle.account_id, &c).map(|b| (b.id, Value::Array(members(handle, &c))))).collect()
}

/// A call in a direct or group message with somebody else in it and not this account rings; it stops when the account
/// joins (from anywhere) or the call ends.
fn ring_if_wanted(state: &AppState, handle: &Handle, channel: &str) {
    let own = handle.own_id();
    let personal = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel).is_some_and(|c| c.is_personal());
    let call = calls().lock().unwrap_or_else(|e| e.into_inner()).get(&handle.account_id).and_then(|c| c.get(channel).cloned());
    let should = personal && call.as_ref().is_some_and(|c| !c.sessions.is_empty() && !c.sessions.values().any(|s| s.user == own)) && !joined().lock().unwrap_or_else(|e| e.into_inner()).keys().any(|b| messages::buffer_of(state, &handle.account_id, channel).is_some_and(|x| x.id == *b));
    let was = {
        let mut set = ringing_set().lock().unwrap_or_else(|e| e.into_inner());
        let mine = set.entry(handle.account_id.clone()).or_default();
        if should { !mine.insert(channel.to_string()) } else { !mine.remove(channel) }
    };
    // Said when it changes: `was` is whether it already was what it now is.
    if !was {
        if let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) {
            state.events.emit("incomingCall", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "channelId": channel, "ringing": should }));
        }
    }
}

/// The conversations ringing now, for a window that opens in the middle of it.
pub fn ringing(state: &AppState, handle: &Handle) -> Vec<Value> {
    let channels: Vec<String> = ringing_set().lock().unwrap_or_else(|e| e.into_inner()).get(&handle.account_id).map(|s| s.iter().cloned().collect()).unwrap_or_default();
    channels.into_iter().filter_map(|c| messages::buffer_of(state, &handle.account_id, &c).map(|b| json!({ "accountId": handle.account_id, "bufferId": b.id, "channelId": c, "ringing": true }))).collect()
}

/// One of the plugin's events, on the account's ordinary websocket.
pub fn event(state: &AppState, handle: &Handle, kind: &str, data: &Value, broadcast: &Value) {
    let Some(what) = kind.strip_prefix(PREFIX) else { return };
    let channel = data["channelID"].as_str().or(data["channel_id"].as_str()).filter(|c| !c.is_empty()).or(broadcast["channel_id"].as_str()).unwrap_or("").to_string();
    if channel.is_empty() {
        return;
    }
    let session = data["session_id"].as_str().unwrap_or("").to_string();
    let user = data["user_id"].as_str().or(data["userID"].as_str()).unwrap_or("").to_string();
    {
        let mut all = calls().lock().unwrap_or_else(|e| e.into_inner());
        let account = all.entry(handle.account_id.clone()).or_default();
        match what {
            "call_start" => {
                account.insert(channel.clone(), Call { id: data["id"].as_str().unwrap_or("").to_string(), host: data["host_id"].as_str().or(data["owner_id"].as_str()).unwrap_or("").to_string(), sessions: HashMap::new() });
            }
            "call_end" => {
                account.remove(&channel);
            }
            "user_joined" => {
                let call = account.entry(channel.clone()).or_default();
                call.sessions.insert(session.clone(), Session { user: user.clone(), ..Default::default() });
            }
            "user_left" => {
                if let Some(call) = account.get_mut(&channel) {
                    call.sessions.remove(&session);
                }
            }
            "user_muted" | "user_unmuted" | "user_raise_hand" | "user_unraise_hand" | "user_screen_on" | "user_screen_off" => {
                if let Some(s) = account.get_mut(&channel).and_then(|c| c.sessions.get_mut(&session)) {
                    match what {
                        "user_muted" => s.unmuted = false,
                        "user_unmuted" => s.unmuted = true,
                        "user_raise_hand" => s.raised_hand = true,
                        "user_unraise_hand" => s.raised_hand = false,
                        "user_screen_on" => s.screen = true,
                        _ => s.screen = false,
                    }
                }
            }
            "call_host_changed" => {
                if let Some(call) = account.get_mut(&channel) {
                    call.host = data["hostID"].as_str().unwrap_or("").to_string();
                }
            }
            "user_reacted" => {
                // A reaction in the call, said as a passing line in the conversation's call bar by the window.
                if let Some(buffer) = messages::buffer_of(state, &handle.account_id, &channel) {
                    let emoji = data["reaction"].as_str().map(|r| serde_json::from_str::<Value>(r).unwrap_or(Value::Null)).unwrap_or_else(|| data["emoji"].clone());
                    let name = emoji["name"].as_str().or(emoji["emoji"]["name"].as_str()).unwrap_or("");
                    state.events.emit("callReaction", json!({ "bufferId": buffer.id, "userId": user, "emoji": super::emoji::shown(handle, name) }));
                }
                return;
            }
            "host_mute" | "host_removed" | "host_screen_off" | "host_lower_hand" => {
                if let Some(buffer) = messages::buffer_of(state, &handle.account_id, &channel) {
                    if data["session_id"].as_str().is_some_and(|s| joined_session(&buffer.id).as_deref() == Some(s)) || what == "host_removed" {
                        state.events.emit("mattermostCallHost", json!({ "bufferId": buffer.id, "what": what }));
                    }
                }
                return;
            }
            _ => return,
        }
    }
    tell(state, handle, &channel);
    ring_if_wanted(state, handle, &channel);
}

/// The session id this process's call in a buffer has on the server, while it is in one.
fn joined_session(buffer_id: &str) -> Option<String> {
    sessions().lock().unwrap_or_else(|e| e.into_inner()).get(buffer_id).cloned()
}

fn sessions() -> &'static Mutex<HashMap<String, String>> {
    static S: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// A minimal msgpack writer: a map of strings, integers, nested maps and bytes - what the server's websocket reads.
pub mod msgpack {
    pub enum V<'a> {
        Str(&'a str),
        Int(u64),
        Bin(&'a [u8]),
        Map(Vec<(&'a str, V<'a>)>),
    }

    fn len(out: &mut Vec<u8>, n: usize, small: u8, small_max: usize, eight: Option<u8>, sixteen: u8, thirtytwo: u8) {
        if n <= small_max {
            out.push(small | n as u8);
        } else if let (Some(tag), true) = (eight, n <= 0xff) {
            out.push(tag);
            out.push(n as u8);
        } else if n <= 0xffff {
            out.push(sixteen);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        } else {
            out.push(thirtytwo);
            out.extend_from_slice(&(n as u32).to_be_bytes());
        }
    }

    pub fn write(out: &mut Vec<u8>, v: &V) {
        match v {
            V::Str(s) => {
                len(out, s.len(), 0xa0, 31, Some(0xd9), 0xda, 0xdb);
                out.extend_from_slice(s.as_bytes());
            }
            V::Int(n) => {
                if *n < 128 {
                    out.push(*n as u8);
                } else if *n <= u32::MAX as u64 {
                    out.push(0xce);
                    out.extend_from_slice(&(*n as u32).to_be_bytes());
                } else {
                    out.push(0xcf);
                    out.extend_from_slice(&n.to_be_bytes());
                }
            }
            V::Bin(b) => {
                if b.len() <= 0xff {
                    out.push(0xc4);
                    out.push(b.len() as u8);
                } else if b.len() <= 0xffff {
                    out.push(0xc5);
                    out.extend_from_slice(&(b.len() as u16).to_be_bytes());
                } else {
                    out.push(0xc6);
                    out.extend_from_slice(&(b.len() as u32).to_be_bytes());
                }
                out.extend_from_slice(b);
            }
            V::Map(entries) => {
                len(out, entries.len(), 0x80, 15, None, 0xde, 0xdf);
                for (k, v) in entries {
                    write(out, &V::Str(k));
                    write(out, v);
                }
            }
        }
    }
}

/// The frame that carries a description: `{action, seq, data: {data: zlib(json)}}` in msgpack.
pub fn sdp_frame(seq: u64, description: &str) -> Result<Vec<u8>> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(description.as_bytes())?;
    let packed = z.finish()?;
    let action = format!("{PREFIX}sdp");
    let mut out = Vec::new();
    msgpack::write(&mut out, &msgpack::V::Map(vec![("action", msgpack::V::Str(&action)), ("seq", msgpack::V::Int(seq)), ("data", msgpack::V::Map(vec![("data", msgpack::V::Bin(&packed))]))]));
    Ok(out)
}

/// The ICE servers the plugin wants clients to use, with TURN credentials where it hands them out.
async fn ice_servers(handle: &Handle) -> Value {
    let config = handle.api.get(&format!("/plugins/{PLUGIN}/config"), &[]).await.unwrap_or(Value::Null);
    let mut servers: Vec<Value> = config["ICEServersConfigs"].as_array().cloned().unwrap_or_default();
    if config["NeedsTURNCredentials"].as_bool() == Some(true) {
        if let Ok(turn) = handle.api.get(&format!("/plugins/{PLUGIN}/turn-credentials"), &[]).await {
            servers.extend(turn.as_array().cloned().unwrap_or_default());
        }
    }
    json!(servers)
}

/// Joins (or starts) the call in a conversation: opens the call's websocket, says `join`, and waits for the plugin to
/// take it. Answers the ICE servers to make the window's peer connection with.
pub async fn join(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<Value> {
    if !handle.server.has_calls() {
        bail!("This server does not have calls (the Calls plugin is not running)");
    }
    let channel = buffer.remote_id.clone().ok_or_else(|| anyhow!("this is not a Mattermost conversation"))?;
    if joined().lock().unwrap_or_else(|e| e.into_inner()).contains_key(&buffer.id) {
        bail!("Already in this call");
    }
    let config = state.accounts.get_mattermost(&handle.account_id).ok_or_else(|| anyhow!("no such account"))?;
    let address = super::connect::socket_address(&config.server, "", 0)?;
    let mut request = tokio_tungstenite::tungstenite::client::IntoClientRequest::into_client_request(address.as_str())?;
    request.headers_mut().insert("Authorization", format!("Bearer {}", config.token).parse().context("the session")?);
    let (socket, _) = crate::net::route::websocket(&handle.account_id, request).await.context("connecting the call")?;
    let (mut sink, mut stream) = socket.split();
    let mut seq: u64 = 1;
    sink.send(Ws::text(json!({ "seq": seq, "action": "authentication_challenge", "data": { "token": config.token } }).to_string())).await?;
    // The connection's id, which the plugin names the call session by.
    let connection = loop {
        let frame = tokio::time::timeout(Duration::from_secs(15), stream.next()).await.map_err(|_| anyhow!("the server did not answer"))?.ok_or_else(|| anyhow!("the server closed the call's connection"))??;
        if let Ws::Text(t) = frame {
            let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
            if v["event"].as_str() == Some("hello") {
                break v["data"]["connection_id"].as_str().unwrap_or("").to_string();
            }
        }
    };
    seq += 1;
    sink.send(Ws::text(json!({ "seq": seq, "action": format!("{PREFIX}join"), "data": { "channelID": channel } }).to_string())).await?;
    // The plugin takes it (`join`) or says why not (`error`).
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(15), stream.next()).await.map_err(|_| anyhow!("the call did not start"))?.ok_or_else(|| anyhow!("the server closed the call's connection"))??;
        let Ws::Text(t) = frame else { continue };
        let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
        let ours = v["data"]["connID"].as_str() == Some(connection.as_str());
        match v["event"].as_str() {
            Some(e) if e == format!("{PREFIX}join") && ours => break,
            Some(e) if e == format!("{PREFIX}error") && ours => bail!("{}", refusal(v["data"]["data"].as_str().unwrap_or(""))),
            _ => {}
        }
    }
    sessions().lock().unwrap_or_else(|e| e.into_inner()).insert(buffer.id.clone(), connection.clone());
    let (tx, mut rx) = mpsc::unbounded_channel::<Say>();
    joined().lock().unwrap_or_else(|e| e.into_inner()).insert(buffer.id.clone(), tx);
    ring_if_wanted(state, handle, &channel);
    let (state2, handle2, buffer_id) = (state.clone(), handle.clone(), buffer.id.clone());
    tokio::spawn(async move {
        let mut ping = tokio::time::interval(Duration::from_secs(15));
        let ended = loop {
            tokio::select! {
                frame = stream.next() => {
                    let Some(Ok(frame)) = frame else { break "the call's connection was lost" };
                    let Ws::Text(t) = frame else { continue };
                    let v: Value = serde_json::from_str(&t).unwrap_or(Value::Null);
                    if v["data"]["connID"].as_str() != Some(connection.as_str()) {
                        continue;
                    }
                    match v["event"].as_str().and_then(|e| e.strip_prefix(PREFIX)) {
                        Some("signal") => {
                            let data: Value = v["data"]["data"].as_str().and_then(|d| serde_json::from_str(d).ok()).unwrap_or(Value::Null);
                            state2.events.emit("mattermostCallSignal", json!({ "bufferId": buffer_id, "data": data }));
                        }
                        Some("error") => {
                            state2.events.emit("mattermostCallEnded", json!({ "bufferId": buffer_id, "error": v["data"]["data"] }));
                            break "";
                        }
                        _ => {}
                    }
                }
                said = rx.recv() => {
                    seq += 1;
                    let sent = match said {
                        Some(Say::Sdp(description)) => match sdp_frame(seq, &description) {
                            Ok(frame) => sink.send(Ws::binary(frame)).await,
                            Err(_) => continue,
                        },
                        Some(Say::Ice(candidate)) => sink.send(Ws::text(json!({ "seq": seq, "action": format!("{PREFIX}ice"), "data": { "data": candidate } }).to_string())).await,
                        Some(Say::Action(name, data)) => sink.send(Ws::text(json!({ "seq": seq, "action": format!("{PREFIX}{name}"), "data": data }).to_string())).await,
                        Some(Say::Leave) | None => {
                            let _ = sink.send(Ws::text(json!({ "seq": seq, "action": format!("{PREFIX}leave") }).to_string())).await;
                            let _ = sink.close().await;
                            break "";
                        }
                    };
                    if sent.is_err() {
                        break "the call's connection was lost";
                    }
                }
                _ = ping.tick() => {
                    seq += 1;
                    if sink.send(Ws::text(json!({ "seq": seq, "action": "ping" }).to_string())).await.is_err() {
                        break "the call's connection was lost";
                    }
                }
            }
        };
        joined().lock().unwrap_or_else(|e| e.into_inner()).remove(&buffer_id);
        sessions().lock().unwrap_or_else(|e| e.into_inner()).remove(&buffer_id);
        if !ended.is_empty() {
            state2.events.emit("mattermostCallEnded", json!({ "bufferId": buffer_id, "error": ended }));
        }
        let channel = state2.runtime.get_buffer(&buffer_id).and_then(|b| b.remote_id);
        if let Some(channel) = channel {
            ring_if_wanted(&state2, &handle2, &channel);
        }
    });
    Ok(json!({ "iceServers": ice_servers(handle).await, "sessionId": joined_session(&buffer.id) }))
}

/// Why the plugin would not start or join a call, said so a person can act on it.
pub fn refusal(said: &str) -> String {
    match said {
        "insufficient permissions" => "Calls are not turned on in this channel, and only a system admin can start one here. An admin can turn calls on for the channel or for the server".to_string(),
        s if s.contains("only allow calls in DMs") => "This server allows calls only in direct messages (calls with more people need a Mattermost licence)".to_string(),
        "" => "The server would not start the call".to_string(),
        other => other.to_string(),
    }
}

/// Something the window says on its call.
pub fn say(buffer_id: &str, what: Say) -> Result<()> {
    let sender = joined().lock().unwrap_or_else(|e| e.into_inner()).get(buffer_id).cloned().ok_or_else(|| anyhow!("not in a call there"))?;
    sender.send(what).map_err(|_| anyhow!("the call has ended"))
}

/// Says no to a call that rings, so it stops ringing on the account's other apps.
pub async fn decline(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<()> {
    let channel = buffer.remote_id.clone().ok_or_else(|| anyhow!("this is not a Mattermost conversation"))?;
    match handle.api.post(&format!("/plugins/{PLUGIN}/calls/{channel}/dismiss-notification"), &json!({})).await {
        Ok(_) | Err(ApiError::Refused { status: 404, .. }) => {}
        Err(e) => return Err(anyhow!("{e}")),
    }
    ringing_set().lock().unwrap_or_else(|e| e.into_inner()).entry(handle.account_id.clone()).or_default().remove(&channel);
    state.events.emit("incomingCall", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "channelId": channel, "ringing": false }));
    Ok(())
}

/// A host's control over somebody else's session (`mute`, `remove`, `screen-off`, `lower-hand`, `make` host), or over
/// everybody (`mute-others`, `end`). Licensed servers only; the server says so otherwise.
pub async fn host(handle: &Handle, buffer: &Buffer, action: &str, session: Option<&str>) -> Result<()> {
    let channel = buffer.remote_id.clone().ok_or_else(|| anyhow!("this is not a Mattermost conversation"))?;
    let call = calls().lock().unwrap_or_else(|e| e.into_inner()).get(&handle.account_id).and_then(|c| c.get(&channel).cloned()).ok_or_else(|| anyhow!("there is no call here"))?;
    let body = match (action, session) {
        ("make", Some(s)) => json!({ "new_host_id": call.sessions.get(s).map(|x| x.user.clone()).unwrap_or_default() }),
        (_, Some(s)) => json!({ "session_id": s }),
        _ => json!({}),
    };
    handle.api.post(&format!("/plugins/{PLUGIN}/calls/{}/host/{action}", call.id), &body).await.map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_description_goes_as_a_msgpack_frame_with_its_json_compressed() {
        let frame = sdp_frame(7, "{\"type\":\"offer\",\"sdp\":\"v=0\"}").unwrap();
        assert_eq!(frame[0], 0x83, "a map of three");
        assert_eq!(&frame[1..8], b"\xa6action");
        let action = format!("{PREFIX}sdp");
        assert_eq!(action.len(), 31);
        assert_eq!(frame[8], 0xa0 | 31, "a fixstr holds up to 31 bytes");
        assert_eq!(&frame[9..40], action.as_bytes());
        let at = frame.windows(4).position(|w| w == b"\xa3seq").unwrap();
        assert_eq!(frame[at + 4], 7);
        let bin = frame.windows(6).rposition(|w| w == b"\xa4data\xc4").unwrap() + 5;
        let packed = &frame[bin + 2..];
        assert_eq!(packed.len(), frame[bin + 1] as usize);
        let mut out = String::new();
        std::io::Read::read_to_string(&mut flate2::read::ZlibDecoder::new(packed), &mut out).unwrap();
        assert_eq!(out, "{\"type\":\"offer\",\"sdp\":\"v=0\"}");
    }

    #[test]
    fn a_call_is_read_with_who_is_in_it_and_who_shares_a_screen() {
        let call = parse_call(&json!({ "id": "c", "host_id": "u1", "screen_sharing_session_id": "s2", "sessions": [{ "session_id": "s1", "user_id": "u1", "unmuted": true, "raised_hand": 0 }, { "session_id": "s2", "user_id": "u2", "unmuted": false, "raised_hand": 1700000000 }] })).unwrap();
        assert_eq!(call.host, "u1");
        assert_eq!(call.sessions["s1"], Session { user: "u1".into(), unmuted: true, raised_hand: false, screen: false });
        assert_eq!(call.sessions["s2"], Session { user: "u2".into(), unmuted: false, raised_hand: true, screen: true });
    }
}
