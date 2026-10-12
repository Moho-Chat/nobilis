//! Becoming friends and ceasing to be: asking, answering and ending.
//!
//! On Steam these are two messages. `ClientAddFriend` is both "I would like
//! to be your friend" and "yes, I would like that too", depending on whether
//! the other has already asked - so accepting a request is adding the person
//! who made it. `ClientRemoveFriend` ends a friendship, takes back a request
//! we made, and turns down one made to us.
//!
//! Who is waiting on whom is kept in the roster; what the window is shown of
//! it is built here.

use super::friends::Roster;
use super::link::Link;
use super::proto::{ClientAddFriend, ClientAddFriendResponse, ClientRemoveFriend};
use super::wire::emsg;
use super::{eresult, steamid};
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::HashSet;

/// Who somebody means when they type a person into the add box.
#[derive(Debug, PartialEq)]
pub enum Target {
    /// Somebody's number, however it was written.
    Id(u64),
    /// An account name, which Steam looks up itself.
    Name(String),
}

/// Reads what a person typed: the 17-digit id, the number a friend's "friend
/// code" is, the `[U:1:n]` form, a profile address that carries the id, or
/// else an account name.
pub fn parse_target(text: &str) -> Result<Target, String> {
    let text = text.trim().trim_end_matches('/');
    if text.is_empty() {
        return Err("type who to add: a Steam id, a friend code, or an account name".to_string());
    }
    if let Some(inner) = text.strip_prefix("[U:1:").and_then(|t| t.strip_suffix(']')) {
        return inner.parse::<u32>().map(|n| Target::Id(steamid::steamid_of(n))).map_err(|_| format!("{text} is not a Steam id"));
    }
    // A profile address. Only the numeric form says who it is; a custom name
    // in one is looked up on the website, which this does not talk to.
    if let Some((_, rest)) = text.split_once("steamcommunity.com/") {
        return match rest.split_once('/') {
            Some(("profiles", id)) => parse_target(id),
            Some(("id", _)) => Err("that profile address uses a custom name, which moho cannot look up; use the id from the number-form address, or the account name".to_string()),
            _ => Err("that is not a profile address".to_string()),
        };
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        let n: u64 = text.parse().map_err(|_| format!("{text} is too long to be a Steam id"))?;
        // A friend code is the account number alone; a full id is that plus
        // the fixed part for a person.
        return match steamid::account_of(n) {
            Some(_) => Ok(Target::Id(n)),
            None if n <= u64::from(u32::MAX) => Ok(Target::Id(steamid::steamid_of(n as u32))),
            None => Err(format!("{text} is not a person's Steam id")),
        };
    }
    Ok(Target::Name(text.to_string()))
}

/// A friendship that was begun, or a request that was answered.
#[derive(Debug, PartialEq)]
pub struct Added {
    pub steamid: u64,
    pub name: Option<String>,
}

/// Asks somebody to be our friend, or says yes to their request.
pub async fn add(link: &Link, target: &Target) -> Result<Added, String> {
    let request = match target {
        Target::Id(id) => ClientAddFriend { steamid_to_add: Some(*id), accountname_or_email_to_add: None },
        Target::Name(name) => ClientAddFriend { steamid_to_add: None, accountname_or_email_to_add: Some(name.clone()) },
    };
    let answer: ClientAddFriendResponse = link.ask(emsg::CLIENT_ADD_FRIEND, &request).await.map_err(|e| e.to_string())?;
    let code = answer.eresult.unwrap_or(eresult::FAIL);
    if code != eresult::OK {
        return Err(eresult::describe(code));
    }
    Ok(Added { steamid: answer.steam_id_added.unwrap_or_default(), name: answer.persona_name_added.filter(|n| !n.is_empty()) })
}

/// Ends a friendship, takes back a request, or turns one down. Steam sends
/// no answer to this; the friends list that follows is the answer.
pub fn remove(link: &Link, steamid: u64) -> Result<(), String> {
    steamid::account_of(steamid).ok_or_else(|| "that is not a person's Steam id".to_string())?;
    link.send(emsg::CLIENT_REMOVE_FRIEND, &ClientRemoveFriend { friendid: Some(steamid) }).map_err(|e| e.to_string())
}

fn person(roster: &Roster, account: u32) -> Value {
    json!({
        "steamid": steamid::text_of(account),
        "name": roster.persona(account).and_then(|p| p.name.clone()).filter(|n| !n.is_empty()),
        "avatarUrl": roster.persona(account).and_then(|p| p.avatar.clone()),
    })
}

/// What the window is shown: who has asked, and who we have asked.
pub fn snapshot(roster: &Roster) -> Value {
    json!({
        "incoming": roster.asked_us().into_iter().map(|a| person(roster, a)).collect::<Vec<_>>(),
        "outgoing": roster.asked_by_us().into_iter().map(|a| person(roster, a)).collect::<Vec<_>>(),
    })
}

/// Keeps the window's picture of the requests current, and says out loud
/// when somebody new has asked. `told` holds who the window has already been
/// told about, so a name arriving a moment after the request, or the list
/// being sent again, does not announce the same person twice.
pub fn publish(state: &AppState, account_id: &str, roster: &Roster, told: &mut HashSet<u32>) {
    let now = snapshot(roster);
    if state.runtime.steam_requests(account_id) == now {
        return;
    }
    state.runtime.set_steam_requests(account_id, now.clone());
    let asking = roster.asked_us();
    told.retain(|a| asking.contains(a));
    for account in asking {
        if told.insert(account) {
            state.events.emit("steamFriendRequest", json!({ "accountId": account_id, "request": person(roster, account) }));
        }
    }
    state.events.emit("steamFriendRequests", json!({ "accountId": account_id, "requests": now }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::steam::link::fake::serve;
    use crate::backend::steam::proto::{ClientFriendsList, FriendsListEntry};
    use prost::Message;

    /// Steam's answer to an add: its own message, matched to the question by job.
    fn reply(request: &crate::backend::steam::wire::Packet, body: Vec<u8>) -> crate::backend::steam::wire::Packet {
        use crate::backend::steam::{proto::Header, wire::Packet};
        Packet::new(emsg::CLIENT_ADD_FRIEND_RESPONSE, Header { jobid_target: request.header.jobid_source, ..Header::default() }, body)
    }

    #[test]
    fn a_person_can_be_named_every_way_steam_writes_them() {
        let id = steamid::steamid_of(22202);
        assert_eq!(parse_target("76561197960287930"), Ok(Target::Id(id)));
        assert_eq!(parse_target("  76561197960287930\n"), Ok(Target::Id(id)));
        assert_eq!(parse_target("22202"), Ok(Target::Id(id)), "a friend code is the account number");
        assert_eq!(parse_target("[U:1:22202]"), Ok(Target::Id(id)));
        assert_eq!(parse_target("https://steamcommunity.com/profiles/76561197960287930/"), Ok(Target::Id(id)));
        assert_eq!(parse_target("mohotester"), Ok(Target::Name("mohotester".to_string())));
    }

    #[test]
    fn what_cannot_be_read_is_said_rather_than_guessed_at() {
        assert!(parse_target("").is_err());
        assert!(parse_target("   ").is_err());
        assert!(parse_target("https://steamcommunity.com/id/somebody").unwrap_err().contains("custom name"));
        assert!(parse_target("https://steamcommunity.com/groups/x").is_err());
        assert!(parse_target("[U:1:abc]").is_err());
        // A group's id, which has the digits of an id and is not a person's.
        assert!(parse_target("103582791429521412").is_err());
    }

    #[tokio::test]
    async fn adding_by_id_sends_the_id_and_reports_who_was_added() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let url = serve(Box::new({
            let seen = seen.clone();
            move |packet| {
                *seen.lock().unwrap() = Some((packet.emsg, ClientAddFriend::decode(packet.body.as_slice()).unwrap()));
                let body = ClientAddFriendResponse { eresult: Some(1), steam_id_added: Some(steamid::steamid_of(7)), persona_name_added: Some("Seven".into()) };
                Some(vec![reply(&packet, body.encode_to_vec())])
            }
        }))
        .await;
        let (link, _told) = crate::backend::steam::link::open_with("test", &url, std::time::Duration::from_secs(5)).await.unwrap();
        let added = add(&link, &Target::Id(steamid::steamid_of(7))).await.unwrap();
        assert_eq!(added, Added { steamid: steamid::steamid_of(7), name: Some("Seven".into()) });
        let (kind, sent) = seen.lock().unwrap().take().unwrap();
        assert_eq!(kind, emsg::CLIENT_ADD_FRIEND);
        assert_eq!(sent.steamid_to_add, Some(steamid::steamid_of(7)));
        assert_eq!(sent.accountname_or_email_to_add, None, "one or the other, never both");
    }

    #[tokio::test]
    async fn adding_by_name_sends_the_name() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let url = serve(Box::new({
            let seen = seen.clone();
            move |packet| {
                *seen.lock().unwrap() = Some(ClientAddFriend::decode(packet.body.as_slice()).unwrap());
                let body = ClientAddFriendResponse { eresult: Some(1), steam_id_added: Some(steamid::steamid_of(8)), persona_name_added: None };
                Some(vec![reply(&packet, body.encode_to_vec())])
            }
        }))
        .await;
        let (link, _told) = crate::backend::steam::link::open_with("test", &url, std::time::Duration::from_secs(5)).await.unwrap();
        let added = add(&link, &Target::Name("mohotester".into())).await.unwrap();
        assert_eq!(added.name, None, "an empty or absent name is not a name");
        assert_eq!(seen.lock().unwrap().take().unwrap().accountname_or_email_to_add.as_deref(), Some("mohotester"));
    }

    #[tokio::test]
    async fn a_refusal_is_said_in_words_and_a_missing_result_is_a_failure() {
        for (code, words) in [(Some(42), "nobody"), (Some(29), "already"), (Some(40), "blocked"), (None, "without saying why")] {
            let url = serve(Box::new(move |packet| {
                let body = ClientAddFriendResponse { eresult: code, ..Default::default() };
                Some(vec![reply(&packet, body.encode_to_vec())])
            }))
            .await;
            let (link, _told) = crate::backend::steam::link::open_with("test", &url, std::time::Duration::from_secs(5)).await.unwrap();
            let err = add(&link, &Target::Name("x".into())).await.unwrap_err();
            assert!(err.contains(words), "{code:?} -> {err}");
        }
    }

    #[tokio::test]
    async fn removing_sends_the_id_and_refuses_what_is_not_a_person() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let url = serve(Box::new({
            let seen = seen.clone();
            move |packet| {
                *seen.lock().unwrap() = Some((packet.emsg, ClientRemoveFriend::decode(packet.body.as_slice()).unwrap()));
                Some(vec![])
            }
        }))
        .await;
        let (link, _told) = crate::backend::steam::link::open_with("test", &url, std::time::Duration::from_secs(5)).await.unwrap();
        assert!(remove(&link, 103582791429521412).is_err());
        assert!(seen.lock().unwrap().is_none(), "nothing was sent for a bad id");
        remove(&link, steamid::steamid_of(9)).unwrap();
        for _ in 0..100 {
            if seen.lock().unwrap().is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let (kind, sent) = seen.lock().unwrap().take().unwrap();
        assert_eq!((kind, sent.friendid), (emsg::CLIENT_REMOVE_FRIEND, Some(steamid::steamid_of(9))));
    }

    fn roster_with_requests() -> Roster {
        let mut r = Roster::default();
        let entry = |a: u32, rel: u32| FriendsListEntry { ulfriendid: Some(steamid::steamid_of(a)), efriendrelationship: Some(rel) };
        r.apply_friends_list(&ClientFriendsList { bincremental: Some(false), friends: vec![entry(10, 3), entry(20, 2), entry(30, 4)], ..Default::default() });
        r
    }

    #[test]
    fn the_snapshot_lists_who_asked_and_who_was_asked() {
        let now = snapshot(&roster_with_requests());
        assert_eq!(now["incoming"][0]["steamid"], steamid::text_of(20));
        assert_eq!(now["outgoing"][0]["steamid"], steamid::text_of(30));
        assert_eq!(now["incoming"].as_array().unwrap().len(), 1, "a friend is not a request");
        assert!(now["incoming"][0]["name"].is_null(), "not named until Steam says");
    }

    #[tokio::test]
    async fn somebody_asking_is_announced_once() {
        let state = crate::backend::kick::testkit::simulated_daemon("steam-friendship");
        let mut events = state.events.subscribe();
        let roster = roster_with_requests();
        let mut told = HashSet::new();
        publish(&state, "steam:me", &roster, &mut told);
        publish(&state, "steam:me", &roster, &mut told);

        let mut asked = 0;
        let mut lists = 0;
        while let Ok(event) = events.try_recv() {
            match event.name {
                "steamFriendRequest" => asked += 1,
                "steamFriendRequests" => lists += 1,
                _ => {}
            }
        }
        assert_eq!((asked, lists), (1, 1), "the same list again says nothing");
        assert_eq!(state.runtime.steam_requests("steam:me")["incoming"][0]["steamid"], steamid::text_of(20));
    }
}
