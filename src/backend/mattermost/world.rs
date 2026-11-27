//! What the account can see: the people, teams and channels the server has told it of, its own memberships (where it
//! last read to, what it has muted, which roles it holds), its sidebar categories, and the permissions those roles give.
//!
//! Kept as plain data read out of the JSON the REST API answers and the websocket sends, so that the questions that
//! matter - what is this channel called here, may this account write in it, how much of it is unread - are functions of
//! data, written down once and tested.
//!
//! Permissions are Mattermost's own calculation as its web app makes it: the permissions of every role the account
//! holds - on the system, in the channel's team, in the channel (whose roles come from the channel's scheme, which is
//! how a channel is made read-only) - put together.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// What the server said of itself when the connection was made.
#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub version: String,
    pub site_name: String,
    /// The most characters a post may have.
    pub max_post: usize,
    /// The most a file may weigh, in bytes.
    pub max_file: u64,
    pub files: bool,
    pub custom_emoji: bool,
    /// `disabled`, `default_on`, `default_off` or `always_on`: whether replies are kept out of the channel and read as
    /// threads (Collapsed Reply Threads).
    pub collapsed_threads: String,
    /// How a teammate is named: `username`, `nickname_full_name` or `full_name`.
    pub teammate_name: String,
    pub custom_statuses: bool,
    pub post_priority: bool,
    pub synced_drafts: bool,
    /// The plugins the server runs, by id: `com.mattermost.calls` is what makes calls.
    pub plugins: HashSet<String>,
    /// How many different reactions a post may carry.
    pub reaction_limit: usize,
}

impl ServerInfo {
    pub fn from_config(config: &Value, plugins: &Value) -> Self {
        let text = |k: &str| config[k].as_str().unwrap_or("").to_string();
        let on = |k: &str| config[k].as_str() == Some("true");
        let number = |k: &str, d: u64| config[k].as_str().and_then(|v| v.parse::<u64>().ok()).unwrap_or(d);
        Self {
            version: text("Version"),
            site_name: text("SiteName"),
            max_post: number("MaxPostSize", 16383) as usize,
            max_file: number("MaxFileSize", 50 * 1024 * 1024),
            files: config["EnableFile"].as_str() != Some("false"),
            custom_emoji: on("EnableCustomEmoji"),
            collapsed_threads: config["CollapsedThreads"].as_str().unwrap_or("disabled").to_string(),
            teammate_name: config["TeammateNameDisplay"].as_str().filter(|v| !v.is_empty()).unwrap_or("username").to_string(),
            custom_statuses: on("EnableCustomUserStatuses"),
            post_priority: on("PostPriority"),
            synced_drafts: on("AllowSyncedDrafts"),
            plugins: plugins.as_array().into_iter().flatten().filter_map(|p| p["id"].as_str().map(str::to_string)).collect(),
            reaction_limit: number("UniqueEmojiReactionLimitPerPost", 50) as usize,
        }
    }

    /// Whether the server's major and minor version are at least these: a newer endpoint is asked only of a server that
    /// has it.
    pub fn at_least(&self, major: u32, minor: u32) -> bool {
        let mut parts = self.version.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
        let (a, b) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
        (a, b) >= (major, minor)
    }

    pub fn has_calls(&self) -> bool {
        self.plugins.contains("com.mattermost.calls")
    }
}

#[derive(Debug, Clone, Default)]
pub struct User {
    pub id: String,
    pub username: String,
    pub nickname: String,
    pub first_name: String,
    pub last_name: String,
    pub position: String,
    pub bot: bool,
    pub deleted: bool,
    /// When the picture last changed, which is part of its address so a new one is fetched.
    pub picture: i64,
    /// `online`, `away`, `dnd` or `offline`, as last heard.
    pub status: String,
    /// The emoji and words they put beside their name, while it lasts.
    pub custom_status: Option<(String, String)>,
    pub roles: String,
}

impl User {
    pub fn full_name(&self) -> String {
        format!("{} {}", self.first_name.trim(), self.last_name.trim()).trim().to_string()
    }
}

pub fn parse_user(v: &Value) -> Option<User> {
    let custom: Option<(String, String)> = v["props"]["customStatus"]
        .as_str()
        .and_then(|s| serde_json::from_str::<Value>(s).ok())
        .filter(|c| {
            // Gone once its time is up.
            let expires = c["expires_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok());
            expires.is_none_or(|t| t.timestamp() <= 0 || t > chrono::Utc::now())
        })
        .map(|c| (c["emoji"].as_str().unwrap_or("").to_string(), c["text"].as_str().unwrap_or("").to_string()))
        .filter(|(e, t)| !e.is_empty() || !t.is_empty());
    Some(User {
        id: v["id"].as_str()?.to_string(),
        username: v["username"].as_str().unwrap_or("").to_string(),
        nickname: v["nickname"].as_str().unwrap_or("").to_string(),
        first_name: v["first_name"].as_str().unwrap_or("").to_string(),
        last_name: v["last_name"].as_str().unwrap_or("").to_string(),
        position: v["position"].as_str().unwrap_or("").to_string(),
        bot: v["is_bot"].as_bool().unwrap_or(false),
        deleted: v["delete_at"].as_i64().unwrap_or(0) > 0,
        picture: v["last_picture_update"].as_i64().unwrap_or(0),
        status: String::new(),
        custom_status: custom,
        roles: v["roles"].as_str().unwrap_or("").to_string(),
    })
}

#[derive(Debug, Clone, Default)]
pub struct Team {
    pub id: String,
    /// The team's name in addresses (`/alpha/channels/town-square`).
    pub name: String,
    pub display_name: String,
    pub icon_update: i64,
}

pub fn parse_team(v: &Value) -> Option<Team> {
    Some(Team {
        id: v["id"].as_str()?.to_string(),
        name: v["name"].as_str().unwrap_or("").to_string(),
        display_name: v["display_name"].as_str().unwrap_or("team").to_string(),
        icon_update: v["last_team_icon_update"].as_i64().unwrap_or(0),
    })
}

#[derive(Debug, Clone, Default)]
pub struct Channel {
    pub id: String,
    /// Empty for a direct or group message.
    pub team_id: String,
    /// `O` open, `P` private, `D` direct, `G` group.
    pub kind: String,
    pub name: String,
    pub display_name: String,
    pub header: String,
    pub purpose: String,
    pub total: i64,
    pub total_root: i64,
    pub last_post_at: i64,
    pub archived: bool,
}

impl Channel {
    pub fn is_personal(&self) -> bool {
        matches!(self.kind.as_str(), "D" | "G")
    }

    /// The other person in a direct message (this account, for one with itself).
    pub fn other(&self, own: &str) -> Option<String> {
        if self.kind != "D" {
            return None;
        }
        let (a, b) = self.name.split_once("__")?;
        Some(if a == own { b } else { a }.to_string())
    }
}

pub fn parse_channel(v: &Value) -> Option<Channel> {
    Some(Channel {
        id: v["id"].as_str()?.to_string(),
        team_id: v["team_id"].as_str().unwrap_or("").to_string(),
        kind: v["type"].as_str().unwrap_or("O").to_string(),
        name: v["name"].as_str().unwrap_or("").to_string(),
        display_name: v["display_name"].as_str().unwrap_or("").to_string(),
        header: v["header"].as_str().unwrap_or("").to_string(),
        purpose: v["purpose"].as_str().unwrap_or("").to_string(),
        total: v["total_msg_count"].as_i64().unwrap_or(0),
        total_root: v["total_msg_count_root"].as_i64().unwrap_or(0),
        last_post_at: v["last_post_at"].as_i64().unwrap_or(0),
        archived: v["delete_at"].as_i64().unwrap_or(0) > 0,
    })
}

/// This account's membership of a channel.
#[derive(Debug, Clone, Default)]
pub struct Membership {
    pub roles: Vec<String>,
    pub admin: bool,
    pub msg_count: i64,
    pub msg_count_root: i64,
    pub mentions: i64,
    pub mentions_root: i64,
    /// `all` or `mention`: whether anything but a mention makes the channel unread (a muted channel is `mention`).
    pub mark_unread: String,
    pub notify: Value,
}

pub fn parse_membership(v: &Value) -> Option<(String, Membership)> {
    Some((
        v["channel_id"].as_str()?.to_string(),
        Membership {
            roles: v["roles"].as_str().unwrap_or("").split_whitespace().map(str::to_string).collect(),
            admin: v["scheme_admin"].as_bool().unwrap_or(false),
            msg_count: v["msg_count"].as_i64().unwrap_or(0),
            msg_count_root: v["msg_count_root"].as_i64().unwrap_or(0),
            mentions: v["mention_count"].as_i64().unwrap_or(0),
            mentions_root: v["mention_count_root"].as_i64().unwrap_or(0),
            mark_unread: v["notify_props"]["mark_unread"].as_str().unwrap_or("all").to_string(),
            notify: v["notify_props"].clone(),
        },
    ))
}

/// A sidebar category of a team: a heading and the channels under it, in the order the person put them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Category {
    pub id: String,
    /// `favorites`, `channels`, `direct_messages` or `custom`.
    pub kind: String,
    pub name: String,
    pub channels: Vec<String>,
}

pub fn parse_categories(v: &Value) -> Vec<Category> {
    let by_id: HashMap<&str, &Value> = v["categories"].as_array().into_iter().flatten().filter_map(|c| c["id"].as_str().map(|id| (id, c))).collect();
    let order: Vec<&str> = v["order"].as_array().map(|o| o.iter().filter_map(Value::as_str).collect()).unwrap_or_else(|| by_id.keys().copied().collect());
    order
        .into_iter()
        .filter_map(|id| by_id.get(id))
        .map(|c| Category {
            id: c["id"].as_str().unwrap_or("").to_string(),
            kind: c["type"].as_str().unwrap_or("custom").to_string(),
            name: c["display_name"].as_str().unwrap_or("").to_string(),
            channels: c["channel_ids"].as_array().map(|l| l.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default(),
        })
        .collect()
}

/// Said in place of the message box where the account may not write.
pub const READ_ONLY: &str = "This channel is read-only: only its administrators can post here.";
pub const ARCHIVED: &str = "This channel is archived: nothing more can be posted in it.";
pub const DEACTIVATED: &str = "This person's account is deactivated: they cannot read messages sent here.";

#[derive(Debug, Default)]
pub struct World {
    pub own_id: String,
    pub users: HashMap<String, User>,
    pub teams: HashMap<String, Team>,
    /// The teams in the order the account keeps them.
    pub team_order: Vec<String>,
    pub channels: HashMap<String, Channel>,
    pub members: HashMap<String, Membership>,
    /// This account's roles in each team, by team id.
    pub team_roles: HashMap<String, Vec<String>>,
    /// This account's roles on the system.
    pub system_roles: Vec<String>,
    /// The permissions of every role the account holds, by role name.
    pub role_permissions: HashMap<String, HashSet<String>>,
    /// Each team's sidebar categories.
    pub categories: HashMap<String, Vec<Category>>,
    /// The server's custom emoji, by name (its id).
    pub emoji: HashMap<String, String>,
    /// The post ids of the account's saved posts.
    pub flagged: HashSet<String>,
    /// The account's own choice of collapsed threads (its `display_settings` preference), where the server leaves it.
    pub collapsed_choice: Option<bool>,
}

impl World {
    /// What a person is called, as the server says teammates are named.
    pub fn name_of(&self, user: &str, style: &str) -> String {
        match self.users.get(user) {
            Some(u) => {
                let full = u.full_name();
                match style {
                    "nickname_full_name" if !u.nickname.is_empty() => u.nickname.clone(),
                    "nickname_full_name" | "full_name" if !full.is_empty() => full,
                    _ => u.username.clone(),
                }
            }
            None => "someone".to_string(),
        }
    }

    /// The username of a person, for `@mentions`.
    pub fn username_of(&self, user: &str) -> Option<String> {
        self.users.get(user).map(|u| u.username.clone()).filter(|u| !u.is_empty())
    }

    /// The id of a person by username.
    pub fn user_by_name(&self, username: &str) -> Option<String> {
        let wanted = username.trim_start_matches('@').to_ascii_lowercase();
        self.users.values().find(|u| u.username.to_ascii_lowercase() == wanted).map(|u| u.id.clone())
    }

    /// What a channel is called in the list: its display name, or the people in a direct or group message.
    pub fn title_of(&self, channel: &Channel, style: &str) -> String {
        match channel.kind.as_str() {
            "D" => match channel.other(&self.own_id) {
                Some(other) if other == self.own_id => format!("{} (you)", self.name_of(&other, style)),
                Some(other) => self.name_of(&other, style),
                None => channel.display_name.clone(),
            },
            "G" => {
                // The display name Mattermost gives a group is its members' usernames: they are named as the server names
                // people when they are all known.
                let names: Vec<String> = channel.display_name.split(',').map(|n| n.trim()).filter(|n| !n.is_empty()).map(|n| self.user_by_name(n).map(|id| self.name_of(&id, style)).unwrap_or_else(|| n.to_string())).collect();
                if names.is_empty() {
                    channel.display_name.clone()
                } else {
                    names.join(", ")
                }
            }
            _ if channel.display_name.is_empty() => channel.name.clone(),
            _ => channel.display_name.clone(),
        }
    }

    /// Every role name the account holds that bears on a channel.
    fn roles_for(&self, channel: &Channel) -> Vec<&str> {
        let mut out: Vec<&str> = self.system_roles.iter().map(String::as_str).collect();
        if let Some(team) = self.team_roles.get(&channel.team_id) {
            out.extend(team.iter().map(String::as_str));
        }
        if let Some(member) = self.members.get(&channel.id) {
            out.extend(member.roles.iter().map(String::as_str));
        }
        out
    }

    /// Whether the account's roles give it a permission in a channel. Without the roles' permissions known (they were not
    /// to be had), the answer is yes: the server is the one that refuses.
    pub fn may(&self, channel: &Channel, permission: &str) -> bool {
        if self.role_permissions.is_empty() {
            return true;
        }
        self.roles_for(channel).iter().any(|r| self.role_permissions.get(*r).is_some_and(|p| p.contains(permission)))
    }

    /// Whether the account's roles give it a permission in a team (or, with no team, on the system).
    pub fn may_in_team(&self, team: &str, permission: &str) -> bool {
        if self.role_permissions.is_empty() {
            return false;
        }
        self.system_roles.iter().chain(self.team_roles.get(team).into_iter().flatten()).any(|r| self.role_permissions.get(r).is_some_and(|p| p.contains(permission)))
    }

    /// Why the account may not write in a channel, or nothing.
    pub fn read_only(&self, channel: &Channel) -> Option<&'static str> {
        if channel.archived {
            return Some(ARCHIVED);
        }
        if channel.kind == "D" {
            if let Some(other) = channel.other(&self.own_id).and_then(|o| self.users.get(&o)) {
                if other.deleted {
                    return Some(DEACTIVATED);
                }
            }
            return None;
        }
        if channel.kind == "G" {
            return None;
        }
        (!self.may(channel, "create_post")).then_some(READ_ONLY)
    }

    /// How much of a channel is unread, and how many mentions: by the whole count, or (with threads collapsed) by the
    /// posts that are not replies. A channel set to count only mentions has no unread beyond them.
    pub fn unread(&self, channel: &Channel, collapsed: bool) -> (u32, u32) {
        let Some(member) = self.members.get(&channel.id) else { return (0, 0) };
        let (total, read, mentions) = if collapsed { (channel.total_root, member.msg_count_root, member.mentions_root) } else { (channel.total, member.msg_count, member.mentions) };
        let unread = (total - read).max(0) as u32;
        let mentions = mentions.max(0) as u32;
        if member.mark_unread == "mention" {
            (mentions, mentions)
        } else {
            (unread.max(mentions), mentions)
        }
    }

    /// The category a channel is filed under in its team (or, for a direct or group message, in any team's), and its place
    /// there: the category's place first, then the channel's.
    pub fn filed(&self, channel: &Channel) -> Option<(String, i64)> {
        let lists: Vec<&Vec<Category>> = if channel.team_id.is_empty() { self.team_order.iter().filter_map(|t| self.categories.get(t)).collect() } else { self.categories.get(&channel.team_id).into_iter().collect() };
        for list in lists {
            for (place, category) in list.iter().enumerate() {
                if let Some(at) = category.channels.iter().position(|c| *c == channel.id) {
                    return Some((category.name.clone(), (place as i64 + 1) * 1000 + at as i64));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ME: &str = "bx955xywntf83q76gx1ost4pkr";
    const PAL: &str = "dd8wsa59spg7fjmxan8qgjs16y";

    fn world() -> World {
        let mut w = World { own_id: ME.into(), ..Default::default() };
        for u in [json!({ "id": ME, "username": "mohoa", "first_name": "Ann", "last_name": "A" }), json!({ "id": PAL, "username": "mohob", "nickname": "Bee" })] {
            let u = parse_user(&u).unwrap();
            w.users.insert(u.id.clone(), u);
        }
        w
    }

    #[test]
    fn a_person_is_named_as_the_server_says_teammates_are() {
        let w = world();
        assert_eq!(w.name_of(ME, "username"), "mohoa");
        assert_eq!(w.name_of(ME, "full_name"), "Ann A");
        assert_eq!(w.name_of(ME, "nickname_full_name"), "Ann A", "no nickname: the full name");
        assert_eq!(w.name_of(PAL, "nickname_full_name"), "Bee");
        assert_eq!(w.name_of(PAL, "full_name"), "mohob", "no full name: the username");
        assert_eq!(w.name_of("nobody", "username"), "someone");
    }

    #[test]
    fn a_direct_message_is_called_by_the_other_person_and_a_group_by_its_people() {
        let w = world();
        let dm = parse_channel(&json!({ "id": "c1", "type": "D", "name": format!("{ME}__{PAL}") })).unwrap();
        assert_eq!(dm.other(ME).as_deref(), Some(PAL));
        assert_eq!(w.title_of(&dm, "nickname_full_name"), "Bee");
        let own = parse_channel(&json!({ "id": "c2", "type": "D", "name": format!("{ME}__{ME}") })).unwrap();
        assert_eq!(w.title_of(&own, "username"), "mohoa (you)");
        let group = parse_channel(&json!({ "id": "c3", "type": "G", "display_name": "mohoa, mohob, mohoc" })).unwrap();
        assert_eq!(w.title_of(&group, "nickname_full_name"), "Ann A, Bee, mohoc");
        let open = parse_channel(&json!({ "id": "c4", "type": "O", "name": "town-square", "display_name": "Town Square" })).unwrap();
        assert_eq!(w.title_of(&open, "username"), "Town Square");
    }

    #[test]
    fn unread_counts_follow_collapsed_threads_and_a_muted_channel_counts_only_mentions() {
        let mut w = world();
        let c = parse_channel(&json!({ "id": "c1", "type": "O", "total_msg_count": 10, "total_msg_count_root": 4 })).unwrap();
        let (_, m) = parse_membership(&json!({ "channel_id": "c1", "msg_count": 7, "msg_count_root": 1, "mention_count": 1, "mention_count_root": 0, "notify_props": { "mark_unread": "all" } })).unwrap();
        w.members.insert("c1".into(), m);
        assert_eq!(w.unread(&c, false), (3, 1));
        assert_eq!(w.unread(&c, true), (3, 0));
        w.members.get_mut("c1").unwrap().mark_unread = "mention".into();
        assert_eq!(w.unread(&c, false), (1, 1));
        assert_eq!(w.unread(&parse_channel(&json!({ "id": "elsewhere" })).unwrap(), false), (0, 0));
    }

    #[test]
    fn a_channel_is_read_only_when_no_role_of_the_accounts_gives_create_post() {
        let mut w = world();
        let c = parse_channel(&json!({ "id": "c1", "type": "O", "team_id": "t1" })).unwrap();
        assert_eq!(w.read_only(&c), None, "roles not known: the server decides");
        w.system_roles = vec!["system_user".into()];
        w.team_roles.insert("t1".into(), vec!["team_user".into()]);
        w.members.insert("c1".into(), Membership { roles: vec!["channel_user".into()], ..Default::default() });
        w.role_permissions.insert("system_user".into(), HashSet::from(["create_team".to_string()]));
        w.role_permissions.insert("team_user".into(), HashSet::from(["join_public_channels".to_string()]));
        w.role_permissions.insert("channel_user".into(), HashSet::new());
        assert_eq!(w.read_only(&c), Some(READ_ONLY));
        w.role_permissions.insert("channel_user".into(), HashSet::from(["create_post".to_string()]));
        assert_eq!(w.read_only(&c), None);
        let archived = parse_channel(&json!({ "id": "c1", "type": "O", "team_id": "t1", "delete_at": 5 })).unwrap();
        assert_eq!(w.read_only(&archived), Some(ARCHIVED));
    }

    #[test]
    fn categories_are_read_in_the_order_the_person_keeps_them() {
        let got = parse_categories(&json!({
            "categories": [
                { "id": "k2", "type": "channels", "display_name": "Channels", "channel_ids": ["c2", "c3"] },
                { "id": "k1", "type": "favorites", "display_name": "Favorites", "channel_ids": ["c1"] }
            ],
            "order": ["k1", "k2"]
        }));
        assert_eq!(got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), vec!["Favorites", "Channels"]);
        let mut w = world();
        w.categories.insert("t1".into(), got);
        w.team_order = vec!["t1".into()];
        let c3 = parse_channel(&json!({ "id": "c3", "type": "O", "team_id": "t1" })).unwrap();
        assert_eq!(w.filed(&c3), Some(("Channels".to_string(), 2001)));
    }

    #[test]
    fn a_custom_status_that_has_ended_is_not_shown() {
        let live = parse_user(&json!({ "id": "u", "props": { "customStatus": "{\"emoji\":\"calendar\",\"text\":\"In a meeting\",\"duration\":\"\"}" } })).unwrap();
        assert_eq!(live.custom_status, Some(("calendar".into(), "In a meeting".into())));
        let ended = parse_user(&json!({ "id": "u", "props": { "customStatus": "{\"emoji\":\"calendar\",\"text\":\"x\",\"expires_at\":\"2001-01-01T00:00:00Z\"}" } })).unwrap();
        assert_eq!(ended.custom_status, None);
    }

    #[test]
    fn a_newer_endpoint_is_asked_only_of_a_server_new_enough() {
        let s = ServerInfo { version: "12.1.0".into(), ..Default::default() };
        assert!(s.at_least(10, 3) && s.at_least(12, 1) && !s.at_least(12, 2));
    }
}
