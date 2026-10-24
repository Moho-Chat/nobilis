//! What the account can see: the people, servers, channels, roles and emoji Stoat has told it of, and what each channel
//! lets it do.
//!
//! Kept as plain data read out of the JSON Stoat sends (`Ready` and the events after it), so that the question that
//! matters most - may this account write here? - is a function of data, written down once and tested. It is Stoat's
//! own calculation (its backend's `permissions` crate): the server's default, then the channel's default, then the
//! account's roles from the lowest-ranked up, first in the server and then in the channel; an account that is timed out
//! may only look; one that cannot see a channel has nothing there.

use serde_json::Value;
use std::collections::HashMap;

// Permission bits, as the developer documentation gives them.
pub const MANAGE_CHANNEL: u64 = 1 << 0;
pub const VIEW_CHANNEL: u64 = 1 << 20;
pub const READ_MESSAGE_HISTORY: u64 = 1 << 21;
pub const SEND_MESSAGE: u64 = 1 << 22;
pub const KICK_MEMBERS: u64 = 1 << 6;
pub const BAN_MEMBERS: u64 = 1 << 7;
pub const TIMEOUT_MEMBERS: u64 = 1 << 8;
pub const ASSIGN_ROLES: u64 = 1 << 9;
pub const MANAGE_MESSAGES: u64 = 1 << 23;
pub const INVITE_OTHERS: u64 = 1 << 25;
pub const SEND_EMBEDS: u64 = 1 << 26;
pub const UPLOAD_FILES: u64 = 1 << 27;
pub const MASQUERADE: u64 = 1 << 28;
pub const REACT: u64 = 1 << 29;
pub const CONNECT: u64 = 1 << 30;
pub const SPEAK: u64 = 1 << 31;
pub const VIDEO: u64 = 1 << 32;
pub const LISTEN: u64 = 1 << 36;

/// Every permission there is, safely: what an owner has.
pub const GRANT_ALL_SAFE: u64 = 0x000F_FFFF_FFFF_FFFF;
/// What a member held in a timeout may still do.
pub const ALLOWED_IN_TIMEOUT: u64 = VIEW_CHANNEL | READ_MESSAGE_HISTORY;
pub const VIEW_ONLY: u64 = VIEW_CHANNEL | READ_MESSAGE_HISTORY;
const DEFAULT_PERMISSION: u64 = VIEW_ONLY | SEND_MESSAGE | INVITE_OTHERS | SEND_EMBEDS | UPLOAD_FILES | CONNECT | SPEAK | LISTEN | VIDEO;
/// What a direct message or a group gives by default.
pub const DIRECT_MESSAGE: u64 = DEFAULT_PERMISSION | MANAGE_CHANNEL | REACT | MASQUERADE;

/// Said in place of the message box where the account may not write.
pub const NO_SEND_PERMISSION: &str = "You do not have permission to send messages in this channel.";
pub const BLOCKED_DM: &str = "You cannot message this person: one of you has blocked the other.";
pub const NOT_FRIENDS: &str = "Stoat only lets you message people who are your friends, or bots. Add them as a friend first.";
pub const TIMED_OUT: &str = "You are timed out in this server.";

/// What an account of one standing may do, as the instance states it.
#[derive(Debug, Clone, Default)]
pub struct UserLimits {
    pub message_length: usize,
    pub message_attachments: usize,
    pub outgoing_friend_requests: usize,
    pub servers: usize,
    /// The audio rate calls are held to, in hertz.
    pub voice_quality: u64,
    pub video: bool,
    /// The most each kind of file may weigh, in bytes.
    pub attachment_bytes: u64,
    pub avatar_bytes: u64,
    pub icon_bytes: u64,
    pub banner_bytes: u64,
    pub emoji_bytes: u64,
}

impl UserLimits {
    fn from_json(v: &Value) -> Self {
        let n = |x: &Value, d: usize| x.as_u64().map(|n| n as usize).unwrap_or(d);
        let bytes = |key: &str, d: u64| v["file_upload_size_limits"][key].as_u64().unwrap_or(d);
        Self {
            message_length: n(&v["message_length"], 2000),
            message_attachments: n(&v["message_attachments"], 5),
            outgoing_friend_requests: n(&v["outgoing_friend_requests"], 10),
            servers: n(&v["servers"], 100),
            voice_quality: v["voice_quality"].as_u64().unwrap_or(16_000),
            video: v["video"].as_bool().unwrap_or(true),
            attachment_bytes: bytes("attachments", 20_000_000),
            avatar_bytes: bytes("avatars", 4_000_000),
            icon_bytes: bytes("icons", 2_500_000),
            banner_bytes: bytes("banners", 6_000_000),
            emoji_bytes: bytes("emojis", 500_000),
        }
    }
}

/// What the instance said of itself at its root.
#[derive(Debug, Clone, Default)]
pub struct Instance {
    pub ws: String,
    /// The media server to join calls on: the first the instance lists (the official one lists one; a self-hosted one,
    /// "worldwide").
    pub voice_node: Option<String>,
    /// The instance's own web app, where an invite link lives.
    pub app: Option<String>,
    /// The file server (Autumn).
    pub autumn: Option<String>,
    /// The link proxy (January).
    pub january: Option<String>,
    /// Longest message, in characters, for an account of ordinary standing.
    pub message_length: usize,
    pub message_attachments: usize,
    /// The most a file attached to a message may weigh, in bytes.
    pub attachment_bytes: u64,
    /// For a new account, until `new_user_hours` have passed.
    pub new_message_length: usize,
    pub new_message_attachments: usize,
    pub new_attachment_bytes: u64,
    pub new_user_hours: u64,
    /// The same, whole, for an account of ordinary standing and for a new one.
    pub standing: UserLimits,
    pub fresh: UserLimits,
    /// What holds for everybody: how many roles and channels a server may have, how many different reactions a
    /// message.
    pub server_roles: usize,
    pub server_channels: usize,
    pub message_reactions: usize,
}

impl Instance {
    pub fn from_root(root: &Value) -> Self {
        let url = |v: &Value| v["enabled"].as_bool().unwrap_or(false).then(|| v["url"].as_str().map(|u| u.trim_end_matches('/').to_string())).flatten();
        let limits = &root["features"]["limits"];
        let number = |v: &Value, d: usize| v.as_u64().map(|n| n as usize).unwrap_or(d);
        let (standing, fresh) = (UserLimits::from_json(&limits["default"]), UserLimits::from_json(&limits["new_user"]));
        Self {
            ws: root["ws"].as_str().unwrap_or("").to_string(),
            voice_node: root["features"]["livekit"]["nodes"][0]["name"].as_str().map(str::to_string),
            app: root["app"].as_str().filter(|a| !a.is_empty()).map(|a| a.trim_end_matches('/').to_string()),
            autumn: url(&root["features"]["autumn"]),
            january: url(&root["features"]["january"]),
            message_length: standing.message_length,
            message_attachments: standing.message_attachments,
            attachment_bytes: standing.attachment_bytes,
            new_message_attachments: fresh.message_attachments,
            new_attachment_bytes: fresh.attachment_bytes,
            new_message_length: fresh.message_length,
            new_user_hours: limits["global"]["new_user_hours"].as_u64().unwrap_or(72),
            server_roles: number(&limits["global"]["server_roles"], 200),
            server_channels: number(&limits["global"]["server_channels"], 200),
            message_reactions: number(&limits["global"]["message_reactions"], 20),
            standing,
            fresh,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct User {
    pub name: String,
    /// The avatar's file id.
    pub avatar: Option<String>,
    pub bot: bool,
    /// What they chose to be shown as: `Online`, `Idle`, `Focus`, `Busy`, `Invisible`.
    pub presence: String,
    /// The words they put beside it.
    pub status_text: Option<String>,
    /// What this account is to them: `Friend`, `Blocked`, `BlockedOther`, `Incoming`, `Outgoing`, `User`, `None`.
    pub relationship: String,
    pub online: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Role {
    pub name: String,
    pub rank: i64,
    pub allow: u64,
    pub deny: u64,
    pub colour: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Category {
    pub title: String,
    pub channels: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub owner: String,
    pub roles: HashMap<String, Role>,
    pub default_permissions: u64,
    pub categories: Vec<Category>,
    pub channels: Vec<String>,
    pub icon: Option<String>,
}

/// This account's own membership of a server.
#[derive(Debug, Clone, Default)]
pub struct Member {
    pub roles: Vec<String>,
    /// Until when it may not speak, Unix seconds.
    pub timeout: Option<i64>,
    pub nickname: Option<String>,
    pub can_publish: bool,
    pub can_receive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Voice,
    Direct,
    Group,
    Saved,
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub id: String,
    pub kind: Kind,
    pub name: String,
    pub server: Option<String>,
    pub description: Option<String>,
    /// A text channel that can also be joined for a call: Stoat's voice channels are text channels with a `voice` part.
    pub voice: bool,
    pub recipients: Vec<String>,
    pub owner: Option<String>,
    /// A server channel's default override, and its overrides by role.
    pub default: (u64, u64),
    pub roles: HashMap<String, (u64, u64)>,
    /// A group's own permissions.
    pub group_permissions: Option<u64>,
    pub icon: Option<String>,
    pub last_message_id: Option<String>,
}

impl Default for Channel {
    fn default() -> Self {
        Self {
            id: String::new(),
            kind: Kind::Text,
            name: String::new(),
            server: None,
            description: None,
            voice: false,
            recipients: Vec::new(),
            owner: None,
            default: (0, 0),
            roles: HashMap::new(),
            group_permissions: None,
            icon: None,
            last_message_id: None,
        }
    }
}

/// A custom emoji: what it is called, the server it belongs to (none for one that stands alone), and whether it moves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EmojiInfo {
    pub name: String,
    pub server: Option<String>,
    pub animated: bool,
}

pub fn parse_emoji(v: &Value) -> Option<(String, EmojiInfo)> {
    Some((
        v["_id"].as_str()?.to_string(),
        EmojiInfo {
            name: v["name"].as_str()?.to_string(),
            server: (v["parent"]["type"].as_str() == Some("Server")).then(|| v["parent"]["id"].as_str().map(str::to_string)).flatten(),
            animated: v["animated"].as_bool().unwrap_or(false),
        },
    ))
}

/// Somebody in a voice channel, and what they are sending.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VoiceMember {
    pub user: String,
    pub camera: bool,
    pub screen: bool,
    /// Whether they are sending sound: not muted.
    pub publishing: bool,
}

pub fn parse_voice_member(v: &Value) -> Option<VoiceMember> {
    Some(VoiceMember {
        user: v["id"].as_str()?.to_string(),
        camera: v["camera"].as_bool().unwrap_or(false),
        screen: v["screensharing"].as_bool().unwrap_or(false),
        publishing: v["is_publishing"].as_bool().unwrap_or(true),
    })
}

#[derive(Debug, Default)]
pub struct World {
    /// Who is in each voice channel, by channel id.
    pub voice: HashMap<String, Vec<VoiceMember>>,
    /// Until when (Unix seconds) a channel's slowmode keeps this account from sending, as Stoat last said.
    pub slow_until: HashMap<String, i64>,
    /// The direct messages and groups a call has begun in without this account: ringing, until it joins or the call ends.
    pub ringing: std::collections::HashSet<String>,
    /// The account's notification settings, as Stoat keeps them (see `settings`).
    pub notifications: Value,
    pub own_id: String,
    pub users: HashMap<String, User>,
    pub servers: HashMap<String, Server>,
    pub channels: HashMap<String, Channel>,
    /// Custom emoji by id.
    pub emojis: HashMap<String, EmojiInfo>,
    /// This account's membership of each server, by server id.
    pub members: HashMap<String, Member>,
}

fn file_id(v: &Value) -> Option<String> {
    v["_id"].as_str().map(str::to_string)
}

fn pair(v: &Value) -> (u64, u64) {
    (v["a"].as_u64().unwrap_or(0), v["d"].as_u64().unwrap_or(0))
}

fn when(v: &Value) -> Option<i64> {
    v.as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp())
}

pub fn parse_user(v: &Value) -> Option<(String, User)> {
    let id = v["_id"].as_str()?.to_string();
    let username = v["username"].as_str().unwrap_or("").to_string();
    let name = v["display_name"].as_str().filter(|n| !n.is_empty()).map(str::to_string).unwrap_or_else(|| username.clone());
    Some((
        id,
        User {
            name,
            avatar: file_id(&v["avatar"]),
            bot: v.get("bot").is_some_and(|b| b.is_object()),
            presence: v["status"]["presence"].as_str().unwrap_or("Online").to_string(),
            status_text: v["status"]["text"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
            relationship: v["relationship"].as_str().unwrap_or("None").to_string(),
            online: v["online"].as_bool().unwrap_or(false),
        },
    ))
}

pub fn parse_server(v: &Value) -> Option<Server> {
    let id = v["_id"].as_str()?.to_string();
    let roles = v["roles"]
        .as_object()
        .map(|o| {
            o.iter()
                .map(|(rid, r)| {
                    let (allow, deny) = pair(&r["permissions"]);
                    (rid.clone(), Role { name: r["name"].as_str().unwrap_or("role").to_string(), rank: r["rank"].as_i64().unwrap_or(0), allow, deny, colour: r["colour"].as_str().map(str::to_string) })
                })
                .collect()
        })
        .unwrap_or_default();
    let categories = v["categories"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| Category { title: c["title"].as_str().unwrap_or("").to_string(), channels: c["channels"].as_array().map(|l| l.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default() })
                .collect()
        })
        .unwrap_or_default();
    Some(Server {
        id,
        name: v["name"].as_str().unwrap_or("server").to_string(),
        owner: v["owner"].as_str().unwrap_or("").to_string(),
        roles,
        default_permissions: v["default_permissions"].as_u64().unwrap_or(0),
        categories,
        channels: v["channels"].as_array().map(|l| l.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        icon: file_id(&v["icon"]),
    })
}

pub fn parse_channel(v: &Value) -> Option<Channel> {
    let id = v["_id"].as_str()?.to_string();
    let kind = match v["channel_type"].as_str()? {
        "TextChannel" => Kind::Text,
        "VoiceChannel" => Kind::Voice,
        "DirectMessage" => Kind::Direct,
        "Group" => Kind::Group,
        "SavedMessages" => Kind::Saved,
        _ => return None,
    };
    Some(Channel {
        id,
        kind,
        name: v["name"].as_str().unwrap_or("").to_string(),
        server: v["server"].as_str().map(str::to_string),
        description: v["description"].as_str().filter(|d| !d.is_empty()).map(str::to_string),
        voice: v.get("voice").is_some_and(|x| x.is_object()),
        recipients: v["recipients"].as_array().map(|l| l.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        owner: v["owner"].as_str().map(str::to_string),
        default: pair(&v["default_permissions"]),
        roles: v["role_permissions"].as_object().map(|o| o.iter().map(|(k, p)| (k.clone(), pair(p))).collect()).unwrap_or_default(),
        group_permissions: v["permissions"].as_u64(),
        icon: file_id(&v["icon"]),
        last_message_id: v["last_message_id"].as_str().map(str::to_string),
    })
}

pub fn parse_member(v: &Value) -> Option<(String, String, Member)> {
    let server = v["_id"]["server"].as_str()?.to_string();
    let user = v["_id"]["user"].as_str()?.to_string();
    Some((
        server,
        user,
        Member {
            roles: v["roles"].as_array().map(|l| l.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            timeout: when(&v["timeout"]),
            nickname: v["nickname"].as_str().filter(|n| !n.is_empty()).map(str::to_string),
            can_publish: v["can_publish"].as_bool().unwrap_or(true),
            can_receive: v["can_receive"].as_bool().unwrap_or(true),
        },
    ))
}

impl World {
    /// What is written in the box, as Stoat reads it (see `messages::outgoing`).
    pub fn pipe_outgoing(&self, text: &str) -> String {
        super::messages::outgoing(self, text)
    }

    /// Takes in what `Ready` carries.
    pub fn absorb_ready(&mut self, own_id: &str, ready: &Value) {
        self.own_id = own_id.to_string();
        for u in ready["users"].as_array().into_iter().flatten() {
            if let Some((id, user)) = parse_user(u) {
                self.users.insert(id, user);
            }
        }
        self.servers.clear();
        for s in ready["servers"].as_array().into_iter().flatten() {
            if let Some(server) = parse_server(s) {
                self.servers.insert(server.id.clone(), server);
            }
        }
        self.channels.clear();
        for c in ready["channels"].as_array().into_iter().flatten() {
            if let Some(channel) = parse_channel(c) {
                self.channels.insert(channel.id.clone(), channel);
            }
        }
        self.members.clear();
        for m in ready["members"].as_array().into_iter().flatten() {
            if let Some((server, user, member)) = parse_member(m) {
                if user == self.own_id {
                    self.members.insert(server, member);
                }
            }
        }
        self.voice.clear();
        for state in ready["voice_states"].as_array().into_iter().flatten() {
            if let Some(channel) = state["id"].as_str() {
                let members: Vec<VoiceMember> = state["participants"].as_array().into_iter().flatten().filter_map(parse_voice_member).collect();
                if !members.is_empty() {
                    self.voice.insert(channel.to_string(), members);
                }
            }
        }
        for e in ready["emojis"].as_array().into_iter().flatten() {
            if let Some((id, info)) = parse_emoji(e) {
                self.emojis.insert(id, info);
            }
        }
    }

    /// What somebody is called: their display name, or their id if nothing is known of them yet.
    pub fn name_of(&self, user: &str) -> String {
        self.users.get(user).map(|u| u.name.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| user.to_string())
    }

    /// What a channel is called in the list: a direct message is the other person, a group its name, a server channel
    /// `Server/#channel`.
    pub fn title_of(&self, channel: &Channel) -> String {
        match channel.kind {
            Kind::Text | Kind::Voice => {
                let server = channel.server.as_ref().and_then(|s| self.servers.get(s)).map(|s| s.name.as_str()).unwrap_or("server");
                format!("{server}/#{}", channel.name)
            }
            Kind::Direct => channel.recipients.iter().find(|r| **r != self.own_id).map(|r| self.name_of(r)).unwrap_or_else(|| "Direct message".to_string()),
            Kind::Group => channel.name.clone(),
            Kind::Saved => "Saved Notes".to_string(),
        }
    }

    /// What the account may do in a channel, by Stoat's own reckoning.
    pub fn permissions(&self, channel: &Channel, now: i64) -> u64 {
        match channel.kind {
            Kind::Saved => GRANT_ALL_SAFE,
            Kind::Direct => {
                // Stoat's own rule: a direct message may be written in with a friend, or where either side is a bot;
                // between anybody else it can be read and not written in, and a block closes it.
                let other = channel.recipients.iter().find(|r| **r != self.own_id).and_then(|r| self.users.get(r));
                let relationship = other.map(|u| u.relationship.as_str()).unwrap_or("None");
                let a_bot = other.is_some_and(|u| u.bot) || self.users.get(&self.own_id).is_some_and(|u| u.bot);
                if relationship == "Friend" || relationship == "User" || (a_bot && relationship != "Blocked" && relationship != "BlockedOther") {
                    DIRECT_MESSAGE
                } else {
                    VIEW_ONLY
                }
            }
            Kind::Group => {
                if channel.owner.as_deref() == Some(self.own_id.as_str()) {
                    GRANT_ALL_SAFE
                } else {
                    VIEW_ONLY | channel.group_permissions.unwrap_or(DIRECT_MESSAGE)
                }
            }
            Kind::Text | Kind::Voice => {
                let Some(server) = channel.server.as_ref().and_then(|s| self.servers.get(s)) else { return 0 };
                if server.owner == self.own_id {
                    return GRANT_ALL_SAFE;
                }
                let Some(member) = self.members.get(&server.id) else { return 0 };
                let mut permissions = server.default_permissions;
                let apply = |p: &mut u64, (allow, deny): (u64, u64)| {
                    *p |= allow;
                    *p &= !deny;
                };
                apply(&mut permissions, channel.default);
                // The account's roles, the lowest-ranked (the least powerful) first, so the most powerful speaks last.
                let mut mine: Vec<(&String, &Role)> = server.roles.iter().filter(|(id, _)| member.roles.contains(id)).collect();
                mine.sort_by_key(|(_, role)| std::cmp::Reverse(role.rank));
                for (_, role) in &mine {
                    apply(&mut permissions, (role.allow, role.deny));
                }
                for (id, _) in &mine {
                    if let Some(over) = channel.roles.get(*id) {
                        apply(&mut permissions, *over);
                    }
                }
                if !member.can_publish {
                    permissions &= !(SPEAK | VIDEO);
                }
                if !member.can_receive {
                    permissions &= !LISTEN;
                }
                if member.timeout.is_some_and(|t| t > now) {
                    permissions &= ALLOWED_IN_TIMEOUT;
                }
                if permissions & VIEW_CHANNEL == 0 {
                    permissions = 0;
                }
                permissions
            }
        }
    }

    /// What the account may do in a server as a whole: its default, then the account's roles, the most powerful last.
    pub fn server_permissions(&self, server: &Server) -> u64 {
        if server.owner == self.own_id {
            return GRANT_ALL_SAFE;
        }
        let Some(member) = self.members.get(&server.id) else { return 0 };
        let mut permissions = server.default_permissions;
        let mut mine: Vec<&Role> = server.roles.iter().filter(|(id, _)| member.roles.contains(id)).map(|(_, r)| r).collect();
        mine.sort_by_key(|r| std::cmp::Reverse(r.rank));
        for role in mine {
            permissions |= role.allow;
            permissions &= !role.deny;
        }
        permissions
    }

    /// How powerful the account's best role in a server is, as Stoat ranks them (a lower number is more powerful), or
    /// `None` where it holds none; the owner outranks everything.
    pub fn own_rank(&self, server: &Server) -> Option<i64> {
        if server.owner == self.own_id {
            return Some(i64::MIN);
        }
        let member = self.members.get(&server.id)?;
        member.roles.iter().filter_map(|r| server.roles.get(r)).map(|r| r.rank).min()
    }

    /// Why a channel is read-only, or nothing where it is not.
    pub fn read_only(&self, channel: &Channel, now: i64) -> Option<&'static str> {
        let permissions = self.permissions(channel, now);
        if permissions & SEND_MESSAGE != 0 {
            return None;
        }
        let timed_out = channel.server.as_ref().and_then(|s| self.members.get(s)).is_some_and(|m| m.timeout.is_some_and(|t| t > now));
        let blocked = channel.kind == Kind::Direct && channel.recipients.iter().find(|r| **r != self.own_id).and_then(|r| self.users.get(r)).is_some_and(|u| u.relationship == "Blocked" || u.relationship == "BlockedOther");
        Some(if timed_out {
            TIMED_OUT
        } else if blocked {
            BLOCKED_DM
        } else if channel.kind == Kind::Direct {
            NOT_FRIENDS
        } else {
            NO_SEND_PERMISSION
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ME: &str = "01ME000000000000000000000A";
    const OWNER: &str = "01OWNER00000000000000000A";

    fn world() -> World {
        let mut w = World::default();
        w.absorb_ready(
            ME,
            &json!({
                "users": [
                    { "_id": ME, "username": "me", "display_name": "Me" },
                    { "_id": OWNER, "username": "boss" },
                    { "_id": "U3", "username": "pal", "display_name": "Pal", "relationship": "Friend" },
                    { "_id": "U4", "username": "foe", "relationship": "Blocked" }
                ],
                "servers": [{
                    "_id": "S1", "owner": OWNER, "name": "Chasm", "default_permissions": VIEW_ONLY | SEND_MESSAGE,
                    "channels": ["C1", "C2"],
                    "categories": [{ "id": "K1", "title": "General", "channels": ["C1"] }],
                    "roles": {
                        "R1": { "name": "Mods", "rank": 1, "permissions": { "a": MANAGE_CHANNEL, "d": 0 }, "colour": "#ff0000" },
                        "R2": { "name": "Muted", "rank": 5, "permissions": { "a": 0, "d": SEND_MESSAGE } }
                    }
                }],
                "channels": [
                    { "_id": "C1", "channel_type": "TextChannel", "server": "S1", "name": "general" },
                    { "_id": "C2", "channel_type": "TextChannel", "server": "S1", "name": "rules", "default_permissions": { "a": 0, "d": SEND_MESSAGE }, "role_permissions": { "R1": { "a": SEND_MESSAGE, "d": 0 } } },
                    { "_id": "D1", "channel_type": "DirectMessage", "recipients": [ME, "U3"], "active": true },
                    { "_id": "D2", "channel_type": "DirectMessage", "recipients": ["U4", ME], "active": true },
                    { "_id": "G1", "channel_type": "Group", "name": "Raid", "owner": "U3", "recipients": [ME, "U3"] },
                    { "_id": "N1", "channel_type": "SavedMessages", "user": ME }
                ],
                "members": [
                    { "_id": { "server": "S1", "user": ME }, "roles": [] },
                    { "_id": { "server": "S1", "user": "U3" }, "roles": ["R1"] }
                ]
            }),
        );
        w
    }

    #[test]
    fn a_text_channel_with_a_voice_part_is_still_a_conversation_and_can_be_called_into() {
        let voice = parse_channel(&json!({ "_id": "V", "channel_type": "TextChannel", "server": "S1", "name": "Hangout", "voice": {} })).unwrap();
        assert!(voice.voice && voice.kind == Kind::Text);
        let plain = parse_channel(&json!({ "_id": "T", "channel_type": "TextChannel", "server": "S1", "name": "talk" })).unwrap();
        assert!(!plain.voice);
    }

    #[test]
    fn a_channel_is_named_for_what_it_is() {
        let w = world();
        assert_eq!(w.title_of(&w.channels["C1"]), "Chasm/#general");
        assert_eq!(w.title_of(&w.channels["D1"]), "Pal");
        assert_eq!(w.title_of(&w.channels["D2"]), "foe");
        assert_eq!(w.title_of(&w.channels["G1"]), "Raid");
        assert_eq!(w.title_of(&w.channels["N1"]), "Saved Notes");
        assert_eq!(w.name_of("nobody"), "nobody");
        assert_eq!(w.name_of(OWNER), "boss", "a user with no display name is called by their username");
    }

    #[test]
    fn a_member_with_the_server_default_may_write_and_a_channel_that_denies_it_says_so() {
        let w = world();
        assert!(w.permissions(&w.channels["C1"], 0) & SEND_MESSAGE != 0);
        assert_eq!(w.read_only(&w.channels["C1"], 0), None);
        assert_eq!(w.read_only(&w.channels["C2"], 0), Some(NO_SEND_PERMISSION));
    }

    #[test]
    fn a_role_can_lift_what_a_channel_denies_and_a_denying_role_takes_the_default_away() {
        let mut w = world();
        w.members.get_mut("S1").unwrap().roles = vec!["R1".into()];
        assert_eq!(w.read_only(&w.channels["C2"], 0), None, "the Mods role is allowed to write in the rules channel");
        w.members.get_mut("S1").unwrap().roles = vec!["R2".into()];
        assert_eq!(w.read_only(&w.channels["C1"], 0), Some(NO_SEND_PERMISSION), "the Muted role denies sending");
    }

    #[test]
    fn the_more_powerful_role_has_the_last_word() {
        let mut w = world();
        // Rank 1 allows what rank 5 denies: the lower number is the stronger.
        w.servers.get_mut("S1").unwrap().roles.get_mut("R1").unwrap().allow |= SEND_MESSAGE;
        w.members.get_mut("S1").unwrap().roles = vec!["R1".into(), "R2".into()];
        assert_eq!(w.read_only(&w.channels["C1"], 0), None);
    }

    #[test]
    fn the_owner_may_do_everything_and_a_timed_out_member_may_only_look() {
        let mut w = world();
        w.own_id = OWNER.to_string();
        assert_eq!(w.permissions(&w.channels["C2"], 0), GRANT_ALL_SAFE);
        w.own_id = ME.to_string();
        w.members.get_mut("S1").unwrap().timeout = Some(1000);
        assert_eq!(w.permissions(&w.channels["C1"], 500), VIEW_ONLY);
        assert_eq!(w.read_only(&w.channels["C1"], 500), Some(TIMED_OUT));
        assert!(w.permissions(&w.channels["C1"], 2000) & SEND_MESSAGE != 0, "it is over");
    }

    #[test]
    fn a_channel_that_cannot_be_seen_has_nothing_in_it() {
        let mut w = world();
        w.servers.get_mut("S1").unwrap().default_permissions = SEND_MESSAGE;
        assert_eq!(w.permissions(&w.channels["C1"], 0), 0);
        w.members.clear();
        assert_eq!(w.permissions(&w.channels["C1"], 0), 0, "not a member");
    }

    #[test]
    fn direct_messages_groups_and_saved_notes() {
        let mut w = world();
        assert_eq!(w.read_only(&w.channels["D1"], 0), None);
        assert_eq!(w.read_only(&w.channels["D2"], 0), Some(BLOCKED_DM));
        // Somebody who is neither a friend nor a bot can be read and not written to.
        w.users.get_mut("U3").unwrap().relationship = "None".into();
        assert_eq!(w.read_only(&w.channels["D1"], 0), Some(NOT_FRIENDS));
        w.users.get_mut("U3").unwrap().bot = true;
        assert_eq!(w.read_only(&w.channels["D1"], 0), None, "a bot may be written to");
        w.users.get_mut("U3").unwrap().relationship = "Friend".into();
        assert_eq!(w.read_only(&w.channels["G1"], 0), None);
        assert_eq!(w.permissions(&w.channels["N1"], 0), GRANT_ALL_SAFE);
    }

    #[test]
    fn the_instance_says_where_its_files_are_and_how_long_a_message_may_be() {
        let root = json!({
            "ws": "wss://events.example", "features": {
                "autumn": { "enabled": true, "url": "https://cdn.example/" },
                "livekit": { "enabled": true, "nodes": [{ "name": "hel1", "public_url": "wss://livekit.example" }] },
                "january": { "enabled": false, "url": "https://proxy.example" },
                "limits": { "global": { "new_user_hours": 72 }, "default": { "message_length": 2000, "message_attachments": 5 }, "new_user": { "message_length": 1500, "message_attachments": 3, "file_upload_size_limits": { "attachments": 5000000 } } }
            }
        });
        let i = Instance::from_root(&root);
        assert_eq!(i.ws, "wss://events.example");
        assert_eq!(i.autumn.as_deref(), Some("https://cdn.example"));
        assert_eq!(i.voice_node.as_deref(), Some("hel1"));
        assert_eq!(i.january, None, "a disabled service has no address");
        assert_eq!((i.message_length, i.new_message_length, i.new_user_hours), (2000, 1500, 72));
        assert_eq!((i.message_attachments, i.new_message_attachments, i.attachment_bytes, i.new_attachment_bytes), (5, 3, 20_000_000, 5_000_000));
    }
}
