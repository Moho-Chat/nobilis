//! What the window sees of the roster: a conversation for each person, a
//! name that follows them, and how they are doing.
//!
//! A friend is a direct-message conversation. Its name is their name, so it
//! changes when they rename themselves - and then the conversation is moved,
//! history and all, rather than a second one begun. Its presence is a
//! one-person member list, which is what the window reads a direct message's
//! dot from.
//!
//! This runs after every batch of updates and so is written to say nothing
//! when nothing changed. Signing in brings hundreds of persona updates in a
//! few seconds, and a window told "this friend is online" four times for each
//! of them would spend its first moments redrawing.

use super::friends::Roster;
use super::{presence, steamid};
use crate::state::AppState;
use serde_json::json;
use std::collections::HashMap;

/// What the window was last told about one person.
#[derive(Clone, Debug, PartialEq)]
struct Shown {
    name: String,
    status: &'static str,
    avatar: Option<String>,
}

pub struct Conversations {
    account_id: String,
    shown: HashMap<u32, Shown>,
}

impl Conversations {
    pub fn new(account_id: &str) -> Self {
        Self { account_id: account_id.to_string(), shown: HashMap::new() }
    }

    /// Takes up the conversations that already exist for this account, as the
    /// daemon restored them, so that a friend who was renamed while moho was
    /// closed has their conversation moved rather than a second one made.
    /// A conversation says who it is with in its `remote_id`.
    pub fn adopt(&mut self, state: &AppState, account_id: &str) {
        for buffer in state.runtime.list_buffers() {
            if buffer.account_id != account_id || buffer.kind != "dm" {
                continue;
            }
            let Some(account) = buffer.remote_id.as_deref().and_then(|id| id.parse::<u64>().ok()).and_then(steamid::account_of) else { continue };
            self.shown.entry(account).or_insert(Shown { name: buffer.name, status: "offline", avatar: None });
        }
    }

    /// The name of the conversation with this person, as it is on screen now.
    pub fn name_of(&self, account: u32) -> Option<&str> {
        self.shown.get(&account).map(|s| s.name.as_str())
    }

    /// Brings the conversations in line with the roster, telling the window
    /// only about what is different from last time.
    pub fn publish(&mut self, state: &AppState, roster: &Roster) {
        let names = roster.conversation_names();
        for account in roster.participants() {
            let Some(name) = names.get(&account).cloned() else { continue };
            let persona = roster.persona(account);
            let status = persona.map_or("offline", |p| presence::status_of(p.state));
            let avatar = persona.and_then(|p| p.avatar.clone());
            let next = Shown { name: name.clone(), status, avatar: avatar.clone() };
            let before = self.shown.get(&account);
            if before == Some(&next) {
                continue;
            }

            // A person who changed their name keeps their conversation.
            if let Some(old) = before.map(|b| b.name.clone()).filter(|old| *old != name) {
                state.runtime.rename_buffer(state, &self.account_id, &old, &name, "dm");
            }
            let buffer = state.runtime.ensure_buffer(state, &self.account_id, &name, "dm");
            state.runtime.set_buffer_remote_id(state, &buffer.id, &steamid::text_of(account));
            if let Some(url) = &avatar {
                state.runtime.set_buffer_avatar(state, &buffer.id, url);
            }

            // The window reads a direct message's dot from its member list;
            // for a one-to-one that is the other person alone.
            let member = json!([{
                "nick": persona.and_then(|p| p.name.clone()).unwrap_or_else(|| name.clone()),
                "userId": steamid::text_of(account),
                "status": status,
                "away": status == "idle",
                "avatarUrl": avatar,
            }]);
            state.runtime.set_presence(&buffer.id, member.clone());
            state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": member }));

            self.shown.insert(account, next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::simulated_daemon;
    use crate::backend::steam::friends::{REL_FRIEND, Roster};
    use crate::backend::steam::proto::*;

    const ACCOUNT: &str = "steam:tester";

    fn friends(accounts: &[u32]) -> ClientFriendsList {
        ClientFriendsList {
            bincremental: Some(false),
            friends: accounts.iter().map(|a| FriendsListEntry { ulfriendid: Some(steamid::steamid_of(*a)), efriendrelationship: Some(REL_FRIEND) }).collect(),
            ..Default::default()
        }
    }

    fn who(account: u32, name: &str, state: u32) -> PersonaFriend {
        PersonaFriend { friendid: Some(steamid::steamid_of(account)), player_name: Some(name.into()), persona_state: Some(state), avatar_hash: Some(vec![7; 20]), ..Default::default() }
    }

    fn update(people: Vec<PersonaFriend>) -> ClientPersonaState {
        ClientPersonaState { friends: people, ..Default::default() }
    }

    fn events(rx: &mut tokio::sync::broadcast::Receiver<crate::events::Event>, name: &str) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        while let Ok(e) = rx.try_recv() {
            if e.name == name {
                out.push(e.data);
            }
        }
        out
    }

    #[test]
    fn a_friend_becomes_a_direct_message_with_a_face_and_a_dot() {
        let state = simulated_daemon("steam-publish-basic");
        let mut rx = state.events.subscribe();
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[22202]));
        roster.apply_persona(&update(vec![who(22202, "Gabe", 1)]));
        let mut shown = Conversations::new(ACCOUNT);
        shown.publish(&state, &roster);

        let buffer = state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).expect("a conversation for the friend");
        assert_eq!(buffer.kind, "dm");
        assert_eq!(buffer.remote_id.as_deref(), Some("76561197960287930"), "the person, not the name, is what identifies them");
        assert!(buffer.avatar_url.as_deref().is_some_and(|u| u.starts_with("https://avatars.steamstatic.com/07070707")));

        let presence = events(&mut rx, "presenceChange");
        assert_eq!(presence.len(), 1);
        let members = &presence[0]["members"];
        assert_eq!(members.as_array().unwrap().len(), 1, "a direct message's list is the other person alone");
        assert_eq!((members[0]["nick"].as_str(), members[0]["status"].as_str(), members[0]["away"].as_bool()), (Some("Gabe"), Some("online"), Some(false)));
        assert_eq!(shown.name_of(22202), Some("Gabe"));
    }

    #[test]
    fn nothing_is_said_again_when_nothing_changed() {
        let state = simulated_daemon("steam-publish-quiet");
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[1, 2]));
        roster.apply_persona(&update(vec![who(1, "One", 1), who(2, "Two", 3)]));
        let mut shown = Conversations::new(ACCOUNT);
        shown.publish(&state, &roster);

        let mut rx = state.events.subscribe();
        shown.publish(&state, &roster);
        assert!(events(&mut rx, "presenceChange").is_empty());
        assert!(events(&mut rx, "bufferListChange").is_empty());

        // One of them changes: only that one is said.
        roster.apply_persona(&update(vec![who(2, "Two", 1)]));
        shown.publish(&state, &roster);
        let said = events(&mut rx, "presenceChange");
        assert_eq!(said.len(), 1);
        assert_eq!(said[0]["members"][0]["nick"], "Two");
        assert_eq!(said[0]["members"][0]["status"], "online");
    }

    #[test]
    fn away_is_idle_and_offline_is_offline_and_a_game_does_not_change_either() {
        let state = simulated_daemon("steam-publish-states");
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[1, 2, 3, 4]));
        roster.apply_persona(&update(vec![who(1, "Away", 3), who(2, "Busy", 2), who(3, "Gone", 0), who(4, "Hidden", 7)]));
        let mut rx = state.events.subscribe();
        Conversations::new(ACCOUNT).publish(&state, &roster);
        let said = events(&mut rx, "presenceChange");
        let by_name: HashMap<String, (String, bool)> = said
            .iter()
            .map(|e| (e["members"][0]["nick"].as_str().unwrap().to_string(), (e["members"][0]["status"].as_str().unwrap().to_string(), e["members"][0]["away"].as_bool().unwrap())))
            .collect();
        assert_eq!(by_name["Away"], ("idle".to_string(), true));
        assert_eq!(by_name["Busy"], ("dnd".to_string(), false));
        assert_eq!(by_name["Gone"], ("offline".to_string(), false));
        assert_eq!(by_name["Hidden"].0, "offline", "invisible reads as gone, as it does in Steam");
    }

    #[test]
    fn a_friend_who_renames_keeps_their_conversation_and_its_history() {
        let state = simulated_daemon("steam-publish-rename");
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[5]));
        roster.apply_persona(&update(vec![who(5, "Old Name", 1)]));
        let mut shown = Conversations::new(ACCOUNT);
        shown.publish(&state, &roster);
        state.runtime.record_message(&state, ACCOUNT, "Old Name", "dm", "Old Name", "hello there", false, "chat", None, Some("1:1".into()), false, None, Vec::new(), Vec::new(), None);

        roster.apply_persona(&update(vec![who(5, "New Name", 1)]));
        shown.publish(&state, &roster);

        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Old Name")).is_none(), "not left behind as a second conversation");
        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "New Name")).is_some());
        let history = state.store.get_backlog(&crate::model::buffer_id(ACCOUNT, "New Name"), 0, 10).unwrap();
        assert_eq!(history.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), ["hello there"], "the history moved with them");
    }

    #[test]
    fn two_friends_who_share_a_name_get_two_conversations_and_stay_two() {
        let state = simulated_daemon("steam-publish-shared");
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[10, 20]));
        roster.apply_persona(&update(vec![who(10, "Alex", 1)]));
        let mut shown = Conversations::new(ACCOUNT);
        shown.publish(&state, &roster);
        assert_eq!(shown.name_of(10), Some("Alex"));

        roster.apply_persona(&update(vec![who(20, "Alex", 1)]));
        shown.publish(&state, &roster);
        assert_eq!((shown.name_of(10), shown.name_of(20)), (Some("Alex (10)"), Some("Alex (20)")));
        let one = state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Alex (10)")).unwrap();
        let two = state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Alex (20)")).unwrap();
        assert_ne!(one.remote_id, two.remote_id);
        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Alex")).is_none());
    }

    #[test]
    fn a_friend_renamed_while_moho_was_closed_keeps_their_conversation() {
        let state = simulated_daemon("steam-publish-adopt");
        // What a previous run left: a conversation with its person recorded.
        let buffer = state.runtime.ensure_buffer(&state, ACCOUNT, "Before", "dm");
        state.runtime.set_buffer_remote_id(&state, &buffer.id, &steamid::text_of(9));
        state.runtime.record_message(&state, ACCOUNT, "Before", "dm", "Before", "an old message", false, "chat", None, Some("1:0".into()), false, None, Vec::new(), Vec::new(), None);

        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[9]));
        roster.apply_persona(&update(vec![who(9, "After", 1)]));
        let mut shown = Conversations::new(ACCOUNT);
        shown.adopt(&state, ACCOUNT);
        shown.publish(&state, &roster);

        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Before")).is_none());
        let moved = state.store.get_backlog(&crate::model::buffer_id(ACCOUNT, "After"), 0, 10).unwrap();
        assert_eq!(moved.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), ["an old message"]);
    }

    #[test]
    fn another_accounts_conversations_are_not_adopted() {
        let state = simulated_daemon("steam-publish-adopt-other");
        let theirs = state.runtime.ensure_buffer(&state, "steam:someone-else", "Friend", "dm");
        state.runtime.set_buffer_remote_id(&state, &theirs.id, &steamid::text_of(9));
        let mut shown = Conversations::new(ACCOUNT);
        shown.adopt(&state, ACCOUNT);
        assert_eq!(shown.name_of(9), None);
    }

    #[test]
    fn somebody_who_has_not_been_named_yet_is_called_by_their_id_and_renamed_when_known() {
        let state = simulated_daemon("steam-publish-unnamed");
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[22202]));
        let mut shown = Conversations::new(ACCOUNT);
        shown.publish(&state, &roster);
        assert_eq!(shown.name_of(22202), Some("76561197960287930"), "until Steam says who they are, their id");
        roster.apply_persona(&update(vec![who(22202, "Gabe", 1)]));
        shown.publish(&state, &roster);
        assert_eq!(shown.name_of(22202), Some("Gabe"));
        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "76561197960287930")).is_none());
    }
}
