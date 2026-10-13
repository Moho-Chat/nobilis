//! Steam voice: calls to a friend and voice in a chat group's room.
//!
//! Steam's voice is WebRTC with Valve's own server as the one peer: the client
//! offers, the server answers with an ICE-lite description of a relay it holds
//! for the session, media goes there, and the server mixes and forwards it. A
//! browser speaks all of that already, so the media lives in the window - the
//! same division `lib/webrtc.ts` draws for Matrix - and this carries what is
//! the daemon's: the signalling, which is calls and notifications on the
//! account's own connection.
//!
//! None of it is in the definitions extracted from the client binary. It is in
//! the Steam client's own JavaScript, which is where `voicewire.rs` and the
//! order of calls below were read from: offer, then `InitiateWebRTCConnection`;
//! the server says the session is connected; then either `JoinVoiceChat` for a
//! room or `RequestOneOnOneChat` for a friend; then `UpdateVoiceChatWebRTCData`
//! tells the chat which stream is ours. While it runs the server renegotiates
//! as people come and go, and says who is who by stream number.
//!
//! The window sends a call as `{ op, ... }` and hears everything the server says
//! as a `steamVoice` event. Every 64-bit number crosses as text, since the
//! window cannot hold one.

use super::chat::refusal;
use super::chatgroups;
use super::link::Link;
use super::steamid;
use super::voicewire::*;
use crate::state::AppState;
use prost::Message;
use serde_json::{json, Value};

fn id_of(v: &Value, key: &str) -> Option<u64> {
    match &v[key] {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

fn num(v: &Value, key: &str) -> Option<u32> {
    v[key].as_u64().and_then(|n| u32::try_from(n).ok())
}

fn text(v: &Value, key: &str) -> Option<String> {
    v[key].as_str().map(str::to_string)
}

/// A call that is only said, with no answer to wait for.
fn say<Q: Message>(link: &Link, method: &str, request: &Q) -> Result<(), String> {
    link.notify(method, request).map_err(|e| e.to_string())
}

async fn ask<Q: Message, A: Message + Default>(link: &Link, method: &str, request: &Q) -> Result<A, String> {
    link.call(method, request, true).await.map_err(|e| refusal(&e))
}

/// One thing the window asks of the voice, and what Steam answers.
pub async fn act(state: &AppState, account_id: &str, params: &Value) -> Result<Value, String> {
    let link = state.runtime.steam_link(account_id).ok_or("this Steam account is not connected")?;
    let op = params["op"].as_str().ok_or("steamVoice needs an \"op\"")?;
    match op {
        // The offer, and the answer from the server that will carry the session.
        "initiate" => {
            let request = InitiateWebRTCConnectionRequest { sdp: text(params, "sdp"), browser_name: text(params, "browserName"), browser_version: text(params, "browserVersion") };
            let answer: InitiateWebRTCConnectionResponse = ask(&link, "WebRTCClient.InitiateWebRTCConnection#1", &request).await?;
            Ok(json!({ "remoteDescription": answer.remote_description }))
        }
        // A new offer on a session that is up (the browser's own renegotiation).
        "update" => {
            let request = UpdateWebRTCConnectionRequest {
                ip_webrtc_server: num(params, "serverIp"),
                port_webrtc_server: num(params, "serverPort"),
                ip_webrtc_session_client: num(params, "clientIp"),
                port_webrtc_session_client: num(params, "clientPort"),
                sdp: text(params, "sdp"),
            };
            let answer: UpdateWebRTCConnectionResponse = ask(&link, "WebRTCClient.UpdateWebRTCConnection#1", &request).await?;
            Ok(json!({ "remoteDescription": answer.remote_description }))
        }
        // The window has taken the server's new offer and answered it.
        "acknowledge" => {
            let request = AcknowledgeUpdatedRemoteDescriptionRequest {
                ip_webrtc_server: num(params, "serverIp"),
                port_webrtc_server: num(params, "serverPort"),
                ip_webrtc_session_client: num(params, "clientIp"),
                port_webrtc_session_client: num(params, "clientPort"),
                remote_description_version: id_of(params, "version"),
            };
            let _: AcknowledgeUpdatedRemoteDescriptionResponse = ask(&link, "WebRTCClient.AcknowledgeUpdatedRemoteDescription#1", &request).await?;
            Ok(json!({}))
        }
        "joinRoom" => {
            let (Some(group), Some(room)) = (id_of(params, "groupId"), id_of(params, "roomId")) else { return Err("joinRoom needs a group and a room".into()) };
            let answer: RoomJoinVoiceChatResponse = ask(&link, "ChatRoom.JoinVoiceChat#1", &RoomJoinVoiceChatRequest { chat_group_id: Some(group), chat_id: Some(room) }).await?;
            Ok(json!({ "voiceChatId": answer.voice_chatid.map(|v| v.to_string()) }))
        }
        "leaveRoom" => {
            let (Some(group), Some(room)) = (id_of(params, "groupId"), id_of(params, "roomId")) else { return Err("leaveRoom needs a group and a room".into()) };
            let _: RoomLeaveVoiceChatResponse = ask(&link, "ChatRoom.LeaveVoiceChat#1", &RoomLeaveVoiceChatRequest { chat_group_id: Some(group), chat_id: Some(room) }).await?;
            Ok(json!({}))
        }
        // Which stream is ours, once the session is connected: what makes the chat
        // count this connection as being in it.
        "data" => {
            let request = UpdateVoiceChatWebRTCDataRequest {
                voice_chatid: id_of(params, "voiceChatId"),
                ip_webrtc_server: num(params, "serverIp"),
                port_webrtc_server: num(params, "serverPort"),
                ip_webrtc_client: num(params, "clientIp"),
                port_webrtc_client: num(params, "clientPort"),
                ssrc_my_sending_stream: num(params, "ssrc"),
                user_agent: text(params, "userAgent"),
                has_audio_worklets_support: Some(params["worklets"].as_bool().unwrap_or(false)),
                has_webrtc_data_channel: Some(params["dataChannel"].as_bool().unwrap_or(false)),
                accepts_webrtc_video: Some(false),
                sends_webrtc_video: Some(false),
            };
            let answer: UpdateVoiceChatWebRTCDataResponse = ask(&link, "VoiceChat.UpdateVoiceChatWebRTCData#1", &request).await?;
            Ok(json!({ "sendLogs": answer.send_client_voice_logs.unwrap_or(false) }))
        }
        // How we are showing to the others in the call: muted, deafened, no microphone.
        "status" => {
            let own = state.accounts.get_steam(account_id).map(|c| c.steamid).ok_or("this Steam account is not set up")?;
            let request = UserVoiceStatusNotification {
                voice_chatid: id_of(params, "voiceChatId"),
                user_steamid: Some(own),
                user_muted_mic_locally: Some(params["micMuted"].as_bool().unwrap_or(false)),
                user_muted_output_locally: Some(params["outputMuted"].as_bool().unwrap_or(false)),
                user_has_no_mic_for_session: Some(params["noMic"].as_bool().unwrap_or(false)),
                user_webaudio_sample_rate: params["sampleRate"].as_i64().and_then(|n| i32::try_from(n).ok()),
                user_sends_video: Some(false),
            };
            say(&link, "VoiceChat.NotifyUserVoiceStatus#1", &request)?;
            Ok(json!({}))
        }
        "requestCall" => {
            let partner = id_of(params, "steamid").ok_or("requestCall needs a person")?;
            let answer: RequestOneOnOneChatResponse = ask(&link, "VoiceChat.RequestOneOnOneChat#1", &RequestOneOnOneChatRequest { steamid_partner: Some(partner) }).await?;
            Ok(json!({ "voiceChatId": answer.voice_chatid.map(|v| v.to_string()) }))
        }
        "answerCall" => {
            let partner = id_of(params, "steamid").ok_or("answerCall needs a person")?;
            let request = AnswerOneOnOneChatRequest { voice_chatid: id_of(params, "voiceChatId"), steamid_partner: Some(partner), accepted_request: Some(params["accept"].as_bool().unwrap_or(true)) };
            let _: AnswerOneOnOneChatResponse = ask(&link, "VoiceChat.AnswerOneOnOneChat#1", &request).await?;
            Ok(json!({}))
        }
        // Hanging up on them: it ends the call for both.
        "endCall" => {
            let partner = id_of(params, "steamid").ok_or("endCall needs a person")?;
            let _: EndOneOnOneChatResponse = ask(&link, "VoiceChat.EndOneOnOneChat#1", &EndOneOnOneChatRequest { steamid_partner: Some(partner) }).await?;
            Ok(json!({}))
        }
        // Leaving it running for them, where they can come back to it.
        "leaveCall" => {
            let partner = id_of(params, "steamid").ok_or("leaveCall needs a person")?;
            let request = LeaveOneOnOneChatRequest { steamid_partner: Some(partner), voice_chatid: id_of(params, "voiceChatId") };
            let _: LeaveOneOnOneChatResponse = ask(&link, "VoiceChat.LeaveOneOnOneChat#1", &request).await?;
            Ok(json!({}))
        }
        other => Err(format!("{other} is not something Steam voice can be asked to do")),
    }
}

fn who(account: u64) -> String {
    steamid::text_of(steamid::account_of(account).unwrap_or(0))
}

fn status_json(s: &UserVoiceStatusNotification) -> Value {
    json!({
        "voiceChatId": s.voice_chatid.map(|v| v.to_string()),
        "steamid": s.user_steamid.map(who),
        "micMuted": s.user_muted_mic_locally.unwrap_or(false),
        "outputMuted": s.user_muted_output_locally.unwrap_or(false),
        "noMic": s.user_has_no_mic_for_session.unwrap_or(false),
    })
}

/// Tells the window what the server said about the call, if this is one of its notifications.
/// Returns whether it was.
pub fn heard(state: &AppState, account_id: &str, method: &str, body: &[u8]) -> bool {
    let tell = |kind: &str, data: Value| {
        state.events.emit("steamVoice", json!({ "accountId": account_id, "kind": kind, "data": data }));
    };
    match method {
        "WebRTCClientNotifications.NotifyWebRTCSessionConnected#1" => {
            let Ok(n) = WebRTCSessionConnectedNotification::decode(body) else { return true };
            tell("sessionConnected", json!({ "ssrc": n.ssrc, "clientIp": n.client_ip, "clientPort": n.client_port, "serverIp": n.server_ip, "serverPort": n.server_port }));
        }
        "WebRTCClientNotifications.NotifyWebRTCUpdateRemoteDescription#1" => {
            let Ok(n) = WebRTCUpdateRemoteDescriptionNotification::decode(body) else { return true };
            tell(
                "remoteDescription",
                json!({
                    "sdp": n.remote_description,
                    "version": n.remote_description_version.map(|v| v.to_string()),
                    "ssrcs": n.ssrcs_to_accountids.iter().map(|m| json!({ "ssrc": m.ssrc, "steamid": m.accountid.map(steamid::text_of) })).collect::<Vec<_>>(),
                }),
            );
        }
        "VoiceChatClient.NotifyUserJoinedVoiceChat#1" | "VoiceChatClient.NotifyUserLeftVoiceChat#1" => {
            let joined = method.contains("Joined");
            let Ok(n) = UserJoinedVoiceChatNotification::decode(body) else { return true };
            let (group, room) = (n.chat_group_id.filter(|g| *g != 0), n.chatid.filter(|c| *c != 0));
            if let (Some(group), Some(room), Some(user)) = (group, room, n.user_steamid.and_then(steamid::account_of)) {
                chatgroups::voice_changed(state, account_id, group, room, user, joined);
            }
            tell(if joined { "userJoined" } else { "userLeft" }, json!({ "voiceChatId": n.voice_chatid.map(|v| v.to_string()), "steamid": n.user_steamid.map(who), "groupId": group.map(|g| g.to_string()), "roomId": room.map(|r| r.to_string()) }));
        }
        "VoiceChatClient.NotifyVoiceChatEnded#1" => {
            let Ok(n) = VoiceChatEndedNotification::decode(body) else { return true };
            tell("ended", json!({ "voiceChatId": n.voice_chatid.map(|v| v.to_string()), "groupId": n.chat_group_id.map(|g| g.to_string()), "roomId": n.chatid.map(|r| r.to_string()) }));
        }
        "VoiceChatClient.NotifyUserVoiceStatus#1" => {
            let Ok(n) = UserVoiceStatusNotification::decode(body) else { return true };
            tell("status", status_json(&n));
        }
        "VoiceChatClient.NotifyAllUsersVoiceStatus#1" => {
            let Ok(n) = AllMembersStatusNotification::decode(body) else { return true };
            tell("allStatus", json!({ "voiceChatId": n.voice_chatid.map(|v| v.to_string()), "users": n.users.iter().map(status_json).collect::<Vec<_>>() }));
        }
        "VoiceChatClient.NotifyOneOnOneChatRequested#1" => {
            let Ok(n) = OneOnOneChatRequestedNotification::decode(body) else { return true };
            tell("callRequested", json!({ "voiceChatId": n.voice_chatid.map(|v| v.to_string()), "steamid": n.steamid_partner.map(who) }));
        }
        "VoiceChatClient.NotifyOneOnOneChatResponse#1" => {
            let Ok(n) = OneOnOneChatRequestResponseNotification::decode(body) else { return true };
            tell("callResponse", json!({ "voiceChatId": n.voicechat_id.map(|v| v.to_string()), "steamid": n.steamid_partner.map(who), "accepted": n.accepted_request.unwrap_or(false) }));
        }
        "ChatRoomClient.NotifyShouldRejoinChatRoomVoiceChat#1" => {
            let Ok(n) = RoomNotifyShouldRejoinChatRoomVoiceChatNotification::decode(body) else { return true };
            tell("shouldRejoin", json!({ "groupId": n.chat_group_id.map(|g| g.to_string()), "roomId": n.chat_id.map(|r| r.to_string()) }));
        }
        _ => return false,
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::simulated_daemon;

    /// What the window is told when the server says something, as `(kind, data)` of the next event.
    fn told(state: &AppState, method: &str, body: Vec<u8>) -> (bool, Option<Value>) {
        let mut rx = state.events.subscribe();
        let known = heard(state, "steam:voice-test", method, &body);
        let event = loop {
            match rx.try_recv() {
                Ok(e) if e.name == "steamVoice" => break Some(e.data),
                Ok(_) => continue,
                Err(_) => break None,
            }
        };
        (known, event)
    }

    #[tokio::test]
    async fn what_the_server_says_of_the_session_reaches_the_window_in_the_window_s_words() {
        let state = simulated_daemon("steam-voice-heard");
        let connected = WebRTCSessionConnectedNotification { ssrc: Some(5), client_ip: Some(1), client_port: Some(2), server_ip: Some(3), server_port: Some(4) };
        let (known, event) = told(&state, "WebRTCClientNotifications.NotifyWebRTCSessionConnected#1", connected.encode_to_vec());
        assert!(known);
        let event = event.unwrap();
        assert_eq!((event["kind"].as_str(), event["data"]["ssrc"].as_u64(), event["data"]["serverPort"].as_u64()), (Some("sessionConnected"), Some(5), Some(4)));

        let offer = WebRTCUpdateRemoteDescriptionNotification {
            remote_description: Some("{}".into()),
            remote_description_version: Some(u64::MAX - 1),
            ssrcs_to_accountids: vec![WebRTCUpdateRemoteDescriptionNotificationSsrcAccount { ssrc: Some(9), accountid: Some(100) }],
        };
        let (_, event) = told(&state, "WebRTCClientNotifications.NotifyWebRTCUpdateRemoteDescription#1", offer.encode_to_vec());
        let data = event.unwrap()["data"].clone();
        assert_eq!(data["version"], (u64::MAX - 1).to_string(), "a 64-bit number crosses as text, since the window cannot hold it");
        assert_eq!((data["ssrcs"][0]["ssrc"].as_u64(), data["ssrcs"][0]["steamid"].as_str()), (Some(9), Some("76561197960265828")));

        let call = OneOnOneChatRequestedNotification { voice_chatid: Some(u64::MAX), steamid_partner: Some(steamid::steamid_of(100)) };
        let (_, event) = told(&state, "VoiceChatClient.NotifyOneOnOneChatRequested#1", call.encode_to_vec());
        let data = event.unwrap()["data"].clone();
        assert_eq!((data["voiceChatId"].as_str(), data["steamid"].as_str()), (Some("18446744073709551615"), Some("76561197960265828")));

        let (known, event) = told(&state, "VoiceChatClient.NotifyNothingInParticular#1", Vec::new());
        assert!(!known && event.is_none(), "a method this does not know is not told");
    }

    #[test]
    fn numbers_cross_as_text_and_a_number_too_big_for_its_place_is_not_one() {
        assert_eq!(id_of(&json!({ "a": "18446744073709551615" }), "a"), Some(u64::MAX));
        assert_eq!(id_of(&json!({ "a": 5 }), "a"), Some(5));
        assert_eq!(id_of(&json!({}), "a"), None);
        assert_eq!(num(&json!({ "n": 4294967296u64 }), "n"), None, "a number too big for 32 bits is not one");
    }
}
