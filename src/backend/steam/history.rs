//! What was said before, and what has been read.
//!
//! Steam keeps every conversation, so nothing here is moho's to lose: opening
//! a conversation asks for its recent end, scrolling up asks for the part
//! before, and signing in asks what arrived while nobody was listening. What
//! comes back is stored under the same id a live message gets - the second
//! Steam stamped it and where it fell within that second - so hearing a
//! message twice, once live and once in history, is hearing it once.
//!
//! Reading is shared the other way too: looking at a conversation here tells
//! Steam it is read, so a phone stops buzzing about it, and Steam tells us
//! when it was read there.

use super::chat::message_id;
use super::friends::Roster;
use super::publish::Conversations;
use super::link::{CallError, Link};
use super::proto::{AckMessage, GetActiveSessionsRequest, GetActiveSessionsResponse, GetRecentMessagesRequest, GetRecentMessagesResponse, PastMessage};
use super::steamid;
use crate::model::Buffer;
use crate::state::AppState;

/// How many conversations a catch-up reads, most recent first.
const CATCH_UP_CONVERSATIONS: usize = 30;

/// How many messages one request asks for. Steam's own client loads fifty at
/// a time as you scroll up.
pub const PAGE: u32 = 50;

/// One message out of the past, oldest-first once it leaves `fetch`.
#[derive(Debug, PartialEq)]
pub struct Past {
    pub account: u32,
    pub timestamp: u32,
    pub ordinal: u32,
    pub text: String,
    /// Who has reacted with what, as held here; empty in a test that has none.
    pub reactions: Vec<crate::model::Reaction>,
    /// The web pages Steam's client described in it, which are cards.
    pub embeds: Vec<crate::model::Embed>,
}

impl Past {
    pub fn id(&self) -> String {
        message_id(self.timestamp, self.ordinal)
    }
}

fn read(message: &PastMessage, own: u32) -> Option<Past> {
    let account = message.accountid.filter(|a| *a != 0)?;
    // Asked for with its tags, and read into what moho draws.
    let rendered = super::bbcode::render_message(message.message.as_deref().unwrap_or_default());
    let (text, embeds) = (rendered.text, rendered.cards);
    if text.trim().is_empty() {
        return None;
    }
    Some(Past { account, timestamp: message.timestamp.unwrap_or_default(), ordinal: message.ordinal.unwrap_or_default(), text, reactions: super::reactions::from_history(own, &message.reactions), embeds })
}

/// Asks for up to `count` messages of a conversation that are older than
/// `older_than` - a second and an ordinal within it - or the newest when that
/// is absent. Oldest first, and whether there are more before them.
pub async fn fetch(link: &Link, own: u64, friend: u64, older_than: Option<(u32, u32)>, count: u32) -> Result<(Vec<Past>, bool), String> {
    let (time_last, ordinal_last) = older_than.unwrap_or((u32::MAX, 0));
    let request = GetRecentMessagesRequest {
        steamid1: Some(own),
        steamid2: Some(friend),
        count: Some(count),
        rtime32_start_time: Some(0),
        start_ordinal: Some(0),
        time_last: Some(time_last),
        ordinal_last: Some(ordinal_last),
        bbcode_format: Some(true),
        ..Default::default()
    };
    let answer: GetRecentMessagesResponse = link.call("FriendMessages.GetRecentMessages#1", &request, true).await.map_err(|e| super::chat::refusal(&e))?;
    let own_number = steamid::account_of(own).unwrap_or_default();
    let mut messages: Vec<Past> = answer.messages.iter().filter_map(|m| read(m, own_number)).collect();
    // Steam answers newest first.
    messages.reverse();
    Ok((messages, answer.more_available.unwrap_or(false)))
}

/// Everything Steam holds a session for: who it has had messages with, when it
/// was last looked at, and how many are unread.
pub async fn sessions(link: &Link, since: u32) -> Result<GetActiveSessionsResponse, CallError> {
    link.call("FriendMessages.GetActiveMessageSessions#1", &GetActiveSessionsRequest { lastmessage_since: Some(since), only_sessions_with_messages: Some(true) }, true).await
}

/// Says a conversation has been read up to a moment.
pub fn ack(link: &Link, friend: u64, timestamp: u32) -> Result<(), CallError> {
    link.notify("FriendMessages.AckMessage#1", &AckMessage { steamid_partner: Some(friend), timestamp: Some(timestamp) })
}

/// The friend's 64-bit id from the conversation, and our own number.
fn people(state: &AppState, buffer: &Buffer) -> Result<(u64, u64), String> {
    let friend: u64 = buffer.remote_id.as_deref().and_then(|id| id.parse().ok()).filter(|id| steamid::account_of(*id).is_some()).ok_or("this conversation is not with a Steam account")?;
    let own = state.accounts.get_steam(&buffer.account_id).map(|c| c.steamid).ok_or("this Steam account is not set up")?;
    Ok((own, friend))
}

/// Puts messages in a conversation's log without announcing them - they are
/// old, and a window told about each as news would treat them as unread.
/// Returns how many were new to the log.
pub fn store_quietly(state: &AppState, buffer: &Buffer, own: u32, past: &[Past]) -> usize {
    let own_name = state.runtime.own_identity(&buffer.account_id).unwrap_or_default();
    let mut added = 0;
    for p in past {
        let is_own = p.account == own;
        let from = if is_own { own_name.as_str() } else { buffer.name.as_str() };
        let sender = (!is_own).then(|| steamid::text_of(p.account));
        let stored = state.store.append_message(&buffer.id, &p.id(), from, &p.text, i64::from(p.timestamp), false, false, "chat", None, &p.reactions, is_own, None, &p.embeds, &[], sender.as_deref(), None, "dm", None, &[]);
        if matches!(stored, Ok(true)) {
            added += 1;
        } else if !p.reactions.is_empty() {
            // Already held, but reacted to since: Steam's list is the truth.
            let _ = state.store.set_reactions(&buffer.id, &p.id(), &p.reactions);
        }
    }
    added
}

/// The recent end of a conversation, stored: what opening it asks for.
pub async fn open(state: &AppState, buffer: &Buffer) -> Result<usize, String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let (own, friend) = people(state, buffer)?;
    let (past, _) = fetch(&link, own, friend, None, PAGE).await?;
    Ok(store_quietly(state, buffer, steamid::account_of(own).unwrap_or_default(), &past))
}

/// The part before what is shown, stored: what scrolling to the top asks for.
/// `before` is the second of the oldest message on screen.
pub async fn older(state: &AppState, buffer: &Buffer, before: i64) -> Result<usize, String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let (own, friend) = people(state, buffer)?;
    let before = u32::try_from(before).map_err(|_| "that is not a time Steam knows".to_string())?;
    // Up to and including the second asked about, whichever ordinal: what is
    // already stored is recognised and not stored again.
    let (past, _) = fetch(&link, own, friend, Some((before, u32::MAX)), PAGE).await?;
    Ok(store_quietly(state, buffer, steamid::account_of(own).unwrap_or_default(), &past))
}

/// Tells Steam a conversation has been read, up to its newest message.
pub fn mark_read(state: &AppState, buffer: &Buffer) -> Result<(), String> {
    let link = state.runtime.steam_link(&buffer.account_id).ok_or("this Steam account is not connected")?;
    let (_, friend) = people(state, buffer)?;
    let newest = state.store.get_backlog(&buffer.id, 0, 1).ok().and_then(|m| m.last().map(|m| m.ts)).filter(|t| *t > 0);
    let at = newest.and_then(|t| u32::try_from(t).ok()).unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as u32);
    ack(&link, friend, at).map_err(|e| e.to_string())
}

/// Catches up on what arrived while nobody was listening: for each
/// conversation Steam says has something this log does not, the newest page of
/// it. What was sent to us and not yet looked at anywhere is announced as news,
/// so it counts as unread and is notified; everything else - what we sent from
/// another client, what was read there - is only put in the log.
///
/// Returns how many unread conversations had no friend named yet to be read
/// into, so the caller can try again as the names arrive.
///
/// Run after a connection settles, and so on every reconnect as well as every
/// start: whatever the gap was, this is what fills it. A conversation whose log
/// is already up to date is left alone, and one with no log at all is left for
/// opening it, which asks for the same.
pub async fn catch_up(state: &AppState, link: &Link, account_id: &str, own: u32, roster: &Roster, shown: &Conversations) -> usize {
    let answer = match sessions(link, 0).await {
        Ok(a) => a,
        Err(e) => {
            tracing::info!("steam[{account_id}]: could not ask what was missed: {e}");
            return 0;
        }
    };
    let mut unnamed = 0;
    let own_id = steamid::steamid_of(own);
    // The recently active first, and only so many: a start must not spend its
    // first minute reading what was said to somebody years ago. What is left
    // is read when the conversation is opened, which asks for the same.
    let mut sessions = answer.message_sessions;
    sessions.sort_by_key(|s| std::cmp::Reverse(s.last_message.unwrap_or(0)));
    tracing::debug!("steam[{account_id}]: {} conversation(s) with messages, {} unread", sessions.len(), sessions.iter().filter(|s| s.unread_message_count.unwrap_or(0) > 0).count());
    sessions.truncate(CATCH_UP_CONVERSATIONS);
    for session in sessions {
        let Some(friend) = session.accountid_friend else { continue };
        let Some(name) = shown.name_of(friend) else {
            if session.unread_message_count.unwrap_or(0) > 0 {
                unnamed += 1;
            }
            continue;
        };
        let buffer_id = crate::model::buffer_id(account_id, name);
        let Some(buffer) = state.runtime.get_buffer(&buffer_id) else {
            tracing::debug!("steam[{account_id}]: no conversation {buffer_id} to read what was missed into");
            continue;
        };
        let newest_held = state.store.get_backlog(&buffer_id, 0, 1).ok().and_then(|m| m.last().map(|m| m.ts)).unwrap_or(0);
        let unread = session.unread_message_count.unwrap_or(0);
        let behind = newest_held > 0 && i64::from(session.last_message.unwrap_or(0)) > newest_held;
        tracing::debug!("steam[{account_id}]: {name}: {unread} unread, last message {}, held up to {newest_held}, behind: {behind}", session.last_message.unwrap_or(0));
        if unread == 0 && !behind {
            continue;
        }
        let (past, _) = match fetch(link, own_id, steamid::steamid_of(friend), None, PAGE).await {
            Ok(page) => page,
            Err(e) => {
                tracing::info!("steam[{account_id}]: could not catch up on {name}: {e}");
                continue;
            }
        };
        let last_view = i64::from(session.last_view.unwrap_or(0));
        let friend_name = roster.persona(friend).and_then(|p| p.name.clone()).unwrap_or_else(|| name.to_string());
        let (news, old): (Vec<Past>, Vec<Past>) = past
            .into_iter()
            .filter(|p| i64::from(p.timestamp) > newest_held)
            .partition(|p| p.account != own && i64::from(p.timestamp) > last_view);
        store_quietly(state, &buffer, own, &old);
        for p in &news {
            state.runtime.record_message_at(
                state,
                account_id,
                name,
                "dm",
                &friend_name,
                &p.text,
                false,
                "chat",
                None,
                Some(p.id()),
                false,
                roster.persona(friend).and_then(|x| x.avatar.clone()),
                p.embeds.clone(),
                Vec::new(),
                Some(steamid::text_of(p.account)),
                Some(i64::from(p.timestamp)),
                None,
                None,
            );
        }
        // Read elsewhere, so whatever this window counted is not waiting.
        if unread == 0 {
            state.events.emit("bufferRead", serde_json::json!({ "accountId": account_id, "bufferId": buffer_id }));
        }
    }
    unnamed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::simulated_daemon;
    use crate::backend::steam::friends::{Roster, REL_FRIEND};
    use crate::backend::steam::link::{self, fake::*};
    use crate::backend::steam::proto::*;
    use crate::backend::steam::wire::Packet;
    use prost::Message;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    const ACCOUNT: &str = "steam:tester";
    const ME: u32 = 22202;
    const FRIEND: u32 = 100;

    fn past(account: u32, ts: u32, ord: u32, text: &str) -> PastMessage {
        PastMessage { accountid: Some(account), timestamp: Some(ts), message: Some(text.into()), ordinal: Some(ord), reactions: vec![] }
    }

    /// A Steam holding a conversation, newest first as it answers, which also
    /// remembers what it was asked and what it was told.
    fn steam(history: Vec<PastMessage>, sessions: Vec<MessageSession>, seen: Arc<Mutex<Vec<Packet>>>) -> Script {
        Box::new(move |p| {
            seen.lock().unwrap().push(p.clone());
            let method = p.header.target_job_name.clone().unwrap_or_default();
            if method.contains("GetRecentMessages") {
                let ask = GetRecentMessagesRequest::decode(p.body.as_slice()).unwrap();
                // Older than the boundary, newest first, as many as asked for.
                let (t, o) = (ask.time_last.unwrap_or(u32::MAX), ask.ordinal_last.unwrap_or(0));
                let mut older: Vec<PastMessage> = history.iter().filter(|m| (m.timestamp.unwrap_or(0), m.ordinal.unwrap_or(0)) < (t, o)).cloned().collect();
                older.sort_by_key(|m| std::cmp::Reverse((m.timestamp.unwrap_or(0), m.ordinal.unwrap_or(0))));
                let count = ask.count.unwrap_or(50) as usize;
                let more = older.len() > count;
                older.truncate(count);
                Some(vec![answer(&p, 1, GetRecentMessagesResponse { messages: older, more_available: Some(more) }.encode_to_vec())])
            } else if method.contains("GetActiveMessageSessions") {
                Some(vec![answer(&p, 1, GetActiveSessionsResponse { message_sessions: sessions.clone(), timestamp: Some(1) }.encode_to_vec())])
            } else {
                Some(vec![])
            }
        })
    }

    async fn link_to(script: Script) -> Link {
        let url = serve(script).await;
        link::open_with("test", &url, Duration::from_secs(5)).await.unwrap().0
    }

    fn friends_list() -> (Roster, Conversations) {
        let mut roster = Roster::default();
        roster.apply_friends_list(&ClientFriendsList {
            bincremental: Some(false),
            friends: vec![FriendsListEntry { ulfriendid: Some(steamid::steamid_of(FRIEND)), efriendrelationship: Some(REL_FRIEND) }],
            ..Default::default()
        });
        roster.apply_persona(&ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(FRIEND)), player_name: Some("Gabe".into()), persona_state: Some(1), ..Default::default() }], ..Default::default() });
        (roster, Conversations::new(ACCOUNT))
    }

    fn held(state: &AppState) -> Vec<(String, String, bool)> {
        state.store.get_backlog(&crate::model::buffer_id(ACCOUNT, "Gabe"), 0, 200).unwrap().into_iter().map(|m| (m.id, m.body, m.is_own)).collect()
    }

    fn five() -> Vec<PastMessage> {
        vec![past(FRIEND, 100, 0, "one"), past(ME, 110, 0, "two"), past(FRIEND, 120, 0, "three"), past(FRIEND, 120, 1, "same second"), past(ME, 130, 0, "five")]
    }

    #[tokio::test]
    async fn history_comes_back_oldest_first_and_asks_the_way_steams_own_client_does() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let link = link_to(steam(five(), vec![], seen.clone())).await;
        let (newest, more) = fetch(&link, steamid::steamid_of(ME), steamid::steamid_of(FRIEND), None, 3).await.unwrap();
        assert_eq!(newest.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["three", "same second", "five"], "the newest three, oldest first");
        assert!(more);
        let ask = GetRecentMessagesRequest::decode(seen.lock().unwrap()[0].body.as_slice()).unwrap();
        assert_eq!((ask.steamid1, ask.steamid2, ask.count), (Some(steamid::steamid_of(ME)), Some(steamid::steamid_of(FRIEND)), Some(3)));
        assert_eq!((ask.rtime32_start_time, ask.time_last, ask.ordinal_last), (Some(0), Some(u32::MAX), Some(0)), "no lower bound, no upper one");

        // Paging: strictly older than a second and an ordinal within it.
        let (before, more) = fetch(&link, steamid::steamid_of(ME), steamid::steamid_of(FRIEND), Some((120, 0)), 3).await.unwrap();
        assert_eq!(before.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["one", "two"]);
        assert!(!more, "that was the start of it");
    }

    #[tokio::test]
    async fn a_message_with_nothing_in_it_or_nobody_behind_it_is_not_history() {
        let blank = vec![past(FRIEND, 1, 0, "  "), past(0, 2, 0, "nobody"), PastMessage { accountid: Some(FRIEND), timestamp: Some(3), message: None, ordinal: None, reactions: vec![] }, past(FRIEND, 4, 0, "kept ")];
        let link = link_to(steam(blank, vec![], Arc::default())).await;
        let (got, _) = fetch(&link, steamid::steamid_of(ME), steamid::steamid_of(FRIEND), None, 50).await.unwrap();
        assert_eq!(got, vec![Past { account: FRIEND, timestamp: 4, ordinal: 0, text: "kept".into(), reactions: vec![], embeds: vec![] }], "and trailing space is dropped");
    }

    #[tokio::test]
    async fn stored_history_is_the_same_message_as_one_heard_live() {
        let state = simulated_daemon("steam-history-dedupe");
        state.accounts.add_steam(crate::accounts::SteamAccountConfig { username: "tester".into(), steamid: steamid::steamid_of(ME), ..Default::default() }).unwrap();
        state.runtime.set_own_identity(ACCOUNT, "Me");
        let (roster, mut shown) = friends_list();
        shown.publish(&state, &roster);
        let buffer = state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).unwrap();
        let link = link_to(steam(five(), vec![], Arc::default())).await;
        state.runtime.set_steam_link(ACCOUNT, link);

        // One of them was heard live first, under the id a live message gets.
        crate::backend::steam::chat::record_incoming(
            &state, ACCOUNT, &mut friends_list().0, &mut shown, "Me",
            &crate::backend::steam::chat::Incoming { account: FRIEND, text: "three".into(), timestamp: 120, id: message_id(120, 0), from_us: false, embeds: Vec::new() },
        );
        assert_eq!(held(&state).len(), 1);

        let added = open(&state, &buffer).await.unwrap();
        assert_eq!(added, 4, "the four it did not have, not the one it did");
        let all = held(&state);
        assert_eq!(all.len(), 5);
        assert_eq!(all.iter().map(|m| m.1.as_str()).collect::<Vec<_>>().len(), 5);
        let mine: Vec<&str> = all.iter().filter(|m| m.2).map(|m| m.1.as_str()).collect();
        assert_eq!(mine.len(), 2, "what we sent is ours: {mine:?}");
        assert_eq!(open(&state, &buffer).await.unwrap(), 0, "and asking again adds nothing");
    }

    #[tokio::test]
    async fn scrolling_up_asks_for_what_is_older_and_not_for_what_is_there() {
        let state = simulated_daemon("steam-history-older");
        state.accounts.add_steam(crate::accounts::SteamAccountConfig { username: "tester".into(), steamid: steamid::steamid_of(ME), ..Default::default() }).unwrap();
        let (roster, mut shown) = friends_list();
        shown.publish(&state, &roster);
        let buffer = state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).unwrap();
        state.runtime.set_steam_link(ACCOUNT, link_to(steam(five(), vec![], Arc::default())).await);
        // Everything from the second of 120 on is on screen.
        assert_eq!(older(&state, &buffer, 120).await.unwrap(), 4, "everything up to and including that second is stored, whichever of it was shown");
        let texts: Vec<String> = held(&state).into_iter().map(|m| m.1).collect();
        assert_eq!(texts, ["one", "two", "three", "same second"]);
        assert!(older(&state, &buffer, -5).await.is_err());
    }

    #[tokio::test]
    async fn reading_is_told_to_steam_up_to_the_newest_message_held() {
        let state = simulated_daemon("steam-history-ack");
        state.accounts.add_steam(crate::accounts::SteamAccountConfig { username: "tester".into(), steamid: steamid::steamid_of(ME), ..Default::default() }).unwrap();
        let (roster, mut shown) = friends_list();
        shown.publish(&state, &roster);
        let buffer = state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        state.runtime.set_steam_link(ACCOUNT, link_to(steam(five(), vec![], seen.clone())).await);
        open(&state, &buffer).await.unwrap();
        mark_read(&state, &buffer).unwrap();
        for _ in 0..100 {
            if seen.lock().unwrap().iter().any(|p| p.header.target_job_name.as_deref() == Some("FriendMessages.AckMessage#1")) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let seen = seen.lock().unwrap();
        let ack = seen.iter().find(|p| p.header.target_job_name.as_deref() == Some("FriendMessages.AckMessage#1")).expect("an ack was sent");
        let body = AckMessage::decode(ack.body.as_slice()).unwrap();
        assert_eq!((body.steamid_partner, body.timestamp), (Some(steamid::steamid_of(FRIEND)), Some(130)));
        assert_eq!(ack.header.jobid_source, None, "a notification, which nothing waits on");
    }

    fn session(unread: u32, last_message: u32, last_view: u32) -> MessageSession {
        MessageSession { accountid_friend: Some(FRIEND), last_message: Some(last_message), last_view: Some(last_view), unread_message_count: Some(unread), notices: vec![] }
    }

    async fn caught_up(name: &str, held_before: Vec<PastMessage>, session: MessageSession) -> (AppState, Vec<serde_json::Value>, Vec<serde_json::Value>) {
        let state = simulated_daemon(name);
        state.accounts.add_steam(crate::accounts::SteamAccountConfig { username: "tester".into(), steamid: steamid::steamid_of(ME), ..Default::default() }).unwrap();
        state.runtime.set_own_identity(ACCOUNT, "Me");
        let (roster, mut shown) = friends_list();
        shown.publish(&state, &roster);
        let buffer = state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).unwrap();
        let link = link_to(steam(five(), vec![session], Arc::default())).await;
        store_quietly(&state, &buffer, ME, &held_before.iter().filter_map(|m| read(m, ME)).collect::<Vec<_>>());
        let mut rx = state.events.subscribe();
        let _ = catch_up(&state, &link, ACCOUNT, ME, &roster, &shown).await;
        let (mut said, mut read_events) = (vec![], vec![]);
        while let Ok(e) = rx.try_recv() {
            match e.name {
                "message" => said.push(e.data),
                "bufferRead" => read_events.push(e.data),
                _ => {}
            }
        }
        (state, said, read_events)
    }

    #[tokio::test]
    async fn what_arrived_while_away_and_was_not_read_anywhere_is_news() {
        // Held up to 110; Steam says three since, unread, nothing viewed since 110.
        let (state, said, read) = caught_up("steam-catchup-news", five()[..2].to_vec(), session(3, 130, 110)).await;
        let heard: Vec<&str> = said.iter().map(|m| m["body"].as_str().unwrap()).collect();
        assert_eq!(heard, ["three", "same second"], "what the friend sent after 110 is news; what we sent ('five') is not");
        assert!(read.is_empty(), "something is still unread");
        let all: Vec<String> = held(&state).into_iter().map(|m| m.1).collect();
        assert_eq!(all, ["one", "two", "three", "same second", "five"], "and all of it is in the log, in order");
    }

    #[tokio::test]
    async fn what_was_read_on_another_client_is_logged_and_clears_the_unread_count() {
        let (state, said, read) = caught_up("steam-catchup-read", five()[..2].to_vec(), session(0, 130, 130)).await;
        assert!(said.is_empty(), "read on the phone, so nothing to announce: {said:?}");
        assert_eq!(held(&state).len(), 5, "but in the log");
        assert_eq!(read.len(), 1, "and what this window counted is cleared");
        assert_eq!(read[0]["bufferId"], crate::model::buffer_id(ACCOUNT, "Gabe"));
    }

    #[tokio::test]
    async fn a_conversation_that_is_up_to_date_is_left_alone() {
        let (state, said, read) = caught_up("steam-catchup-current", five(), session(0, 130, 130)).await;
        assert!(said.is_empty() && read.is_empty());
        assert_eq!(held(&state).len(), 5);
    }

    #[tokio::test]
    async fn a_conversation_with_no_log_and_nothing_unread_waits_to_be_opened() {
        let (state, said, _) = caught_up("steam-catchup-unopened", vec![], session(0, 130, 130)).await;
        assert!(said.is_empty());
        assert!(held(&state).is_empty(), "years of history are not fetched for every friend at every start");
    }
}
