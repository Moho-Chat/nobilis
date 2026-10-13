//! Steam chat groups: servers with rooms in them.
//!
//! A group is what Steam's own client calls a chat room group - a name, a
//! picture, members with ranks, and one or more rooms to talk in. Here a group
//! is a server on the rail and each room is a channel under it, the way a
//! Discord guild is. A group that belongs to a Steam community group (a clan)
//! is the same thing with a `clanid` on it; nothing is done differently.
//!
//! What Steam says about the groups is kept here for the account, whole, and
//! the window is shown a reading of it: the rail entry, a channel for each
//! room, and a member list for each of those. Messages are stored under the
//! same `second:ordinal` id the direct messages use, so a message heard live
//! and again from history is one message.
//!
//! The account's connection tells it which groups it is "active" in; Steam
//! sends messages only for those, so every group the account is in is named
//! when it signs in and any it joins afterwards is added.

use super::chat::{self, message_id};
use super::friends::{self, Roster};
use super::link::Link;
use super::proto::*;
use super::{presence, reactions, steamid};
use crate::model::{Buffer, BufferGroup};
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{LazyLock, Mutex};

/// `EChatRoomJoinState`: in the group, as against invited or gone.
pub const JOINED: i32 = 2;

/// `EChatRoomGroupRank`.
pub const RANK_VIEWER: i32 = 10;
pub const RANK_GUEST: i32 = 15;
pub const RANK_MEMBER: i32 = 20;
pub const RANK_MODERATOR: i32 = 30;
pub const RANK_OFFICER: i32 = 40;
pub const RANK_OWNER: i32 = 50;

/// `EChatRoomServerMessage`: the group saying something about itself.
const SERVER_RENAMED: i32 = 1;
const SERVER_JOINED: i32 = 2;
const SERVER_PARTED: i32 = 3;
const SERVER_KICKED: i32 = 4;
const SERVER_INVITED: i32 = 5;
const SERVER_TAGLINE: i32 = 9;
const SERVER_AVATAR: i32 = 10;
const SERVER_JOINED_MANY: i32 = 12;
const SERVER_PARTED_MANY: i32 = 13;

/// `EChatRoomMemberStateChange`.
const CHANGE_JOINED: i32 = 1;
const CHANGE_PARTED: i32 = 2;
const CHANGE_KICKED: i32 = 3;
const CHANGE_RANK: i32 = 7;
const CHANGE_BANNED: i32 = 10;
const CHANGE_ROLES: i32 = 12;

/// How many messages one request for history asks for.
pub const PAGE: u32 = 50;

/// What a room with no name of its own is called: the first room of a group has none.
const UNNAMED_ROOM: &str = "General";

/// How long a message may be.
pub const MAX_LENGTH: usize = 2000;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Member {
    pub account: u32,
    pub rank: i32,
    pub roles: Vec<u64>,
    /// When a mute or a kick ends, unix seconds; zero when there is none.
    pub kick_expire: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Room {
    pub id: u64,
    pub name: String,
    pub voice: bool,
    /// Who is in this room's voice chat now, by account number.
    pub in_voice: Vec<u32>,
    pub sort: u32,
    /// The second of its newest message, as Steam last said.
    pub last_message: u32,
    /// The second this account has read it to.
    pub acked: u32,
    pub last_mention: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Group {
    pub id: u64,
    pub name: String,
    pub tagline: String,
    pub owner: u32,
    pub clan: Option<u32>,
    pub avatar: Option<String>,
    pub roles: Vec<(u64, String)>,
    /// What each role may do, where Steam has said.
    pub role_actions: HashMap<u64, ChatRoleActions>,
    pub default_role: u64,
    pub members: Vec<Member>,
    pub rooms: Vec<Room>,
    pub default_room: u64,
    pub kicked: Vec<Member>,
    /// This account's own rank, and number.
    pub my_rank: i32,
    pub me: u32,
    pub muted: bool,
    pub notify: i32,
    pub disabled: bool,
    /// Members Steam says may not be who they appear to be, by account number.
    pub suspicious: std::collections::BTreeSet<u32>,
}

impl Group {
    fn room_mut(&mut self, id: u64) -> Option<&mut Room> {
        self.rooms.iter_mut().find(|r| r.id == id)
    }

    /// Whether this account holds a permission: the owner holds all; others hold
    /// what their roles (and the default one) allow. Where Steam has not said
    /// what the roles allow, the rank stands in for it.
    fn can(&self, by_actions: impl Fn(&ChatRoleActions) -> bool, by_rank: impl Fn(i32) -> bool) -> bool {
        if self.my_rank == RANK_OWNER {
            return true;
        }
        if self.role_actions.is_empty() {
            return by_rank(self.my_rank);
        }
        let mine = self.members.iter().find(|m| m.account == self.me).map(|m| m.roles.clone()).unwrap_or_default();
        mine.iter().chain(std::iter::once(&self.default_role)).filter_map(|r| self.role_actions.get(r)).any(&by_actions)
    }

    /// Whether the account may speak here: a viewer or a guest reads only.
    pub fn may_speak(&self) -> bool {
        self.can(|a| a.can_chat.unwrap_or(false), |r| r >= RANK_MEMBER || r == 0)
    }

    /// Whether this account may kick, mute and ban.
    pub fn may_moderate(&self) -> bool {
        self.can(|a| a.can_kick.unwrap_or(false) || a.can_ban.unwrap_or(false), |r| r >= RANK_MODERATOR)
    }

    /// Whether this account may change the group itself: its rooms, its name.
    pub fn may_manage(&self) -> bool {
        self.can(|a| a.can_create_rename_delete_channel.unwrap_or(false) || a.can_change_tagline_avatar_name.unwrap_or(false), |r| r >= RANK_OFFICER)
    }

    /// Whether this account may make invite links.
    pub fn may_invite(&self) -> bool {
        self.can(|a| a.can_invite.unwrap_or(false), |r| r >= RANK_MEMBER)
    }

    /// Whether this account may change roles and who has them.
    pub fn may_change_roles(&self) -> bool {
        self.can(|a| a.can_change_group_roles.unwrap_or(false) || a.can_change_user_roles.unwrap_or(false), |r| r >= RANK_OFFICER)
    }

    fn take_header(&mut self, h: &ChatRoomGroupHeaderState) {
        if let Some(name) = &h.chat_name {
            self.name = name.clone();
        }
        if let Some(t) = &h.tagline {
            self.tagline = t.clone();
        }
        if let Some(o) = h.accountid_owner {
            self.owner = o;
        }
        if h.clanid.is_some() {
            self.clan = h.clanid;
        }
        self.avatar = h
            .avatar_ugc_url
            .clone()
            .filter(|u| !u.is_empty())
            .or_else(|| h.avatar_sha.as_deref().filter(|s| !s.is_empty() && s.iter().any(|b| *b != 0)).map(friends::avatar_url))
            .or(self.avatar.take());
        if !h.roles.is_empty() {
            self.roles = h.roles.iter().filter_map(|r| Some((r.role_id?, pretty_role(r.name.as_deref().unwrap_or_default())))).collect();
        }
        if !h.role_actions.is_empty() {
            self.role_actions = h.role_actions.iter().filter_map(|a| Some((a.role_id?, a.clone()))).collect();
        }
        if let Some(d) = h.default_role_id {
            self.default_role = d;
        }
        self.disabled = h.disabled.unwrap_or(false);
    }

    fn take_rooms(&mut self, rooms: &[ChatRoomState]) {
        let old: HashMap<u64, Room> = self.rooms.drain(..).map(|r| (r.id, r)).collect();
        for r in rooms {
            let Some(id) = r.chat_id else { continue };
            // A room can be named twice at once - by a notification and by the answer to the
            // call that made it - and is one room.
            if self.rooms.iter().any(|x| x.id == id) {
                self.rooms.retain(|x| x.id != id);
            }
            let held = old.get(&id);
            self.rooms.push(Room {
                id,
                name: r.chat_name.clone().unwrap_or_default(),
                voice: r.voice_allowed.unwrap_or(false),
                in_voice: r.members_in_voice.clone(),
                sort: r.sort_order.unwrap_or(0),
                last_message: r.time_last_message.unwrap_or_else(|| held.map_or(0, |h| h.last_message)),
                acked: held.map_or(0, |h| h.acked),
                last_mention: held.map_or(0, |h| h.last_mention),
            });
        }
        self.rooms.sort_by_key(|r| (r.sort, r.id));
    }

    fn take_members(&mut self, members: &[ChatRoomMember], own: u32) {
        self.members = members
            .iter()
            .filter(|m| m.state.unwrap_or(JOINED) == JOINED)
            .filter_map(|m| Some(Member { account: m.accountid?, rank: m.rank.unwrap_or(RANK_MEMBER), roles: m.role_ids.clone(), kick_expire: m.time_kick_expire.unwrap_or(0) }))
            .collect();
        self.members.sort_by_key(|m| (std::cmp::Reverse(m.rank), m.account));
        self.me = own;
        if let Some(me) = self.members.iter().find(|m| m.account == own) {
            self.my_rank = me.rank;
        }
    }

    fn take_state(&mut self, s: &ChatRoomGroupState, own: u32) {
        if let Some(h) = &s.header_state {
            self.take_header(h);
        }
        self.take_members(&s.members, own);
        self.take_rooms(&s.chat_rooms);
        if let Some(d) = s.default_chat_id {
            self.default_room = d;
        }
        self.kicked = s.kicked.iter().filter_map(|m| Some(Member { account: m.accountid?, rank: m.rank.unwrap_or(0), roles: Vec::new(), kick_expire: m.time_kick_expire.unwrap_or(0) })).collect();
    }

    fn take_user(&mut self, u: &UserChatRoomGroupState) {
        self.muted = u.desktop_notification_level == Some(1) || u.unread_indicator_muted == Some(true) && u.desktop_notification_level.is_none();
        self.notify = u.desktop_notification_level.unwrap_or(0);
        for r in &u.user_chat_room_state {
            let Some(id) = r.chat_id else { continue };
            if let Some(room) = self.room_mut(id) {
                room.acked = r.time_last_ack.unwrap_or(0);
                room.last_mention = r.time_last_mention.unwrap_or(0);
            }
        }
    }
}

/// What a role is called: Steam's own default roles come as a localisation key.
fn pretty_role(name: &str) -> String {
    name.strip_prefix("#ChatRoomRole_").unwrap_or(name).to_string()
}

/// A rank, said in words.
pub fn rank_word(rank: i32) -> &'static str {
    match rank {
        RANK_OWNER => "Owner",
        RANK_OFFICER => "Officer",
        RANK_MODERATOR => "Moderator",
        RANK_GUEST => "Guest",
        RANK_VIEWER => "Viewer",
        _ => "Member",
    }
}

/// What is known of a person in a group, kept so that a request from the window
/// (history, a settings list) reads names without the connection's roster.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Person {
    pub name: String,
    pub avatar: Option<String>,
    pub friend: bool,
}

/// What is held for one account.
#[derive(Default)]
struct Account {
    groups: BTreeMap<u64, Group>,
    /// The name each room's channel is shown under, to find a rename.
    shown: HashMap<(u64, u64), String>,
    /// The members last put on screen for a channel, to say nothing when they have not changed.
    members: HashMap<String, Value>,
    /// Who people are, as last seen, for what is read when the roster is not to hand.
    people: HashMap<u32, Person>,
    own: u32,
    /// What the window was last told of each group's voice rooms.
    voice_told: HashMap<u64, Value>,
    /// Whether Steam has said what the groups are yet: before it has, an empty
    /// list means nothing, and acting on it would remove every channel.
    loaded: bool,
}

static HELD: LazyLock<Mutex<HashMap<String, Account>>> = LazyLock::new(Default::default);

fn with<R>(account_id: &str, f: impl FnOnce(&mut Account) -> R) -> R {
    let mut all = HELD.lock().unwrap();
    f(all.entry(account_id.to_string()).or_default())
}

pub fn clear(account_id: &str) {
    HELD.lock().unwrap().remove(account_id);
}

/// How a request from the window tells the connection that owns the roster to
/// put the groups on screen again: it is the roster that names the people.
static POKES: LazyLock<Mutex<HashMap<String, std::sync::Arc<tokio::sync::Notify>>>> = LazyLock::new(Default::default);

/// What the connection of an account waits on to be told its groups changed.
pub fn pokes(account_id: &str) -> std::sync::Arc<tokio::sync::Notify> {
    POKES.lock().unwrap().entry(account_id.to_string()).or_default().clone()
}

fn poke(account_id: &str) {
    pokes(account_id).notify_one();
}

pub fn held(account_id: &str, group: u64) -> Option<Group> {
    HELD.lock().unwrap().get(account_id).and_then(|a| a.groups.get(&group).cloned())
}

/// The id of a group on the rail.
pub fn rail_id(account_id: &str, group: u64) -> String {
    format!("{account_id}|steamgroup:{group}")
}

/// The group a rail id names, if it is one of these.
pub fn group_of_rail_id(rail: &str) -> Option<(&str, u64)> {
    let (account, rest) = rail.split_once('|')?;
    Some((account, rest.strip_prefix("steamgroup:")?.parse().ok()?))
}

/// A channel's `remote_id`: the group and the room, which is all it takes to find either.
pub fn remote_id(group: u64, room: u64) -> String {
    format!("{group}:{room}")
}

/// The group and room a channel is, from its `remote_id`.
pub fn ids_of(buffer: &Buffer) -> Option<(u64, u64)> {
    let (g, r) = buffer.remote_id.as_deref()?.split_once(':')?;
    Some((g.parse().ok()?, r.parse().ok()?))
}

/// Whether a buffer is one of these rather than a conversation with a friend.
pub fn is_room(buffer: &Buffer) -> bool {
    buffer.account_id.starts_with("steam:") && buffer.kind == "channel"
}

fn room_title(room: &Room) -> String {
    let name = friends::clean_name(&room.name);
    if name.is_empty() {
        UNNAMED_ROOM.to_string()
    } else {
        name
    }
}

/// What every channel of the account is called: the group, a slash, and the
/// room. Two groups with one name are told apart by their number, since a
/// channel's name is also its identity.
fn titles(groups: &BTreeMap<u64, Group>) -> HashMap<(u64, u64), String> {
    let mut by_name: HashMap<String, usize> = HashMap::new();
    let name_of = |g: &Group| {
        let n = friends::clean_name(&g.name);
        if n.is_empty() { format!("group {}", g.id) } else { n }
    };
    for g in groups.values() {
        *by_name.entry(name_of(g).to_lowercase()).or_default() += 1;
    }
    let mut out = HashMap::new();
    for g in groups.values() {
        let mut base = name_of(g);
        if by_name[&base.to_lowercase()] > 1 {
            base = format!("{base} ({})", g.id);
        }
        // Two rooms of one group with one name: the second and on are numbered.
        let mut seen: HashMap<String, u32> = HashMap::new();
        for r in &g.rooms {
            let mut t = room_title(r);
            let n = seen.entry(t.to_lowercase()).or_default();
            *n += 1;
            if *n > 1 {
                t = format!("{t} ({n})");
            }
            out.insert((g.id, r.id), format!("{base}/#{t}"));
        }
    }
    out
}

/// What the account is told about its groups and what it has read.
pub async fn load(link: &Link, account_id: &str, own: u32) -> Result<Vec<u32>, String> {
    let mine: GetMyChatRoomGroupsResponse = link.call("ChatRoom.GetMyChatRoomGroups#1", &GetMyChatRoomGroupsRequest {}, true).await.map_err(|e| chat::refusal(&e))?;
    let ids: Vec<u64> = mine.chat_room_groups.iter().filter_map(|p| p.group_summary.as_ref()?.chat_group_id.or(p.user_chat_group_state.as_ref()?.chat_group_id)).collect();
    // The groups' members come with being active in them.
    let active: SetSessionActiveGroupsResponse = if ids.is_empty() {
        SetSessionActiveGroupsResponse::default()
    } else {
        link.call("ChatRoom.SetSessionActiveChatRoomGroups#1", &SetSessionActiveGroupsRequest { chat_group_ids: ids.clone(), chat_groups_data_requested: ids.clone(), virtualize_members_threshold: None }, true)
            .await
            .map_err(|e| chat::refusal(&e))?
    };
    let mut groups = BTreeMap::new();
    for pair in &mine.chat_room_groups {
        let Some(summary) = &pair.group_summary else { continue };
        let Some(id) = summary.chat_group_id else { continue };
        let mut g = Group { id, name: summary.chat_group_name.clone().unwrap_or_default(), tagline: summary.chat_group_tagline.clone().unwrap_or_default(), owner: summary.accountid_owner.unwrap_or(0), clan: summary.clanid.filter(|c| *c != 0), my_rank: summary.rank.unwrap_or(0), default_room: summary.default_chat_id.unwrap_or(0), default_role: summary.default_role_id.unwrap_or(0), disabled: summary.disabled.unwrap_or(false), ..Default::default() };
        g.avatar = summary.avatar_ugc_url.clone().filter(|u| !u.is_empty()).or_else(|| summary.chat_group_avatar_sha.as_deref().filter(|s| s.iter().any(|b| *b != 0)).map(friends::avatar_url));
        g.take_rooms(&summary.chat_rooms);
        if let Some(full) = active.chat_states.iter().find(|s| s.header_state.as_ref().and_then(|h| h.chat_group_id) == Some(id)) {
            g.take_state(full, own);
        }
        if let Some(user) = &pair.user_chat_group_state {
            g.take_user(user);
        }
        g.suspicious = active.chat_group_notices.iter().filter(|n| n.chat_group_id == Some(id)).flat_map(|n| n.member_notices.iter()).filter(|n| n.notice_type == Some(super::notices::SUSPICIOUS)).filter_map(|n| n.accountid).collect();
        groups.insert(id, g);
    }
    let people: Vec<u32> = groups.values().flat_map(|g| g.members.iter().map(|m| m.account)).chain(groups.values().map(|g| g.owner)).filter(|a| *a != 0 && *a != own).collect();
    with(account_id, |a| {
        a.groups = groups;
        a.own = own;
        a.loaded = true;
    });
    Ok(people)
}

/// Brings the window in line with what is held: the rail entry for each group,
/// a channel for each room under it, and who is in them. Says nothing about
/// what has not changed.
pub fn publish(state: &AppState, account_id: &str, roster: &Roster) {
    // The channels that already exist for this account, as the daemon restored
    // them, so a room renamed while moho was closed is moved and not made twice.
    let existing: Vec<(u64, u64, String)> = state
        .runtime
        .list_buffers()
        .into_iter()
        .filter(|b| b.account_id == account_id && is_room(b))
        .filter_map(|b| ids_of(&b).map(|(g, r)| (g, r, b.name)))
        .collect();
    let Some((groups, titles_now, mut shown_before, own)) = with(account_id, |a| a.loaded.then(|| (a.groups.clone(), titles(&a.groups), a.shown.clone(), a.own))) else { return };
    for (g, r, name) in existing {
        shown_before.entry((g, r)).or_insert(name);
    }
    let rails: Vec<String> = groups.keys().map(|g| rail_id(account_id, *g)).collect();
    // A group that is gone: it takes its channels with it.
    for buffer_group in state.runtime.list_buffer_groups() {
        if buffer_group.account_id == account_id && buffer_group.id.contains("|steamgroup:") && !rails.contains(&buffer_group.id) {
            state.runtime.remove_buffer_group(state, &buffer_group.id);
        }
    }
    let mut names = HashMap::new();
    for (position, g) in groups.values().enumerate() {
        let rail = rail_id(account_id, g.id);
        state.runtime.upsert_buffer_group(
            state,
            BufferGroup { id: rail.clone(), account_id: account_id.to_string(), service: "steam".to_string(), kind: "guild".to_string(), name: friends::clean_name(&g.name), icon_url: g.avatar.clone(), position: position as i64, pending: false },
        );
        let member_list = member_json(g, roster, own);
        for r in &g.rooms {
            // A voice-only room has nothing to read: voice is Steam's own client's.
            let Some(name) = titles_now.get(&(g.id, r.id)).cloned() else { continue };
            if let Some(old) = shown_before.get(&(g.id, r.id)).filter(|old| **old != name) {
                state.runtime.rename_buffer(state, account_id, old, &name, "channel");
            }
            let buffer = state.runtime.ensure_buffer(state, account_id, &name, "channel");
            state.runtime.set_buffer_remote_id(state, &buffer.id, &remote_id(g.id, r.id));
            state.runtime.set_buffer_group(state, &buffer.id, &rail);
            state.runtime.set_buffer_category(state, &buffer.id, None, i64::from(r.sort));
            state.runtime.set_buffer_topic(state, &buffer.id, &g.tagline);
            state.runtime.set_buffer_read_only(state, &buffer.id, (!g.may_speak()).then_some("You can read this group but not write in it."));
            state.runtime.set_silenced(state, &buffer.id, g.muted);
            state.runtime.set_buffer_notice(state, &buffer.id, suspicious_notice(account_id, g, roster));
            if let Some(url) = &g.avatar {
                state.runtime.set_buffer_avatar(state, &buffer.id, url);
            }
            let changed = with(account_id, |a| a.members.insert(buffer.id.clone(), member_list.clone()).as_ref() != Some(&member_list));
            if changed {
                state.runtime.set_presence(&buffer.id, member_list.clone());
                state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": member_list }));
            }
            names.insert((g.id, r.id), name);
        }
    }
    // Rooms that are gone.
    for ((g, r), old) in shown_before.iter() {
        if !names.contains_key(&(*g, *r)) {
            state.runtime.remove_buffer(state, &crate::model::buffer_id(account_id, old));
            with(account_id, |a| a.members.remove(&crate::model::buffer_id(account_id, old)));
        }
    }
    for g in groups.values() {
        let now = voice_rooms(account_id, g.id);
        let changed = with(account_id, |a| a.voice_told.insert(g.id, now.clone()).as_ref() != Some(&now));
        if changed {
            tell_voice_rooms(state, account_id, g.id);
        }
    }
    let everyone: Vec<u32> = groups.values().flat_map(|g| g.members.iter().map(|m| m.account)).collect();
    learn(account_id, roster, everyone);
    with(account_id, |a| a.shown = names);
}

/// Remembers who these people are, from the roster.
pub fn learn(account_id: &str, roster: &Roster, accounts: impl IntoIterator<Item = u32>) {
    with(account_id, |a| {
        for account in accounts {
            if let Some(name) = roster.persona(account).and_then(|p| p.name.clone()).or_else(|| roster.nickname(account).map(str::to_string)) {
                a.people.insert(account, Person { name: friends::clean_name(&name), avatar: roster.persona(account).and_then(|p| p.avatar.clone()), friend: roster.is_friend(account) });
            }
        }
    });
}

/// What is known of somebody; their number, for a name, when nothing is.
fn person(account_id: &str, account: u32) -> Person {
    with(account_id, |a| a.people.get(&account).cloned()).unwrap_or_else(|| Person { name: steamid::text_of(account), avatar: None, friend: false })
}

/// The banner over a group's rooms while Steam warns of somebody in it.
fn suspicious_notice(account_id: &str, g: &Group, roster: &Roster) -> Option<crate::model::BufferNotice> {
    if g.suspicious.is_empty() {
        return None;
    }
    let names: Vec<String> = g.suspicious.iter().map(|a| if roster.has_name(*a) { who(roster, *a) } else { person(account_id, *a).name }).collect();
    let list = names.join(", ");
    Some(crate::model::BufferNotice { kind: "suspicious".to_string(), text: format!("Steam warns that {list} may not be who they appear to be. Be careful with links, offers and requests for anything - and do not give out your account details.") })
}

/// The people in a group as the window's member list reads them: online ones
/// with what they are playing, the owner and moderators carrying their rank.
fn member_json(g: &Group, roster: &Roster, own: u32) -> Value {
    let role_name = |id: u64| g.roles.iter().find(|(r, _)| *r == id).map(|(_, n)| n.clone());
    let list: Vec<Value> = g
        .members
        .iter()
        .map(|m| {
            let persona = roster.persona(m.account);
            let name = roster.nickname(m.account).map(friends::clean_name).filter(|n| !n.is_empty()).or_else(|| persona.and_then(|p| p.name.clone())).unwrap_or_else(|| steamid::text_of(m.account));
            // Ourselves are known to Steam under the name we signed in with.
            let status = if m.account == own { persona.map_or("online", |p| presence::status_of(p.state)) } else { persona.map_or("offline", |p| presence::status_of(p.state)) };
            let activity = persona.and_then(|p| presence::activity(p.state, roster.game_of(m.account).as_deref()));
            let mut roles: Vec<String> = m.roles.iter().filter_map(|r| role_name(*r)).filter(|n| n != "Member" || m.rank < RANK_MODERATOR).collect();
            let word = rank_word(m.rank);
            if m.rank >= RANK_MODERATOR && !roles.iter().any(|r| r == word) {
                roles.insert(0, word.to_string());
            }
            json!({
                "nick": name,
                "userId": steamid::text_of(m.account),
                "prefix": match m.rank { RANK_OWNER => "👑", RANK_OFFICER | RANK_MODERATOR => "🛡", _ => "" },
                "status": status,
                "away": status == "idle",
                "activity": activity.map(|(kind, text)| json!({ "kind": kind, "text": text })),
                "avatarUrl": persona.and_then(|p| p.avatar.clone()),
                "roles": roles,
                "powerLevel": m.rank,
            })
        })
        .collect();
    Value::Array(list)
}

/// A person's name for a line of conversation.
fn who(roster: &Roster, account: u32) -> String {
    roster.nickname(account).map(friends::clean_name).filter(|n| !n.is_empty()).or_else(|| roster.persona(account).and_then(|p| p.name.clone())).unwrap_or_else(|| steamid::text_of(account))
}

/// The name a room's channel is shown under, if it is one the account has.
fn channel_of(account_id: &str, group: u64, room: u64) -> Option<String> {
    with(account_id, |a| a.shown.get(&(group, room)).cloned())
}

/// Whether any of these people are in a group of the account's: their names and
/// states are on screen in a member list, which has to follow.
pub fn involves(account_id: &str, people: &[u32]) -> bool {
    with(account_id, |a| a.loaded && a.groups.values().any(|g| g.members.iter().any(|m| people.contains(&m.account))))
}

/// Somebody joined or left a room's voice chat.
pub fn voice_changed(state: &AppState, account_id: &str, group: u64, room: u64, user: u32, joined: bool) {
    with(account_id, |a| {
        if let Some(r) = a.groups.get_mut(&group).and_then(|g| g.room_mut(room)) {
            r.in_voice.retain(|u| *u != user);
            if joined {
                r.in_voice.push(user);
            }
        }
    });
    tell_voice_rooms(state, account_id, group);
}

/// The rooms of a group that have voice, and who is in each, as the window lists them.
pub fn voice_rooms(account_id: &str, group: u64) -> Value {
    with(account_id, |a| {
        let Some(g) = a.groups.get(&group) else { return json!([]) };
        let rooms: Vec<Value> = g
            .rooms
            .iter()
            .filter(|r| r.voice)
            .map(|r| {
                json!({
                    "roomId": r.id.to_string(),
                    "bufferId": a.shown.get(&(g.id, r.id)).map(|n| crate::model::buffer_id(account_id, n)),
                    "name": room_title(r),
                    "members": r.in_voice.iter().map(|m| {
                        let p = a.people.get(m);
                        json!({ "userId": steamid::text_of(*m), "nick": p.map(|p| p.name.clone()).unwrap_or_else(|| steamid::text_of(*m)), "avatarUrl": p.and_then(|p| p.avatar.clone()), "isSelf": *m == a.own })
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        Value::Array(rooms)
    })
}

/// Tells the window who is in a group's voice rooms.
fn tell_voice_rooms(state: &AppState, account_id: &str, group: u64) {
    state.events.emit("steamVoiceRooms", json!({ "accountId": account_id, "groupId": group.to_string(), "rooms": voice_rooms(account_id, group) }));
}

/// The accounts in groups the roster has no name for, to be asked about.
pub fn unknown(account_id: &str, roster: &Roster) -> Vec<u32> {
    with(account_id, |a| a.groups.values().flat_map(|g| g.members.iter().map(|m| m.account)).filter(|m| *m != a.own && !roster.has_name(*m)).collect())
}

/// Puts a server's line in a room: someone joined, a room was renamed.
fn say(state: &AppState, account_id: &str, name: &str, text: &str, at: u32) {
    state.runtime.record_message_at(state, account_id, name, "channel", "*", text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None, Some(i64::from(at)).filter(|t| *t > 0), None, None);
}

/// What a message from the server says, in words.
fn server_line(name_of: &dyn Fn(u32) -> String, m: &ServerMessage, text_param: &str) -> Option<String> {
    let person = m.accountid_param.map(name_of).unwrap_or_default();
    Some(match m.message? {
        SERVER_JOINED | SERVER_JOINED_MANY => format!("{person} joined"),
        SERVER_PARTED | SERVER_PARTED_MANY => format!("{person} left"),
        SERVER_KICKED => format!("{person} was removed"),
        SERVER_INVITED => format!("{person} was invited"),
        SERVER_RENAMED => format!("the room was renamed to {}", m.string_param.as_deref().unwrap_or(text_param)),
        SERVER_TAGLINE => "the group's tagline changed".to_string(),
        SERVER_AVATAR => "the group's picture changed".to_string(),
        _ => return None,
    })
}

/// What a notification asked the caller to follow up.
#[derive(Default)]
pub struct Heard {
    /// People whose names are not known yet.
    pub ask: Vec<u32>,
    /// Groups to read again from Steam: ones that appeared or are not known.
    pub reload: bool,
}

/// One notification from the ChatRoomClient service.
pub fn heard(state: &AppState, account_id: &str, roster: &Roster, method: &str, body: &[u8]) -> Option<Heard> {
    use prost::Message;
    let own = with(account_id, |a| a.own);
    let mut out = Heard::default();
    match method {
        "ChatRoomClient.NotifyIncomingChatMessage#1" => {
            let n = ChatIncoming::decode(body).ok()?;
            let (group, room) = (n.chat_group_id?, n.chat_id?);
            let Some(name) = channel_of(account_id, group, room) else {
                out.reload = true;
                return Some(out);
            };
            let at = n.timestamp.unwrap_or(0);
            let sender = n.steamid_sender.and_then(steamid::account_of).unwrap_or(0);
            if let Some(server) = &n.server_message {
                if let Some(line) = server_line(&|a| who(roster, a), server, n.chat_name.as_deref().unwrap_or_default()) {
                    say(state, account_id, &name, &line, at);
                }
                if matches!(server.message, Some(SERVER_JOINED | SERVER_PARTED | SERVER_KICKED | SERVER_JOINED_MANY | SERVER_PARTED_MANY)) {
                    out.reload = true;
                }
                if let Some(a) = server.accountid_param.filter(|a| !roster.has_name(*a)) {
                    out.ask.push(a);
                }
                return Some(out);
            }
            let rendered = super::bbcode::render_message(n.message.as_deref().unwrap_or_default());
            let text = Some(rendered.text).filter(|t| !t.trim().is_empty()).or_else(|| n.message_no_bbcode.clone().map(|t| t.trim_end().to_string()))?;
            if text.trim().is_empty() {
                return None;
            }
            learn(account_id, roster, [sender]);
            let from_us = sender == own;
            let mentioned = n.mentions.as_ref().is_some_and(|m| m.mention_all.unwrap_or(false) || m.mention_here.unwrap_or(false) || m.mention_accountids.contains(&own));
            if !roster.has_name(sender) && sender != 0 {
                out.ask.push(sender);
            }
            let sender_name = if from_us { state.runtime.own_identity(account_id).unwrap_or_default() } else { who(roster, sender) };
            let shown = if from_us { text } else { super::account::filtered(account_id, roster.is_friend(sender), &text) };
            state.runtime.record_message_at(
                state,
                account_id,
                &name,
                "channel",
                &sender_name,
                &shown,
                false,
                "chat",
                None,
                Some(message_id(at, n.ordinal.unwrap_or(0))),
                mentioned && !from_us,
                roster.persona(sender).filter(|_| !from_us).and_then(|p| p.avatar.clone()),
                rendered.cards,
                Vec::new(),
                (!from_us).then(|| steamid::text_of(sender)),
                Some(i64::from(at)).filter(|t| *t > 0),
                None,
                None,
            );
            with(account_id, |a| {
                if let Some(r) = a.groups.get_mut(&group).and_then(|g| g.room_mut(room)) {
                    r.last_message = r.last_message.max(at);
                }
            });
        }
        "ChatRoomClient.NotifyChatMessageModified#1" => {
            let n = ChatModified::decode(body).ok()?;
            let name = channel_of(account_id, n.chat_group_id?, n.chat_id?)?;
            let buffer_id = crate::model::buffer_id(account_id, &name);
            for m in n.messages.iter().filter(|m| m.deleted.unwrap_or(false)) {
                let id = message_id(m.server_timestamp.unwrap_or(0), m.ordinal.unwrap_or(0));
                state.runtime.delete_message(state, &buffer_id, &id);
            }
        }
        "ChatRoomClient.NotifyMessageReaction#1" => {
            let n = ChatReactionNotice::decode(body).ok()?;
            let name = channel_of(account_id, n.chat_group_id?, n.chat_id?)?;
            let notice = MessageReactionNotice { steamid_friend: None, server_timestamp: n.server_timestamp, ordinal: n.ordinal, reactor: n.reactor, reaction_type: n.reaction_type, reaction: n.reaction.clone(), is_add: n.is_add };
            reactions::heard(state, &crate::model::buffer_id(account_id, &name), own, &notice);
        }
        "ChatRoomClient.NotifyMemberStateChange#1" => {
            let n = ChatMemberChange::decode(body).ok()?;
            let group = n.chat_group_id?;
            let member = n.member?;
            let account = member.accountid?;
            let change = n.change.unwrap_or(0);
            if !roster.has_name(account) {
                out.ask.push(account);
            }
            let known = with(account_id, |a| {
                let Some(g) = a.groups.get_mut(&group) else { return false };
                match change {
                    CHANGE_PARTED | CHANGE_KICKED | CHANGE_BANNED => g.members.retain(|m| m.account != account),
                    CHANGE_JOINED | CHANGE_RANK | CHANGE_ROLES => {
                        let fresh = Member { account, rank: member.rank.unwrap_or(RANK_MEMBER), roles: member.role_ids.clone(), kick_expire: member.time_kick_expire.unwrap_or(0) };
                        match g.members.iter_mut().find(|m| m.account == account) {
                            Some(m) => *m = fresh,
                            None => g.members.push(fresh),
                        }
                        g.members.sort_by_key(|m| (std::cmp::Reverse(m.rank), m.account));
                        if account == own {
                            g.my_rank = member.rank.unwrap_or(g.my_rank);
                        }
                    }
                    _ => {}
                }
                true
            });
            if !known {
                out.reload = true;
            } else {
                publish(state, account_id, roster);
                // Removed from a group means it is gone from the rail.
                if account == own && matches!(change, CHANGE_PARTED | CHANGE_KICKED | CHANGE_BANNED) {
                    with(account_id, |a| a.groups.remove(&group));
                    publish(state, account_id, roster);
                }
            }
        }
        "ChatRoomClient.NotifyChatRoomHeaderStateChange#1" => {
            let n = ChatHeaderChange::decode(body).ok()?;
            let h = n.header_state?;
            let group = h.chat_group_id?;
            let known = with(account_id, |a| a.groups.get_mut(&group).map(|g| g.take_header(&h)).is_some());
            if known {
                publish(state, account_id, roster);
            } else {
                out.reload = true;
            }
        }
        "ChatRoomClient.NotifyChatRoomGroupRoomsChange#1" => {
            let n = ChatRoomsChange::decode(body).ok()?;
            let group = n.chat_group_id?;
            let known = with(account_id, |a| {
                a.groups.get_mut(&group).map(|g| {
                    g.take_rooms(&n.chat_rooms);
                    if let Some(d) = n.default_chat_id {
                        g.default_room = d;
                    }
                })
                .is_some()
            });
            if known {
                publish(state, account_id, roster);
            } else {
                out.reload = true;
            }
        }
        // Joined, or left, from another client of the account.
        "ChatRoomClient.NotifyChatGroupUserStateChanged#1" => {
            let n = ChatUserStateChange::decode(body).ok()?;
            let group = n.chat_group_id?;
            // 1 joined, 2 parted, 3 invited, 4 dismissed
            if matches!(n.user_action, Some(2)) {
                with(account_id, |a| a.groups.remove(&group));
                publish(state, account_id, roster);
            } else if matches!(n.user_action, Some(1)) {
                out.reload = true;
            } else if let Some(user) = &n.user_chat_group_state {
                let known = with(account_id, |a| a.groups.get_mut(&group).map(|g| g.take_user(user)).is_some());
                if known {
                    publish(state, account_id, roster);
                }
            }
        }
        // Steam warning of somebody in the group, or lifting the warning.
        "ChatRoomClient.NotifyChatRoomNotice#1" => {
            let n = ChatRoomNoticeNotification::decode(body).ok()?;
            if n.notice_type != Some(super::notices::SUSPICIOUS) {
                return None;
            }
            let (group, who) = (n.chat_group_id?, n.steamid_sender.and_then(steamid::account_of)?);
            let known = with(account_id, |a| {
                a.groups.get_mut(&group).map(|g| {
                    if n.active.unwrap_or(true) {
                        g.suspicious.insert(who);
                    } else {
                        g.suspicious.remove(&who);
                    }
                })
                .is_some()
            });
            if known {
                if !roster.has_name(who) {
                    out.ask.push(who);
                }
                publish(state, account_id, roster);
            }
        }
        // Read on another client.
        "ChatRoomClient.NotifyAckChatMessageEcho#1" => {
            let n = ChatAck::decode(body).ok()?;
            let (group, room) = (n.chat_group_id?, n.chat_id?);
            let name = channel_of(account_id, group, room)?;
            let buffer_id = crate::model::buffer_id(account_id, &name);
            let at = n.timestamp.unwrap_or(0);
            with(account_id, |a| {
                if let Some(r) = a.groups.get_mut(&group).and_then(|g| g.room_mut(room)) {
                    r.acked = r.acked.max(at);
                }
            });
            let newest = state.store.get_backlog(&buffer_id, 0, 1).ok().and_then(|m| m.last().map(|m| m.ts)).unwrap_or(0);
            if i64::from(at) >= newest {
                state.events.emit("bufferRead", json!({ "accountId": account_id, "bufferId": buffer_id }));
            }
        }
        _ => return None,
    }
    Some(out)
}

/// A message read out of history, ready to store.
struct Past {
    account: u32,
    at: u32,
    ordinal: u32,
    text: String,
    cards: Vec<crate::model::Embed>,
    reactions: Vec<crate::model::Reaction>,
    server: Option<ServerMessage>,
}

fn read_past(m: &ChatHistoryMessage, own: u32) -> Option<Past> {
    if m.deleted.unwrap_or(false) {
        return None;
    }
    let rendered = super::bbcode::render_message(m.message.as_deref().unwrap_or_default());
    if rendered.text.trim().is_empty() && m.server_message.is_none() {
        return None;
    }
    let reactions = m
        .reactions
        .iter()
        .filter_map(|r| {
            let emoji = reactions::token(r.reaction_type.unwrap_or(reactions::EMOTICON), r.reaction.as_deref()?)?;
            (r.num_reactors.unwrap_or(0) > 0).then(|| crate::model::Reaction { emoji, count: i64::from(r.num_reactors.unwrap_or(0)), me: r.has_user_reacted.unwrap_or(false), animated: false })
        })
        .collect();
    let _ = own;
    Some(Past { account: m.sender.unwrap_or(0), at: m.server_timestamp.unwrap_or(0), ordinal: m.ordinal.unwrap_or(0), text: rendered.text, cards: rendered.cards, reactions, server: m.server_message.clone() })
}

/// Asks for a page of a room, oldest first, and whether there is more before it.
async fn fetch(link: &Link, own: u32, group: u64, room: u64, older_than: Option<(u32, u32)>) -> Result<Vec<Past>, String> {
    let (last_time, last_ordinal) = older_than.unwrap_or((u32::MAX, 0));
    let answer: ChatHistoryResponse = link
        .call("ChatRoom.GetMessageHistory#1", &ChatHistoryRequest { chat_group_id: Some(group), chat_id: Some(room), last_time: Some(last_time), last_ordinal: Some(last_ordinal), start_time: Some(0), start_ordinal: Some(0), max_count: Some(PAGE) }, true)
        .await
        .map_err(|e| chat::refusal(&e))?;
    let mut past: Vec<Past> = answer.messages.iter().filter_map(|m| read_past(m, own)).collect();
    // Steam answers newest first.
    past.reverse();
    Ok(past)
}

fn store_quietly(state: &AppState, account_id: &str, name: &str, own: u32, past: &[Past]) -> usize {
    let buffer_id = crate::model::buffer_id(account_id, name);
    let own_name = state.runtime.own_identity(account_id).unwrap_or_default();
    let mut added = 0;
    for p in past {
        let id = message_id(p.at, p.ordinal);
        if let Some(server) = &p.server {
            if let Some(line) = server_line(&|a| person(account_id, a).name, server, "") {
                if matches!(state.store.append_message(&buffer_id, &id, "*", &line, i64::from(p.at), false, false, "system", None, &[], false, None, &[], &[], None, None, "channel", None, &[]), Ok(true)) {
                    added += 1;
                }
            }
            continue;
        }
        let is_own = p.account == own;
        let who = person(account_id, p.account);
        let from = if is_own { own_name.clone() } else { who.name.clone() };
        let sender = (!is_own).then(|| steamid::text_of(p.account));
        let text = if is_own { p.text.clone() } else { super::account::filtered(account_id, who.friend, &p.text) };
        let avatar = who.avatar.clone().filter(|_| !is_own);
        let stored = state.store.append_message(&buffer_id, &id, &from, &text, i64::from(p.at), false, false, "chat", None, &p.reactions, is_own, avatar.as_deref(), &p.cards, &[], sender.as_deref(), None, "channel", None, &[]);
        if matches!(stored, Ok(true)) {
            added += 1;
        } else if !p.reactions.is_empty() {
            let _ = state.store.set_reactions(&buffer_id, &id, &p.reactions);
        }
    }
    added
}

/// The recent end of a room, or the part before what is shown: what opening or
/// scrolling asks for. Quiet, because it is old.
pub async fn history(state: &AppState, buffer: &Buffer, before: Option<i64>) -> Result<usize, String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let (group, room) = ids_of(buffer).ok_or("this is not a Steam chat room")?;
    let own = with(&buffer.account_id, |a| a.own);
    let older = match before {
        Some(b) => Some((u32::try_from(b).map_err(|_| "that is not a time Steam knows".to_string())?, u32::MAX)),
        None => None,
    };
    let past = fetch(&link, own, group, room, older).await?;
    Ok(store_quietly(state, &buffer.account_id, &buffer.name, own, &past))
}

/// Turns `@Name` for each of these people into the tag that pings them: Steam
/// notices a mention only when it is written as one. Longest names first, so
/// "@Sam Smith" is not taken for "@Sam"; and only where the name ends at a
/// word boundary, so "@Sammy" does not ping Sam.
pub fn mentionize(text: &str, people: &[(u32, String)]) -> String {
    let mut people: Vec<&(u32, String)> = people.iter().filter(|(_, n)| !n.is_empty()).collect();
    people.sort_by_key(|(_, n)| std::cmp::Reverse(n.chars().count()));
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let starts_word = i == 0 || chars[i - 1].is_whitespace();
        if chars[i] == '@' && starts_word {
            let found = people.iter().find(|(_, name)| {
                let n: Vec<char> = name.chars().collect();
                let end = i + 1 + n.len();
                end <= chars.len() && chars[i + 1..end].iter().zip(&n).all(|(a, b)| a.to_lowercase().eq(b.to_lowercase())) && chars.get(end).is_none_or(|c| !c.is_alphanumeric() && *c != '_')
            });
            if let Some((account, name)) = found {
                out.push_str(&format!("[mention={account}]@{name}[/mention]"));
                i += 1 + name.chars().count();
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Sends words to a room, and stores them.
pub async fn send(state: &AppState, buffer: &Buffer, text: &str) -> Result<(), String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let (group, room) = ids_of(buffer).ok_or("this is not a Steam chat room")?;
    let text = text.trim_end();
    if text.trim().is_empty() {
        return Err("there is nothing to send".to_string());
    }
    if text.chars().count() > MAX_LENGTH {
        return Err(format!("Steam takes messages of up to {MAX_LENGTH} characters; this one is {}", text.chars().count()));
    }
    // People named with an @ are pinged.
    let people: Vec<(u32, String)> = held(&buffer.account_id, group).map(|g| g.members.iter().map(|m| (m.account, person(&buffer.account_id, m.account).name)).collect()).unwrap_or_default();
    let text = mentionize(text, &people);
    let answer: ChatSendResponse = link
        .call("ChatRoom.SendChatMessage#1", &ChatSendRequest { chat_group_id: Some(group), chat_id: Some(room), message: Some(text.clone()), echo_to_sender: Some(false) }, true)
        .await
        .map_err(|e| super::explained(&buffer.account_id, chat::refusal(&e)))?;
    let said = answer.modified_message.as_deref().filter(|m| !m.trim().is_empty()).unwrap_or(&text);
    let rendered = super::bbcode::render_message(said);
    let at = answer.server_timestamp.unwrap_or(0);
    let own = state.runtime.own_identity(&buffer.account_id).unwrap_or_default();
    state.runtime.record_message_at(
        state,
        &buffer.account_id,
        &buffer.name,
        "channel",
        &own,
        &rendered.text,
        false,
        "chat",
        None,
        Some(message_id(at, answer.ordinal.unwrap_or(0))),
        false,
        None,
        rendered.cards,
        Vec::new(),
        None,
        Some(i64::from(at)).filter(|t| *t > 0),
        None,
        None,
    );
    Ok(())
}

/// Says a room has been read up to its newest message.
pub fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let (group, room) = ids_of(buffer).ok_or("this is not a Steam chat room")?;
    let newest = state.store.get_backlog(&buffer.id, 0, 1).ok().and_then(|m| m.last().map(|m| m.ts)).filter(|t| *t > 0);
    let at = newest.and_then(|t| u32::try_from(t).ok()).unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as u32);
    link.notify("ChatRoom.AckChatMessage#1", &ChatAck { chat_group_id: Some(group), chat_id: Some(room), timestamp: Some(at) }).map_err(|e| e.to_string())
}

/// Deletes a message from a room, for everyone.
pub async fn delete_message(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<(), String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let (group, room) = ids_of(buffer).ok_or("this is not a Steam chat room")?;
    let (at, ordinal) = reactions::parse_message_id(msg_id).ok_or("that message has no place in Steam's log")?;
    let _: DeleteChatMessagesResponse = link
        .call("ChatRoom.DeleteChatMessages#1", &DeleteChatMessagesRequest { chat_group_id: Some(group), chat_id: Some(room), messages: vec![DeleteChatMessage { server_timestamp: Some(at), ordinal: Some(ordinal) }] }, true)
        .await
        .map_err(|e| chat::refusal(&e))?;
    state.runtime.delete_message(state, &buffer.id, msg_id);
    Ok(())
}

/// Reacts to a message in a room, or takes the reaction back.
pub async fn react(state: &AppState, buffer: &Buffer, msg_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let (group, room) = ids_of(buffer).ok_or("this is not a Steam chat room")?;
    let (kind, name) = reactions::parse_token(emoji).ok_or("Steam reactions are its own emoticons and stickers")?;
    let (at, ordinal) = reactions::parse_message_id(msg_id).ok_or("that message has no place in Steam's log")?;
    let reaction = if kind == reactions::EMOTICON { format!(":{name}:") } else { name };
    let _: ChatReactionResponse = link
        .call("ChatRoom.UpdateMessageReaction#1", &ChatReactionRequest { chat_group_id: Some(group), chat_id: Some(room), server_timestamp: Some(at), ordinal: Some(ordinal), reaction_type: Some(kind), reaction: Some(reaction), is_add: Some(add) }, true)
        .await
        .map_err(|e| chat::refusal(&e))?;
    state.runtime.update_reaction(state, &buffer.id, msg_id, emoji, true, add);
    Ok(())
}

/// What was missed in each room while nobody was listening: the newest page of
/// every room Steam says has something this log does not. What others said
/// after the account last read is announced as news; the rest is only stored.
pub async fn catch_up(state: &AppState, link: &Link, account_id: &str, roster: &Roster) {
    let (rooms, own) = with(account_id, |a| {
        let rooms: Vec<(u64, u64, String, u32, u32)> = a.groups.values().filter(|g| !g.muted).flat_map(|g| g.rooms.iter().filter_map(|r| Some((g.id, r.id, a.shown.get(&(g.id, r.id))?.clone(), r.last_message, r.acked)))).collect();
        (rooms, a.own)
    });
    for (group, room, name, last, acked) in rooms {
        let buffer_id = crate::model::buffer_id(account_id, &name);
        let held = state.store.get_backlog(&buffer_id, 0, 1).ok().and_then(|m| m.last().map(|m| m.ts)).unwrap_or(0);
        if last == 0 || i64::from(last) <= held {
            continue;
        }
        let past = match fetch(link, own, group, room, None).await {
            Ok(p) => p,
            Err(e) => {
                tracing::info!("steam[{account_id}]: could not catch up on {name}: {e}");
                continue;
            }
        };
        learn(account_id, roster, past.iter().map(|p| p.account));
        let (news, old): (Vec<Past>, Vec<Past>) = past.into_iter().filter(|p| i64::from(p.at) > held).partition(|p| p.account != own && p.server.is_none() && p.at > acked);
        store_quietly(state, account_id, &name, own, &old);
        for p in &news {
            state.runtime.record_message_at(
                state,
                account_id,
                &name,
                "channel",
                &who(roster, p.account),
                &super::account::filtered(account_id, roster.is_friend(p.account), &p.text),
                false,
                "chat",
                None,
                Some(message_id(p.at, p.ordinal)),
                false,
                roster.persona(p.account).and_then(|x| x.avatar.clone()),
                p.cards.clone(),
                Vec::new(),
                Some(steamid::text_of(p.account)),
                Some(i64::from(p.at)),
                None,
                None,
            );
        }
    }
}

// ---- Changing things ----

/// `ChatRoom.<method>#1` for a request that has no answer worth reading.
async fn command<Q: prost::Message, A: prost::Message + Default>(link: &Link, method: &str, request: &Q) -> Result<A, String> {
    link.call(&format!("ChatRoom.{method}#1"), request, true).await.map_err(|e| chat::refusal(&e))
}

/// What the window can ask of a group.
#[derive(Debug, PartialEq)]
pub enum Action {
    Create { name: String },
    Join { invite: String },
    Leave { group: u64 },
    Rename { group: u64, name: String },
    Tagline { group: u64, text: String },
    CreateRoom { group: u64, name: String, voice: bool },
    RenameRoom { group: u64, room: u64, name: String },
    DeleteRoom { group: u64, room: u64 },
    Kick { group: u64, who: u64, seconds: i32 },
    Ban { group: u64, who: u64, banned: bool },
    Mute { group: u64, who: u64, seconds: i32 },
    InviteFriend { group: u64, who: u64 },
    MakeInvite { group: u64, seconds: u32 },
    RevokeInvite { group: u64, code: String },
    Links { group: u64 },
    Notify { group: u64, level: i32 },
    Bans { group: u64 },
    Avatar { group: u64, path: String },
    DismissNotices { group: u64 },
    CreateRole { group: u64, name: String },
    RenameRole { group: u64, role: u64, name: String },
    DeleteRole { group: u64, role: u64 },
    RoleActions { group: u64, role: u64 },
    SetRoleActions { group: u64, role: u64, actions: ChatRoleActions },
    GiveRole { group: u64, who: u64, role: u64 },
    TakeRole { group: u64, who: u64, role: u64 },
}

/// The permissions of a role as the window reads and writes them: by name.
pub const PERMISSIONS: [(&str, &str); 10] = [
    ("channels", "Create, rename and delete rooms"),
    ("kick", "Remove people"),
    ("ban", "Ban people"),
    ("invite", "Invite people and make links"),
    ("group", "Change the name, tagline and picture"),
    ("chat", "Write messages"),
    ("history", "Read the history"),
    ("roles", "Change the roles"),
    ("members", "Give and take roles from people"),
    ("mentionAll", "Mention everyone"),
];

/// A role's permissions as the window reads them.
pub fn actions_json(a: &ChatRoleActions) -> Value {
    json!({
        "channels": a.can_create_rename_delete_channel.unwrap_or(false),
        "kick": a.can_kick.unwrap_or(false),
        "ban": a.can_ban.unwrap_or(false),
        "invite": a.can_invite.unwrap_or(false),
        "group": a.can_change_tagline_avatar_name.unwrap_or(false),
        "chat": a.can_chat.unwrap_or(false),
        "history": a.can_view_history.unwrap_or(false),
        "roles": a.can_change_group_roles.unwrap_or(false),
        "members": a.can_change_user_roles.unwrap_or(false),
        "mentionAll": a.can_mention_all.unwrap_or(false),
    })
}

/// A role's permissions as the window sends them.
pub fn actions_of(role: u64, v: &Value) -> ChatRoleActions {
    let b = |k: &str| Some(v[k].as_bool().unwrap_or(false));
    ChatRoleActions {
        role_id: Some(role),
        can_create_rename_delete_channel: b("channels"),
        can_kick: b("kick"),
        can_ban: b("ban"),
        can_invite: b("invite"),
        can_change_tagline_avatar_name: b("group"),
        can_chat: b("chat"),
        can_view_history: b("history"),
        can_change_group_roles: b("roles"),
        can_change_user_roles: b("members"),
        can_mention_all: b("mentionAll"),
        can_set_watching_broadcast: Some(false),
    }
}

/// The code in an invite to a group, from a link or the code itself.
pub fn invite_code_of(text: &str) -> Option<String> {
    let text = text.trim();
    let code = match text.find("/chat/") {
        Some(at) => text[at + 6..].trim_start_matches("invite/").split(['?', '#', '/']).next()?,
        None => text,
    };
    (code.len() >= 4 && code.len() <= 32 && code.chars().all(|c| c.is_ascii_alphanumeric()) && (text.contains("/chat/") || !text.contains('/'))).then(|| code.to_string())
}

/// What an action did, for the window.
pub async fn act(state: &AppState, account_id: &str, action: Action) -> Result<Value, String> {
    let done = do_act(state, account_id, action).await;
    if done.is_ok() {
        poke(account_id);
    }
    done
}

async fn do_act(state: &AppState, account_id: &str, action: Action) -> Result<Value, String> {
    let link = state.runtime.steam_link(account_id).ok_or("this Steam account is not connected")?;
    let own = with(account_id, |a| a.own);
    match action {
        Action::Create { name } => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err("a group needs a name".to_string());
            }
            let made: CreateChatGroupResponse = command(&link, "CreateChatRoomGroup", &CreateChatGroupRequest { name: Some(name), ..Default::default() }).await?;
            if let (Some(id), Some(s)) = (made.chat_group_id, &made.state) {
                let mut g = Group { id, ..Default::default() };
                g.take_state(s, own);
                if let Some(u) = &made.user_chat_state {
                    g.take_user(u);
                }
                with(account_id, |a| a.groups.insert(id, g));
                keep_active(&link, account_id).await;
            }
            Ok(json!({ "groupId": made.chat_group_id.map(|g| rail_id(account_id, g)) }))
        }
        Action::Join { invite } => {
            let code = invite_code_of(&invite).ok_or("that is not a link to a Steam chat group")?;
            // What the link is for, so a link that is dead or a group one is banned from says so.
            let info: GetInviteLinkInfoResponse = command(&link, "GetInviteLinkInfo", &GetInviteLinkInfoRequest { invite_code: Some(code.clone()) }).await?;
            if info.banned.unwrap_or(false) {
                return Err("you have been banned from that group".to_string());
            }
            let group = info.group_summary.as_ref().and_then(|s| s.chat_group_id).ok_or("that link does not lead to a group")?;
            let joined: JoinChatGroupResponse = command(&link, "JoinChatRoomGroup", &JoinChatGroupRequest { chat_group_id: Some(group), invite_code: Some(code), chat_id: info.chat_id }).await?;
            let mut g = Group { id: group, ..Default::default() };
            if let Some(s) = &info.group_summary {
                g.name = s.chat_group_name.clone().unwrap_or_default();
                g.tagline = s.chat_group_tagline.clone().unwrap_or_default();
            }
            if let Some(s) = &joined.state {
                g.take_state(s, own);
            }
            if let Some(u) = &joined.user_chat_state {
                g.take_user(u);
            }
            let name = g.name.clone();
            with(account_id, |a| a.groups.insert(group, g));
            keep_active(&link, account_id).await;
            Ok(json!({ "groupId": rail_id(account_id, group), "name": name }))
        }
        Action::Leave { group } => {
            let _: LeaveChatGroupResponse = command(&link, "LeaveChatRoomGroup", &LeaveChatGroupRequest { chat_group_id: Some(group) }).await?;
            with(account_id, |a| a.groups.remove(&group));
            Ok(json!({}))
        }
        Action::Rename { group, name } => {
            let _: RenameChatGroupResponse = command(&link, "RenameChatRoomGroup", &RenameChatGroupRequest { chat_group_id: Some(group), name: Some(name.trim().to_string()) }).await?;
            with(account_id, |a| a.groups.get_mut(&group).map(|g| g.name = name.trim().to_string()));
            Ok(json!({}))
        }
        Action::Tagline { group, text } => {
            let _: SetTaglineResponse = command(&link, "SetChatRoomGroupTagline", &SetTaglineRequest { chat_group_id: Some(group), tagline: Some(text.clone()) }).await?;
            with(account_id, |a| a.groups.get_mut(&group).map(|g| g.tagline = text));
            Ok(json!({}))
        }
        Action::CreateRoom { group, name, voice } => {
            let made: CreateChatRoomResponse = command(&link, "CreateChatRoom", &CreateChatRoomRequest { chat_group_id: Some(group), name: Some(name.trim().to_string()), allow_voice: Some(voice) }).await?;
            if let Some(r) = made.chat_room {
                with(account_id, |a| {
                    if let Some(g) = a.groups.get_mut(&group) {
                        let mut rooms: Vec<ChatRoomState> = g.rooms.iter().map(|r| ChatRoomState { chat_id: Some(r.id), chat_name: Some(r.name.clone()), voice_allowed: Some(r.voice), members_in_voice: r.in_voice.clone(), sort_order: Some(r.sort), time_last_message: Some(r.last_message), ..Default::default() }).collect();
                        rooms.push(r);
                        g.take_rooms(&rooms);
                    }
                });
            }
            Ok(json!({}))
        }
        Action::RenameRoom { group, room, name } => {
            let _: RenameChatRoomResponse = command(&link, "RenameChatRoom", &RenameChatRoomRequest { chat_group_id: Some(group), chat_id: Some(room), name: Some(name.trim().to_string()) }).await?;
            with(account_id, |a| a.groups.get_mut(&group).and_then(|g| g.room_mut(room)).map(|r| r.name = name.trim().to_string()));
            Ok(json!({}))
        }
        Action::DeleteRoom { group, room } => {
            let _: DeleteChatRoomResponse = command(&link, "DeleteChatRoom", &DeleteChatRoomRequest { chat_group_id: Some(group), chat_id: Some(room) }).await?;
            with(account_id, |a| a.groups.get_mut(&group).map(|g| g.rooms.retain(|r| r.id != room)));
            Ok(json!({}))
        }
        Action::Kick { group, who, seconds } => {
            let _: KickChatUserResponse = command(&link, "KickUserFromGroup", &KickChatUserRequest { chat_group_id: Some(group), steamid: Some(who), expiration: Some(seconds) }).await?;
            Ok(json!({}))
        }
        Action::Ban { group, who, banned } => {
            let _: BanChatUserResponse = command(&link, "SetUserBanState", &BanChatUserRequest { chat_group_id: Some(group), steamid: Some(who), ban_state: Some(banned) }).await?;
            Ok(json!({}))
        }
        Action::Mute { group, who, seconds } => {
            let _: MuteChatUserResponse = command(&link, "MuteUserInGroup", &MuteChatUserRequest { chat_group_id: Some(group), steamid: Some(who), expiration: Some(seconds) }).await?;
            Ok(json!({}))
        }
        Action::InviteFriend { group, who } => {
            let _: InviteFriendToChatGroupResponse = command(&link, "InviteFriendToChatRoomGroup", &InviteFriendToChatGroupRequest { chat_group_id: Some(group), steamid: Some(who), chat_id: None, skip_friendsui_check: None }).await?;
            Ok(json!({}))
        }
        Action::MakeInvite { group, seconds } => {
            let made: CreateInviteLinkResponse = command(&link, "CreateInviteLink", &CreateInviteLinkRequest { chat_group_id: Some(group), seconds_valid: Some(seconds), chat_id: None }).await?;
            Ok(json!({ "code": made.invite_code, "link": made.invite_code.as_ref().map(|c| format!("https://s.team/chat/{c}")) }))
        }
        Action::RevokeInvite { group, code } => {
            let _: DeleteInviteLinkResponse = command(&link, "DeleteInviteLink", &DeleteInviteLinkRequest { chat_group_id: Some(group), invite_code: Some(code) }).await?;
            Ok(json!({}))
        }
        Action::Links { group } => {
            let listed: GetInviteLinksResponse = command(&link, "GetInviteLinksForGroup", &GetInviteLinksRequest { chat_group_id: Some(group) }).await?;
            Ok(json!({
                "links": listed.invite_links.iter().filter_map(|l| {
                    let code = l.invite_code.clone()?;
                    Some(json!({ "code": code, "link": format!("https://s.team/chat/{code}"), "expires": l.time_expires.filter(|t| *t > 0), "creator": l.steamid_creator.and_then(steamid::account_of).map(steamid::text_of) }))
                }).collect::<Vec<_>>()
            }))
        }
        // A new picture: sent to the site, then the group is told what it is called.
        Action::Avatar { group, path } => {
            let config = state.accounts.get_steam(account_id).ok_or("this Steam account is not set up")?;
            let sha = super::web::upload_avatar(&link, account_id, config.steamid, &config.refresh_token, &path).await?;
            let _: SetAvatarResponse = command(&link, "SetChatRoomGroupAvatar", &SetAvatarRequest { chat_group_id: Some(group), avatar_sha: Some(sha) }).await?;
            reread(&link, account_id, group, own).await;
            Ok(json!({}))
        }
        // The warnings about people in the group have been seen: Steam is told, so it does not
        // raise them again, and the banner goes.
        Action::DismissNotices { group } => {
            let senders: Vec<u32> = with(account_id, |a| a.groups.get_mut(&group).map(|g| std::mem::take(&mut g.suspicious).into_iter().collect()).unwrap_or_default());
            for who in senders {
                let _ = link.notify("ChatRoom.DismissChatRoomNotice#1", &DismissChatRoomNotice { chat_group_id: Some(group), steamid_sender: Some(steamid::steamid_of(who)), notice_type: Some(super::notices::SUSPICIOUS) });
            }
            Ok(json!({}))
        }
        Action::Bans { group } => {
            let listed: GetBanListResponse = command(&link, "GetBanList", &GetBanListRequest { chat_group_id: Some(group) }).await?;
            Ok(json!({ "bans": listed.bans.iter().filter_map(|b| Some(json!({ "steamid": steamid::text_of(b.accountid?), "name": person(account_id, b.accountid?).name, "when": b.time_banned, "reason": b.ban_reason }))).collect::<Vec<_>>() }))
        }
        Action::CreateRole { group, name } => {
            let _: CreateRoleResponse = command(&link, "CreateRole", &CreateRoleRequest { chat_group_id: Some(group), name: Some(name.trim().to_string()) }).await?;
            reread(&link, account_id, group, own).await;
            Ok(json!({}))
        }
        Action::RenameRole { group, role, name } => {
            let _: RenameRoleResponse = command(&link, "RenameRole", &RenameRoleRequest { chat_group_id: Some(group), role_id: Some(role), name: Some(name.trim().to_string()) }).await?;
            reread(&link, account_id, group, own).await;
            Ok(json!({}))
        }
        Action::DeleteRole { group, role } => {
            let _: DeleteRoleResponse = command(&link, "DeleteRole", &DeleteRoleRequest { chat_group_id: Some(group), role_id: Some(role) }).await?;
            reread(&link, account_id, group, own).await;
            Ok(json!({}))
        }
        Action::RoleActions { group, role } => {
            let held = with(account_id, |a| a.groups.get(&group).and_then(|g| g.role_actions.get(&role).cloned()));
            let actions = match held {
                Some(a) => a,
                None => {
                    let got: GetRoleActionsResponse = command(&link, "GetRoleActions", &GetRoleActionsRequest { chat_group_id: Some(group), role_id: Some(role) }).await?;
                    got.actions.into_iter().find(|a| a.role_id == Some(role)).unwrap_or_default()
                }
            };
            Ok(actions_json(&actions))
        }
        Action::SetRoleActions { group, role, actions } => {
            let _: ReplaceRoleActionsResponse = command(&link, "ReplaceRoleActions", &ReplaceRoleActionsRequest { chat_group_id: Some(group), role_id: Some(role), actions: Some(actions.clone()) }).await?;
            with(account_id, |a| a.groups.get_mut(&group).map(|g| g.role_actions.insert(role, actions)));
            Ok(json!({}))
        }
        Action::GiveRole { group, who, role } => {
            let _: AddRoleToUserResponse = command(&link, "AddRoleToUser", &AddRoleToUserRequest { chat_group_id: Some(group), role_id: Some(role), steamid: Some(who) }).await?;
            reread(&link, account_id, group, own).await;
            Ok(json!({}))
        }
        Action::TakeRole { group, who, role } => {
            let _: DeleteRoleFromUserResponse = command(&link, "DeleteRoleFromUser", &DeleteRoleFromUserRequest { chat_group_id: Some(group), role_id: Some(role), steamid: Some(who) }).await?;
            reread(&link, account_id, group, own).await;
            Ok(json!({}))
        }
        Action::Notify { group, level } => {
            let _: SetUserPreferencesResponse = command(&link, "SetUserChatGroupPreferences", &SetUserPreferencesRequest { chat_group_id: Some(group), chat_group_preferences: Some(UserChatGroupPreferences { desktop_notification_level: Some(level), mobile_notification_level: Some(level), unread_indicator_muted: Some(level == 1), direct_messages_allowed: None }) }).await?;
            with(account_id, |a| a.groups.get_mut(&group).map(|g| {
                g.notify = level;
                g.muted = level == 1;
            }));
            Ok(json!({}))
        }
    }
}

/// Reads one group's state again, for a change whose result is too much to
/// work out by hand: who holds which role after one is deleted.
async fn reread(link: &Link, account_id: &str, group: u64, own: u32) {
    let got: Result<GetChatRoomGroupStateResponse, _> = command(link, "GetChatRoomGroupState", &GetChatRoomGroupStateRequest { chat_group_id: Some(group) }).await;
    if let Ok(GetChatRoomGroupStateResponse { state: Some(s) }) = got {
        with(account_id, |a| a.groups.get_mut(&group).map(|g| g.take_state(&s, own)));
    }
}

/// Tells Steam which groups this connection wants messages for, now the set changed.
async fn keep_active(link: &Link, account_id: &str) {
    let ids: Vec<u64> = with(account_id, |a| a.groups.keys().copied().collect());
    let _: Result<SetSessionActiveGroupsResponse, _> = link
        .call("ChatRoom.SetSessionActiveChatRoomGroups#1", &SetSessionActiveGroupsRequest { chat_group_ids: ids.clone(), chat_groups_data_requested: ids, virtualize_members_threshold: None }, true)
        .await;
}

/// The groups as the window's settings read them.
pub fn to_json(account_id: &str, group: Option<u64>) -> Value {
    let all = HELD.lock().unwrap();
    let Some(a) = all.get(account_id) else { return json!([]) };
    let name = |acc: u32| a.people.get(&acc).map(|p| p.name.clone()).unwrap_or_else(|| steamid::text_of(acc));
    Value::Array(
        a.groups
            .values()
            .filter(|g| group.is_none_or(|id| id == g.id))
            .map(|g| {
                json!({
                    "id": g.id.to_string(),
                    "railId": rail_id(account_id, g.id),
                    "name": g.name,
                    "tagline": g.tagline,
                    "owner": steamid::text_of(g.owner),
                    "clan": g.clan,
                    "myRank": g.my_rank,
                    "myRankWord": rank_word(g.my_rank),
                    "mayModerate": g.may_moderate(),
                    "mayManage": g.may_manage(),
                    "mayInvite": g.may_invite(),
                    "mayChangeRoles": g.may_change_roles(),
                    "permissions": PERMISSIONS.iter().map(|(k, d)| json!({ "key": k, "label": d })).collect::<Vec<_>>(),
                    "muted": g.muted,
                    "notify": g.notify,
                    "rooms": g.rooms.iter().map(|r| json!({ "id": r.id.to_string(), "name": r.name, "voice": r.voice, "inVoice": r.in_voice.iter().map(|a| steamid::text_of(*a)).collect::<Vec<_>>() })).collect::<Vec<_>>(),
                    "roles": g.roles.iter().map(|(id, n)| json!({ "id": id.to_string(), "name": n })).collect::<Vec<_>>(),
                    "members": g.members.iter().map(|m| json!({ "steamid": steamid::text_of(m.account), "name": name(m.account), "rank": m.rank, "rankWord": rank_word(m.rank), "roles": m.roles.iter().map(|r| r.to_string()).collect::<Vec<_>>() })).collect::<Vec<_>>(),
                    "kicked": g.kicked.iter().map(|m| json!({ "steamid": steamid::text_of(m.account), "name": name(m.account) })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invite_is_found_in_a_link_and_in_a_bare_code() {
        assert_eq!(invite_code_of("https://s.team/chat/Ga3kODnC").as_deref(), Some("Ga3kODnC"));
        assert_eq!(invite_code_of("  https://steamcommunity.com/chat/invite/Ga3kODnC?x=1 ").as_deref(), Some("Ga3kODnC"));
        assert_eq!(invite_code_of("Ga3kODnC").as_deref(), Some("Ga3kODnC"));
        assert_eq!(invite_code_of("https://example.com/other/Ga3kODnC"), None);
        assert_eq!(invite_code_of("hi"), None);
        assert_eq!(invite_code_of("two words"), None);
    }

    #[test]
    fn at_a_name_pings_that_person_only_when_it_is_a_whole_name_at_the_start_of_a_word() {
        let people = vec![(5, "Sam".to_string()), (6, "Sam Smith".to_string()), (7, "Al".to_string())];
        assert_eq!(mentionize("hi @Sam and @sam smith!", &people), "hi [mention=5]@Sam[/mention] and [mention=6]@Sam Smith[/mention]!");
        assert_eq!(mentionize("@Sammy no, mail@Al no", &people), "@Sammy no, mail@Al no", "a longer name or one inside a word is not them");
        assert_eq!(mentionize("@Al", &people), "[mention=7]@Al[/mention]");
        assert_eq!(mentionize("nobody @here", &people), "nobody @here");
    }

    #[test]
    fn a_rail_id_names_the_account_and_the_group_and_back() {
        let rail = rail_id("steam:me", 78989756);
        assert_eq!(group_of_rail_id(&rail), Some(("steam:me", 78989756)));
        assert_eq!(group_of_rail_id("steam:me|guild:5"), None);
        assert_eq!(group_of_rail_id("nonsense"), None);
    }

    fn group(id: u64, name: &str, rooms: &[(u64, &str)]) -> Group {
        Group { id, name: name.to_string(), rooms: rooms.iter().map(|(i, n)| Room { id: *i, name: n.to_string(), ..Default::default() }).collect(), ..Default::default() }
    }

    #[test]
    fn channels_are_named_for_their_group_and_told_apart_when_names_clash() {
        let mut groups = BTreeMap::new();
        groups.insert(1, group(1, "Raiders", &[(10, ""), (11, "loot")]));
        groups.insert(2, group(2, "Work", &[(20, "general")]));
        let t = titles(&groups);
        assert_eq!(t[&(1, 10)], "Raiders/#General");
        assert_eq!(t[&(1, 11)], "Raiders/#loot");
        groups.insert(3, group(3, "raiders", &[(30, "")]));
        let t = titles(&groups);
        assert_eq!(t[&(1, 10)], "Raiders (1)/#General", "two groups of one name carry their numbers");
        assert_eq!(t[&(3, 30)], "raiders (3)/#General");
        groups.insert(4, group(4, "Twin", &[(40, "a"), (41, "A")]));
        let t = titles(&groups);
        assert_eq!(t[&(4, 40)], "Twin/#a");
        assert_eq!(t[&(4, 41)], "Twin/#A (2)", "rooms of one name in one group are numbered");
    }

    #[test]
    fn the_roles_steam_names_by_key_are_said_plainly_and_a_rank_in_words() {
        assert_eq!(pretty_role("#ChatRoomRole_Member"), "Member");
        assert_eq!(pretty_role("Raid leader"), "Raid leader");
        assert_eq!(rank_word(RANK_OWNER), "Owner");
        assert_eq!(rank_word(RANK_MODERATOR), "Moderator");
        assert_eq!(rank_word(0), "Member");
    }

    #[test]
    fn a_group_is_read_from_what_steam_says_of_it() {
        let state = ChatRoomGroupState {
            header_state: Some(ChatRoomGroupHeaderState { chat_group_id: Some(7), chat_name: Some("Raiders".into()), accountid_owner: Some(5), tagline: Some("we raid".into()), roles: vec![ChatRole { role_id: Some(9), name: Some("#ChatRoomRole_Member".into()), ordinal: None }], default_role_id: Some(9), ..Default::default() }),
            members: vec![ChatRoomMember { accountid: Some(5), state: Some(2), rank: Some(50), ..Default::default() }, ChatRoomMember { accountid: Some(6), state: Some(2), rank: Some(20), ..Default::default() }, ChatRoomMember { accountid: Some(8), state: Some(1), rank: Some(20), ..Default::default() }],
            default_chat_id: Some(70),
            chat_rooms: vec![ChatRoomState { chat_id: Some(71), chat_name: Some("b".into()), sort_order: Some(2), ..Default::default() }, ChatRoomState { chat_id: Some(70), chat_name: Some("".into()), sort_order: Some(1), ..Default::default() }],
            ..Default::default()
        };
        let mut g = Group { id: 7, ..Default::default() };
        g.take_state(&state, 5);
        assert_eq!((g.name.as_str(), g.tagline.as_str(), g.owner, g.my_rank), ("Raiders", "we raid", 5, 50));
        assert_eq!(g.members.iter().map(|m| m.account).collect::<Vec<_>>(), [5, 6], "only those who are in it, highest rank first");
        assert_eq!(g.rooms.iter().map(|r| r.id).collect::<Vec<_>>(), [70, 71], "rooms in Steam's order");
        assert!(g.may_manage() && g.may_moderate() && g.may_speak());
        let mut viewer = g.clone();
        viewer.my_rank = RANK_VIEWER;
        assert!(!viewer.may_speak());
    }

    fn set_up(state: &AppState, account: &str) -> Roster {
        let mut g = Group { id: 7, name: "Raiders".into(), owner: 5, ..Default::default() };
        g.take_rooms(&[ChatRoomState { chat_id: Some(70), chat_name: Some("".into()), sort_order: Some(1), ..Default::default() }]);
        g.take_members(&[ChatRoomMember { accountid: Some(5), state: Some(2), rank: Some(50), ..Default::default() }, ChatRoomMember { accountid: Some(6), state: Some(2), rank: Some(20), ..Default::default() }], 5);
        with(account, |a| {
            a.groups.insert(7, g);
            a.own = 5;
            a.loaded = true;
        });
        state.runtime.set_own_identity(account, "Me");
        Roster::default()
    }

    fn notify(state: &AppState, account: &str, roster: &Roster, method: &str, body: Vec<u8>) -> Option<Heard> {
        heard(state, account, roster, method, &body)
    }

    #[tokio::test]
    async fn a_group_is_a_server_with_a_channel_that_hears_the_room_and_follows_it() {
        use prost::Message;
        let state = crate::backend::kick::testkit::simulated_daemon("steam-groups");
        let roster = set_up(&state, "steam:groups-a");
        publish(&state, "steam:groups-a", &roster);
        let rail = state.runtime.list_buffer_groups().into_iter().find(|g| g.id == rail_id("steam:groups-a", 7)).expect("the group is on the rail");
        assert_eq!((rail.name.as_str(), rail.kind.as_str(), rail.service.as_str()), ("Raiders", "guild", "steam"));
        let buffer = state.runtime.get_buffer(&crate::model::buffer_id("steam:groups-a", "Raiders/#General")).expect("its room is a channel");
        assert_eq!((buffer.kind.as_str(), buffer.remote_id.as_deref(), buffer.group_id.as_deref()), ("channel", Some("7:70"), Some(rail.id.as_str())));
        assert_eq!(state.runtime.get_presence(&buffer.id).unwrap().as_array().unwrap().len(), 2, "both members are listed");

        // Somebody speaks.
        let said = ChatIncoming { chat_group_id: Some(7), chat_id: Some(70), steamid_sender: Some(steamid::steamid_of(6)), message: Some("hi [b]all[/b]".into()), timestamp: Some(1790000000), mentions: Some(ChatMentions { mention_accountids: vec![5], ..Default::default() }), ordinal: Some(2), ..Default::default() };
        let out = notify(&state, "steam:groups-a", &roster, "ChatRoomClient.NotifyIncomingChatMessage#1", said.encode_to_vec()).unwrap();
        assert!(!out.reload, "a room we have is not a reload");
        assert_eq!(out.ask, vec![6], "who they are is not known, so it is asked");
        let log = state.store.get_backlog(&buffer.id, 0, 10).unwrap();
        assert_eq!(log.iter().map(|m| (m.id.as_str(), m.body.as_str())).collect::<Vec<_>>(), [("1790000000:2", "hi **all**")]);
        assert!(log[0].is_highlight, "named in it, so it is a mention");

        // The group says somebody arrived: a line, and the member list is read again.
        let joined = ChatIncoming { chat_group_id: Some(7), chat_id: Some(70), timestamp: Some(1790000005), server_message: Some(ServerMessage { message: Some(SERVER_JOINED), string_param: None, accountid_param: Some(9) }), ..Default::default() };
        let out = notify(&state, "steam:groups-a", &roster, "ChatRoomClient.NotifyIncomingChatMessage#1", joined.encode_to_vec()).unwrap();
        assert!(out.reload && out.ask == vec![9]);
        assert!(state.store.get_backlog(&buffer.id, 0, 10).unwrap().iter().any(|m| m.kind == "system" && m.body == "9 joined" || m.body.ends_with("joined")));

        // A message is taken back.
        let gone = ChatModified { chat_group_id: Some(7), chat_id: Some(70), messages: vec![ChatModifiedMessage { server_timestamp: Some(1790000000), ordinal: Some(2), deleted: Some(true) }] };
        notify(&state, "steam:groups-a", &roster, "ChatRoomClient.NotifyChatMessageModified#1", gone.encode_to_vec());
        assert!(!state.store.get_backlog(&buffer.id, 0, 10).unwrap().iter().any(|m| m.id == "1790000000:2"));

        // The room is renamed: the channel moves with its history.
        let renamed = ChatRoomsChange { chat_group_id: Some(7), default_chat_id: Some(70), chat_rooms: vec![ChatRoomState { chat_id: Some(70), chat_name: Some("raids".into()), sort_order: Some(1), ..Default::default() }] };
        notify(&state, "steam:groups-a", &roster, "ChatRoomClient.NotifyChatRoomGroupRoomsChange#1", renamed.encode_to_vec());
        assert!(state.runtime.get_buffer(&crate::model::buffer_id("steam:groups-a", "Raiders/#raids")).is_some());
        assert!(state.runtime.get_buffer(&buffer.id).is_none(), "and the old name is gone");

        // Somebody leaves; then we are removed, and the whole group goes.
        let parted = ChatMemberChange { chat_group_id: Some(7), member: Some(ChatRoomMember { accountid: Some(6), ..Default::default() }), change: Some(CHANGE_PARTED) };
        notify(&state, "steam:groups-a", &roster, "ChatRoomClient.NotifyMemberStateChange#1", parted.encode_to_vec());
        assert_eq!(held("steam:groups-a", 7).unwrap().members.len(), 1);
        let removed = ChatMemberChange { chat_group_id: Some(7), member: Some(ChatRoomMember { accountid: Some(5), ..Default::default() }), change: Some(CHANGE_KICKED) };
        notify(&state, "steam:groups-a", &roster, "ChatRoomClient.NotifyMemberStateChange#1", removed.encode_to_vec());
        assert!(held("steam:groups-a", 7).is_none());
        assert!(state.runtime.list_buffer_groups().iter().all(|g| g.id != rail_id("steam:groups-a", 7)), "the server leaves the rail");
        assert!(state.runtime.list_buffers().iter().all(|b| b.group_id.as_deref() != Some(&rail_id("steam:groups-a", 7))), "and its channels with it");
        clear("steam:groups-a");
    }

    #[tokio::test]
    async fn before_steam_has_said_what_the_groups_are_nothing_is_taken_off_the_rail() {
        let state = crate::backend::kick::testkit::simulated_daemon("steam-groups-early");
        let roster = set_up(&state, "steam:groups-b");
        publish(&state, "steam:groups-b", &roster);
        clear("steam:groups-b");
        // A reconnect: nothing is loaded yet, so nothing may be concluded from the list being empty.
        publish(&state, "steam:groups-b", &roster);
        assert!(state.runtime.list_buffer_groups().iter().any(|g| g.id == rail_id("steam:groups-b", 7)));
    }

    #[tokio::test]
    async fn steam_warning_of_a_member_is_a_banner_over_the_rooms_until_it_is_lifted() {
        use prost::Message;
        let state = crate::backend::kick::testkit::simulated_daemon("steam-groups-notice");
        let acc = "steam:groups-notice";
        let roster = set_up(&state, acc);
        publish(&state, acc, &roster);
        let room = crate::model::buffer_id(acc, "Raiders/#General");
        assert!(state.runtime.get_buffer(&room).unwrap().notice.is_none());
        let raised = ChatRoomNoticeNotification { chat_group_id: Some(7), steamid_sender: Some(steamid::steamid_of(6)), notice_type: Some(1), active: Some(true) };
        notify(&state, acc, &roster, "ChatRoomClient.NotifyChatRoomNotice#1", raised.encode_to_vec());
        let banner = state.runtime.get_buffer(&room).unwrap().notice.expect("a banner");
        assert_eq!(banner.kind, "suspicious");
        assert!(banner.text.contains("may not be who they appear to be"), "{}", banner.text);
        // A kind of notice this does not know says nothing.
        let other = ChatRoomNoticeNotification { notice_type: Some(9), ..raised.clone() };
        notify(&state, acc, &roster, "ChatRoomClient.NotifyChatRoomNotice#1", other.encode_to_vec());
        // Lifted: the banner goes.
        notify(&state, acc, &roster, "ChatRoomClient.NotifyChatRoomNotice#1", ChatRoomNoticeNotification { active: Some(false), ..raised }.encode_to_vec());
        assert!(state.runtime.get_buffer(&room).unwrap().notice.is_none());
        clear(acc);
    }
}
