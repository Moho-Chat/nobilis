//! The messages Steam's connection managers exchange, as far as this backend
//! speaks them.
//!
//! Written out by hand, from the definitions extracted out of the official
//! client (`steammessages_*.proto`), rather than generated: there are a few
//! dozen messages here out of the thousands in those files, a build that needs
//! `protoc` is a build some machine cannot make, and a hand-written struct
//! shows at a glance which fields this client reads and sends. The cost is that
//! a wrong tag number is silent - the field simply never arrives - so every
//! message here is decoded in `tests` from bytes that Valve's own definitions
//! produced, and the tests say how to make more.
//!
//! Every field is optional, as in proto2, and a missing one is the schema's
//! default and not a mistake: `eresult` in particular defaults to 2 (a failure),
//! which is why success has to be on the wire to be seen.
//!
//! Enums are carried as the plain integers they are on the wire, and given
//! names where they are used.

use prost::Message;

/// The header every protobuf packet carries ahead of its body.
///
/// Only what a client needs of it: who it is for, which request it answers or
/// starts, and how it went. Fields not named here are skipped on decoding.
#[derive(Clone, PartialEq, Message)]
pub struct Header {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    pub client_sessionid: Option<i32>,
    #[prost(uint32, optional, tag = "3")]
    pub routing_appid: Option<u32>,
    #[prost(fixed64, optional, tag = "10")]
    pub jobid_source: Option<u64>,
    #[prost(fixed64, optional, tag = "11")]
    pub jobid_target: Option<u64>,
    #[prost(string, optional, tag = "12")]
    pub target_job_name: Option<String>,
    #[prost(int32, optional, tag = "13")]
    pub eresult: Option<i32>,
    #[prost(string, optional, tag = "14")]
    pub error_message: Option<String>,
    #[prost(uint32, optional, tag = "32")]
    pub realm: Option<u32>,
}

/// A run of packets, usually compressed, sent as one.
#[derive(Clone, PartialEq, Message)]
pub struct Multi {
    /// The length once unzipped; absent when the body is not compressed.
    #[prost(uint32, optional, tag = "1")]
    pub size_unzipped: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub message_body: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct IpAddress {
    #[prost(oneof = "ip_address::Ip", tags = "1, 2")]
    pub ip: Option<ip_address::Ip>,
}

pub mod ip_address {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Ip {
        #[prost(fixed32, tag = "1")]
        V4(u32),
        #[prost(bytes, tag = "2")]
        V6(Vec<u8>),
    }
}

// --- the session -----------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct ClientHello {
    #[prost(uint32, optional, tag = "1")]
    pub protocol_version: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientHeartBeat {
    #[prost(bool, optional, tag = "1")]
    pub send_reply: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientLogon {
    #[prost(uint32, optional, tag = "1")]
    pub protocol_version: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub cell_id: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub client_package_version: Option<u32>,
    #[prost(string, optional, tag = "6")]
    pub client_language: Option<String>,
    #[prost(uint32, optional, tag = "7")]
    pub client_os_type: Option<u32>,
    #[prost(bool, optional, tag = "8")]
    pub should_remember_password: Option<bool>,
    #[prost(message, optional, tag = "11")]
    pub obfuscated_private_ip: Option<IpAddress>,
    #[prost(uint32, optional, tag = "21")]
    pub qos_level: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "30")]
    pub machine_id: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "32")]
    pub ui_mode: Option<u32>,
    #[prost(uint32, optional, tag = "33")]
    pub chat_mode: Option<u32>,
    #[prost(string, optional, tag = "50")]
    pub account_name: Option<String>,
    #[prost(bool, optional, tag = "95")]
    pub steamguard_dont_remember_computer: Option<bool>,
    #[prost(string, optional, tag = "96")]
    pub machine_name: Option<String>,
    #[prost(uint64, optional, tag = "100")]
    pub client_instance_id: Option<u64>,
    #[prost(bool, optional, tag = "102")]
    pub supports_rate_limit_response: Option<bool>,
    /// The refresh token from signing in, in spite of the name.
    #[prost(string, optional, tag = "108")]
    pub access_token: Option<String>,
    #[prost(uint32, optional, tag = "111")]
    pub gaming_device_type: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientLogonResponse {
    #[prost(int32, optional, tag = "1")]
    pub eresult: Option<i32>,
    #[prost(int32, optional, tag = "3")]
    pub heartbeat_seconds: Option<i32>,
    #[prost(fixed32, optional, tag = "5")]
    pub rtime32_server_time: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub account_flags: Option<u32>,
    #[prost(uint32, optional, tag = "7")]
    pub cell_id: Option<u32>,
    #[prost(int32, optional, tag = "10")]
    pub eresult_extended: Option<i32>,
    #[prost(string, optional, tag = "14")]
    pub vanity_url: Option<String>,
    #[prost(fixed64, optional, tag = "20")]
    pub client_supplied_steamid: Option<u64>,
    #[prost(string, optional, tag = "21")]
    pub ip_country_code: Option<String>,
    #[prost(uint64, optional, tag = "27")]
    pub client_instance_id: Option<u64>,
    #[prost(uint64, optional, tag = "30")]
    pub token_id: Option<u64>,
    #[prost(uint64, optional, tag = "31")]
    pub family_group_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientLogOff {}

#[derive(Clone, PartialEq, Message)]
pub struct ClientLoggedOff {
    #[prost(int32, optional, tag = "1")]
    pub eresult: Option<i32>,
}

/// What the account is called, among other things about it.
#[derive(Clone, PartialEq, Message)]
pub struct ClientAccountInfo {
    #[prost(string, optional, tag = "1")]
    pub persona_name: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub ip_country: Option<String>,
    #[prost(int32, optional, tag = "5")]
    pub count_authed_computers: Option<i32>,
    #[prost(uint32, optional, tag = "7")]
    pub account_flags: Option<u32>,
    #[prost(uint32, optional, tag = "17")]
    pub two_factor_state: Option<u32>,
}

// --- friends and presence --------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct FriendsListEntry {
    #[prost(fixed64, optional, tag = "1")]
    pub ulfriendid: Option<u64>,
    #[prost(uint32, optional, tag = "2")]
    pub efriendrelationship: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientFriendsList {
    /// A change to a list already held, rather than the whole of it.
    #[prost(bool, optional, tag = "1")]
    pub bincremental: Option<bool>,
    #[prost(message, repeated, tag = "2")]
    pub friends: Vec<FriendsListEntry>,
    #[prost(uint32, optional, tag = "3")]
    pub max_friend_count: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub active_friend_count: Option<u32>,
    #[prost(bool, optional, tag = "5")]
    pub friends_limit_hit: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyValue {
    #[prost(string, optional, tag = "1")]
    pub key: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub value: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PersonaFriend {
    #[prost(fixed64, optional, tag = "1")]
    pub friendid: Option<u64>,
    #[prost(uint32, optional, tag = "2")]
    pub persona_state: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub game_played_app_id: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub persona_state_flags: Option<u32>,
    #[prost(bool, optional, tag = "10")]
    pub persona_set_by_user: Option<bool>,
    #[prost(string, optional, tag = "15")]
    pub player_name: Option<String>,
    #[prost(bytes = "vec", optional, tag = "31")]
    pub avatar_hash: Option<Vec<u8>>,
    #[prost(uint32, optional, tag = "45")]
    pub last_logoff: Option<u32>,
    #[prost(uint32, optional, tag = "46")]
    pub last_logon: Option<u32>,
    #[prost(uint32, optional, tag = "47")]
    pub last_seen_online: Option<u32>,
    #[prost(string, optional, tag = "55")]
    pub game_name: Option<String>,
    #[prost(fixed64, optional, tag = "56")]
    pub gameid: Option<u64>,
    #[prost(message, repeated, tag = "71")]
    pub rich_presence: Vec<KeyValue>,
    #[prost(bool, optional, tag = "81")]
    pub on_steam_deck: Option<bool>,
    #[prost(uint32, optional, tag = "83")]
    pub gaming_device_type: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientPersonaState {
    #[prost(uint32, optional, tag = "1")]
    pub status_flags: Option<u32>,
    #[prost(message, repeated, tag = "2")]
    pub friends: Vec<PersonaFriend>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientRequestFriendData {
    /// Which of a friend's details to send: a sum of the `PERSONA_*` flags.
    #[prost(uint32, optional, tag = "1")]
    pub persona_state_requested: Option<u32>,
    #[prost(fixed64, repeated, packed = "false", tag = "2")]
    pub friends: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientChangeStatus {
    #[prost(uint32, optional, tag = "1")]
    pub persona_state: Option<u32>,
    #[prost(string, optional, tag = "2")]
    pub player_name: Option<String>,
    #[prost(bool, optional, tag = "3")]
    pub is_auto_generated_name: Option<bool>,
    #[prost(bool, optional, tag = "4")]
    pub high_priority: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    pub persona_set_by_user: Option<bool>,
    #[prost(uint32, optional, tag = "6")]
    pub persona_state_flags: Option<u32>,
    #[prost(bool, optional, tag = "7")]
    pub need_persona_response: Option<bool>,
    #[prost(bool, optional, tag = "8")]
    pub is_client_idle: Option<bool>,
}

/// Asks to become friends with somebody, or accepts their request: Steam
/// treats the two as one thing, and which it is depends on whether they have
/// already asked. Exactly one of the fields is given.
#[derive(Clone, PartialEq, Message)]
pub struct ClientAddFriend {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_to_add: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub accountname_or_email_to_add: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientAddFriendResponse {
    /// Absent means 2 (fail), as everywhere in Steam's definitions.
    #[prost(int32, optional, tag = "1")]
    pub eresult: Option<i32>,
    #[prost(fixed64, optional, tag = "2")]
    pub steam_id_added: Option<u64>,
    #[prost(string, optional, tag = "3")]
    pub persona_name_added: Option<String>,
}

/// Ends a friendship, withdraws a request we made, or declines one made to us.
#[derive(Clone, PartialEq, Message)]
pub struct ClientRemoveFriend {
    #[prost(fixed64, optional, tag = "1")]
    pub friendid: Option<u64>,
}

// --- messages --------------------------------------------------------------

/// A friend typing, or the older route for a message; messages themselves
/// travel as service calls (`FriendMessages.SendMessage`).
#[derive(Clone, PartialEq, Message)]
pub struct ClientFriendMsg {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    pub chat_entry_type: Option<i32>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub message: Option<Vec<u8>>,
    #[prost(fixed32, optional, tag = "4")]
    pub rtime32_server_timestamp: Option<u32>,
    #[prost(bool, optional, tag = "5")]
    pub echo_to_sender: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientFriendMsgIncoming {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_from: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    pub chat_entry_type: Option<i32>,
    #[prost(bool, optional, tag = "3")]
    pub from_limited_account: Option<bool>,
    #[prost(bytes = "vec", optional, tag = "4")]
    pub message: Option<Vec<u8>>,
    #[prost(fixed32, optional, tag = "5")]
    pub rtime32_server_timestamp: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FriendMessagesSendRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    pub chat_entry_type: Option<i32>,
    #[prost(string, optional, tag = "3")]
    pub message: Option<String>,
    #[prost(bool, optional, tag = "4")]
    pub contains_bbcode: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    pub echo_to_sender: Option<bool>,
    #[prost(bool, optional, tag = "6")]
    pub low_priority: Option<bool>,
    #[prost(string, optional, tag = "8")]
    pub client_message_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FriendMessagesSendResponse {
    #[prost(string, optional, tag = "1")]
    pub modified_message: Option<String>,
    #[prost(uint32, optional, tag = "2")]
    pub server_timestamp: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub ordinal: Option<u32>,
    #[prost(string, optional, tag = "4")]
    pub message_without_bb_code: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FriendMessagesIncoming {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_friend: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    pub chat_entry_type: Option<i32>,
    #[prost(bool, optional, tag = "3")]
    pub from_limited_account: Option<bool>,
    #[prost(string, optional, tag = "4")]
    pub message: Option<String>,
    #[prost(fixed32, optional, tag = "5")]
    pub rtime32_server_timestamp: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub ordinal: Option<u32>,
    /// A message this account sent from one of its other sessions.
    #[prost(bool, optional, tag = "7")]
    pub local_echo: Option<bool>,
    #[prost(string, optional, tag = "8")]
    pub message_no_bbcode: Option<String>,
    #[prost(bool, optional, tag = "9")]
    pub low_priority: Option<bool>,
}

// --- history and what has been read ---------------------------------------

/// Asks for a stretch of a conversation, newest first. `time_last` and
/// `ordinal_last` bound it from above: what is asked for is older than that.
#[derive(Clone, PartialEq, Message)]
pub struct GetRecentMessagesRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid1: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid2: Option<u64>,
    #[prost(uint32, optional, tag = "3")]
    pub count: Option<u32>,
    #[prost(bool, optional, tag = "4")]
    pub most_recent_conversation: Option<bool>,
    #[prost(fixed32, optional, tag = "5")]
    pub rtime32_start_time: Option<u32>,
    #[prost(bool, optional, tag = "6")]
    pub bbcode_format: Option<bool>,
    #[prost(uint32, optional, tag = "7")]
    pub start_ordinal: Option<u32>,
    #[prost(uint32, optional, tag = "8")]
    pub time_last: Option<u32>,
    #[prost(uint32, optional, tag = "9")]
    pub ordinal_last: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PastReaction {
    #[prost(int32, optional, tag = "1")]
    pub reaction_type: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub reaction: Option<String>,
    #[prost(uint32, repeated, packed = "false", tag = "3")]
    pub reactors: Vec<u32>,
}

/// One message out of the history, by account number rather than 64-bit id.
#[derive(Clone, PartialEq, Message)]
pub struct PastMessage {
    #[prost(uint32, optional, tag = "1")]
    pub accountid: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub timestamp: Option<u32>,
    #[prost(string, optional, tag = "3")]
    pub message: Option<String>,
    #[prost(uint32, optional, tag = "4")]
    pub ordinal: Option<u32>,
    #[prost(message, repeated, tag = "5")]
    pub reactions: Vec<PastReaction>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetRecentMessagesResponse {
    #[prost(message, repeated, tag = "1")]
    pub messages: Vec<PastMessage>,
    #[prost(bool, optional, tag = "4")]
    pub more_available: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetActiveSessionsRequest {
    #[prost(uint32, optional, tag = "1")]
    pub lastmessage_since: Option<u32>,
    #[prost(bool, optional, tag = "2")]
    pub only_sessions_with_messages: Option<bool>,
}

/// What Steam keeps of one conversation: when it last had a message, when it
/// was last looked at, and how many have come since.
#[derive(Clone, PartialEq, Message)]
pub struct MessageSession {
    #[prost(uint32, optional, tag = "1")]
    pub accountid_friend: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub last_message: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub last_view: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub unread_message_count: Option<u32>,
    #[prost(int32, repeated, packed = "false", tag = "5")]
    pub notices: Vec<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetActiveSessionsResponse {
    #[prost(message, repeated, tag = "1")]
    pub message_sessions: Vec<MessageSession>,
    #[prost(uint32, optional, tag = "2")]
    pub timestamp: Option<u32>,
}

/// Says a conversation has been read up to a moment. Sent to Steam when it is
/// read here, and sent by Steam (as an echo) when it is read anywhere else.
#[derive(Clone, PartialEq, Message)]
pub struct AckMessage {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_partner: Option<u64>,
    #[prost(uint32, optional, tag = "2")]
    pub timestamp: Option<u32>,
}

// --- reactions -------------------------------------------------------------

/// Puts a reaction on a message, or takes ours off. The message is named by when
/// Steam stamped it and where it fell in that second.
#[derive(Clone, PartialEq, Message)]
pub struct UpdateMessageReactionRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid: Option<u64>,
    #[prost(uint32, optional, tag = "2")]
    pub server_timestamp: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub ordinal: Option<u32>,
    /// 1 an emoticon, 2 a sticker.
    #[prost(int32, optional, tag = "4")]
    pub reaction_type: Option<i32>,
    #[prost(string, optional, tag = "5")]
    pub reaction: Option<String>,
    #[prost(bool, optional, tag = "6")]
    pub is_add: Option<bool>,
}

/// Who now holds that reaction on the message, by account number.
#[derive(Clone, PartialEq, Message)]
pub struct UpdateMessageReactionResponse {
    #[prost(uint32, repeated, packed = "false", tag = "1")]
    pub reactors: Vec<u32>,
}

/// Somebody reacted to a message in a conversation with us, or took it back.
#[derive(Clone, PartialEq, Message)]
pub struct MessageReactionNotice {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_friend: Option<u64>,
    #[prost(uint32, optional, tag = "2")]
    pub server_timestamp: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub ordinal: Option<u32>,
    #[prost(fixed64, optional, tag = "4")]
    pub reactor: Option<u64>,
    #[prost(int32, optional, tag = "5")]
    pub reaction_type: Option<i32>,
    #[prost(string, optional, tag = "6")]
    pub reaction: Option<String>,
    #[prost(bool, optional, tag = "7")]
    pub is_add: Option<bool>,
}

// --- session notices -------------------------------------------------------

/// Steam flagging a conversation: `notice_type` 1 is "suspicious", a person who
/// may not be who they appear to be.
#[derive(Clone, PartialEq, Message)]
pub struct SessionNoticeNotification {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_friend: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    pub notice_type: Option<i32>,
    #[prost(bool, optional, tag = "3")]
    pub active: Option<bool>,
}

/// Says a notice has been seen, so Steam stops raising it.
#[derive(Clone, PartialEq, Message)]
pub struct DismissSessionNotice {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_friend: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    pub notice_type: Option<i32>,
}

// --- emoticons -------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct GetEmoticonListRequest {}

/// One emoticon an account has, by name, with how it has been used.
#[derive(Clone, PartialEq, Message)]
pub struct EmoticonInfo {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(int32, optional, tag = "2")]
    pub count: Option<i32>,
    #[prost(uint32, optional, tag = "3")]
    pub time_last_used: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub use_count: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub time_received: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub appid: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetEmoticonListResponse {
    #[prost(message, repeated, tag = "1")]
    pub emoticons: Vec<EmoticonInfo>,
}

// --- signing in ------------------------------------------------------------

#[derive(Clone, PartialEq, Message)]
pub struct GetPasswordRsaPublicKeyRequest {
    #[prost(string, optional, tag = "1")]
    pub account_name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetPasswordRsaPublicKeyResponse {
    #[prost(string, optional, tag = "1")]
    pub publickey_mod: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub publickey_exp: Option<String>,
    #[prost(uint64, optional, tag = "3")]
    pub timestamp: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeviceDetails {
    #[prost(string, optional, tag = "1")]
    pub device_friendly_name: Option<String>,
    #[prost(int32, optional, tag = "2")]
    pub platform_type: Option<i32>,
    #[prost(int32, optional, tag = "3")]
    pub os_type: Option<i32>,
    #[prost(uint32, optional, tag = "4")]
    pub gaming_device_type: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub client_count: Option<u32>,
    #[prost(bytes = "vec", optional, tag = "6")]
    pub machine_id: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "7")]
    pub app_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AllowedConfirmation {
    #[prost(int32, optional, tag = "1")]
    pub confirmation_type: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub associated_message: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BeginAuthViaCredentialsRequest {
    #[prost(string, optional, tag = "1")]
    pub device_friendly_name: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub account_name: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub encrypted_password: Option<String>,
    #[prost(uint64, optional, tag = "4")]
    pub encryption_timestamp: Option<u64>,
    #[prost(bool, optional, tag = "5")]
    pub remember_login: Option<bool>,
    #[prost(int32, optional, tag = "6")]
    pub platform_type: Option<i32>,
    #[prost(int32, optional, tag = "7")]
    pub persistence: Option<i32>,
    #[prost(string, optional, tag = "8")]
    pub website_id: Option<String>,
    #[prost(message, optional, tag = "9")]
    pub device_details: Option<DeviceDetails>,
    #[prost(string, optional, tag = "10")]
    pub guard_data: Option<String>,
    #[prost(uint32, optional, tag = "11")]
    pub language: Option<u32>,
    #[prost(int32, optional, tag = "12")]
    pub qos_level: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BeginAuthViaCredentialsResponse {
    #[prost(uint64, optional, tag = "1")]
    pub client_id: Option<u64>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub request_id: Option<Vec<u8>>,
    #[prost(float, optional, tag = "3")]
    pub interval: Option<f32>,
    #[prost(message, repeated, tag = "4")]
    pub allowed_confirmations: Vec<AllowedConfirmation>,
    #[prost(uint64, optional, tag = "5")]
    pub steamid: Option<u64>,
    #[prost(string, optional, tag = "6")]
    pub weak_token: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub agreement_session_url: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub extended_error_message: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BeginAuthViaQrRequest {
    #[prost(string, optional, tag = "1")]
    pub device_friendly_name: Option<String>,
    #[prost(int32, optional, tag = "2")]
    pub platform_type: Option<i32>,
    #[prost(message, optional, tag = "3")]
    pub device_details: Option<DeviceDetails>,
    #[prost(string, optional, tag = "4")]
    pub website_id: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BeginAuthViaQrResponse {
    #[prost(uint64, optional, tag = "1")]
    pub client_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub challenge_url: Option<String>,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub request_id: Option<Vec<u8>>,
    #[prost(float, optional, tag = "4")]
    pub interval: Option<f32>,
    #[prost(message, repeated, tag = "5")]
    pub allowed_confirmations: Vec<AllowedConfirmation>,
    #[prost(int32, optional, tag = "6")]
    pub version: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PollAuthSessionRequest {
    #[prost(uint64, optional, tag = "1")]
    pub client_id: Option<u64>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub request_id: Option<Vec<u8>>,
    #[prost(fixed64, optional, tag = "3")]
    pub token_to_revoke: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct PollAuthSessionResponse {
    #[prost(uint64, optional, tag = "1")]
    pub new_client_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub new_challenge_url: Option<String>,
    #[prost(string, optional, tag = "3")]
    pub refresh_token: Option<String>,
    #[prost(string, optional, tag = "4")]
    pub access_token: Option<String>,
    #[prost(bool, optional, tag = "5")]
    pub had_remote_interaction: Option<bool>,
    #[prost(string, optional, tag = "6")]
    pub account_name: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub new_guard_data: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub agreement_session_url: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpdateAuthWithGuardCodeRequest {
    #[prost(uint64, optional, tag = "1")]
    pub client_id: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid: Option<u64>,
    #[prost(string, optional, tag = "3")]
    pub code: Option<String>,
    #[prost(int32, optional, tag = "4")]
    pub code_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UpdateAuthWithGuardCodeResponse {
    #[prost(string, optional, tag = "7")]
    pub agreement_session_url: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GenerateAccessTokenRequest {
    #[prost(string, optional, tag = "1")]
    pub refresh_token: Option<String>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid: Option<u64>,
    #[prost(int32, optional, tag = "3")]
    pub renewal_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GenerateAccessTokenResponse {
    #[prost(string, optional, tag = "1")]
    pub access_token: Option<String>,
    #[prost(string, optional, tag = "2")]
    pub refresh_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One line per message: its name here, and the bytes Valve's own
    /// definitions made of a fully populated one. See `golden/make_golden.py`,
    /// which also fails if a field of any struct above is left out of it.
    const GOLDEN: &str = include_str!("golden/golden.txt");

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex")).collect()
    }

    /// Decodes into the type, and requires the same bytes back. A field with
    /// the wrong number is dropped on the way in and so missing on the way
    /// out; one with the wrong type fails to decode or comes out differently.
    fn round_trips<M: Message + Default>(name: &str, bytes: &[u8]) {
        let decoded = M::decode(bytes).unwrap_or_else(|e| panic!("{name} did not decode: {e}"));
        assert_eq!(decoded.encode_to_vec(), bytes, "{name} changed on the way through, so a field is wrong or missing");
    }

    #[test]
    fn every_message_matches_what_valves_definitions_produce() {
        let mut seen = 0;
        for line in GOLDEN.lines().filter(|l| !l.trim().is_empty()) {
            let (name, hex) = line.split_once(' ').expect("name and bytes");
            let bytes = unhex(hex);
            macro_rules! cases {
                ($($t:ident),* $(,)?) => {
                    match name {
                        $( stringify!($t) => round_trips::<$t>(name, &bytes), )*
                        other => panic!("golden.txt has {other}, which proto.rs does not"),
                    }
                };
            }
            cases!(
                Header, Multi, IpAddress, ClientHello, ClientHeartBeat, ClientLogon, ClientLogonResponse,
                ClientLogOff, ClientLoggedOff, ClientAccountInfo, FriendsListEntry, ClientFriendsList,
                KeyValue, PersonaFriend, ClientPersonaState, ClientRequestFriendData, ClientChangeStatus,
                ClientAddFriend, ClientAddFriendResponse, ClientRemoveFriend,
                ClientFriendMsg, ClientFriendMsgIncoming, FriendMessagesSendRequest,
                FriendMessagesSendResponse, FriendMessagesIncoming, GetPasswordRsaPublicKeyRequest,
                GetPasswordRsaPublicKeyResponse, DeviceDetails, AllowedConfirmation,
                BeginAuthViaCredentialsRequest, BeginAuthViaCredentialsResponse, BeginAuthViaQrRequest,
                BeginAuthViaQrResponse, PollAuthSessionRequest, PollAuthSessionResponse,
                UpdateAuthWithGuardCodeRequest, UpdateAuthWithGuardCodeResponse,
                GenerateAccessTokenRequest, GenerateAccessTokenResponse,
                GetRecentMessagesRequest, PastReaction, PastMessage, GetRecentMessagesResponse,
                GetActiveSessionsRequest, MessageSession, GetActiveSessionsResponse, AckMessage,
                GetEmoticonListRequest, EmoticonInfo, GetEmoticonListResponse,
                UpdateMessageReactionRequest, UpdateMessageReactionResponse, MessageReactionNotice,
                SessionNoticeNotification, DismissSessionNotice,
            );
            seen += 1;
        }
        assert_eq!(seen, 55, "a message was added without a vector, or one was lost");
    }

    /// Spot values too, so a failure above has something concrete to compare
    /// against: the fields a connection cannot work without.
    #[test]
    fn the_fields_a_connection_depends_on_are_where_steam_puts_them() {
        let line = GOLDEN.lines().find(|l| l.starts_with("ClientChangeStatus ")).unwrap();
        let status = ClientChangeStatus::decode(unhex(line.split_once(' ').unwrap().1).as_slice()).unwrap();
        assert_eq!(status.persona_state, Some(7), "invisible is persona state 7");
        assert_eq!(status.is_client_idle, Some(true));

        let line = GOLDEN.lines().find(|l| l.starts_with("Header ")).unwrap();
        let header = Header::decode(unhex(line.split_once(' ').unwrap().1).as_slice()).unwrap();
        assert_eq!(header.steamid, Some(76561197960287930));
        assert_eq!(header.client_sessionid, Some(-1234));
        assert_eq!((header.jobid_source, header.jobid_target), (Some(17), Some(18)));
        assert_eq!(header.target_job_name.as_deref(), Some("FriendMessagesClient.IncomingMessage#1"));

        let line = GOLDEN.lines().find(|l| l.starts_with("ClientLogon ")).unwrap();
        let logon = ClientLogon::decode(unhex(line.split_once(' ').unwrap().1).as_slice()).unwrap();
        assert_eq!(logon.access_token.as_deref(), Some("eyJ.refresh.token"));
        assert_eq!(logon.account_name.as_deref(), Some("someone"));
        assert_eq!(logon.obfuscated_private_ip.and_then(|ip| ip.ip), Some(ip_address::Ip::V4(0xdeadbeef)));
    }

    /// A message from a newer Steam, with fields this client does not know,
    /// still decodes: the ones it knows are read and the rest are skipped.
    #[test]
    fn fields_added_later_are_skipped_not_fatal() {
        let mut bytes = ClientChangeStatus { persona_state: Some(1), ..Default::default() }.encode_to_vec();
        // An unknown varint field 99, an unknown length-delimited field 100.
        bytes.extend_from_slice(&[0x98, 0x06, 0x05]);
        bytes.extend_from_slice(&[0xa2, 0x06, 0x03, b'a', b'b', b'c']);
        assert_eq!(ClientChangeStatus::decode(bytes.as_slice()).unwrap().persona_state, Some(1));
    }
}
