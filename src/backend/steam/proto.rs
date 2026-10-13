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

// A few friend-group messages keep Valve's own field names (`nGroupID`), which the
// golden check requires of every struct.
#![allow(non_snake_case)]

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

// --- profiles, levels, nicknames and preferences ---------------------------

#[derive(Clone, PartialEq, Message)]
pub struct FriendProfileInfoRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_friend: Option<u64>,
}

/// What somebody wrote on their profile: where they are, a headline, a summary.
#[derive(Clone, PartialEq, Message)]
pub struct FriendProfileInfoResponse {
    #[prost(int32, optional, tag = "1")]
    pub eresult: Option<i32>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid_friend: Option<u64>,
    #[prost(uint32, optional, tag = "3")]
    pub time_created: Option<u32>,
    #[prost(string, optional, tag = "4")]
    pub real_name: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub city_name: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub state_name: Option<String>,
    #[prost(string, optional, tag = "7")]
    pub country_name: Option<String>,
    #[prost(string, optional, tag = "8")]
    pub headline: Option<String>,
    #[prost(string, optional, tag = "9")]
    pub summary: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FriendsSteamLevelsRequest {
    #[prost(uint32, repeated, packed = "false", tag = "1")]
    pub accountids: Vec<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FriendLevel {
    #[prost(uint32, optional, tag = "1")]
    pub accountid: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub level: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct FriendsSteamLevelsResponse {
    #[prost(message, repeated, tag = "1")]
    pub friends: Vec<FriendLevel>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetPerFriendPreferencesRequest {}

/// What an account keeps about one friend: the name it calls them, and how
/// they may notify it. `ENotificationSetting`: 0 the default, 1 always, 2 never.
#[derive(Clone, PartialEq, Message)]
pub struct PerFriendPreferences {
    #[prost(fixed32, optional, tag = "1")]
    pub accountid: Option<u32>,
    #[prost(string, optional, tag = "2")]
    pub nickname: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub notifications_showingame: Option<i32>,
    #[prost(int32, optional, tag = "4")]
    pub notifications_showonline: Option<i32>,
    #[prost(int32, optional, tag = "5")]
    pub notifications_showmessages: Option<i32>,
    #[prost(int32, optional, tag = "6")]
    pub sounds_showingame: Option<i32>,
    #[prost(int32, optional, tag = "7")]
    pub sounds_showonline: Option<i32>,
    #[prost(int32, optional, tag = "8")]
    pub sounds_showmessages: Option<i32>,
    #[prost(int32, optional, tag = "9")]
    pub notifications_sendmobile: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetPerFriendPreferencesResponse {
    #[prost(message, repeated, tag = "1")]
    pub preferences: Vec<PerFriendPreferences>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetPerFriendPreferencesRequest {
    #[prost(message, optional, tag = "1")]
    pub preferences: Option<PerFriendPreferences>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetPerFriendPreferencesResponse {}

/// Said by Steam when preferences about one friend change, here or elsewhere.
#[derive(Clone, PartialEq, Message)]
pub struct PerFriendPreferencesChanged {
    #[prost(fixed32, optional, tag = "1")]
    pub accountid: Option<u32>,
    #[prost(message, optional, tag = "2")]
    pub preferences: Option<PerFriendPreferences>,
}

/// Somebody's nickname changed on one of the account's clients.
#[derive(Clone, PartialEq, Message)]
pub struct FriendNicknameChanged {
    #[prost(fixed32, optional, tag = "1")]
    pub accountid: Option<u32>,
    #[prost(string, optional, tag = "2")]
    pub nickname: Option<String>,
    #[prost(bool, optional, tag = "3")]
    pub is_echo_to_self: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct IgnoreFriendRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid: Option<u64>,
    #[prost(bool, optional, tag = "2")]
    pub unignore: Option<bool>,
}

/// What the relationship is now, `EFriendRelationship`.
#[derive(Clone, PartialEq, Message)]
pub struct IgnoreFriendResponse {
    #[prost(uint32, optional, tag = "1")]
    pub friend_relationship: Option<u32>,
}

// --- emoticons -------------------------------------------------------------
/// Asks for the emoticons, stickers and effects an account has, in one answer.
#[derive(Clone, PartialEq, Message)]
pub struct ClientGetEmoticonList {}

#[derive(Clone, PartialEq, Message)]
pub struct EmoticonOwned {
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
pub struct StickerOwned {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(int32, optional, tag = "2")]
    pub count: Option<i32>,
    #[prost(uint32, optional, tag = "3")]
    pub time_received: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub appid: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub time_last_used: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub use_count: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct EffectOwned {
    #[prost(string, optional, tag = "1")]
    pub name: Option<String>,
    #[prost(int32, optional, tag = "2")]
    pub count: Option<i32>,
    #[prost(uint32, optional, tag = "3")]
    pub time_received: Option<u32>,
    #[prost(bool, optional, tag = "4")]
    pub infinite_use: Option<bool>,
    #[prost(uint32, optional, tag = "5")]
    pub appid: Option<u32>,
}

/// One of the groups the account has sorted its friends into.
#[derive(Clone, PartialEq, Message)]
pub struct FriendGroup {
    #[prost(int32, optional, tag = "1")]
    pub nGroupID: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub strGroupName: Option<String>,
}

/// One friend's place in a group.
#[derive(Clone, PartialEq, Message)]
pub struct FriendGroupMembership {
    #[prost(fixed64, optional, tag = "1")]
    pub ulSteamID: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    pub nGroupID: Option<i32>,
}

/// The groups and who is in them: all of it at sign-in, then what changed.
/// A list that says `removal` is of things taken away.
#[derive(Clone, PartialEq, Message)]
pub struct ClientFriendsGroupsList {
    #[prost(bool, optional, tag = "1")]
    pub bremoval: Option<bool>,
    #[prost(bool, optional, tag = "2")]
    pub bincremental: Option<bool>,
    #[prost(message, repeated, tag = "3")]
    pub friendGroups: Vec<FriendGroup>,
    #[prost(message, repeated, tag = "4")]
    pub memberships: Vec<FriendGroupMembership>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientCreateFriendsGroup {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub groupname: Option<String>,
    #[prost(fixed64, repeated, packed = "false", tag = "3")]
    pub steamid_friends: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientCreateFriendsGroupResponse {
    #[prost(uint32, optional, tag = "1")]
    pub eresult: Option<u32>,
    #[prost(int32, optional, tag = "2")]
    pub groupid: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientDeleteFriendsGroup {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid: Option<u64>,
    #[prost(int32, optional, tag = "2")]
    pub groupid: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientDeleteFriendsGroupResponse {
    #[prost(uint32, optional, tag = "1")]
    pub eresult: Option<u32>,
}

/// Renames a group, or adds and takes away friends, in one.
#[derive(Clone, PartialEq, Message)]
pub struct ClientManageFriendsGroup {
    #[prost(int32, optional, tag = "1")]
    pub groupid: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub groupname: Option<String>,
    #[prost(fixed64, repeated, packed = "false", tag = "3")]
    pub steamid_friends_added: Vec<u64>,
    #[prost(fixed64, repeated, packed = "false", tag = "4")]
    pub steamid_friends_removed: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientManageFriendsGroupResponse {
    #[prost(uint32, optional, tag = "1")]
    pub eresult: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientAddFriendToGroup {
    #[prost(int32, optional, tag = "1")]
    pub groupid: Option<i32>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamiduser: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientAddFriendToGroupResponse {
    #[prost(uint32, optional, tag = "1")]
    pub eresult: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientRemoveFriendFromGroup {
    #[prost(int32, optional, tag = "1")]
    pub groupid: Option<i32>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamiduser: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ClientRemoveFriendFromGroupResponse {
    #[prost(uint32, optional, tag = "1")]
    pub eresult: Option<u32>,
}

/// What the account has set for how text from others is filtered.
#[derive(Clone, PartialEq, Message)]
pub struct GetCommunityPreferencesRequest {}

#[derive(Clone, PartialEq, Message)]
pub struct CommunityPreferences {
    #[prost(uint32, optional, tag = "3")]
    pub timestamp_updated: Option<u32>,
    #[prost(bool, optional, tag = "4")]
    pub parenthesize_nicknames: Option<bool>,
    /// `ETextFilterSetting`: 0 opted out of the lab, 1 on, 2 on but profanity allowed, 3 off.
    #[prost(int32, optional, tag = "5")]
    pub text_filter_setting: Option<i32>,
    #[prost(bool, optional, tag = "6")]
    pub text_filter_ignore_friends: Option<bool>,
    #[prost(uint32, optional, tag = "7")]
    pub text_filter_words_revision: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetCommunityPreferencesResponse {
    #[prost(message, optional, tag = "1")]
    pub preferences: Option<CommunityPreferences>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetTextFilterWordsRequest {}

#[derive(Clone, PartialEq, Message)]
pub struct TextFilterWords {
    #[prost(string, repeated, tag = "1")]
    pub text_filter_custom_banned_words: Vec<String>,
    #[prost(string, repeated, tag = "2")]
    pub text_filter_custom_clean_words: Vec<String>,
    #[prost(uint32, optional, tag = "3")]
    pub text_filter_words_revision: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetTextFilterWordsResponse {
    #[prost(message, optional, tag = "1")]
    pub words: Option<TextFilterWords>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetPrivacySettingsRequest {}

/// Who may see what on the account: 1 private, 2 friends only, 3 public.
#[derive(Clone, PartialEq, Message)]
pub struct PrivacySettings {
    #[prost(int32, optional, tag = "1")]
    pub privacy_state: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub privacy_state_inventory: Option<i32>,
    #[prost(int32, optional, tag = "3")]
    pub privacy_state_gifts: Option<i32>,
    #[prost(int32, optional, tag = "4")]
    pub privacy_state_ownedgames: Option<i32>,
    #[prost(int32, optional, tag = "5")]
    pub privacy_state_playtime: Option<i32>,
    #[prost(int32, optional, tag = "6")]
    pub privacy_state_friendslist: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetPrivacySettingsResponse {
    #[prost(message, optional, tag = "1")]
    pub privacy_settings: Option<PrivacySettings>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetParentalSettingsRequest {
    #[prost(fixed64, optional, tag = "10")]
    pub steamid: Option<u64>,
}

/// Family View, as far as saying whether it is on and what it holds back.
#[derive(Clone, PartialEq, Message)]
pub struct ParentalSettings {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid: Option<u64>,
    #[prost(bool, optional, tag = "9")]
    pub is_enabled: Option<bool>,
    #[prost(uint32, optional, tag = "10")]
    pub enabled_features: Option<u32>,
    #[prost(uint32, optional, tag = "13")]
    pub temporary_enabled_features: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetParentalSettingsResponse {
    #[prost(message, optional, tag = "1")]
    pub settings: Option<ParentalSettings>,
}

#[derive(Clone, PartialEq, Message)]
pub struct EnumerateTokensRequest {
    #[prost(bool, optional, tag = "1")]
    pub include_revoked: Option<bool>,
}

/// Where and when a signed-in session was first or last seen.
#[derive(Clone, PartialEq, Message)]
pub struct TokenUsageEvent {
    #[prost(uint32, optional, tag = "1")]
    pub time: Option<u32>,
    #[prost(string, optional, tag = "4")]
    pub country: Option<String>,
    #[prost(string, optional, tag = "5")]
    pub state: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub city: Option<String>,
}

/// One signed-in session of the account: a phone, a browser, a launcher.
#[derive(Clone, PartialEq, Message)]
pub struct RefreshTokenDescription {
    #[prost(fixed64, optional, tag = "1")]
    pub token_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub token_description: Option<String>,
    #[prost(uint32, optional, tag = "3")]
    pub time_updated: Option<u32>,
    #[prost(int32, optional, tag = "4")]
    pub platform_type: Option<i32>,
    #[prost(bool, optional, tag = "5")]
    pub logged_in: Option<bool>,
    #[prost(message, optional, tag = "9")]
    pub first_seen: Option<TokenUsageEvent>,
    #[prost(message, optional, tag = "10")]
    pub last_seen: Option<TokenUsageEvent>,
}

#[derive(Clone, PartialEq, Message)]
pub struct EnumerateTokensResponse {
    #[prost(message, repeated, tag = "1")]
    pub refresh_tokens: Vec<RefreshTokenDescription>,
    #[prost(fixed64, optional, tag = "2")]
    pub requesting_token: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RevokeRefreshTokenRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub token_id: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RevokeRefreshTokenResponse {}


// ---- Steam chat groups: the messages this backend speaks, by Valve's own field names ----

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoomState {
    #[prost(uint64, optional, tag = "1")]
    pub chat_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub chat_name: Option<String>,
    #[prost(bool, optional, tag = "3")]
    pub voice_allowed: Option<bool>,
    #[prost(uint32, optional, tag = "5")]
    pub time_last_message: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub sort_order: Option<u32>,
    #[prost(string, optional, tag = "7")]
    pub last_message: Option<String>,
    #[prost(uint32, optional, tag = "8")]
    pub accountid_last_message: Option<u32>,
    #[prost(uint32, repeated, packed = "false", tag = "4")]
    pub members_in_voice: Vec<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoomMember {
    #[prost(uint32, optional, tag = "1")]
    pub accountid: Option<u32>,
    #[prost(int32, optional, tag = "3")]
    pub state: Option<i32>,
    #[prost(int32, optional, tag = "4")]
    pub rank: Option<i32>,
    #[prost(uint32, optional, tag = "6")]
    pub time_kick_expire: Option<u32>,
    #[prost(uint64, repeated, packed = "false", tag = "7")]
    pub role_ids: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRole {
    #[prost(uint64, optional, tag = "1")]
    pub role_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub name: Option<String>,
    #[prost(uint32, optional, tag = "3")]
    pub ordinal: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoomGroupHeaderState {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub chat_name: Option<String>,
    #[prost(uint32, optional, tag = "13")]
    pub clanid: Option<u32>,
    #[prost(uint32, optional, tag = "14")]
    pub accountid_owner: Option<u32>,
    #[prost(string, optional, tag = "15")]
    pub tagline: Option<String>,
    #[prost(bytes = "vec", optional, tag = "16")]
    pub avatar_sha: Option<Vec<u8>>,
    #[prost(uint64, optional, tag = "17")]
    pub default_role_id: Option<u64>,
    #[prost(message, repeated, tag = "18")]
    pub roles: Vec<ChatRole>,
    #[prost(message, repeated, tag = "19")]
    pub role_actions: Vec<ChatRoleActions>,
    #[prost(string, optional, tag = "25")]
    pub avatar_ugc_url: Option<String>,
    #[prost(bool, optional, tag = "26")]
    pub disabled: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoomGroupState {
    #[prost(message, optional, tag = "1")]
    pub header_state: Option<ChatRoomGroupHeaderState>,
    #[prost(message, repeated, tag = "2")]
    pub members: Vec<ChatRoomMember>,
    #[prost(uint64, optional, tag = "4")]
    pub default_chat_id: Option<u64>,
    #[prost(message, repeated, tag = "5")]
    pub chat_rooms: Vec<ChatRoomState>,
    #[prost(message, repeated, tag = "7")]
    pub kicked: Vec<ChatRoomMember>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UserChatRoomState {
    #[prost(uint64, optional, tag = "1")]
    pub chat_id: Option<u64>,
    #[prost(uint32, optional, tag = "2")]
    pub time_joined: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub time_last_ack: Option<u32>,
    #[prost(int32, optional, tag = "4")]
    pub desktop_notification_level: Option<i32>,
    #[prost(int32, optional, tag = "5")]
    pub mobile_notification_level: Option<i32>,
    #[prost(uint32, optional, tag = "6")]
    pub time_last_mention: Option<u32>,
    #[prost(bool, optional, tag = "7")]
    pub unread_indicator_muted: Option<bool>,
    #[prost(uint32, optional, tag = "8")]
    pub time_first_unread: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct UserChatRoomGroupState {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint32, optional, tag = "2")]
    pub time_joined: Option<u32>,
    #[prost(message, repeated, tag = "3")]
    pub user_chat_room_state: Vec<UserChatRoomState>,
    #[prost(int32, optional, tag = "4")]
    pub desktop_notification_level: Option<i32>,
    #[prost(int32, optional, tag = "5")]
    pub mobile_notification_level: Option<i32>,
    #[prost(uint32, optional, tag = "6")]
    pub time_last_group_ack: Option<u32>,
    #[prost(bool, optional, tag = "7")]
    pub unread_indicator_muted: Option<bool>,
    #[prost(bool, optional, tag = "8")]
    pub direct_messages_allowed: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatGroupSummary {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub chat_group_name: Option<String>,
    #[prost(uint32, optional, tag = "3")]
    pub active_member_count: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub active_voice_member_count: Option<u32>,
    #[prost(uint64, optional, tag = "5")]
    pub default_chat_id: Option<u64>,
    #[prost(message, repeated, tag = "6")]
    pub chat_rooms: Vec<ChatRoomState>,
    #[prost(uint32, optional, tag = "7")]
    pub clanid: Option<u32>,
    #[prost(string, optional, tag = "8")]
    pub chat_group_tagline: Option<String>,
    #[prost(uint32, optional, tag = "9")]
    pub accountid_owner: Option<u32>,
    #[prost(uint32, repeated, packed = "false", tag = "10")]
    pub top_members: Vec<u32>,
    #[prost(bytes = "vec", optional, tag = "11")]
    pub chat_group_avatar_sha: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "12")]
    pub rank: Option<i32>,
    #[prost(uint64, optional, tag = "13")]
    pub default_role_id: Option<u64>,
    #[prost(uint64, repeated, packed = "false", tag = "14")]
    pub role_ids: Vec<u64>,
    #[prost(uint32, optional, tag = "17")]
    pub appid: Option<u32>,
    #[prost(string, optional, tag = "21")]
    pub avatar_ugc_url: Option<String>,
    #[prost(bool, optional, tag = "22")]
    pub disabled: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoomSummaryPair {
    #[prost(message, optional, tag = "1")]
    pub user_chat_group_state: Option<UserChatRoomGroupState>,
    #[prost(message, optional, tag = "2")]
    pub group_summary: Option<ChatGroupSummary>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetMyChatRoomGroupsRequest {}

#[derive(Clone, PartialEq, Message)]
pub struct GetMyChatRoomGroupsResponse {
    #[prost(message, repeated, tag = "1")]
    pub chat_room_groups: Vec<ChatRoomSummaryPair>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetChatRoomGroupStateRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetChatRoomGroupStateResponse {
    #[prost(message, optional, tag = "1")]
    pub state: Option<ChatRoomGroupState>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetSessionActiveGroupsRequest {
    #[prost(uint64, repeated, packed = "false", tag = "1")]
    pub chat_group_ids: Vec<u64>,
    #[prost(uint64, repeated, packed = "false", tag = "2")]
    pub chat_groups_data_requested: Vec<u64>,
    #[prost(int32, optional, tag = "3")]
    pub virtualize_members_threshold: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetSessionActiveGroupsResponse {
    #[prost(message, repeated, tag = "1")]
    pub chat_states: Vec<ChatRoomGroupState>,
    #[prost(uint64, repeated, packed = "false", tag = "2")]
    pub virtualize_members_chat_group_ids: Vec<u64>,
    #[prost(message, repeated, tag = "3")]
    pub chat_group_notices: Vec<ChatRoomGroupMemberNotices>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatSendRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
    #[prost(string, optional, tag = "3")]
    pub message: Option<String>,
    #[prost(bool, optional, tag = "4")]
    pub echo_to_sender: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatSendResponse {
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
pub struct ChatHistoryRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
    #[prost(uint32, optional, tag = "3")]
    pub last_time: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub last_ordinal: Option<u32>,
    #[prost(uint32, optional, tag = "5")]
    pub start_time: Option<u32>,
    #[prost(uint32, optional, tag = "6")]
    pub start_ordinal: Option<u32>,
    #[prost(uint32, optional, tag = "7")]
    pub max_count: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ServerMessage {
    #[prost(int32, optional, tag = "1")]
    pub message: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub string_param: Option<String>,
    #[prost(uint32, optional, tag = "3")]
    pub accountid_param: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatMessageReaction {
    #[prost(int32, optional, tag = "1")]
    pub reaction_type: Option<i32>,
    #[prost(string, optional, tag = "2")]
    pub reaction: Option<String>,
    #[prost(uint32, optional, tag = "3")]
    pub num_reactors: Option<u32>,
    #[prost(bool, optional, tag = "4")]
    pub has_user_reacted: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatHistoryMessage {
    #[prost(uint32, optional, tag = "1")]
    pub sender: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub server_timestamp: Option<u32>,
    #[prost(string, optional, tag = "3")]
    pub message: Option<String>,
    #[prost(uint32, optional, tag = "4")]
    pub ordinal: Option<u32>,
    #[prost(message, optional, tag = "5")]
    pub server_message: Option<ServerMessage>,
    #[prost(bool, optional, tag = "6")]
    pub deleted: Option<bool>,
    #[prost(message, repeated, tag = "7")]
    pub reactions: Vec<ChatMessageReaction>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatHistoryResponse {
    #[prost(message, repeated, tag = "1")]
    pub messages: Vec<ChatHistoryMessage>,
    #[prost(bool, optional, tag = "4")]
    pub more_available: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatMentions {
    #[prost(bool, optional, tag = "1")]
    pub mention_all: Option<bool>,
    #[prost(bool, optional, tag = "2")]
    pub mention_here: Option<bool>,
    #[prost(uint32, repeated, packed = "false", tag = "3")]
    pub mention_accountids: Vec<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatIncoming {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
    #[prost(fixed64, optional, tag = "3")]
    pub steamid_sender: Option<u64>,
    #[prost(string, optional, tag = "4")]
    pub message: Option<String>,
    #[prost(uint32, optional, tag = "5")]
    pub timestamp: Option<u32>,
    #[prost(message, optional, tag = "6")]
    pub mentions: Option<ChatMentions>,
    #[prost(uint32, optional, tag = "7")]
    pub ordinal: Option<u32>,
    #[prost(message, optional, tag = "8")]
    pub server_message: Option<ServerMessage>,
    #[prost(string, optional, tag = "9")]
    pub message_no_bbcode: Option<String>,
    #[prost(string, optional, tag = "10")]
    pub chat_name: Option<String>,
    #[prost(string, optional, tag = "11")]
    pub notification_key: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatAck {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
    #[prost(uint32, optional, tag = "3")]
    pub timestamp: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatModifiedMessage {
    #[prost(uint32, optional, tag = "1")]
    pub server_timestamp: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub ordinal: Option<u32>,
    #[prost(bool, optional, tag = "3")]
    pub deleted: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatModified {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
    #[prost(message, repeated, tag = "3")]
    pub messages: Vec<ChatModifiedMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatMemberChange {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(message, optional, tag = "2")]
    pub member: Option<ChatRoomMember>,
    #[prost(int32, optional, tag = "3")]
    pub change: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatHeaderChange {
    #[prost(message, optional, tag = "1")]
    pub header_state: Option<ChatRoomGroupHeaderState>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoomsChange {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub default_chat_id: Option<u64>,
    #[prost(message, repeated, tag = "3")]
    pub chat_rooms: Vec<ChatRoomState>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatUserStateChange {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(message, optional, tag = "2")]
    pub user_chat_group_state: Option<UserChatRoomGroupState>,
    #[prost(message, optional, tag = "3")]
    pub group_summary: Option<ChatGroupSummary>,
    #[prost(int32, optional, tag = "4")]
    pub user_action: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CreateChatGroupRequest {
    #[prost(fixed64, optional, tag = "1")]
    pub steamid_partner: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid_invited: Option<u64>,
    #[prost(string, optional, tag = "3")]
    pub name: Option<String>,
    #[prost(fixed64, repeated, packed = "false", tag = "4")]
    pub steamid_invitees: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CreateChatGroupResponse {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(message, optional, tag = "2")]
    pub state: Option<ChatRoomGroupState>,
    #[prost(message, optional, tag = "3")]
    pub user_chat_state: Option<UserChatRoomGroupState>,
}

#[derive(Clone, PartialEq, Message)]
pub struct JoinChatGroupRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub invite_code: Option<String>,
    #[prost(uint64, optional, tag = "3")]
    pub chat_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct JoinChatGroupResponse {
    #[prost(message, optional, tag = "1")]
    pub state: Option<ChatRoomGroupState>,
    #[prost(message, optional, tag = "3")]
    pub user_chat_state: Option<UserChatRoomGroupState>,
    #[prost(uint64, optional, tag = "4")]
    pub join_chat_id: Option<u64>,
    #[prost(uint32, optional, tag = "5")]
    pub time_expire: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LeaveChatGroupRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct LeaveChatGroupResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct CreateChatRoomRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub name: Option<String>,
    #[prost(bool, optional, tag = "3")]
    pub allow_voice: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CreateChatRoomResponse {
    #[prost(message, optional, tag = "1")]
    pub chat_room: Option<ChatRoomState>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteChatRoomRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteChatRoomResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct RenameChatRoomRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
    #[prost(string, optional, tag = "3")]
    pub name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RenameChatRoomResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct RenameChatGroupRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RenameChatGroupResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct CreateInviteLinkRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint32, optional, tag = "2")]
    pub seconds_valid: Option<u32>,
    #[prost(uint64, optional, tag = "3")]
    pub chat_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CreateInviteLinkResponse {
    #[prost(string, optional, tag = "1")]
    pub invite_code: Option<String>,
    #[prost(uint32, optional, tag = "2")]
    pub seconds_valid: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetInviteLinksRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InviteLinkInfo {
    #[prost(string, optional, tag = "1")]
    pub invite_code: Option<String>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid_creator: Option<u64>,
    #[prost(uint32, optional, tag = "3")]
    pub time_expires: Option<u32>,
    #[prost(uint64, optional, tag = "4")]
    pub chat_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetInviteLinksResponse {
    #[prost(message, repeated, tag = "1")]
    pub invite_links: Vec<InviteLinkInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetInviteLinkInfoRequest {
    #[prost(string, optional, tag = "1")]
    pub invite_code: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetInviteLinkInfoResponse {
    #[prost(fixed64, optional, tag = "3")]
    pub steamid_sender: Option<u64>,
    #[prost(uint32, optional, tag = "4")]
    pub time_expires: Option<u32>,
    #[prost(uint64, optional, tag = "6")]
    pub chat_id: Option<u64>,
    #[prost(message, optional, tag = "8")]
    pub group_summary: Option<ChatGroupSummary>,
    #[prost(message, optional, tag = "9")]
    pub user_chat_group_state: Option<UserChatRoomGroupState>,
    #[prost(uint32, optional, tag = "10")]
    pub time_kick_expire: Option<u32>,
    #[prost(bool, optional, tag = "11")]
    pub banned: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteInviteLinkRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub invite_code: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteInviteLinkResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct KickChatUserRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid: Option<u64>,
    #[prost(int32, optional, tag = "3")]
    pub expiration: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct KickChatUserResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct BanChatUserRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid: Option<u64>,
    #[prost(bool, optional, tag = "3")]
    pub ban_state: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BanChatUserResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct MuteChatUserRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid: Option<u64>,
    #[prost(int32, optional, tag = "3")]
    pub expiration: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MuteChatUserResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteChatMessage {
    #[prost(uint32, optional, tag = "1")]
    pub server_timestamp: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub ordinal: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteChatMessagesRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
    #[prost(message, repeated, tag = "3")]
    pub messages: Vec<DeleteChatMessage>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteChatMessagesResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct ChatReactionRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
    #[prost(uint32, optional, tag = "3")]
    pub server_timestamp: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub ordinal: Option<u32>,
    #[prost(int32, optional, tag = "5")]
    pub reaction_type: Option<i32>,
    #[prost(string, optional, tag = "6")]
    pub reaction: Option<String>,
    #[prost(bool, optional, tag = "7")]
    pub is_add: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatReactionResponse {
    #[prost(uint32, optional, tag = "1")]
    pub num_reactors: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatReactionNotice {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub chat_id: Option<u64>,
    #[prost(uint32, optional, tag = "3")]
    pub server_timestamp: Option<u32>,
    #[prost(uint32, optional, tag = "4")]
    pub ordinal: Option<u32>,
    #[prost(fixed64, optional, tag = "5")]
    pub reactor: Option<u64>,
    #[prost(int32, optional, tag = "6")]
    pub reaction_type: Option<i32>,
    #[prost(string, optional, tag = "7")]
    pub reaction: Option<String>,
    #[prost(bool, optional, tag = "8")]
    pub is_add: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InviteFriendToChatGroupRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    pub chat_id: Option<u64>,
    #[prost(bool, optional, tag = "4")]
    pub skip_friendsui_check: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InviteFriendToChatGroupResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoleActions {
    #[prost(uint64, optional, tag = "1")]
    pub role_id: Option<u64>,
    #[prost(bool, optional, tag = "2")]
    pub can_create_rename_delete_channel: Option<bool>,
    #[prost(bool, optional, tag = "3")]
    pub can_kick: Option<bool>,
    #[prost(bool, optional, tag = "4")]
    pub can_ban: Option<bool>,
    #[prost(bool, optional, tag = "5")]
    pub can_invite: Option<bool>,
    #[prost(bool, optional, tag = "6")]
    pub can_change_tagline_avatar_name: Option<bool>,
    #[prost(bool, optional, tag = "7")]
    pub can_chat: Option<bool>,
    #[prost(bool, optional, tag = "8")]
    pub can_view_history: Option<bool>,
    #[prost(bool, optional, tag = "9")]
    pub can_change_group_roles: Option<bool>,
    #[prost(bool, optional, tag = "10")]
    pub can_change_user_roles: Option<bool>,
    #[prost(bool, optional, tag = "11")]
    pub can_mention_all: Option<bool>,
    #[prost(bool, optional, tag = "12")]
    pub can_set_watching_broadcast: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetTaglineRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub tagline: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetTaglineResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct UserChatGroupPreferences {
    #[prost(int32, optional, tag = "1")]
    pub desktop_notification_level: Option<i32>,
    #[prost(int32, optional, tag = "2")]
    pub mobile_notification_level: Option<i32>,
    #[prost(bool, optional, tag = "3")]
    pub unread_indicator_muted: Option<bool>,
    #[prost(bool, optional, tag = "4")]
    pub direct_messages_allowed: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetUserPreferencesRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(message, optional, tag = "2")]
    pub chat_group_preferences: Option<UserChatGroupPreferences>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetUserPreferencesResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct CreateRoleRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(string, optional, tag = "2")]
    pub name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CreateRoleResponse {
    #[prost(message, optional, tag = "2")]
    pub actions: Option<ChatRoleActions>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RenameRoleRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub role_id: Option<u64>,
    #[prost(string, optional, tag = "3")]
    pub name: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct RenameRoleResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteRoleRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub role_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteRoleResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct GetRoleActionsRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub role_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetRoleActionsResponse {
    #[prost(message, repeated, tag = "1")]
    pub actions: Vec<ChatRoleActions>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ReplaceRoleActionsRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "2")]
    pub role_id: Option<u64>,
    #[prost(message, optional, tag = "4")]
    pub actions: Option<ChatRoleActions>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ReplaceRoleActionsResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct AddRoleToUserRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    pub role_id: Option<u64>,
    #[prost(fixed64, optional, tag = "4")]
    pub steamid: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct AddRoleToUserResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteRoleFromUserRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(uint64, optional, tag = "3")]
    pub role_id: Option<u64>,
    #[prost(fixed64, optional, tag = "4")]
    pub steamid: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DeleteRoleFromUserResponse {}

#[derive(Clone, PartialEq, Message)]
pub struct GetBanListRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BanInfo {
    #[prost(uint32, optional, tag = "1")]
    pub accountid: Option<u32>,
    #[prost(uint32, optional, tag = "2")]
    pub accountid_actor: Option<u32>,
    #[prost(uint32, optional, tag = "3")]
    pub time_banned: Option<u32>,
    #[prost(string, optional, tag = "4")]
    pub ban_reason: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct GetBanListResponse {
    #[prost(message, repeated, tag = "1")]
    pub bans: Vec<BanInfo>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetAvatarRequest {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(bytes = "vec", optional, tag = "2")]
    pub avatar_sha: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SetAvatarResponse {
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoomNoticeNotification {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid_sender: Option<u64>,
    #[prost(int32, optional, tag = "3")]
    pub notice_type: Option<i32>,
    #[prost(bool, optional, tag = "4")]
    pub active: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct DismissChatRoomNotice {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(fixed64, optional, tag = "2")]
    pub steamid_sender: Option<u64>,
    #[prost(int32, optional, tag = "3")]
    pub notice_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoomMemberNotice {
    #[prost(uint32, optional, tag = "1")]
    pub accountid: Option<u32>,
    #[prost(int32, optional, tag = "2")]
    pub notice_type: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ChatRoomGroupMemberNotices {
    #[prost(uint64, optional, tag = "1")]
    pub chat_group_id: Option<u64>,
    #[prost(message, repeated, tag = "2")]
    pub member_notices: Vec<ChatRoomMemberNotice>,
}

/// Everything an account may put in a message beyond words.
#[derive(Clone, PartialEq, Message)]
pub struct ClientEmoticonList {
    #[prost(message, repeated, tag = "1")]
    pub emoticons: Vec<EmoticonOwned>,
    #[prost(message, repeated, tag = "2")]
    pub stickers: Vec<StickerOwned>,
    #[prost(message, repeated, tag = "3")]
    pub effects: Vec<EffectOwned>,
}


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
                FriendProfileInfoRequest, FriendProfileInfoResponse, FriendsSteamLevelsRequest, FriendLevel,
                FriendsSteamLevelsResponse, GetPerFriendPreferencesRequest, PerFriendPreferences,
                GetPerFriendPreferencesResponse, SetPerFriendPreferencesRequest, SetPerFriendPreferencesResponse,
                PerFriendPreferencesChanged, FriendNicknameChanged, IgnoreFriendRequest, IgnoreFriendResponse,
                ClientGetEmoticonList, EmoticonOwned, StickerOwned, EffectOwned, ClientEmoticonList,
                GetCommunityPreferencesRequest, CommunityPreferences, GetCommunityPreferencesResponse, GetTextFilterWordsRequest,
                TextFilterWords, GetTextFilterWordsResponse, GetPrivacySettingsRequest, PrivacySettings, GetPrivacySettingsResponse,
                GetParentalSettingsRequest, ParentalSettings, GetParentalSettingsResponse, EnumerateTokensRequest, TokenUsageEvent,
                RefreshTokenDescription, EnumerateTokensResponse, RevokeRefreshTokenRequest, RevokeRefreshTokenResponse,
                ChatRoomState, ChatRoomMember, ChatRole, ChatRoomGroupHeaderState,
                ChatRoomGroupState, UserChatRoomState, UserChatRoomGroupState, ChatGroupSummary,
                ChatRoomSummaryPair, GetMyChatRoomGroupsRequest, GetMyChatRoomGroupsResponse,
                GetChatRoomGroupStateRequest, GetChatRoomGroupStateResponse,
                SetSessionActiveGroupsRequest, SetSessionActiveGroupsResponse, ChatSendRequest,
                ChatSendResponse, ChatHistoryRequest, ServerMessage, ChatMessageReaction,
                ChatHistoryMessage, ChatHistoryResponse, ChatMentions, ChatIncoming, ChatAck,
                ChatModifiedMessage, ChatModified, ChatMemberChange, ChatHeaderChange,
                ChatRoomsChange, ChatUserStateChange, CreateChatGroupRequest,
                CreateChatGroupResponse, JoinChatGroupRequest, JoinChatGroupResponse,
                LeaveChatGroupRequest, LeaveChatGroupResponse, CreateChatRoomRequest,
                CreateChatRoomResponse, DeleteChatRoomRequest, DeleteChatRoomResponse,
                RenameChatRoomRequest, RenameChatRoomResponse, RenameChatGroupRequest,
                RenameChatGroupResponse, CreateInviteLinkRequest, CreateInviteLinkResponse,
                GetInviteLinksRequest, InviteLinkInfo, GetInviteLinksResponse,
                GetInviteLinkInfoRequest, GetInviteLinkInfoResponse, DeleteInviteLinkRequest,
                DeleteInviteLinkResponse, KickChatUserRequest, KickChatUserResponse,
                BanChatUserRequest, BanChatUserResponse, MuteChatUserRequest, MuteChatUserResponse,
                DeleteChatMessage, DeleteChatMessagesRequest, DeleteChatMessagesResponse,
                ChatReactionRequest, ChatReactionResponse, ChatReactionNotice,
                InviteFriendToChatGroupRequest, InviteFriendToChatGroupResponse,
                ChatRoleActions, SetTaglineRequest, SetTaglineResponse, UserChatGroupPreferences, SetUserPreferencesRequest, SetUserPreferencesResponse, CreateRoleRequest, CreateRoleResponse, RenameRoleRequest, RenameRoleResponse, DeleteRoleRequest, DeleteRoleResponse, GetRoleActionsRequest, GetRoleActionsResponse, ReplaceRoleActionsRequest, ReplaceRoleActionsResponse, AddRoleToUserRequest, AddRoleToUserResponse, DeleteRoleFromUserRequest, DeleteRoleFromUserResponse, GetBanListRequest, BanInfo, GetBanListResponse,
        SetAvatarRequest, SetAvatarResponse, ChatRoomNoticeNotification, DismissChatRoomNotice, ChatRoomMemberNotice, ChatRoomGroupMemberNotices,
        FriendGroup, FriendGroupMembership, ClientFriendsGroupsList, ClientCreateFriendsGroup,
                ClientCreateFriendsGroupResponse, ClientDeleteFriendsGroup, ClientDeleteFriendsGroupResponse,
                ClientManageFriendsGroup, ClientManageFriendsGroupResponse, ClientAddFriendToGroup,
                ClientAddFriendToGroupResponse, ClientRemoveFriendFromGroup, ClientRemoveFriendFromGroupResponse,
            );
            seen += 1;
        }
        assert_eq!(seen, 202, "a message was added without a vector, or one was lost");
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
