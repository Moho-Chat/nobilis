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
    /// What is said under the name: the game, or a state the colour cannot give.
    activity: Option<(&'static str, String)>,
    avatar: Option<String>,
    /// Which kind of client, when they were last seen, and what their game says: for their card.
    detail: (Option<&'static str>, Option<u32>, Option<String>),
}

/// Which kind of device a person is on, in words: a Steam Machine is said by what Steam calls the
/// device, the rest by what their client says.
fn client_name(p: &super::friends::Persona) -> Option<&'static str> {
    let said = presence::client_of(p.state, p.flags, p.on_deck)?;
    Some(if p.device == super::friends::DEVICE_STEAM_MACHINE && !p.on_deck { "Steam Machine" } else { said })
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
            self.shown.entry(account).or_insert(Shown { name: buffer.name, status: "offline", activity: None, avatar: None, detail: (None, None, None) });
        }
    }

    /// Follows the account's mutes - set in another client, or here - onto the
    /// conversations: muted on Steam is silent here, and the row says so.
    pub fn apply_mutes(&self, state: &AppState, roster: &Roster) {
        for (account, shown) in &self.shown {
            state.runtime.set_silenced(state, &crate::model::buffer_id(&self.account_id, &shown.name), roster.muted(*account));
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
            let Some(mut name) = names.get(&account).cloned() else { continue };
            // A session that has only just begun knows its friends by number
            // until their names arrive. A conversation already called something
            // keeps that: renamed to a number and back, a friend list would
            // flicker into digits at every reconnect, and a connection that
            // keeps dropping would leave it that way.
            if !roster.has_name(account) {
                if let Some(held) = self.shown.get(&account).map(|s| s.name.clone()).filter(|n| *n != steamid::text_of(account)) {
                    name = held;
                }
            }
            let persona = roster.persona(account);
            let status = persona.map_or("offline", |p| presence::status_of(p.state));
            let avatar = persona.and_then(|p| p.avatar.clone());
            let activity = persona.and_then(|p| presence::activity(p.state, roster.game_of(account).as_deref()));
            let detail = persona.map_or((None, None, None), |p| (client_name(p), p.last_seen, p.rich.clone()));
            let next = Shown { name: name.clone(), status, activity: activity.clone(), avatar: avatar.clone(), detail: detail.clone() };
            let before = self.shown.get(&account);
            if before == Some(&next) {
                continue;
            }

            // A person who changed their name keeps their conversation.
            if let Some(old) = before.map(|b| b.name.clone()).filter(|old| *old != name) {
                state.runtime.rename_buffer(state, &self.account_id, &old, &name, "dm");
            }
            let buffer = state.runtime.ensure_buffer(state, &self.account_id, &name, "dm");
            // Made new, or renamed from its number: a warning on it comes back.
            super::notices::restore_buffer(state, &self.account_id, &buffer.id);
            // And whether the account has muted them, wherever it did.
            state.runtime.set_silenced(state, &buffer.id, roster.muted(account));
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
                "activity": activity.map(|(kind, text)| json!({ "kind": kind, "text": text })),
                "playingSince": persona.filter(|p| p.app.is_some() || p.game.is_some()).and_then(|p| p.playing_since),
                "client": detail.0,
                "lastSeen": detail.1,
                "rich": detail.2,
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
    fn a_reconnect_does_not_turn_the_friends_back_into_numbers() {
        let state = simulated_daemon("steam-publish-reconnect");
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[22202]));
        roster.apply_persona(&update(vec![who(22202, "Gabe", 1)]));
        Conversations::new(ACCOUNT).publish(&state, &roster);
        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).is_some());

        // A new session: the conversations as they were, and a roster that has
        // the friends list and nobody's name yet.
        let mut shown = Conversations::new(ACCOUNT);
        shown.adopt(&state, ACCOUNT);
        let mut fresh = Roster::default();
        fresh.apply_friends_list(&friends(&[22202]));
        shown.publish(&state, &fresh);
        assert_eq!(shown.name_of(22202), Some("Gabe"), "still called what they were called");
        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "76561197960287930")).is_none(), "and no conversation made under the number");
        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).is_some());

        // When the name does arrive, a changed one is honoured.
        fresh.apply_persona(&update(vec![who(22202, "Gabriel", 1)]));
        shown.publish(&state, &fresh);
        assert_eq!(shown.name_of(22202), Some("Gabriel"));
    }

    #[test]
    fn a_warning_survives_the_conversation_being_renamed_from_its_number() {
        let state = simulated_daemon("steam-publish-warning-rename");
        // As the last run left it: known by its number, with a warning said in it.
        let number = steamid::text_of(22202);
        state.runtime.record_message_at(&state, ACCOUNT, &number, "dm", "Steam", "Steam warns that this person may not be who they appear to be.", false, "system", None, Some(crate::backend::steam::notices::notice_id(22202, 1)), false, None, vec![], vec![], None, None, None, None);
        let mut shown = Conversations::new(ACCOUNT);
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[22202]));
        shown.adopt(&state, ACCOUNT);
        shown.publish(&state, &roster);
        crate::backend::steam::notices::restore(&state, ACCOUNT);
        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, &number)).unwrap().notice.is_some());
        // Steam says who it is: the conversation becomes Gabe, and the banner is on it.
        roster.apply_persona(&update(vec![who(22202, "Gabe", 1)]));
        shown.publish(&state, &roster);
        let gabe = state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).expect("renamed");
        assert!(gabe.notice.is_some(), "the banner is still up on the renamed conversation");
    }

    #[test]
    fn a_game_on_steam_is_in_game_until_its_name_is_known_and_then_its_name() {
        let state = simulated_daemon("steam-publish-game-by-number");
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[22202, 22203]));
        // Steam names no game of its own: the number alone, as it sends it.
        let playing = |account: u32, app: u64| PersonaFriend { friendid: Some(steamid::steamid_of(account)), player_name: Some(format!("P{account}")), persona_state: Some(1), gameid: Some(app), game_played_app_id: Some(app as u32), ..Default::default() };
        roster.apply_persona(&update(vec![playing(22202, 730), playing(22203, 730)]));
        assert_eq!(roster.unnamed_apps(), vec![730], "one question for two people in the same game");
        let mut shown = Conversations::new(ACCOUNT);
        let mut rx = state.events.subscribe();
        shown.publish(&state, &roster);
        let said = events(&mut rx, "presenceChange");
        let text = |said: &[serde_json::Value], who: &str| said.iter().rev().find(|e| e["bufferId"].as_str().is_some_and(|b| b.ends_with(who))).and_then(|e| e["members"][0]["activity"].clone().as_object().cloned());
        assert_eq!(text(&said, "P22202").unwrap()["text"], "In game");
        roster.set_app_name(730, "Counter-Strike 2".into());
        assert!(roster.unnamed_apps().is_empty());
        shown.publish(&state, &roster);
        let said = events(&mut rx, "presenceChange");
        assert_eq!(text(&said, "P22202").unwrap()["text"], "Counter-Strike 2", "named, and said again");
        assert_eq!(text(&said, "P22203").unwrap()["kind"], "game");
        // A game that has a name from Steam is not asked about, and stopping is stopping.
        roster.apply_persona(&update(vec![PersonaFriend { friendid: Some(steamid::steamid_of(22202)), game_name: Some("My Mod".into()), gameid: Some(1 << 40), ..Default::default() }]));
        assert_eq!(roster.game_of(22202).as_deref(), Some("My Mod"));
        roster.apply_persona(&update(vec![PersonaFriend { friendid: Some(steamid::steamid_of(22202)), gameid: Some(0), ..Default::default() }]));
        assert_eq!(roster.game_of(22202), None);
    }

    #[test]
    fn a_mute_set_on_steam_silences_the_conversation_here_and_lifting_it_unsilences() {
        let state = simulated_daemon("steam-publish-mute");
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[22202]));
        roster.apply_persona(&update(vec![who(22202, "Gabe", 1)]));
        let mut shown = Conversations::new(ACCOUNT);
        shown.publish(&state, &roster);
        let id = crate::model::buffer_id(ACCOUNT, "Gabe");
        assert!(!state.runtime.get_buffer(&id).unwrap().server_muted);
        // Muted in another client: it arrives as the account's preferences for them.
        roster.set_preferences(&PerFriendPreferences { accountid: Some(22202), notifications_showmessages: Some(crate::backend::steam::friends::NOTIFY_NEVER), ..Default::default() });
        shown.apply_mutes(&state, &roster);
        assert!(state.runtime.get_buffer(&id).unwrap().server_muted, "the row says it is muted");
        roster.set_preferences(&PerFriendPreferences { accountid: Some(22202), ..Default::default() });
        shown.apply_mutes(&state, &roster);
        assert!(!state.runtime.get_buffer(&id).unwrap().server_muted);
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
    fn a_game_and_the_states_the_colours_share_are_said_beside_the_dot_and_change_when_they_do() {
        let state = simulated_daemon("steam-publish-text");
        let mut roster = Roster::default();
        roster.apply_friends_list(&friends(&[1, 2, 3]));
        roster.apply_persona(&update(vec![who(1, "Plain", 1), who(2, "Snoozer", 4), who(3, "Gamer", 1)]));
        let game = |name: &str| PersonaFriend { friendid: Some(steamid::steamid_of(3)), gameid: Some(730), game_name: Some(name.into()), ..Default::default() };
        roster.apply_persona(&update(vec![game("Counter-Strike 2")]));
        let mut shown = Conversations::new(ACCOUNT);
        let mut rx = state.events.subscribe();
        shown.publish(&state, &roster);
        let text = |said: &[serde_json::Value], nick: &str| said.iter().find(|e| e["members"][0]["nick"] == nick).map(|e| e["members"][0]["activity"].clone());
        let said = events(&mut rx, "presenceChange");
        assert_eq!(text(&said, "Plain"), Some(serde_json::Value::Null), "online says nothing under the name");
        assert_eq!(text(&said, "Snoozer"), Some(serde_json::json!({ "kind": "custom", "text": "Snoozing" })));
        assert_eq!(text(&said, "Gamer"), Some(serde_json::json!({ "kind": "game", "text": "Counter-Strike 2" })));
        // The dot is still what it was for each: a game does not change it.
        assert_eq!(said.iter().find(|e| e["members"][0]["nick"] == "Gamer").unwrap()["members"][0]["status"], "online");

        // They stop playing: said once, for that person alone.
        roster.apply_persona(&update(vec![PersonaFriend { friendid: Some(steamid::steamid_of(3)), gameid: Some(0), ..Default::default() }]));
        shown.publish(&state, &roster);
        let said = events(&mut rx, "presenceChange");
        assert_eq!(said.len(), 1);
        assert_eq!(said[0]["members"][0]["nick"], "Gamer");
        assert!(said[0]["members"][0]["activity"].is_null());
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
