//! What the account can see: the people, guilds, channels, roles and emoji Fluxer has told it of, and what each channel
//! lets it do.
//!
//! Kept as plain data read out of the JSON the gateway sends (`READY` and the events after it), so that the question
//! that matters most - may this account write here? - is a function of data, written down once and tested. It is
//! Fluxer's own calculation (`http-api/permissions.mdx`, the same as Discord's): the owner has everything; otherwise the
//! everyone role (whose id is the guild's) and the member's roles; `ADMINISTRATOR` ends it; then the channel's
//! overwrites - everyone, the member's roles together, the member - each removing what it denies before adding what it
//! allows. A member in a timeout keeps the mask but may not write.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

pub const CREATE_INSTANT_INVITE: u64 = 1 << 0;
pub const KICK_MEMBERS: u64 = 1 << 1;
pub const BAN_MEMBERS: u64 = 1 << 2;
pub const ADMINISTRATOR: u64 = 1 << 3;
pub const MANAGE_CHANNELS: u64 = 1 << 4;
pub const MANAGE_GUILD: u64 = 1 << 5;
pub const ADD_REACTIONS: u64 = 1 << 6;
pub const VIEW_AUDIT_LOG: u64 = 1 << 7;
pub const STREAM: u64 = 1 << 9;
pub const VIEW_CHANNEL: u64 = 1 << 10;
pub const SEND_MESSAGES: u64 = 1 << 11;
pub const MANAGE_MESSAGES: u64 = 1 << 13;
pub const EMBED_LINKS: u64 = 1 << 14;
pub const ATTACH_FILES: u64 = 1 << 15;
pub const READ_MESSAGE_HISTORY: u64 = 1 << 16;
pub const MENTION_EVERYONE: u64 = 1 << 17;
pub const USE_EXTERNAL_EMOJIS: u64 = 1 << 18;
pub const CONNECT: u64 = 1 << 20;
pub const SPEAK: u64 = 1 << 21;
pub const MUTE_MEMBERS: u64 = 1 << 22;
pub const DEAFEN_MEMBERS: u64 = 1 << 23;
pub const MOVE_MEMBERS: u64 = 1 << 24;
pub const CHANGE_NICKNAME: u64 = 1 << 26;
pub const MANAGE_NICKNAMES: u64 = 1 << 27;
pub const MANAGE_ROLES: u64 = 1 << 28;
pub const MANAGE_WEBHOOKS: u64 = 1 << 29;
pub const MANAGE_EXPRESSIONS: u64 = 1 << 30;
pub const MANAGE_THREADS: u64 = 1 << 34;
pub const CREATE_PUBLIC_THREADS: u64 = 1 << 35;
pub const SEND_MESSAGES_IN_THREADS: u64 = 1 << 38;
pub const MODERATE_MEMBERS: u64 = 1 << 40;
pub const CREATE_EXPRESSIONS: u64 = 1 << 43;
pub const PIN_MESSAGES: u64 = 1 << 51;
pub const BYPASS_SLOWMODE: u64 = 1 << 52;
/// Everything there is.
pub const ALL: u64 = u64::MAX;

/// Said in place of the message box where the account may not write.
pub const NO_SEND_PERMISSION: &str = "You do not have permission to send messages in this channel.";
pub const TIMED_OUT: &str = "You are timed out in this community.";
pub const BLOCKED_DM: &str = "You cannot message this person: one of you has blocked the other.";

/// Fluxer's kinds of channel (`http-api/channels.mdx`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Direct,
    Voice,
    Group,
    Category,
    Announcement,
    Thread,
    Forum,
    Link,
    Notes,
    Other(i64),
}

impl Kind {
    pub fn from(n: i64) -> Self {
        match n {
            0 => Kind::Text,
            1 => Kind::Direct,
            2 => Kind::Voice,
            3 => Kind::Group,
            4 => Kind::Category,
            5 => Kind::Announcement,
            10..=12 => Kind::Thread,
            15 | 16 => Kind::Forum,
            998 => Kind::Link,
            999 => Kind::Notes,
            n => Kind::Other(n),
        }
    }

    /// A conversation of one's own: a direct message, a group or the notes.
    pub fn private(self) -> bool {
        matches!(self, Kind::Direct | Kind::Group | Kind::Notes)
    }
}

/// Parses a permission mask, which Fluxer sends as a decimal string (and accepts as a number).
pub fn mask(v: &Value) -> u64 {
    v.as_str().and_then(|s| s.parse().ok()).or_else(|| v.as_u64()).unwrap_or(0)
}

#[derive(Debug, Clone, Default)]
pub struct User {
    pub username: String,
    pub discriminator: String,
    pub global_name: Option<String>,
    pub avatar: Option<String>,
    pub bot: bool,
}

impl User {
    pub fn shown(&self) -> String {
        self.global_name.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| self.username.clone())
    }
}

pub fn parse_user(v: &Value) -> Option<(String, User)> {
    let id = v["id"].as_str()?.to_string();
    Some((
        id,
        User {
            username: v["username"].as_str().unwrap_or("").to_string(),
            discriminator: v["discriminator"].as_str().unwrap_or("").to_string(),
            global_name: v["global_name"].as_str().map(str::to_string),
            avatar: v["avatar"].as_str().map(str::to_string),
            bot: v["bot"].as_bool().unwrap_or(false),
        },
    ))
}

#[derive(Debug, Clone, Default)]
pub struct Role {
    pub name: String,
    pub color: u32,
    pub position: i64,
    pub permissions: u64,
    pub hoist: bool,
}

#[derive(Debug, Clone)]
pub struct Overwrite {
    pub id: String,
    /// 0 a role, 1 a member.
    pub member: bool,
    pub allow: u64,
    pub deny: u64,
}

#[derive(Debug, Clone)]
pub struct Channel {
    pub id: String,
    pub kind: Kind,
    pub guild: Option<String>,
    pub name: String,
    pub topic: Option<String>,
    pub position: i64,
    pub parent: Option<String>,
    pub overwrites: Vec<Overwrite>,
    pub nsfw: bool,
    pub recipients: Vec<String>,
    pub icon: Option<String>,
    pub slowmode: u64,
    pub last_message_id: Option<String>,
    pub url: Option<String>,
}

pub fn parse_channel(v: &Value, guild: Option<&str>) -> Option<Channel> {
    let id = v["id"].as_str()?.to_string();
    let overwrites = v["permission_overwrites"]
        .as_array()
        .map(|list| list.iter().filter_map(|o| Some(Overwrite { id: o["id"].as_str()?.to_string(), member: o["type"].as_i64() == Some(1), allow: mask(&o["allow"]), deny: mask(&o["deny"]) })).collect())
        .unwrap_or_default();
    let recipients = v["recipients"].as_array().map(|r| r.iter().filter_map(|u| u["id"].as_str().map(str::to_string)).collect()).unwrap_or_default();
    Some(Channel {
        id,
        kind: Kind::from(v["type"].as_i64().unwrap_or(0)),
        guild: v["guild_id"].as_str().or(guild).map(str::to_string),
        name: v["name"].as_str().unwrap_or("").to_string(),
        topic: v["topic"].as_str().filter(|t| !t.is_empty()).map(str::to_string),
        position: v["position"].as_i64().unwrap_or(0),
        parent: v["parent_id"].as_str().map(str::to_string),
        overwrites,
        nsfw: v["nsfw"].as_bool().unwrap_or(false),
        recipients,
        icon: v["icon"].as_str().map(str::to_string),
        slowmode: v["rate_limit_per_user"].as_u64().unwrap_or(0),
        last_message_id: v["last_message_id"].as_str().map(str::to_string),
        url: v["url"].as_str().map(str::to_string),
    })
}

#[derive(Debug, Clone, Default)]
pub struct Member {
    pub roles: Vec<String>,
    pub nick: Option<String>,
    pub avatar: Option<String>,
    /// When a timeout ends, in Unix seconds.
    pub timeout_until: Option<i64>,
}

pub fn parse_member(v: &Value) -> Member {
    Member {
        roles: v["roles"].as_array().map(|r| r.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        nick: v["nick"].as_str().filter(|n| !n.is_empty()).map(str::to_string),
        avatar: v["avatar"].as_str().map(str::to_string),
        timeout_until: v["communication_disabled_until"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp()),
    }
}

#[derive(Debug, Clone)]
pub struct Emoji {
    pub name: String,
    pub animated: bool,
    pub guild: String,
}

#[derive(Debug, Clone, Default)]
pub struct Guild {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
    pub owner: String,
    pub roles: HashMap<String, Role>,
    /// The guild's channels, in no order (sorted when drawn).
    pub channels: Vec<String>,
    /// The members met so far, the account's own among them.
    pub members: HashMap<String, Member>,
    pub member_count: u64,
    pub online_count: u64,
    pub features: Vec<String>,
}

#[derive(Debug, Default)]
pub struct World {
    pub own_id: String,
    pub users: HashMap<String, User>,
    pub guilds: HashMap<String, Guild>,
    pub channels: HashMap<String, Channel>,
    pub emojis: HashMap<String, Emoji>,
    /// People this account has blocked, or who are friends (relationship type 2 and 1).
    pub blocked: HashSet<String>,
    pub friends: HashSet<String>,
    /// Read positions by channel, and unread mentions.
    pub read: HashMap<String, (Option<String>, u64)>,
    /// What Fluxer last said of each person's presence: the status word and the custom status.
    pub presence: HashMap<String, (String, Option<String>)>,
}

impl World {
    /// A guild as `READY`, `GUILD_CREATE` or `GUILD_SYNC` gives it: everything known of it replaced.
    pub fn put_guild(&mut self, g: &Value) -> Option<String> {
        let id = g["id"].as_str()?.to_string();
        if g["unavailable"].as_bool() == Some(true) {
            return None;
        }
        // READY's guild ready object holds the guild in `properties`; an older shape holds it at the top.
        let p = if g["properties"].is_object() { &g["properties"] } else { g };
        let mut guild = Guild {
            id: id.clone(),
            name: p["name"].as_str().unwrap_or("").to_string(),
            icon: p["icon"].as_str().map(str::to_string),
            owner: p["owner_id"].as_str().unwrap_or("").to_string(),
            member_count: g["member_count"].as_u64().unwrap_or(0),
            online_count: g["online_count"].as_u64().unwrap_or(0),
            features: p["features"].as_array().map(|f| f.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
            ..Default::default()
        };
        if let Some(old) = self.guilds.get(&id) {
            guild.members = old.members.clone();
        }
        for r in g["roles"].as_array().into_iter().flatten() {
            if let Some(rid) = r["id"].as_str() {
                guild.roles.insert(rid.to_string(), parse_role(r));
            }
        }
        self.channels.retain(|_, c| c.guild.as_deref() != Some(&id) || c.kind == Kind::Thread);
        for c in g["channels"].as_array().into_iter().flatten() {
            if let Some(channel) = parse_channel(c, Some(&id)) {
                guild.channels.push(channel.id.clone());
                self.channels.insert(channel.id.clone(), channel);
            }
        }
        self.emojis.retain(|_, e| e.guild != id);
        for e in g["emojis"].as_array().into_iter().flatten() {
            if let Some(eid) = e["id"].as_str() {
                self.emojis.insert(eid.to_string(), Emoji { name: e["name"].as_str().unwrap_or("emoji").to_string(), animated: e["animated"].as_bool().unwrap_or(false), guild: id.clone() });
            }
        }
        self.guilds.insert(id.clone(), guild);
        for m in g["members"].as_array().into_iter().flatten() {
            self.put_member(&id, m);
        }
        Some(id)
    }

    pub fn put_member(&mut self, guild: &str, m: &Value) {
        let Some(uid) = m["user"]["id"].as_str().map(str::to_string) else { return };
        if m["user"]["username"].is_string() {
            if let Some((id, user)) = parse_user(&m["user"]) {
                self.users.insert(id, user);
            }
        }
        if let Some(g) = self.guilds.get_mut(guild) {
            g.members.insert(uid, parse_member(m));
        }
    }

    pub fn put_channel(&mut self, v: &Value) -> Option<String> {
        let channel = parse_channel(v, None)?;
        for r in v["recipients"].as_array().into_iter().flatten() {
            if let Some((id, user)) = parse_user(r) {
                self.users.entry(id).or_insert(user);
            }
        }
        let id = channel.id.clone();
        if let Some(g) = channel.guild.as_ref().and_then(|g| self.guilds.get_mut(g)) {
            if !g.channels.contains(&id) {
                g.channels.push(id.clone());
            }
        }
        self.channels.insert(id.clone(), channel);
        Some(id)
    }

    pub fn remove_channel(&mut self, id: &str) {
        if let Some(channel) = self.channels.remove(id) {
            if let Some(g) = channel.guild.and_then(|g| self.guilds.get_mut(&g)) {
                g.channels.retain(|c| c != id);
            }
        }
    }

    /// What a person is called here: a guild nickname where there is one, else their display name.
    pub fn name_in(&self, guild: Option<&str>, user: &str) -> String {
        if let Some(nick) = guild.and_then(|g| self.guilds.get(g)).and_then(|g| g.members.get(user)).and_then(|m| m.nick.clone()) {
            return nick;
        }
        self.users.get(user).map(User::shown).unwrap_or_else(|| "someone".to_string())
    }

    /// A conversation's title: a channel's name, or the people in a direct message or an unnamed group.
    pub fn title_of(&self, channel: &Channel) -> String {
        match channel.kind {
            Kind::Notes => "Personal Notes".to_string(),
            Kind::Direct | Kind::Group if channel.name.is_empty() => {
                let names: Vec<String> = channel.recipients.iter().filter(|r| **r != self.own_id).map(|r| self.name_in(None, r)).collect();
                if names.is_empty() {
                    "Direct message".to_string()
                } else {
                    names.join(", ")
                }
            }
            _ => channel.name.clone(),
        }
    }

    /// The account's permissions in a channel, by Fluxer's calculation.
    pub fn permissions(&self, channel: &Channel) -> u64 {
        let Some(guild) = channel.guild.as_ref().and_then(|g| self.guilds.get(g)) else {
            // A conversation of one's own: everything a member of it may do.
            return VIEW_CHANNEL | SEND_MESSAGES | READ_MESSAGE_HISTORY | ADD_REACTIONS | ATTACH_FILES | EMBED_LINKS | CONNECT | SPEAK | STREAM | USE_EXTERNAL_EMOJIS;
        };
        if guild.owner == self.own_id {
            return ALL;
        }
        let Some(member) = guild.members.get(&self.own_id) else { return 0 };
        let mut base = guild.roles.get(&guild.id).map(|r| r.permissions).unwrap_or(0);
        for role in &member.roles {
            base |= guild.roles.get(role).map(|r| r.permissions).unwrap_or(0);
        }
        if base & ADMINISTRATOR != 0 {
            return ALL;
        }
        // A thread has no overwrites of its own: it is its parent's.
        let source = if channel.kind == Kind::Thread { channel.parent.as_ref().and_then(|p| self.channels.get(p)).unwrap_or(channel) } else { channel };
        let mut perms = base;
        if let Some(o) = source.overwrites.iter().find(|o| !o.member && o.id == guild.id) {
            perms = (perms & !o.deny) | o.allow;
        }
        let (mut allow, mut deny) = (0u64, 0u64);
        for o in source.overwrites.iter().filter(|o| !o.member && member.roles.contains(&o.id)) {
            allow |= o.allow;
            deny |= o.deny;
        }
        perms = (perms & !deny) | allow;
        if let Some(o) = source.overwrites.iter().find(|o| o.member && o.id == self.own_id) {
            perms = (perms & !o.deny) | o.allow;
        }
        perms
    }

    /// Whether the account is timed out in a guild, now.
    pub fn timed_out(&self, guild: &str, now: i64) -> bool {
        self.guilds.get(guild).and_then(|g| g.members.get(&self.own_id)).and_then(|m| m.timeout_until).is_some_and(|t| t > now)
    }

    /// Why the account may not write in a channel, or nothing when it may.
    pub fn read_only(&self, channel: &Channel, now: i64) -> Option<&'static str> {
        if channel.kind == Kind::Direct && channel.recipients.iter().any(|r| self.blocked.contains(r)) {
            return Some(BLOCKED_DM);
        }
        let perms = self.permissions(channel);
        let needed = if channel.kind == Kind::Thread { SEND_MESSAGES_IN_THREADS } else { SEND_MESSAGES };
        if perms & needed == 0 {
            return Some(NO_SEND_PERMISSION);
        }
        if channel.guild.as_deref().is_some_and(|g| self.timed_out(g, now)) {
            return Some(TIMED_OUT);
        }
        None
    }
}

pub fn parse_role(r: &Value) -> Role {
    Role {
        name: r["name"].as_str().unwrap_or("").to_string(),
        color: r["color"].as_u64().unwrap_or(0) as u32,
        position: r["position"].as_i64().unwrap_or(0),
        permissions: mask(&r["permissions"]),
        hoist: r["hoist"].as_bool().unwrap_or(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const G: &str = "100000000000000001";
    const ME: &str = "200000000000000001";
    const MOD: &str = "300000000000000001";

    fn world(own_roles: &[&str], overwrites: Value) -> (World, Channel) {
        let mut w = World { own_id: ME.into(), ..Default::default() };
        w.put_guild(&json!({
            "id": G,
            "properties": { "name": "Guild", "owner_id": "999999999999999999" },
            "roles": [
                { "id": G, "name": "@everyone", "permissions": (VIEW_CHANNEL | SEND_MESSAGES | READ_MESSAGE_HISTORY).to_string(), "position": 0 },
                { "id": MOD, "name": "Mod", "permissions": (MANAGE_MESSAGES | KICK_MEMBERS).to_string(), "position": 1 }
            ],
            "channels": [{ "id": "400000000000000001", "type": 0, "name": "general", "position": 0, "permission_overwrites": overwrites }],
            "members": [{ "user": { "id": ME }, "roles": own_roles }]
        }));
        let c = w.channels.get("400000000000000001").cloned().unwrap();
        (w, c)
    }

    #[test]
    fn everyone_and_roles_add_up_and_an_overwrite_can_take_writing_away() {
        let (w, c) = world(&[MOD], json!([]));
        let p = w.permissions(&c);
        assert!(p & SEND_MESSAGES != 0 && p & MANAGE_MESSAGES != 0 && p & KICK_MEMBERS != 0);
        assert_eq!(w.read_only(&c, 0), None);
        let (w, c) = world(&[], json!([{ "id": G, "type": 0, "allow": "0", "deny": SEND_MESSAGES.to_string() }]));
        assert_eq!(w.read_only(&c, 0), Some(NO_SEND_PERMISSION));
    }

    #[test]
    fn a_role_overwrite_allowing_beats_another_denying_and_the_members_own_comes_last() {
        let deny_everyone = json!({ "id": G, "type": 0, "allow": "0", "deny": SEND_MESSAGES.to_string() });
        let allow_mod = json!({ "id": MOD, "type": 0, "allow": SEND_MESSAGES.to_string(), "deny": "0" });
        let (w, c) = world(&[MOD], json!([deny_everyone, allow_mod]));
        assert_eq!(w.read_only(&c, 0), None, "the role's allow puts it back");
        let deny_me = json!({ "id": ME, "type": 1, "allow": "0", "deny": SEND_MESSAGES.to_string() });
        let (w, c) = world(&[MOD], json!([allow_mod, deny_me]));
        assert_eq!(w.read_only(&c, 0), Some(NO_SEND_PERMISSION), "the member's own overwrite is applied last");
    }

    #[test]
    fn the_owner_and_an_administrator_may_do_anything_and_a_timeout_stops_writing() {
        let (mut w, c) = world(&[], json!([{ "id": G, "type": 0, "allow": "0", "deny": SEND_MESSAGES.to_string() }]));
        w.guilds.get_mut(G).unwrap().owner = ME.into();
        assert_eq!(w.permissions(&c), ALL);
        w.guilds.get_mut(G).unwrap().owner = "1".into();
        w.guilds.get_mut(G).unwrap().roles.get_mut(MOD).unwrap().permissions = ADMINISTRATOR;
        w.guilds.get_mut(G).unwrap().members.get_mut(ME).unwrap().roles = vec![MOD.into()];
        assert_eq!(w.permissions(&c), ALL);
        w.guilds.get_mut(G).unwrap().members.get_mut(ME).unwrap().timeout_until = Some(100);
        assert_eq!(w.read_only(&c, 50), Some(TIMED_OUT));
        assert_eq!(w.read_only(&c, 150), None);
    }

    #[test]
    fn a_direct_message_is_titled_by_the_other_person_and_masks_parse_from_strings() {
        let mut w = World { own_id: ME.into(), ..Default::default() };
        w.put_channel(&json!({ "id": "5", "type": 1, "recipients": [{ "id": "6", "username": "ada", "global_name": "Ada L" }] }));
        assert_eq!(w.title_of(&w.channels["5"]), "Ada L");
        w.put_channel(&json!({ "id": ME, "type": 999 }));
        assert_eq!(w.title_of(&w.channels[ME]), "Personal Notes");
        assert_eq!(mask(&json!("9223372036854775807")), 9223372036854775807);
        assert_eq!(mask(&json!(1024)), 1024);
        assert_eq!(Kind::from(11), Kind::Thread);
        assert!(Kind::from(3).private());
    }
}
