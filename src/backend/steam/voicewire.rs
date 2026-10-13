//! Steam voice: the messages of the VoiceChat and WebRTCClient services and the voice
//! calls of ChatRoom, written out by `golden/make_voice.py` from the Steam client's own
//! JavaScript. Do not edit by hand: change the generator and run it again.

#![allow(dead_code)]

use prost::Message;

#[derive(Clone, PartialEq, Message)]
pub struct RequestOneOnOneChatRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_partner: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RequestOneOnOneChatResponse {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct OneOnOneChatRequestedNotification {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid_partner: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AnswerOneOnOneChatRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid_partner: Option<u64>,
    #[prost(bool, optional, tag = "3")]
    pub accepted_request: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AnswerOneOnOneChatResponse {
}

#[derive(Clone, PartialEq, Message)]
pub struct OneOnOneChatRequestResponseNotification {
    #[prost(fixed64, optional, tag = "1")]
    pub voicechat_id: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid_partner: Option<u64>,
    #[prost(bool, optional, tag = "3")]
    pub accepted_request: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct EndOneOnOneChatRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_partner: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct EndOneOnOneChatResponse {
}

#[derive(Clone, PartialEq, Message)]
pub struct LeaveOneOnOneChatRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_partner: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub voice_chatid: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LeaveOneOnOneChatResponse {
}

#[derive(Clone, PartialEq, Message)]
pub struct UserJoinedVoiceChatNotification {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub user_steamid: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    pub chatid: Option<u64>,
    #[prost(fixed64, optional, tag = "4")]
    pub one_on_one_steamid_lower: Option<u64>,
    #[prost(fixed64, optional, tag = "5")]
    pub one_on_one_steamid_higher: Option<u64>,
    #[prost(uint64, optional, tag = "6")]
    pub chat_group_id: Option<u64>,
    #[prost(uint32, optional, tag = "7")]
    pub user_sessionid: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UserVoiceStatusNotification {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub user_steamid: Option<u64>,
    #[prost(bool, optional, tag = "3")]
    pub user_muted_mic_locally: Option<bool>,
    #[prost(bool, optional, tag = "4")]
    pub user_muted_output_locally: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    pub user_has_no_mic_for_session: Option<bool>,
    #[prost(int32, optional, tag = "6")]
    pub user_webaudio_sample_rate: Option<i32>,
    #[prost(bool, optional, tag = "7")]
    pub user_sends_video: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AllMembersStatusNotification {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(message, repeated, tag = "2")]
    pub users: Vec<UserVoiceStatusNotification>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpdateVoiceChatWebRTCDataRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(uint32, optional, tag = "2")]
    pub ip_webrtc_server: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub port_webrtc_server: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub ip_webrtc_client: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub port_webrtc_client: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub ssrc_my_sending_stream: Option<u32>,
    #[prost(string, optional, tag = "7")]
    pub user_agent: Option<String>,
    #[prost(bool, optional, tag = "8")]
    pub has_audio_worklets_support: Option<bool>,
    #[prost(bool, optional, tag = "9")]
    pub has_webrtc_data_channel: Option<bool>,
    #[prost(bool, optional, tag = "10")]
    pub accepts_webrtc_video: Option<bool>,
    #[prost(bool, optional, tag = "11")]
    pub sends_webrtc_video: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpdateVoiceChatWebRTCDataResponse {
    #[prost(bool, optional, tag = "1")]
    pub send_client_voice_logs: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpdateUserVideoStatusRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub user_steamid: Option<u64>,
    #[prost(bool, optional, tag = "3")]
    pub accept_webrtc_video: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpdateUserVideoStatusResponse {
}

#[derive(Clone, PartialEq, Message)]
pub struct UploadClientVoiceChatLogsRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub client_voice_logs_new_lines: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UploadClientVoiceChatLogsResponse {
}

#[derive(Clone, PartialEq, Message)]
pub struct LeaveVoiceChatRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LeaveVoiceChatResponse {
}

#[derive(Clone, PartialEq, Message)]
pub struct UserLeftVoiceChatNotification {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub user_steamid: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    pub chatid: Option<u64>,
    #[prost(fixed64, optional, tag = "4")]
    pub one_on_one_steamid_lower: Option<u64>,
    #[prost(fixed64, optional, tag = "5")]
    pub one_on_one_steamid_higher: Option<u64>,
    #[prost(uint64, optional, tag = "6")]
    pub chat_group_id: Option<u64>,
    #[prost(uint32, optional, tag = "7")]
    pub user_sessionid: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct VoiceChatEndedNotification {
    #[prost(fixed64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub one_on_one_steamid_lower: Option<u64>,
    #[prost(fixed64, optional, tag = "3")]
    pub one_on_one_steamid_higher: Option<u64>,
    #[prost(uint64, optional, tag = "4")]
    pub chatid: Option<u64>,
    #[prost(uint64, optional, tag = "5")]
    pub chat_group_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InitiateWebRTCConnectionRequest {
    #[prost(string, optional, tag = "1")]
    pub sdp: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub browser_name: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub browser_version: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InitiateWebRTCConnectionResponse {
    #[prost(string, optional, tag = "1")]
    pub remote_description: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpdateWebRTCConnectionRequest {
    #[prost(uint32, optional, tag = "1")]
    pub ip_webrtc_server: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub port_webrtc_server: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub ip_webrtc_session_client: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub port_webrtc_session_client: Option<u32>,
    #[prost(string, optional, tag = "5")]
    pub sdp: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpdateWebRTCConnectionResponse {
    #[prost(string, optional, tag = "1")]
    pub remote_description: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WebRTCSessionConnectedNotification {
    #[prost(uint32, optional, tag = "1")]
    pub ssrc: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub client_ip: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub client_port: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub server_ip: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub server_port: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WebRTCUpdateRemoteDescriptionNotification {
    #[prost(string, optional, tag = "1")]
    pub remote_description: Option<String>,
    #[prost(uint64, optional, tag = "2")]
    pub remote_description_version: Option<u64>,
    #[prost(message, repeated, tag = "3")]
    pub ssrcs_to_accountids: Vec<WebRTCUpdateRemoteDescriptionNotificationSsrcAccount>,
}

#[derive(Clone, PartialEq, Message)]
pub struct WebRTCUpdateRemoteDescriptionNotificationSsrcAccount {
    #[prost(uint32, optional, tag = "1")]
    pub ssrc: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub accountid: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AcknowledgeUpdatedRemoteDescriptionRequest {
    #[prost(uint32, optional, tag = "1")]
    pub ip_webrtc_server: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub port_webrtc_server: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub ip_webrtc_session_client: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub port_webrtc_session_client: Option<u32>,
    #[prost(uint64, optional, tag = "5")]
    pub remote_description_version: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AcknowledgeUpdatedRemoteDescriptionResponse {
}

#[derive(Clone, PartialEq, Message)]
pub struct RoomJoinVoiceChatRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RoomJoinVoiceChatResponse {
    #[prost(uint64, optional, tag = "1")]
    pub voice_chatid: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RoomLeaveVoiceChatRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RoomLeaveVoiceChatResponse {
}

#[derive(Clone, PartialEq, Message)]
pub struct RoomNotifyShouldRejoinChatRoomVoiceChatNotification {
    #[prost(uint64, optional, tag = "1")]
    pub chat_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_group_id: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = include_str!("golden/voice_golden.txt");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    fn round_trips<M: Message + Default>(name: &str, bytes: &[u8]) {
        let decoded = M::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(decoded.encode_to_vec(), bytes, "{name} does not encode to what the reference library wrote");
    }

    #[test]
    fn every_voice_message_matches_the_reference_encoding() {
        let mut seen = 0;
        for line in GOLDEN.lines().filter(|l| !l.trim().is_empty()) {
            let (name, hex) = line.split_once(' ').expect("name and bytes");
            let bytes = unhex(hex);
            macro_rules! cases {
                ($($t:ident),* $(,)?) => {
                    match name {
                        $( stringify!($t) => round_trips::<$t>(name, &bytes), )*
                        other => panic!("voice_golden.txt has {other}, which voicewire.rs does not"),
                    }
                };
            }
            cases!(RequestOneOnOneChatRequest, RequestOneOnOneChatResponse, OneOnOneChatRequestedNotification, AnswerOneOnOneChatRequest, AnswerOneOnOneChatResponse, OneOnOneChatRequestResponseNotification, EndOneOnOneChatRequest, EndOneOnOneChatResponse, LeaveOneOnOneChatRequest, LeaveOneOnOneChatResponse, UserJoinedVoiceChatNotification, UserVoiceStatusNotification, AllMembersStatusNotification, UpdateVoiceChatWebRTCDataRequest, UpdateVoiceChatWebRTCDataResponse, UpdateUserVideoStatusRequest, UpdateUserVideoStatusResponse, UploadClientVoiceChatLogsRequest, UploadClientVoiceChatLogsResponse, LeaveVoiceChatRequest, LeaveVoiceChatResponse, UserLeftVoiceChatNotification, VoiceChatEndedNotification, InitiateWebRTCConnectionRequest, InitiateWebRTCConnectionResponse, UpdateWebRTCConnectionRequest, UpdateWebRTCConnectionResponse, WebRTCSessionConnectedNotification, WebRTCUpdateRemoteDescriptionNotification, WebRTCUpdateRemoteDescriptionNotificationSsrcAccount, AcknowledgeUpdatedRemoteDescriptionRequest, AcknowledgeUpdatedRemoteDescriptionResponse, RoomJoinVoiceChatRequest, RoomJoinVoiceChatResponse, RoomLeaveVoiceChatRequest, RoomLeaveVoiceChatResponse, RoomNotifyShouldRejoinChatRoomVoiceChatNotification);
            seen += 1;
        }
        assert_eq!(seen, 37);
    }
}
