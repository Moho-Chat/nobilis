//! The account's conversations and the people in them, and their buffers.
//!
//! `paginated_world` lists every conversation the account is in: direct messages (with one person or a few) and
//! spaces. A direct message has no name of its own and is called after the people in it, whose names come from
//! `get_members` - a message's author arrives as an id alone.

use super::{pb, Group, Handle};
use crate::model::BufferGroup;
use crate::state::AppState;
use std::collections::HashMap;

/// What is known of a person.
#[derive(Clone, Debug, Default)]
pub struct Person {
    pub name: String,
    pub email: String,
    pub avatar: Option<String>,
    pub bot: bool,
}

/// A conversation the account is in.
#[derive(Clone, Debug, Default)]
pub struct Conversation {
    /// A space's name, or a group message's if somebody named it.
    pub name: Option<String>,
    /// The people in a direct message (the account among them).
    pub members: Vec<String>,
    pub unread: i64,
    pub sort_time: i64,
    /// Whether the space keeps every message in a thread of its own (the older kind of space).
    pub threaded: bool,
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
            continue;
        }
        let mut members: Vec<String> = read.map(|r| r.joined_users.iter().map(|u| u.id().to_string()).collect()).unwrap_or_default();
        if members.is_empty() {
            members = item.dm_members.as_ref().map(|d| d.members.iter().map(|u| u.id().to_string()).collect()).unwrap_or_default();
        }
        members.retain(|m| !m.is_empty());
        let conversation = Conversation {
            name: item.room_name.clone().filter(|n| !n.trim().is_empty()),
            members,
            unread: read.map(|r| r.unread_message_count()).unwrap_or(0),
            sort_time: item.sort_timestamp(),
            threaded: item.threaded_group.is_some(),
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
    let (mut title, unread, avatar) = {
        let world = handle.world();
        let avatar = match world.conversations.get(group) {
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
        (world.title_of(group), world.conversations.get(group).map(|c| c.unread).unwrap_or(0), avatar)
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
        Some(found) if found.name == title => found,
        Some(found) => {
            state.runtime.rename_buffer(state, account_id, &found.name, &title, kind);
            state.runtime.get_buffer(&crate::model::buffer_id(account_id, &title)).unwrap_or(found)
        }
        None => state.runtime.ensure_buffer(state, account_id, &title, kind),
    };
    state.runtime.set_buffer_remote_id(state, &buffer.id, &remote);
    if group.is_dm() {
        state.runtime.set_buffer_category(state, &buffer.id, Some("Direct Messages"), 0);
        if let Some(avatar) = avatar {
            state.runtime.set_buffer_avatar(state, &buffer.id, &avatar);
        }
    } else {
        state.runtime.set_buffer_group(state, &buffer.id, &rail_id(account_id));
        state.runtime.set_buffer_category(state, &buffer.id, Some("Spaces"), 100);
    }
    if (fresh || fresh_unread) && unread > 0 {
        state.runtime.set_buffer_unread(state, &buffer.id, unread.min(u32::MAX as i64) as u32, 0);
    }
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
