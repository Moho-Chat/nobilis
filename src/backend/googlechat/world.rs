//! The account's conversations and the people in them, and their buffers.
//!
//! `paginated_world` lists every conversation the account is in: direct messages (with one person or a few) and
//! spaces. A direct message has no name of its own and is called after the people in it, whose names come from
//! `get_members` - a message's author arrives as an id alone.

use super::{pb, Group, Handle};
use crate::model::BufferGroup;
use crate::notify::NotifyLevel;
use crate::state::AppState;
use std::collections::HashMap;

/// What is known of a person.
#[derive(Clone, Debug, Default)]
pub struct Person {
    pub name: String,
    pub email: String,
    pub avatar: Option<String>,
    pub bot: bool,
    /// `"online"`, `"dnd"`, or `"offline"`.
    pub status: String,
    pub custom_status: Option<String>,
}

/// Metadata remembered per message so edits, deletes, reactions, and quote-replies can build the exact `MessageId`
/// and `SendReplyTarget` Google Chat requires.
#[derive(Clone, Debug, Default)]
pub struct MessageMeta {
    pub topic_id: String,
    pub create_time_micros: i64,
}

/// A conversation the account is in.
#[derive(Clone, Debug, Default)]
pub struct Conversation {
    /// A space's name, or a group message's if somebody named it.
    pub name: Option<String>,
    /// A space's description (topic).
    pub description: Option<String>,
    /// A space's guidelines.
    pub guidelines: Option<String>,
    /// The people in a direct message or space (the account among them).
    pub members: Vec<String>,
    /// Space managers (`ROLE_OWNER`).
    pub owners: Vec<String>,
    pub unread: i64,
    pub sort_time: i64,
    pub muted: bool,
    pub starred: bool,
    /// Whether the space keeps every message in a thread of its own (the older kind of space).
    pub threaded: bool,
    /// History off: new messages are deleted after 24 hours (see retention.rs).
    pub history_off: bool,
    /// A space's emoji.
    pub emoji: Option<String>,
}

#[derive(Default)]
pub struct World {
    pub own_id: String,
    pub own_name: String,
    pub own_email: String,
    pub people: HashMap<String, Person>,
    pub conversations: HashMap<Group, Conversation>,
    /// Each conversation's buffer name, so two that would be called the same are told apart.
    pub titles: HashMap<Group, String>,
    /// How many topics a conversation's history has been read to, for reading further back.
    pub history_depth: HashMap<Group, i32>,
    /// Per-message metadata keyed by `message_id`.
    pub messages: HashMap<String, MessageMeta>,
    /// Known reactors keyed by `(message_id, emoji)` -> list of gaia IDs.
    pub reactors: HashMap<(String, String), Vec<String>>,
    /// Pending space invites received over the realtime stream (`space_id`).
    pub pending_invites: Vec<String>,
    /// The buffer whose member list is currently open.
    pub watched: Option<String>,
}

impl World {
    pub fn name_of(&self, gaia: &str) -> String {
        match self.people.get(gaia) {
            Some(p) if !p.name.is_empty() => p.name.clone(),
            Some(p) if !p.email.is_empty() => p.email.clone(),
            _ => format!("Google Chat user {}", tail(gaia)),
        }
    }

    /// What a conversation is called: a space's name, or the other people in a direct message.
    pub fn title_of(&self, group: &Group) -> String {
        let conversation = self.conversations.get(group);
        if let Some(name) = conversation.and_then(|c| c.name.clone()).filter(|n| !n.trim().is_empty()) {
            return name.trim().to_string();
        }
        let others: Vec<String> = conversation
            .map(|c| c.members.iter().filter(|m| **m != self.own_id).map(|m| self.name_of(m)).collect())
            .unwrap_or_default();
        match (group, others.is_empty()) {
            (Group::Dm(_), true) => "Note to self".to_string(),
            (Group::Dm(_), false) => others.join(", "),
            (Group::Space(id), _) => format!("Space {}", tail(id)),
        }
    }

    /// The people mentioned in the world whose names are not known yet.
    pub fn unknown_people(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.conversations.values().flat_map(|c| c.members.iter().cloned()).filter(|id| !self.people.contains_key(id)).collect();
        ids.sort();
        ids.dedup();
        ids
    }

    /// `(gaia_id, display_name)` pairs for outbound `@mention` resolution.
    pub fn mentionable_members(&self, group: &Group) -> Vec<(String, String)> {
        let ids: Vec<String> = self
            .conversations
            .get(group)
            .map(|c| c.members.clone())
            .unwrap_or_default();
        if ids.is_empty() {
            return self.people.iter().map(|(id, p)| (id.clone(), p.name.clone())).collect();
        }
        ids.into_iter().map(|id| (id.clone(), self.name_of(&id))).collect()
    }

    /// Remembers a message's topic id and creation timestamp.
    pub fn remember_message(&mut self, message_id: &str, topic_id: &str, create_time_micros: i64) {
        if message_id.is_empty() {
            return;
        }
        let topic = if topic_id.is_empty() { message_id } else { topic_id };
        self.messages.insert(
            message_id.to_string(),
            MessageMeta { topic_id: topic.to_string(), create_time_micros },
        );
    }

    /// Resolves a message's topic id (falling back to the message id itself, which is true for head-of-topic messages).
    pub fn topic_of(&self, message_id: &str) -> String {
        self.messages
            .get(message_id)
            .map(|m| m.topic_id.clone())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| message_id.to_string())
    }

    pub fn learn(&mut self, user: &pb::User) {
        let Some(id) = user.user_id.as_ref().map(|u| u.id().to_string()).filter(|s| !s.is_empty()) else { return };
        let mut name = user.name().trim().to_string();
        if name.is_empty() {
            name = [user.first_name(), user.last_name()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join(" ");
        }
        let entry = self.people.entry(id).or_default();
        if !name.is_empty() {
            entry.name = name;
        }
        if !user.email().is_empty() {
            entry.email = user.email().to_string();
        }
        if !user.avatar_url().is_empty() {
            entry.avatar = Some(avatar_url(user.avatar_url()));
        }
        entry.bot = user.user_id.as_ref().is_some_and(|u| u.r#type() == pb::UserType::Bot);
    }
}

/// The last few characters of an id, for a name that is not known.
fn tail(id: &str) -> &str {
    let start = id.char_indices().rev().nth(5).map(|(i, _)| i).unwrap_or(0);
    &id[start..]
}

/// Google's avatar addresses come without a scheme (`//lh3.googleusercontent.com/...`).
pub fn avatar_url(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        raw.to_string()
    }
}

/// Reads the conversation list into the world. Conversations the account has hidden, blocked or not joined (an
/// invitation) are left out.
pub fn absorb_world(world: &mut World, response: &pb::PaginatedWorldResponse) {
    for item in &response.world_items {
        let Some(group) = Group::from_pb(item.group_id.as_ref()) else { continue };
        let read = item.read_state.as_ref();
        if read.is_some_and(|r| r.blocked() || r.hide_timestamp() > 0) {
            continue;
        }
        if read.is_some_and(|r| r.membership_state.is_some() && r.membership_state() != pb::MembershipState::MemberJoined) {
            if read.is_some_and(|r| r.membership_state() == pb::MembershipState::MemberInvited) {
                if let Group::Space(space_id) = group {
                    if !world.pending_invites.contains(&space_id) {
                        world.pending_invites.push(space_id);
                    }
                }
            }
            continue;
        }
        let mut members: Vec<String> = read.map(|r| r.joined_users.iter().map(|u| u.id().to_string()).collect()).unwrap_or_default();
        if members.is_empty() {
            members = item.dm_members.as_ref().map(|d| d.members.iter().map(|u| u.id().to_string()).collect()).unwrap_or_default();
        }
        members.retain(|m| !m.is_empty());
        let muted = read
            .and_then(|r| r.notification_settings.as_ref())
            .and_then(|n| n.mute.as_ref())
            .is_some_and(|m| m.state() == pb::group_notification_settings_update::mute::State::Muted);
        let description = item
            .group_lite
            .as_ref()
            .and_then(|g| g.group_details.as_ref())
            .and_then(|d| d.description.clone())
            .filter(|s| !s.trim().is_empty());
        let existing_owners = world.conversations.get(&group).map(|c| c.owners.clone()).unwrap_or_default();
        let existing_members = world.conversations.get(&group).map(|c| c.members.clone()).unwrap_or_default();
        let existing_guidelines = world.conversations.get(&group).and_then(|c| c.guidelines.clone());
        let starred = read.is_some_and(|r| r.starred());
        let conversation = Conversation {
            name: item.room_name.clone().filter(|n| !n.trim().is_empty()),
            description: description.or_else(|| world.conversations.get(&group).and_then(|c| c.description.clone())),
            guidelines: existing_guidelines,
            members: if members.is_empty() { existing_members } else { members },
            owners: existing_owners,
            unread: read.map(|r| r.unread_message_count()).unwrap_or(0),
            sort_time: item.sort_timestamp(),
            muted,
            starred,
            threaded: item.threaded_group.is_some(),
            history_off: super::retention::world_item_history_off(item),
            emoji: item
                .avatar_info
                .as_ref()
                .and_then(|a| a.emoji.as_ref())
                .and_then(|e| e.content.as_ref())
                .map(|pb::emoji::Content::Unicode(u)| u.clone())
                .filter(|u| !u.is_empty()),
        };
        world.conversations.insert(group, conversation);
    }
}

/// Asks Google for the names of the people the world mentions that are not known yet.
pub async fn learn_people(handle: &Handle, ids: Vec<String>) {
    for batch in ids.chunks(50) {
        match handle.api.members(batch).await {
            Ok(response) => {
                let mut world = handle.world();
                for member in &response.members {
                    if let Some(pb::member::Profile::User(user)) = &member.profile {
                        world.learn(user);
                    }
                }
            }
            Err(e) => tracing::debug!("googlechat[{}]: get_members: {e}", handle.account_id),
        }
    }
}

/// The rail group the spaces sit in.
pub fn rail_id(account_id: &str) -> String {
    format!("{account_id}|spaces")
}

pub fn register_rail(state: &AppState, handle: &Handle) {
    let name = {
        let world = handle.world();
        if world.own_email.is_empty() {
            "Google Chat".to_string()
        } else {
            format!("Google Chat ({})", world.own_email)
        }
    };
    state.runtime.upsert_buffer_group(
        state,
        BufferGroup { id: rail_id(&handle.account_id), account_id: handle.account_id.clone(), service: "googlechat".into(), kind: "guild".into(), name, icon_url: None, position: 0, pending: false },
    );
}

/// The buffer a conversation is shown in, made or renamed as needed.
pub fn register(state: &AppState, handle: &Handle, group: &Group, fresh_unread: bool) -> crate::model::Buffer {
    let account_id = &handle.account_id;
    let remote = group.remote_id();
    let (mut title, unread, avatar, muted, description) = {
        let world = handle.world();
        let conv = world.conversations.get(group);
        let avatar = match conv {
            Some(c) if group.is_dm() => {
                let others: Vec<&String> = c.members.iter().filter(|m| **m != world.own_id).collect();
                if others.len() == 1 {
                    world.people.get(others[0]).and_then(|p| p.avatar.clone())
                } else {
                    None
                }
            }
            _ => None,
        };
        (
            world.title_of(group),
            conv.map(|c| c.unread).unwrap_or(0),
            avatar,
            conv.is_some_and(|c| c.muted),
            conv.and_then(|c| c.description.clone()),
        )
    };
    // Two conversations called the same (two people of one name): the second is told apart by its id.
    {
        let mut world = handle.world();
        if world.titles.iter().any(|(g, t)| g != group && *t == title) {
            let id = match group {
                Group::Dm(id) | Group::Space(id) => id.clone(),
            };
            title = format!("{title} ({})", tail(&id));
        }
        world.titles.insert(group.clone(), title.clone());
    }
    let kind = if group.is_dm() { "dm" } else { "channel" };
    let existing = state.runtime.list_buffers().into_iter().find(|b| b.account_id == *account_id && b.remote_id.as_deref() == Some(remote.as_str()));
    let fresh = existing.is_none();
    let buffer = match existing {
        Some(found) if found.name == title => {
            // Confirm restored layout buffers so runtime's 30s layout sweeper does not remove them.
            state.runtime.ensure_buffer(state, account_id, &title, kind)
        }
        Some(found) => {
            state.runtime.rename_buffer(state, account_id, &found.name, &title, kind);
            state.runtime.ensure_buffer(state, account_id, &title, kind)
        }
        None => state.runtime.ensure_buffer(state, account_id, &title, kind),
    };
    state.runtime.set_buffer_remote_id(state, &buffer.id, &remote);
    let starred = handle.world().conversations.get(group).is_some_and(|c| c.starred);
    if group.is_dm() {
        state.runtime.set_buffer_category(state, &buffer.id, Some(if starred { "Starred" } else { "Direct Messages" }), if starred { -10 } else { 0 });
        if let Some(avatar) = avatar {
            state.runtime.set_buffer_avatar(state, &buffer.id, &avatar);
        }
    } else {
        state.runtime.set_buffer_group(state, &buffer.id, &rail_id(account_id));
        state.runtime.set_buffer_category(state, &buffer.id, Some(if starred { "Starred" } else { "Spaces" }), if starred { -10 } else { 100 });
    }
    if let Some(desc) = description {
        state.runtime.set_buffer_topic(state, &buffer.id, &desc);
    }
    let level = if muted { NotifyLevel::Mute } else { NotifyLevel::Default };
    state.runtime.set_notify_level(state, &buffer.id, level, level);
    if (fresh || fresh_unread) && unread > 0 {
        state.runtime.set_buffer_unread(state, &buffer.id, unread.min(u32::MAX as i64) as u32, 0);
    }
    super::retention::show(state, handle, group);
    buffer
}

/// The buffer of a conversation, if it has one.
pub fn buffer_of(state: &AppState, account_id: &str, group: &Group) -> Option<crate::model::Buffer> {
    let remote = group.remote_id();
    state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_come_from_names_or_people() {
        let mut world = World { own_id: "1".into(), ..Default::default() };
        world.people.insert("2".into(), Person { name: "Ada".into(), ..Default::default() });
        world.conversations.insert(Group::Dm("d".into()), Conversation { members: vec!["1".into(), "2".into()], ..Default::default() });
        world.conversations.insert(Group::Dm("g".into()), Conversation { members: vec!["1".into(), "2".into(), "3456789".into()], ..Default::default() });
        world.conversations.insert(Group::Space("s".into()), Conversation { name: Some("Team".into()), ..Default::default() });
        assert_eq!(world.title_of(&Group::Dm("d".into())), "Ada");
        assert_eq!(world.title_of(&Group::Dm("g".into())), "Ada, Google Chat user 456789");
        assert_eq!(world.title_of(&Group::Space("s".into())), "Team");
        assert_eq!(world.unknown_people(), vec!["1".to_string(), "3456789".to_string()]);
    }

    #[test]
    fn avatars_get_a_scheme() {
        assert_eq!(avatar_url("//lh3.googleusercontent.com/a/x"), "https://lh3.googleusercontent.com/a/x");
    }
}
