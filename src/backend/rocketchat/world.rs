//! In-memory state for a connected Rocket.Chat account:
//! rooms (DMs, public channels, private groups, teams, discussions, livechat),
//! subscriptions (unread counts, mentions, favorites, E2EKey), users, roles, and custom emoji.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RoomKind {
    /// Direct message (`t == "d"`), 1:1 or multi-user.
    Direct,
    /// Public channel (`t == "c"`).
    Channel,
    /// Private group (`t == "p"`).
    Group,
    /// Omnichannel / Livechat (`t == "l"`).
    Livechat,
}

impl RoomKind {
    pub fn from_rc(t: &str) -> Self {
        match t {
            "d" => RoomKind::Direct,
            "p" => RoomKind::Group,
            "l" => RoomKind::Livechat,
            _ => RoomKind::Channel,
        }
    }

    pub fn as_rc(self) -> &'static str {
        match self {
            RoomKind::Direct => "d",
            RoomKind::Channel => "c",
            RoomKind::Group => "p",
            RoomKind::Livechat => "l",
        }
    }

    /// Endpoint prefix for room-scoped REST endpoints (`im.*`, `channels.*`, `groups.*`).
    pub fn endpoint_prefix(self) -> &'static str {
        match self {
            RoomKind::Direct => "im",
            RoomKind::Group => "groups",
            RoomKind::Channel | RoomKind::Livechat => "channels",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Room {
    pub id: String,
    pub kind: RoomKind,
    pub name: String,
    /// Full / human display name (`fname`, used for discussions, teams, DMs).
    pub fname: Option<String>,
    pub topic: Option<String>,
    pub announcement: Option<String>,
    pub description: Option<String>,
    pub read_only: bool,
    pub react_when_read_only: bool,
    pub encrypted: bool,
    pub archived: bool,
    pub broadcast: bool,
    /// Parent room ID for Discussions (`prid`).
    pub prid: Option<String>,
    /// Team ID if this room belongs to a Team.
    pub team_id: Option<String>,
    /// True if this room is the main room of a Team (`teamMain`).
    pub team_main: bool,
    /// True if this room is bridged via Matrix Federation (`federated`).
    pub federated: bool,
    /// True if this account is currently muted in this room.
    pub muted_usernames: Vec<String>,
    /// Participant user IDs (for DMs).
    pub uids: Vec<String>,
    /// Participant usernames (for DMs).
    pub usernames: Vec<String>,
    /// Subscription fields:
    pub open: bool,
    pub favorite: bool,
    pub unread: u32,
    pub user_mentions: u32,
    pub group_mentions: u32,
    pub alert: bool,
    /// User's own roles in this room (`owner`, `moderator`, `leader`).
    pub roles: Vec<String>,
    /// Per-room E2EE key ID (`e2eKeyId`) and RSA-OAEP-encrypted symmetric key (`E2EKey`) from subscription.
    pub e2e_key_id: Option<String>,
    pub e2e_key: Option<String>,
    /// Notification preference fields from subscription.
    pub disable_notifications: bool,
    pub hide_unread_status: bool,
    pub desktop_notifications: Option<String>,
    pub mobile_push_notifications: Option<String>,
    pub email_notifications: Option<String>,
    pub last_message_ts: i64,
}

impl Default for Room {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: RoomKind::Channel,
            name: String::new(),
            fname: None,
            topic: None,
            announcement: None,
            description: None,
            read_only: false,
            react_when_read_only: false,
            encrypted: false,
            archived: false,
            broadcast: false,
            prid: None,
            team_id: None,
            team_main: false,
            federated: false,
            muted_usernames: Vec::new(),
            uids: Vec::new(),
            usernames: Vec::new(),
            open: true,
            favorite: false,
            unread: 0,
            user_mentions: 0,
            group_mentions: 0,
            alert: false,
            roles: Vec::new(),
            e2e_key_id: None,
            e2e_key: None,
            disable_notifications: false,
            hide_unread_status: false,
            desktop_notifications: None,
            mobile_push_notifications: None,
            email_notifications: None,
            last_message_ts: 0,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub name: Option<String>,
    pub status: String, // "online" | "away" | "busy" | "offline"
    pub status_text: String,
    pub roles: Vec<String>,
    pub avatar_etag: Option<String>,
    pub federated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomEmoji {
    pub id: String,
    pub name: String,
    pub aliases: Vec<String>,
    pub extension: String,
}

#[derive(Debug, Default)]
pub struct World {
    pub own_id: String,
    pub own_username: String,
    pub own_name: String,
    pub own_roles: Vec<String>,
    pub server_host: String,
    pub rooms: HashMap<String, Room>,
    pub users: HashMap<String, User>,
    pub users_by_name: HashMap<String, String>,
    pub custom_emoji: HashMap<String, CustomEmoji>,
    pub ignored_users: HashSet<String>,
    /// Room IDs we have already subscribed to on `stream-notify-room`.
    pub subscribed_rooms: HashSet<String>,
}

impl World {
    pub fn upsert_user(&mut self, user: User) {
        if !user.username.is_empty() {
            self.users_by_name.insert(user.username.to_ascii_lowercase(), user.id.clone());
        }
        self.users.insert(user.id.clone(), user);
    }

    pub fn display_name_for_user(&self, uid: &str, fallback_username: &str) -> String {
        if let Some(u) = self.users.get(uid) {
            if let Some(n) = u.name.as_deref().filter(|s| !s.is_empty()) {
                return n.to_string();
            }
            if !u.username.is_empty() {
                return u.username.clone();
            }
        }
        if !fallback_username.is_empty() {
            fallback_username.to_string()
        } else {
            uid.to_string()
        }
    }

    /// Compute the moho buffer title for a Rocket.Chat room.
    pub fn room_title(&self, room: &Room) -> String {
        match room.kind {
            RoomKind::Direct => {
                let others: Vec<&str> = room
                    .usernames
                    .iter()
                    .map(String::as_str)
                    .filter(|u| *u != self.own_username)
                    .collect();
                if !others.is_empty() {
                    return others.join(", ");
                }
                if let Some(f) = room.fname.as_deref().filter(|s| !s.is_empty()) {
                    return f.to_string();
                }
                if !room.name.is_empty() {
                    return room.name.clone();
                }
                room.id.clone()
            }
            RoomKind::Livechat => {
                let label = room
                    .fname
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&room.name);
                format!("livechat/{label}")
            }
            RoomKind::Channel | RoomKind::Group => {
                if room.prid.is_some() {
                    if let Some(f) = room.fname.as_deref().filter(|s| !s.is_empty()) {
                        return f.to_string();
                    }
                }
                let base = room
                    .fname
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(&room.name);
                if base.starts_with('#') {
                    base.to_string()
                } else {
                    format!("#{base}")
                }
            }
        }
    }

    /// Whether the current user has the `livechat-agent` role.
    pub fn is_livechat_agent(&self) -> bool {
        self.own_roles.iter().any(|r| r == "livechat-agent" || r == "livechat-manager")
    }

    /// Whether a username looks like a Rocket.Chat Matrix-federated user (`@user:domain`).
    pub fn is_federated_username(username: &str) -> bool {
        username.contains(':') && !username.starts_with("http")
    }

    /// Merge a Rocket.Chat room JSON object (`rooms.get`, `rooms.info`, `rooms-changed`) into `self.rooms`.
    pub fn merge_room_json(&mut self, v: &Value) -> Option<String> {
        let id = v["_id"].as_str()?.to_string();
        let entry = self.rooms.entry(id.clone()).or_insert_with(|| Room {
            id: id.clone(),
            ..Default::default()
        });
        if let Some(t) = v["t"].as_str() {
            entry.kind = RoomKind::from_rc(t);
        }
        if let Some(name) = v["name"].as_str() {
            entry.name = name.to_string();
        }
        if let Some(fname) = v["fname"].as_str() {
            entry.fname = Some(fname.to_string());
        }
        if let Some(topic) = v["topic"].as_str() {
            entry.topic = Some(topic.to_string());
        }
        if let Some(ann) = v["announcement"].as_str() {
            entry.announcement = Some(ann.to_string());
        }
        if let Some(desc) = v["description"].as_str() {
            entry.description = Some(desc.to_string());
        }
        if let Some(ro) = v["ro"].as_bool() {
            entry.read_only = ro;
        }
        if let Some(rwro) = v["reactWhenReadOnly"].as_bool() {
            entry.react_when_read_only = rwro;
        }
        if let Some(enc) = v["encrypted"].as_bool() {
            entry.encrypted = enc;
        }
        if let Some(arch) = v["archived"].as_bool() {
            entry.archived = arch;
        }
        if let Some(bc) = v["broadcast"].as_bool() {
            entry.broadcast = bc;
        }
        if let Some(prid) = v["prid"].as_str().filter(|s| !s.is_empty()) {
            entry.prid = Some(prid.to_string());
        }
        if let Some(tid) = v["teamId"].as_str().filter(|s| !s.is_empty()) {
            entry.team_id = Some(tid.to_string());
        }
        if let Some(tm) = v["teamMain"].as_bool() {
            entry.team_main = tm;
        }
        if let Some(fed) = v["federated"].as_bool() {
            entry.federated = fed;
        }
        if let Some(key_id) = v["e2eKeyId"].as_str().filter(|s| !s.is_empty()) {
            entry.e2e_key_id = Some(key_id.to_string());
        }
        if let Some(muted) = v["muted"].as_array() {
            entry.muted_usernames = muted
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
        }
        if let Some(uids) = v["uids"].as_array() {
            entry.uids = uids
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
        }
        if let Some(usernames) = v["usernames"].as_array() {
            entry.usernames = usernames
                .iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect();
        }
        if let Some(ts) = parse_rc_timestamp(&v["lm"]).or_else(|| parse_rc_timestamp(&v["_updatedAt"])) {
            entry.last_message_ts = entry.last_message_ts.max(ts);
        }
        Some(id)
    }

    /// Merge a Rocket.Chat subscription JSON object (`subscriptions.get`, `subscriptions-changed`) into `self.rooms`.
    pub fn merge_subscription_json(&mut self, v: &Value) -> Option<String> {
        let rid = v["rid"].as_str()?.to_string();
        let entry = self.rooms.entry(rid.clone()).or_insert_with(|| Room {
            id: rid.clone(),
            ..Default::default()
        });
        if let Some(t) = v["t"].as_str() {
            entry.kind = RoomKind::from_rc(t);
        }
        if let Some(name) = v["name"].as_str() {
            if entry.name.is_empty() || entry.kind != RoomKind::Direct {
                entry.name = name.to_string();
            }
        }
        if let Some(fname) = v["fname"].as_str() {
            entry.fname = Some(fname.to_string());
        }
        if let Some(open) = v["open"].as_bool() {
            entry.open = open;
        }
        if let Some(fav) = v["f"].as_bool() {
            entry.favorite = fav;
        }
        if let Some(unread) = v["unread"].as_u64() {
            entry.unread = unread as u32;
        }
        if let Some(um) = v["userMentions"].as_u64() {
            entry.user_mentions = um as u32;
        }
        if let Some(gm) = v["groupMentions"].as_u64() {
            entry.group_mentions = gm as u32;
        }
        if let Some(alert) = v["alert"].as_bool() {
            entry.alert = alert;
        }
        if let Some(roles) = v["roles"].as_array() {
            entry.roles = roles
                .iter()
                .filter_map(|r| r.as_str().map(str::to_string))
                .collect();
        }
        if let Some(e2e_key) = v["E2EKey"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| v["E2ESuggestedKey"].as_str().filter(|s| !s.is_empty()))
        {
            entry.e2e_key = Some(e2e_key.to_string());
            entry.encrypted = true;
        }
        if let Some(prid) = v["prid"].as_str().filter(|s| !s.is_empty()) {
            entry.prid = Some(prid.to_string());
        }
        if let Some(dn) = v["disableNotifications"].as_bool() {
            entry.disable_notifications = dn;
        }
        if let Some(hus) = v["hideUnreadStatus"].as_bool() {
            entry.hide_unread_status = hus;
        }
        if let Some(d) = v["desktopNotifications"].as_str() {
            entry.desktop_notifications = Some(d.to_string());
        }
        if let Some(m) = v["mobilePushNotifications"].as_str() {
            entry.mobile_push_notifications = Some(m.to_string());
        }
        if let Some(e) = v["emailNotifications"].as_str() {
            entry.email_notifications = Some(e.to_string());
        }
        if let Some(ign) = v["ignored"].as_array() {
            for u in ign.iter().filter_map(|x| x.as_str()) {
                self.ignored_users.insert(u.to_string());
            }
        }
        Some(rid)
    }
}

/// Parse Rocket.Chat timestamps, which appear as ISO-8601 strings OR EJSON `{"$date": <millis>}`.
pub fn parse_rc_timestamp(v: &Value) -> Option<i64> {
    if let Some(s) = v.as_str() {
        return chrono::DateTime::parse_from_rfc3339(s).ok().map(|dt| dt.timestamp());
    }
    if let Some(ms) = v["$date"].as_i64() {
        return Some(ms / 1000);
    }
    if let Some(ms) = v.as_i64() {
        return Some(if ms > 10_000_000_000 { ms / 1000 } else { ms });
    }
    None
}
