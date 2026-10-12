//! Against real Steam: two accounts that are friends, one message between them.
//!
//! Everything else here is tested against a Steam that is played by a script,
//! which can only prove moho agrees with what its author believed Steam does.
//! This is the check that it is true. It signs both accounts in, sends one
//! message from one to the other, and requires that the other hears it.
//!
//! Ignored, because it needs two accounts and the network, and because it
//! sends a real message to a real person. Run it on purpose:
//!
//! ```text
//! MOHO_STEAM_SENDER_USER=...    MOHO_STEAM_SENDER_ID=7656119...   MOHO_STEAM_SENDER_TOKEN=... \
//! MOHO_STEAM_RECEIVER_USER=...  MOHO_STEAM_RECEIVER_ID=7656119... MOHO_STEAM_RECEIVER_TOKEN=... \
//!   cargo test --release probe -- --ignored --nocapture
//! ```
//!
//! The tokens are the refresh tokens an account keeps in `accounts.toml` after
//! it has signed in once, so no password is ever put in an environment. The
//! two must already be friends. Messages go one way only, from the sender, and
//! the receiver only listens - use a throwaway account to send from.
//!
//! Each account has one session of its kind, so run this with those accounts
//! signed out of any moho that is open, or it takes their place.

use super::connect;
use super::history::Past;
use crate::accounts::SteamAccountConfig;
use crate::backend::kick::testkit::simulated_daemon;
use crate::runtime::ConnState;
use std::time::Duration;

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set; see the top of probe.rs"))
}

fn account(role: &str) -> SteamAccountConfig {
    SteamAccountConfig {
        username: var(&format!("MOHO_STEAM_{role}_USER")),
        steamid: var(&format!("MOHO_STEAM_{role}_ID")).parse().expect("the 64-bit Steam id"),
        refresh_token: var(&format!("MOHO_STEAM_{role}_TOKEN")),
        session: "chat".to_string(),
        ..Default::default()
    }
}

async fn within(seconds: u64, what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..seconds * 10 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("not within {seconds}s: {what}");
}

#[tokio::test]
#[ignore = "needs two real Steam accounts that are friends; sends one real message"]
async fn a_message_sent_by_one_account_is_heard_by_the_other() {
    // The daemon does this once at start; a test is its own process.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let (sender_id, receiver_id) = (sender.account_id(), receiver.account_id());
    for config in [&sender, &receiver] {
        state.accounts.add_steam(config.clone()).unwrap();
        connect::spawn(state.clone(), config.clone());
    }

    within(60, &format!("both accounts to be signed in (now {:?} and {:?})", state.runtime.conn_state(&sender_id), state.runtime.conn_state(&receiver_id)), || {
        [&sender_id, &receiver_id].iter().all(|id| state.runtime.conn_state(id) == Some(ConnState::Connected))
    })
    .await;

    // Each learns who its friends are from Steam, which is what makes the
    // other a conversation. If it never does, they are not friends.
    let conversation = |account: &str, with: u64| {
        state.runtime.list_buffers().into_iter().find(|b| b.account_id == account && b.kind == "dm" && b.remote_id.as_deref() == Some(&with.to_string()))
    };
    within(60, "the sender to see the receiver as a friend", || conversation(&sender_id, receiver.steamid).is_some()).await;
    within(60, "the receiver to see the sender as a friend", || conversation(&receiver_id, sender.steamid).is_some()).await;

    let text = format!("moho probe {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    let outgoing = conversation(&sender_id, receiver.steamid).unwrap();
    crate::backend::steam::send_message(&state, &outgoing, &text).await.expect("Steam takes the message");

    let incoming = conversation(&receiver_id, sender.steamid).unwrap();
    within(30, "the receiver to hear it", || {
        state.store.get_backlog(&incoming.id, 0, 50).map(|m| m.iter().any(|m| m.body == text && !m.is_own)).unwrap_or(false)
    })
    .await;
    let sent = state.store.get_backlog(&outgoing.id, 0, 50).unwrap();
    assert!(sent.iter().any(|m| m.body == text && m.is_own), "and the sender kept its own copy");

    for id in [&sender_id, &receiver_id] {
        if let Some(link) = state.runtime.take_steam_link(id) {
            crate::backend::steam::sign_off(link);
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// Paging back through a real conversation: the boundary Steam is given means
/// "older than this", and what comes back is in order and does not repeat what
/// was already had. Needs a conversation with at least seven messages in it.
#[tokio::test]
#[ignore = "needs two real Steam accounts that are friends, with at least seven messages between them"]
async fn history_pages_back_without_gaps_or_repeats() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe-history");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    let link = state.runtime.steam_link(&id).expect("a connection");

    let (newest, more) = super::history::fetch(&link, sender.steamid, receiver.steamid, None, 3).await.unwrap();
    assert_eq!(newest.len(), 3, "the newest three");
    assert!(more, "and Steam says there are more");
    assert!(newest.windows(2).all(|w| (w[0].timestamp, w[0].ordinal) <= (w[1].timestamp, w[1].ordinal)), "oldest first");

    let edge = (newest[0].timestamp, newest[0].ordinal);
    let (before, _) = super::history::fetch(&link, sender.steamid, receiver.steamid, Some(edge), 3).await.unwrap();
    println!("newest {:?}\nolder than {:?}: {:?}", newest.iter().map(Past::id).collect::<Vec<_>>(), edge, before.iter().map(Past::id).collect::<Vec<_>>());
    assert_eq!(before.len(), 3, "the three before those");
    assert!(before.iter().all(|p| (p.timestamp, p.ordinal) < edge), "strictly older than the boundary");
    let ids: std::collections::HashSet<String> = newest.iter().chain(&before).map(Past::id).collect();
    assert_eq!(ids.len(), 6, "no message twice");

    // And with the ordinal left open, as scrolling up asks, the boundary's own
    // second is included rather than skipped: what is already stored is
    // recognised by id, so over-asking is harmless and under-asking is not.
    let (loose, _) = super::history::fetch(&link, sender.steamid, receiver.steamid, Some((edge.0, u32::MAX)), 3).await.unwrap();
    println!("older than ({}, MAX): {:?}", edge.0, loose.iter().map(Past::id).collect::<Vec<_>>());
    assert!(loose.iter().all(|p| p.timestamp <= edge.0));

    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// What reading, typing and being away do between two real accounts: a message
/// sent while the other is signed out is there when it signs in, a notice that
/// somebody is writing is heard, and reading tells Steam so.
#[tokio::test]
#[ignore = "needs two real Steam accounts that are friends; sends one real message"]
async fn missed_messages_typing_and_reading_work_between_two_real_accounts() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam=debug").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-reading");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let (sender_id, receiver_id) = (sender.account_id(), receiver.account_id());
    let conversation = |account: &str, with: u64| {
        state.runtime.list_buffers().into_iter().find(|b| b.account_id == account && b.kind == "dm" && b.remote_id.as_deref() == Some(&with.to_string()))
    };

    // The receiver is not there when this is said.
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the sender to be signed in", || state.runtime.conn_state(&sender_id) == Some(ConnState::Connected)).await;
    within(60, "the sender to know the receiver", || conversation(&sender_id, receiver.steamid).is_some()).await;
    let said = format!("moho probe missed {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    crate::backend::steam::send_message(&state, &conversation(&sender_id, receiver.steamid).unwrap(), &said).await.expect("Steam takes the message");

    // Now it arrives, and finds what it missed.
    state.accounts.add_steam(receiver.clone()).unwrap();
    let mut events = state.events.subscribe();
    connect::spawn(state.clone(), receiver.clone());
    within(60, "the receiver to be signed in", || state.runtime.conn_state(&receiver_id) == Some(ConnState::Connected)).await;
    within(60, "the receiver to know the sender", || conversation(&receiver_id, sender.steamid).is_some()).await;
    // Looked for afresh each time: a friend is called by their number until
    // Steam says their name, and the conversation moves when it does.
    let theirs_now = || conversation(&receiver_id, sender.steamid).expect("the conversation");
    let has = |text: &str| state.store.get_backlog(&theirs_now().id, 0, 100).map(|m| m.iter().any(|m| m.body == text && !m.is_own)).unwrap_or(false);
    for _ in 0..200 {
        if has(&said) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if has(&said) {
        println!("the missed message was caught up on at sign-in");
    } else {
        // Read already somewhere else (an open official client marks it so),
        // which leaves it for the conversation to be opened.
        let held = super::history::sessions(&state.runtime.steam_link(&receiver_id).unwrap(), 0).await.expect("the sessions");
        let mine = held.message_sessions.iter().find(|s| s.accountid_friend == super::steamid::account_of(sender.steamid));
        println!("not caught up at sign-in; Steam's session for it: {mine:?}; asking for it as opening the conversation does");
        super::history::open(&state, &theirs_now()).await.expect("opening the conversation");
        assert!(has(&said), "the message the receiver missed is in its history");
    }

    // A notice that somebody is writing.
    while events.try_recv().is_ok() {}
    let link = state.runtime.steam_link(&sender_id).expect("the sender's connection");
    super::chat::send_typing(&link, &receiver.steamid.to_string());
    let mut heard = false;
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    for _ in 0..100 {
        loop {
            match events.try_recv() {
                Ok(e) => {
                    *seen.entry(e.name.to_string()).or_default() += 1;
                    if e.name == "typing" {
                        heard |= e.data["bufferId"] == theirs_now().id.as_str();
                    }
                }
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
        if heard {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(heard, "the receiver hears that the sender is typing; events seen: {seen:?}");

    // Reading is told to Steam, which then holds nothing unread for it.
    super::history::mark_read(&state, &theirs_now()).expect("telling Steam it was read");
    let receiving = state.runtime.steam_link(&receiver_id).expect("the receiver's connection");
    let mut unread = u32::MAX;
    for _ in 0..50 {
        let answer = super::history::sessions(&receiving, 0).await.expect("the sessions");
        unread = answer.message_sessions.iter().find(|s| s.accountid_friend == super::steamid::account_of(sender.steamid)).and_then(|s| s.unread_message_count).unwrap_or(0);
        if unread == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(unread, 0, "Steam counts nothing unread once it has been told");

    for id in [&sender_id, &receiver_id] {
        if let Some(link) = state.runtime.take_steam_link(id) {
            crate::backend::steam::sign_off(link);
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// Formatting and emoticons through the real thing: what is written as tags
/// comes out the other side as what moho draws, and the account's emoticons
/// are known.
#[tokio::test]
#[ignore = "needs two real Steam accounts that are friends; sends one real message"]
async fn formatting_and_emoticons_arrive_as_what_was_written() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam=debug").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-formatting");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let (sender_id, receiver_id) = (sender.account_id(), receiver.account_id());
    for config in [&sender, &receiver] {
        state.accounts.add_steam(config.clone()).unwrap();
        connect::spawn(state.clone(), config.clone());
    }
    within(60, "both accounts to be signed in", || [&sender_id, &receiver_id].iter().all(|id| state.runtime.conn_state(id) == Some(ConnState::Connected))).await;
    let conversation = |account: &str, with: u64| {
        state.runtime.list_buffers().into_iter().find(|b| b.account_id == account && b.kind == "dm" && b.remote_id.as_deref() == Some(&with.to_string()))
    };
    within(60, "the friends to know each other", || conversation(&sender_id, receiver.steamid).is_some() && conversation(&receiver_id, sender.steamid).is_some()).await;
    within(30, "the sender's emoticons", || !state.runtime.emoji_catalogue(&sender_id).is_empty()).await;
    let have = &state.runtime.emoji_catalogue(&sender_id)[0].emoji;
    println!("{} emoticon(s); first {:?}", have.len(), have.iter().take(3).map(|e| e.name.as_str()).collect::<Vec<_>>());
    assert!(!have.is_empty());

    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    // Steam takes no formatting - it shows [b] as the characters - but it does take
    // emoticons and makes a link of an address.
    let typed = format!("probe {stamp} [b]not bold[/b] see https://example.com/x :steamhappy: and \\[emoticon]plain[/emoticon]");
    let outgoing = conversation(&sender_id, receiver.steamid).unwrap();
    crate::backend::steam::send_message(&state, &outgoing, &typed).await.expect("Steam takes the message");

    let body = |id: &str| state.store.get_backlog(id, 0, 50).unwrap().into_iter().find(|m| m.body.contains(&format!("probe {stamp}"))).map(|m| m.body);
    let mut theirs = None;
    for _ in 0..100 {
        theirs = body(&conversation(&receiver_id, sender.steamid).unwrap().id);
        if theirs.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let ours = body(&outgoing.id);
    println!("sender logged:   {ours:?}\nreceiver logged: {theirs:?}");
    let wanted = [
        "[steam:emote:steamhappy]",
        "https://example.com/x",
        // The literal brackets stay literal, and are not tags to the window.
        "[\u{200b}b]not bold[\u{200b}/b]",
        "[\u{200b}emoticon]plain[\u{200b}/emoticon]",
    ];
    for body in [&theirs, &ours] {
        let body = body.as_deref().expect("the message is logged on both sides");
        for piece in wanted {
            assert!(body.contains(piece), "{piece:?} is in {body:?}");
        }
    }
    assert_eq!(ours, theirs, "both sides hold the same thing");

    for id in [&sender_id, &receiver_id] {
        if let Some(link) = state.runtime.take_steam_link(id) {
            crate::backend::steam::sign_off(link);
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// Which tags Steam's friend chat keeps. Prints, for each tag sent alone, what
/// Steam stored; asserts nothing, since it is a survey of the service.
#[tokio::test]
#[ignore = "survey; needs two real Steam accounts that are friends; sends several real messages"]
async fn which_tags_a_friend_message_keeps() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam::chat=debug").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-tags");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    within(60, "the friend", || state.runtime.list_buffers().iter().any(|b| b.account_id == id && b.remote_id.as_deref() == Some(&receiver.steamid.to_string()))).await;
    let link = state.runtime.steam_link(&id).unwrap();
    for text in [
        "t1 [code]code[/code]",
        "t2 [quote]quote[/quote]",
        "t3 [noparse][b]x[/b][/noparse]",
        "t4 [h1]head[/h1]",
        "t5 [spoiler]hidden[/spoiler]",
        "t6 [sticker type=\"Cats\" limit=\"1\"]CatSurprise[/sticker]",
        "t7 [img]https://example.com/a.png[/img]",
        "t8 *star* _under_ ~~tilde~~ ||bars||",
        "t9 [list][*]a[*]b[/list]",
        "t10 [url]https://example.com[/url]",
    ] {
        let _ = super::chat::send(&link, &receiver.steamid.to_string(), text).await;
    }
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}
