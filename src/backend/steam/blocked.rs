//! The people an account has blocked.
//!
//! A blocked person is not a friend and has no conversation, so there is nowhere
//! in the conversation list to lift a block from. They are listed on the page
//! where friends are added, by name once Steam has said it, and unblocked there.

use super::friends::Roster;
use super::steamid;
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

static HELD: LazyLock<Mutex<HashMap<String, Value>>> = LazyLock::new(Default::default);

/// Who is blocked, as the window reads it: the 64-bit id as text and a name.
pub fn snapshot(roster: &Roster) -> Value {
    Value::Array(
        roster
            .blocked()
            .into_iter()
            .map(|a| json!({ "steamid": steamid::text_of(a), "name": roster.persona(a).and_then(|p| p.name.clone()).unwrap_or_else(|| steamid::text_of(a)) }))
            .collect(),
    )
}

pub fn held(account_id: &str) -> Value {
    HELD.lock().unwrap().get(account_id).cloned().unwrap_or_else(|| json!([]))
}

pub fn clear(account_id: &str) {
    HELD.lock().unwrap().remove(account_id);
}

/// Tells the window who is blocked, when that is different from last time.
pub fn publish(state: &AppState, account_id: &str, roster: &Roster) {
    let now = snapshot(roster);
    {
        let mut all = HELD.lock().unwrap();
        if all.get(account_id) == Some(&now) || (now == json!([]) && !all.contains_key(account_id)) {
            return;
        }
        all.insert(account_id.to_string(), now.clone());
    }
    state.events.emit("steamBlocked", json!({ "accountId": account_id, "blocked": now }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::steam::proto::*;

    #[test]
    fn the_blocked_are_listed_by_name_once_it_is_known_and_by_number_before() {
        let mut roster = Roster::default();
        roster.apply_friends_list(&ClientFriendsList {
            friends: vec![
                FriendsListEntry { ulfriendid: Some(steamid::steamid_of(100)), efriendrelationship: Some(1) },
                FriendsListEntry { ulfriendid: Some(steamid::steamid_of(200)), efriendrelationship: Some(3) },
                FriendsListEntry { ulfriendid: Some(steamid::steamid_of(300)), efriendrelationship: Some(6) },
            ],
            ..Default::default()
        });
        assert_eq!(snapshot(&roster), json!([{ "steamid": steamid::text_of(100), "name": steamid::text_of(100) }, { "steamid": steamid::text_of(300), "name": steamid::text_of(300) }]));
        roster.apply_persona(&ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(100)), player_name: Some("Spammer".into()), ..Default::default() }], ..Default::default() });
        assert_eq!(snapshot(&roster)[0]["name"], "Spammer");
    }

    #[test]
    fn it_is_told_once_and_again_only_when_it_changes() {
        let state = crate::backend::kick::testkit::simulated_daemon("steam-blocked");
        let mut rx = state.events.subscribe();
        let mut roster = Roster::default();
        publish(&state, "steam:blocked-test", &roster);
        assert!(rx.try_recv().is_err(), "nobody blocked, nothing to say");
        roster.apply_friends_list(&ClientFriendsList { friends: vec![FriendsListEntry { ulfriendid: Some(steamid::steamid_of(100)), efriendrelationship: Some(1) }], ..Default::default() });
        publish(&state, "steam:blocked-test", &roster);
        publish(&state, "steam:blocked-test", &roster);
        let mut told = 0;
        while let Ok(e) = rx.try_recv() {
            if e.name == "steamBlocked" {
                told += 1;
            }
        }
        assert_eq!(told, 1);
        assert_eq!(held("steam:blocked-test").as_array().map(Vec::len), Some(1));
        clear("steam:blocked-test");
    }
}
