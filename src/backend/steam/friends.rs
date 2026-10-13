//! Who the friends are, how they are doing, and what each conversation with
//! them is called.
//!
//! Steam sends the friends list and the details of the people on it as two
//! separate streams: a list of ids and how each stands with us (a friend, a
//! request, blocked), and, as they become known and as they change, each
//! person's name, state, game and picture. A change is a partial update -
//! only what changed is in it - so a person is built up from many messages
//! and this is where they are put together.
//!
//! What this does not do is talk to the rest of the daemon: it is plain data
//! in and out, so the rules (who counts as a friend, what a conversation is
//! called when two friends share a name) can be tested without a connection.

use super::proto::{ClientFriendsList, ClientPersonaState, PerFriendPreferences, PersonaFriend};
use super::steamid;
use std::collections::HashMap;

/// `EFriendRelationship`.
pub const REL_NONE: u32 = 0;
/// Somebody has asked to be our friend.
pub const REL_REQUEST_RECIPIENT: u32 = 2;
pub const REL_FRIEND: u32 = 3;
/// We have asked somebody to be ours.
pub const REL_REQUEST_INITIATOR: u32 = 4;

/// `ENotificationSetting` never: this friend's messages do not notify.
pub const NOTIFY_NEVER: i32 = 2;

/// What is known about a person from their persona updates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Persona {
    pub name: Option<String>,
    /// Steam's state number; see `presence`.
    pub state: u32,
    /// The game they are in, when they are in one.
    pub game: Option<String>,
    /// The avatar's hash as hex.
    pub avatar: Option<String>,
    /// `EPersonaStateFlag`: which kind of client they are on, among other things.
    pub flags: u32,
    /// Whether they are on a Steam Deck.
    pub on_deck: bool,
    /// The last time they were seen, unix seconds.
    pub last_seen: Option<u32>,
    /// What the game they are in says about what they are doing, where it says.
    pub rich: Option<String>,
}

/// The picture Steam shows for somebody who has not set one.
const DEFAULT_AVATAR: &str = "fef49e7fa7e1997310d705b2a6158ff8dc1cdfeb";

/// Where an avatar is served from. A hash of all zeros is how Steam says
/// "none set", and is not an image - the default has a hash of its own.
pub fn avatar_url(hash: &[u8]) -> String {
    let hex: String = hash.iter().map(|b| format!("{b:02x}")).collect();
    let hex = if hex.is_empty() || hash.iter().all(|b| *b == 0) { DEFAULT_AVATAR.to_string() } else { hex };
    format!("https://avatars.steamstatic.com/{hex}_full.jpg")
}

/// A name made safe to be a conversation's name: no line breaks or other
/// control characters, no padding.
pub fn clean_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).collect::<String>().trim().to_string()
}

#[derive(Default)]
pub struct Roster {
    relationships: HashMap<u32, u32>,
    personas: HashMap<u32, Persona>,
    /// What the account keeps about each friend: the name it calls them, and how they may notify it.
    preferences: HashMap<u32, PerFriendPreferences>,
    /// People with a conversation who are not friends: somebody who wrote to
    /// us first. They need a conversation of their own and a name that does
    /// not clash with anyone else's, but they are not on the friends list.
    talking: std::collections::HashSet<u32>,
}

impl Roster {
    /// Takes in the friends list. A full list replaces what was held; an
    /// incremental one is only the people whose standing changed. Returns the
    /// accounts whose standing is now different.
    pub fn apply_friends_list(&mut self, list: &ClientFriendsList) -> Vec<u32> {
        let mut changed = Vec::new();
        if !list.bincremental.unwrap_or(false) {
            // Anyone not mentioned any more is no longer on the list.
            let now: std::collections::HashSet<u32> = list.friends.iter().filter_map(|f| f.ulfriendid.and_then(steamid::account_of)).collect();
            let gone: Vec<u32> = self.relationships.keys().copied().filter(|a| !now.contains(a)).collect();
            for account in gone {
                self.relationships.remove(&account);
                changed.push(account);
            }
        }
        for entry in &list.friends {
            let Some(account) = entry.ulfriendid.and_then(steamid::account_of) else { continue };
            let relationship = entry.efriendrelationship.unwrap_or(REL_NONE);
            let before = self.relationships.get(&account).copied().unwrap_or(REL_NONE);
            if relationship == REL_NONE {
                self.relationships.remove(&account);
            } else {
                self.relationships.insert(account, relationship);
            }
            if before != relationship {
                changed.push(account);
            }
        }
        changed
    }

    /// Takes in persona updates, which carry only what changed. Returns the
    /// accounts whose persona is now different.
    pub fn apply_persona(&mut self, update: &ClientPersonaState) -> Vec<u32> {
        let mut changed = Vec::new();
        for friend in &update.friends {
            let Some(account) = friend.friendid.and_then(steamid::account_of) else { continue };
            let held = self.personas.entry(account).or_default();
            let before = held.clone();
            merge(held, friend);
            if *held != before {
                changed.push(account);
            }
        }
        changed
    }

    /// Notes that there is a conversation with this person, friend or not.
    /// Returns whether that is new.
    pub fn note_conversation(&mut self, account: u32) -> bool {
        !self.is_friend(account) && self.talking.insert(account)
    }

    /// Everybody a conversation exists with: the friends, and whoever else
    /// has written. In a steady order.
    pub fn participants(&self) -> Vec<u32> {
        let mut out = self.friends();
        out.extend(self.talking.iter().copied().filter(|a| !self.is_friend(*a)));
        out.sort_unstable();
        out
    }

    /// The people who are friends, in a steady order.
    pub fn friends(&self) -> Vec<u32> {
        let mut out: Vec<u32> = self.relationships.iter().filter(|(_, r)| **r == REL_FRIEND).map(|(a, _)| *a).collect();
        out.sort_unstable();
        out
    }

    /// The people who have asked to be our friend, in a steady order.
    pub fn asked_us(&self) -> Vec<u32> {
        self.with_relationship(REL_REQUEST_RECIPIENT)
    }

    /// The people we have asked, who have not yet said yes.
    pub fn asked_by_us(&self) -> Vec<u32> {
        self.with_relationship(REL_REQUEST_INITIATOR)
    }

    fn with_relationship(&self, wanted: u32) -> Vec<u32> {
        let mut out: Vec<u32> = self.relationships.iter().filter(|(_, r)| **r == wanted).map(|(a, _)| *a).collect();
        out.sort_unstable();
        out
    }

    pub fn is_friend(&self, account: u32) -> bool {
        self.relationships.get(&account) == Some(&REL_FRIEND)
    }

    /// Takes in what the account keeps about a friend. Returns whether the name
    /// it calls them changed, which is what moves a conversation.
    pub fn set_preferences(&mut self, prefs: &PerFriendPreferences) -> bool {
        let Some(account) = prefs.accountid else { return false };
        let before = self.nickname(account).map(str::to_string);
        let bare = PerFriendPreferences { accountid: Some(account), ..Default::default() };
        if *prefs == bare {
            self.preferences.remove(&account);
        } else {
            self.preferences.insert(account, prefs.clone());
        }
        before.as_deref() != self.nickname(account)
    }

    /// Sets only what the account calls a friend, keeping the rest it holds.
    /// Returns whether that name changed.
    pub fn set_nickname(&mut self, account: u32, nickname: Option<&str>) -> bool {
        let mut prefs = self.preferences.get(&account).cloned().unwrap_or(PerFriendPreferences { accountid: Some(account), ..Default::default() });
        prefs.nickname = nickname.map(str::to_string);
        self.set_preferences(&prefs)
    }

    /// What the account calls this friend, if it has said.
    pub fn nickname(&self, account: u32) -> Option<&str> {
        self.preferences.get(&account).and_then(|p| p.nickname.as_deref()).map(str::trim).filter(|n| !n.is_empty())
    }

    pub fn persona(&self, account: u32) -> Option<&Persona> {
        self.personas.get(&account)
    }

    /// What each friend's conversation is called.
    ///
    /// Their name, since that is what a person recognises - but names are not
    /// unique on Steam, and a conversation's name is also its identity, so two
    /// friends called the same would share one and each message would go to
    /// whichever was first. Whenever two or more friends share a name (read
    /// without regard to case) every one of them has their account number
    /// added to it, which is stable and tells them apart; when only one is
    /// left the number is dropped again. Somebody not yet named is called by
    /// their id.
    ///
    /// Distinct is the guarantee, not a hope: a friend can be called anything,
    /// including exactly what another friend's name becomes once decorated, so
    /// the result is checked and anyone still sharing a name is marked again,
    /// differently, until nobody is.
    /// Whether anything is known to call this person: a nickname the account
    /// gave them, or the name they have. Without either, `conversation_names`
    /// has only their number to offer.
    pub fn has_name(&self, account: u32) -> bool {
        self.nickname(account).map(clean_name).is_some_and(|n| !n.is_empty())
            || self.personas.get(&account).and_then(|p| p.name.as_deref()).map(clean_name).is_some_and(|n| !n.is_empty())
    }

    pub fn conversation_names(&self) -> HashMap<u32, String> {
        // What the account calls them, if it has said; otherwise what they
        // call themselves.
        let base = |account: u32| -> String {
            self.nickname(account)
                .map(clean_name)
                .filter(|n| !n.is_empty())
                .or_else(|| self.personas.get(&account).and_then(|p| p.name.as_deref()).map(clean_name).filter(|n| !n.is_empty()))
                .unwrap_or_else(|| steamid::text_of(account))
        };
        let people = self.participants();
        let mut names: HashMap<u32, String> = people.iter().map(|a| (*a, base(*a))).collect();
        // Plain, then with the number in round brackets, in square, in curly;
        // and for the last resort a character no cleaned name can contain.
        let marks: [fn(&str, u32) -> String; 4] = [
            |n, a| format!("{n} ({a})"),
            |n, a| format!("{n} [{a}]"),
            |n, a| format!("{n} {{{a}}}"),
            |n, a| format!("{n}\u{1f}{a}"),
        ];
        for mark in marks {
            let mut by_name: HashMap<String, Vec<u32>> = HashMap::new();
            for (account, name) in &names {
                by_name.entry(name.to_lowercase()).or_default().push(*account);
            }
            let clashing: Vec<u32> = by_name.into_values().filter(|group| group.len() > 1).flatten().collect();
            if clashing.is_empty() {
                break;
            }
            for account in clashing {
                names.insert(account, mark(&base(account), account));
            }
        }
        names
    }
}

fn merge(held: &mut Persona, update: &PersonaFriend) {
    if let Some(name) = update.player_name.as_deref().map(clean_name).filter(|n| !n.is_empty()) {
        held.name = Some(name);
    }
    if let Some(state) = update.persona_state {
        held.state = state;
    }
    // A game is cleared by saying there is none, not by leaving it out.
    if update.gameid == Some(0) || update.game_played_app_id == Some(0) {
        held.game = None;
    }
    if let Some(game) = update.game_name.as_deref().filter(|g| !g.is_empty()) {
        held.game = Some(game.to_string());
    }
    if let Some(hash) = update.avatar_hash.as_deref().filter(|h| !h.is_empty()) {
        held.avatar = Some(avatar_url(hash));
    }
    if let Some(flags) = update.persona_state_flags {
        held.flags = flags;
    }
    if let Some(deck) = update.on_steam_deck {
        held.on_deck = deck;
    }
    if let Some(seen) = [update.last_seen_online, update.last_logoff, update.last_logon].into_iter().flatten().filter(|t| *t > 0).max() {
        held.last_seen = Some(held.last_seen.map_or(seen, |old| old.max(seen)));
    }
    // A game's own words for what is going on: its "status" line, which is
    // plain text where the game sets one. Cleared with the game.
    if !update.rich_presence.is_empty() {
        held.rich = update.rich_presence.iter().find(|kv| kv.key.as_deref() == Some("status")).and_then(|kv| kv.value.clone()).filter(|v| !v.trim().is_empty());
    }
    if update.gameid == Some(0) || update.game_played_app_id == Some(0) {
        held.rich = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::steam::proto::{FriendsListEntry, KeyValue};

    // The other standings a list carries, which matter here only as not being a friend.
    const REL_BLOCKED: u32 = 1;

    #[test]
    fn a_nickname_is_what_a_friend_is_called_and_goes_back_to_their_name_when_taken() {
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND), (20, REL_FRIEND)]));
        r.apply_persona(&ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(10)), player_name: Some("Gabe".into()), ..Default::default() }], ..Default::default() });
        assert_eq!(r.conversation_names()[&10], "Gabe");
        assert!(r.set_nickname(10, Some("Buddy")), "a new name is a change");
        assert_eq!(r.conversation_names()[&10], "Buddy");
        assert!(!r.set_nickname(10, Some("Buddy")), "the same again is not");
        // Muting keeps the nickname, and the other way about.
        r.set_preferences(&PerFriendPreferences { accountid: Some(10), nickname: Some("Buddy".into()), notifications_showmessages: Some(NOTIFY_NEVER), ..Default::default() });
        assert_eq!(r.preferences.get(&10).and_then(|p| p.notifications_showmessages), Some(NOTIFY_NEVER));
        assert!(r.set_nickname(10, None));
        assert_eq!(r.conversation_names()[&10], "Gabe");
        assert_eq!(r.preferences.get(&10).and_then(|p| p.notifications_showmessages), Some(NOTIFY_NEVER), "taking the name back does not unmute");
        // Two friends given the same nickname are still told apart.
        r.set_nickname(10, Some("Pal"));
        r.set_nickname(20, Some("pal"));
        let names = r.conversation_names();
        assert_ne!(names[&10].to_lowercase(), names[&20].to_lowercase());
    }

    #[test]
    fn the_kind_of_client_and_last_seen_are_kept_from_their_updates() {
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND)]));
        r.apply_persona(&ClientPersonaState {
            friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(10)), persona_state_flags: Some(512), last_logoff: Some(100), last_seen_online: Some(300), on_steam_deck: Some(false), rich_presence: vec![KeyValue { key: Some("status".into()), value: Some("In a match".into()) }], ..Default::default() }],
            ..Default::default()
        });
        let p = r.persona(10).unwrap();
        assert_eq!((p.flags, p.last_seen, p.rich.as_deref()), (512, Some(300), Some("In a match")));
        // A later update only says what changed: the newest time wins, and the game ending ends its words.
        r.apply_persona(&ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(10)), last_logoff: Some(200), gameid: Some(0), ..Default::default() }], ..Default::default() });
        let p = r.persona(10).unwrap();
        assert_eq!((p.last_seen, p.rich.as_deref(), p.flags), (Some(300), None, 512));
    }

    #[test]
    fn requests_are_told_apart_by_who_made_them() {
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND), (30, REL_REQUEST_RECIPIENT), (20, REL_REQUEST_RECIPIENT), (50, REL_REQUEST_INITIATOR), (60, REL_BLOCKED)]));
        assert_eq!(r.asked_us(), [20, 30]);
        assert_eq!(r.asked_by_us(), [50]);
        assert_eq!(r.friends(), [10]);
        // Saying yes turns a request into a friend.
        r.apply_friends_list(&list(true, &[(20, REL_FRIEND)]));
        assert_eq!(r.asked_us(), [30]);
        assert!(r.is_friend(20));
    }

    fn entry(account: u32, relationship: u32) -> FriendsListEntry {
        FriendsListEntry { ulfriendid: Some(steamid::steamid_of(account)), efriendrelationship: Some(relationship) }
    }

    fn list(incremental: bool, entries: &[(u32, u32)]) -> ClientFriendsList {
        ClientFriendsList { bincremental: Some(incremental), friends: entries.iter().map(|(a, r)| entry(*a, *r)).collect(), ..Default::default() }
    }

    fn named(account: u32, name: &str, state: u32) -> PersonaFriend {
        PersonaFriend { friendid: Some(steamid::steamid_of(account)), player_name: Some(name.into()), persona_state: Some(state), ..Default::default() }
    }

    fn persona(friends: Vec<PersonaFriend>) -> ClientPersonaState {
        ClientPersonaState { friends, ..Default::default() }
    }

    #[test]
    fn only_friends_are_friends_not_requests_or_the_blocked() {
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND), (20, REL_REQUEST_RECIPIENT), (30, REL_BLOCKED), (40, REL_FRIEND), (50, REL_REQUEST_INITIATOR)]));
        assert_eq!(r.friends(), vec![10, 40]);
        assert!(r.is_friend(10) && !r.is_friend(20) && !r.is_friend(99));
    }

    #[test]
    fn an_incremental_list_changes_the_people_it_names_and_leaves_the_rest() {
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND), (20, REL_FRIEND)]));
        let changed = r.apply_friends_list(&list(true, &[(20, REL_NONE), (30, REL_FRIEND), (10, REL_FRIEND)]));
        assert_eq!(r.friends(), vec![10, 30]);
        changed.iter().for_each(|a| assert!([20, 30].contains(a), "10 did not change, so it is not reported: {changed:?}"));
        assert_eq!(changed.len(), 2);
    }

    #[test]
    fn a_full_list_forgets_whoever_it_no_longer_names() {
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND), (20, REL_FRIEND)]));
        let changed = r.apply_friends_list(&list(false, &[(20, REL_FRIEND)]));
        assert_eq!(r.friends(), vec![20]);
        assert_eq!(changed, vec![10]);
    }

    #[test]
    fn an_id_that_is_not_a_person_is_skipped_not_a_panic() {
        let mut r = Roster::default();
        let group = (1u64 << 56) | (7 << 52) | (1 << 32) | 5;
        let bad = ClientFriendsList { friends: vec![FriendsListEntry { ulfriendid: Some(group), efriendrelationship: Some(3) }, FriendsListEntry { ulfriendid: None, efriendrelationship: Some(3) }], ..Default::default() };
        assert!(r.apply_friends_list(&bad).is_empty());
        assert!(r.friends().is_empty());
    }

    #[test]
    fn a_persona_is_built_up_from_partial_updates() {
        let mut r = Roster::default();
        r.apply_persona(&persona(vec![named(10, "Alex", 1)]));
        // A later update with only a state, and one with only a game.
        r.apply_persona(&persona(vec![PersonaFriend { friendid: Some(steamid::steamid_of(10)), persona_state: Some(3), ..Default::default() }]));
        r.apply_persona(&persona(vec![PersonaFriend { friendid: Some(steamid::steamid_of(10)), game_name: Some("Portal 2".into()), gameid: Some(620), ..Default::default() }]));
        let p = r.persona(10).unwrap();
        assert_eq!(p.name.as_deref(), Some("Alex"), "a name left out is not a name taken away");
        assert_eq!(p.state, 3);
        assert_eq!(p.game.as_deref(), Some("Portal 2"));
        // Quitting the game is said by saying there is none.
        r.apply_persona(&persona(vec![PersonaFriend { friendid: Some(steamid::steamid_of(10)), gameid: Some(0), ..Default::default() }]));
        assert_eq!(r.persona(10).unwrap().game, None);
    }

    #[test]
    fn only_a_real_change_is_reported_as_one() {
        let mut r = Roster::default();
        assert_eq!(r.apply_persona(&persona(vec![named(10, "Alex", 1)])), vec![10]);
        assert!(r.apply_persona(&persona(vec![named(10, "Alex", 1)])).is_empty(), "the same again is not news");
        assert_eq!(r.apply_persona(&persona(vec![named(10, "Alex", 3)])), vec![10]);
    }

    #[test]
    fn a_name_is_made_fit_to_be_a_conversation() {
        assert_eq!(clean_name("  Alex\n"), "Alex");
        assert_eq!(clean_name("A\u{0}l\u{7}ex"), "Alex");
        assert_eq!(clean_name("日本語 名前"), "日本語 名前");
        assert_eq!(clean_name("\n\t "), "");
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND)]));
        r.apply_persona(&persona(vec![named(10, "   ", 1)]));
        assert_eq!(r.persona(10).unwrap().name, None, "a blank name is not a name");
    }

    #[test]
    fn friends_are_called_by_name_and_unnamed_ones_by_their_id() {
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND), (20, REL_FRIEND)]));
        r.apply_persona(&persona(vec![named(10, "Alex", 1)]));
        let names = r.conversation_names();
        assert_eq!(names[&10], "Alex");
        assert_eq!(names[&20], steamid::text_of(20));
    }

    #[test]
    fn two_friends_with_the_same_name_do_not_share_a_conversation() {
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND), (20, REL_FRIEND), (30, REL_FRIEND)]));
        r.apply_persona(&persona(vec![named(10, "Alex", 1), named(20, "alex", 1), named(30, "Sam", 1)]));
        let names = r.conversation_names();
        // Case is not what tells people apart, so these two collide...
        assert_eq!(names[&10], "Alex (10)");
        assert_eq!(names[&20], "alex (20)");
        assert_eq!(names[&30], "Sam");
        let distinct: std::collections::HashSet<_> = names.values().map(|n| n.to_lowercase()).collect();
        assert_eq!(distinct.len(), 3, "every conversation has a name of its own");
        // ...and when one of them is renamed the other goes back to plain.
        r.apply_persona(&persona(vec![named(20, "Robin", 1)]));
        let names = r.conversation_names();
        assert_eq!((names[&10].as_str(), names[&20].as_str()), ("Alex", "Robin"));
    }

    #[test]
    fn the_numbers_cannot_be_fooled_by_a_name_that_looks_like_one() {
        // Somebody calling themselves "Alex (20)" must not collide with the
        // friend who is made "Alex (20)" by sharing a name.
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND), (20, REL_FRIEND), (30, REL_FRIEND)]));
        r.apply_persona(&persona(vec![named(10, "Alex", 1), named(20, "Alex", 1), named(30, "Alex (20)", 1)]));
        let names = r.conversation_names();
        let distinct: std::collections::HashSet<_> = names.values().map(|n| n.to_lowercase()).collect();
        assert_eq!(distinct.len(), 3, "{names:?}");
    }

    /// Whatever people call themselves, no two conversations share a name.
    /// Every way four friends can be given names from a pool made of exactly
    /// the strings the decoration produces, which is the worst case there is.
    #[test]
    fn however_friends_are_named_no_two_conversations_are_called_the_same() {
        let pool = ["a", "A", "a (1)", "a (2)", "a [1]", "a [2]", "a (1) [1]"];
        let mut checked = 0;
        for combo in 0..pool.len().pow(4) {
            let mut r = Roster::default();
            r.apply_friends_list(&list(false, &[(1, REL_FRIEND), (2, REL_FRIEND), (3, REL_FRIEND), (4, REL_FRIEND)]));
            let mut at = combo;
            let updates: Vec<PersonaFriend> = (1..=4u32)
                .map(|account| {
                    let name = pool[at % pool.len()];
                    at /= pool.len();
                    named(account, name, 1)
                })
                .collect();
            r.apply_persona(&persona(updates));
            let names = r.conversation_names();
            let distinct: std::collections::HashSet<String> = names.values().map(|n| n.to_lowercase()).collect();
            assert_eq!(distinct.len(), 4, "combination {combo} gave {names:?}");
            checked += 1;
        }
        assert_eq!(checked, 7 * 7 * 7 * 7);
    }

    #[test]
    fn somebody_who_writes_first_gets_a_conversation_without_becoming_a_friend() {
        let mut r = Roster::default();
        r.apply_friends_list(&list(false, &[(10, REL_FRIEND)]));
        assert!(r.note_conversation(77), "new");
        assert!(!r.note_conversation(77), "not new twice");
        assert!(!r.note_conversation(10), "a friend already has one");
        assert_eq!(r.friends(), vec![10]);
        assert_eq!(r.participants(), vec![10, 77]);
        r.apply_persona(&persona(vec![named(10, "Alex", 1), named(77, "Alex", 1)]));
        let names = r.conversation_names();
        assert_ne!(names[&10].to_lowercase(), names[&77].to_lowercase(), "a stranger sharing a friend's name does not share their conversation");
        // Once they are a friend they are only counted once.
        r.apply_friends_list(&list(true, &[(77, REL_FRIEND)]));
        assert_eq!(r.participants(), vec![10, 77]);
    }

    #[test]
    fn the_avatar_is_where_steam_serves_it_and_none_set_is_the_default_picture() {
        let hash: Vec<u8> = (0..20).collect();
        assert_eq!(avatar_url(&hash), "https://avatars.steamstatic.com/000102030405060708090a0b0c0d0e0f10111213_full.jpg");
        assert_eq!(avatar_url(&[0; 20]), "https://avatars.steamstatic.com/fef49e7fa7e1997310d705b2a6158ff8dc1cdfeb_full.jpg");
        assert_eq!(avatar_url(&[]), avatar_url(&[0; 20]));
    }
}
