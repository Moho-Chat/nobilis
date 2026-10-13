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

/// A reaction between two real accounts: put on, heard by the other, found
/// again in history, and taken off.
#[tokio::test]
#[ignore = "needs two real Steam accounts that are friends; sends one real message and reacts to it"]
async fn a_reaction_is_heard_remembered_and_taken_back() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam=debug").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-reactions");
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
    // Let the friends be named, so the conversations are where they will stay.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let said = format!("moho probe reaction {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs());
    let ours = conversation(&sender_id, receiver.steamid).unwrap();
    crate::backend::steam::send_message(&state, &ours, &said).await.expect("Steam takes the message");
    let id_of = |buffer: &str| state.store.get_backlog(buffer, 0, 50).unwrap().into_iter().find(|m| m.body == said).map(|m| m.id);
    let msg_id = id_of(&ours.id).expect("the sender logged it");
    within(30, "the receiver to hear it", || id_of(&conversation(&receiver_id, sender.steamid).unwrap().id).is_some()).await;

    let token = "[steam:emote:steamhappy]";
    let reactions = |buffer: &str| state.store.get_message(buffer, &msg_id).unwrap().map(|m| m.reactions.iter().map(|r| (r.emoji.clone(), r.count, r.me)).collect::<Vec<_>>()).unwrap_or_default();
    super::reactions::toggle(&state, &ours, &msg_id, token, true).await.expect("Steam takes the reaction");
    assert_eq!(reactions(&ours.id), [(token.to_string(), 1, true)], "held by the sender");
    within(15, "the receiver to hear the reaction", || reactions(&conversation(&receiver_id, sender.steamid).unwrap().id) == [(token.to_string(), 1, false)]).await;

    // Found again in history, which is what a conversation opened later reads.
    let theirs = conversation(&receiver_id, sender.steamid).unwrap();
    state.store.set_reactions(&theirs.id, &msg_id, &[]).unwrap();
    super::history::open(&state, &theirs).await.expect("history");
    assert_eq!(reactions(&theirs.id), [(token.to_string(), 1, false)], "history carries it");

    super::reactions::toggle(&state, &ours, &msg_id, token, false).await.expect("Steam takes it back");
    assert!(reactions(&ours.id).is_empty());
    within(15, "the receiver to hear it taken back", || reactions(&conversation(&receiver_id, sender.steamid).unwrap().id).is_empty()).await;

    for id in [&sender_id, &receiver_id] {
        if let Some(link) = state.runtime.take_steam_link(id) {
            crate::backend::steam::sign_off(link);
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// What an account keeps about a friend, and a friend's card, against the real
/// thing: a nickname set is what the friend is then called, hears itself back as
/// a change, and goes when taken off; and the card has Steam's answers.
#[tokio::test]
#[ignore = "needs two real Steam accounts that are friends; changes the sender's nickname for the receiver and back"]
async fn nicknames_notifications_and_a_friends_card_work_against_steam() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam=info").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-prefs");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the sender to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    let conversation = || state.runtime.list_buffers().into_iter().find(|b| b.account_id == id && b.kind == "dm" && b.remote_id.as_deref() == Some(&receiver.steamid.to_string()));
    within(60, "the friend", || conversation().is_some()).await;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let original = conversation().unwrap().name;
    let link = state.runtime.steam_link(&id).unwrap();
    let number = super::steamid::account_of(receiver.steamid).unwrap();

    // Their card: Steam's answers, in the shape the window draws.
    let card = super::profile::build(&state, &id, &conversation().unwrap().id, &original, "").await;
    let labels: Vec<&str> = card["extra"].as_array().map(|a| a.iter().filter_map(|e| e["label"].as_str()).collect()).unwrap_or_default();
    println!("card: id {} status {} labels {labels:?} created {}", card["id"], card["status"], card.get("createdTs").is_some());
    assert_eq!(card["id"], receiver.steamid.to_string());
    assert!(card.get("createdTs").is_some() || labels.contains(&"Steam level"), "Steam answered something about them");

    // A nickname: set, held by Steam, and the conversation is called it.
    super::prefs::set_nickname(&link, number, "probe-nick").await.expect("Steam takes a nickname");
    let held = super::prefs::load(&link).await.expect("preferences");
    assert_eq!(held.iter().find(|p| p.accountid == Some(number)).and_then(|p| p.nickname.as_deref()), Some("probe-nick"));
    within(15, "the conversation to take the new name", || conversation().is_some_and(|b| b.name == "probe-nick")).await;

    // Muted and back, without losing the nickname.
    super::prefs::set_muted(&link, number, true).await.expect("mute");
    let held = super::prefs::load(&link).await.unwrap();
    let mine = held.iter().find(|p| p.accountid == Some(number)).unwrap();
    assert_eq!((mine.nickname.as_deref(), mine.notifications_showmessages), (Some("probe-nick"), Some(super::friends::NOTIFY_NEVER)));
    super::prefs::set_muted(&link, number, false).await.expect("unmute");

    // And taken off, so it is as it was.
    super::prefs::set_nickname(&link, number, "").await.expect("clear");
    within(15, "the conversation to go back to their name", || conversation().is_some_and(|b| b.name == original)).await;
    let held = super::prefs::load(&link).await.unwrap();
    assert!(held.iter().find(|p| p.accountid == Some(number)).map(|p| p.nickname.as_deref().unwrap_or("").is_empty() && p.notifications_showmessages.unwrap_or(0) == 0).unwrap_or(true), "left as it was");

    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// The account's own emoticons and stickers, asked for in one question; and a
/// sticker sent with the command Steam's own client sends.
#[tokio::test]
#[ignore = "needs two real Steam accounts that are friends; sends one real sticker if the sender has any"]
async fn the_accounts_emoticons_and_stickers_are_listed_and_a_sticker_is_sent() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam=info,nobilis::backend::steam::emoticons=debug,nobilis::backend::steam::connect=debug").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-stickers");
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
    within(60, "the friends", || conversation(&sender_id, receiver.steamid).is_some() && conversation(&receiver_id, sender.steamid).is_some()).await;
    within(30, "the lists", || state.runtime.emoji_catalogue(&sender_id).len() == 2).await;
    let sources = state.runtime.emoji_catalogue(&sender_id);
    let by = |id: &str| sources.iter().find(|s| s.id == id).map(|s| s.emoji.iter().map(|e| e.name.clone()).collect::<Vec<_>>()).unwrap_or_default();
    let (emoticons, stickers) = (by("steam"), by(super::emoticons::STICKERS));
    println!("{} emoticon(s) {:?}; {} sticker(s) {:?}", emoticons.len(), emoticons.iter().take(4).collect::<Vec<_>>(), stickers.len(), stickers.iter().take(4).collect::<Vec<_>>());
    assert!(!emoticons.is_empty(), "an account has the free emoticons at least");
    for (who, id) in [("sender", &sender_id), ("receiver", &receiver_id)] {
        let all = state.runtime.emoji_catalogue(id);
        println!("{who}: {:?}", all.iter().map(|s| (s.id.clone(), s.emoji.len())).collect::<Vec<_>>());
    }

    if let Some(name) = stickers.first() {
        let ours = conversation(&sender_id, receiver.steamid).unwrap();
        let before = state.store.get_backlog(&ours.id, 0, 5).unwrap().len();
        crate::backend::steam::send_message(&state, &ours, &format!("/sticker {name}")).await.expect("Steam takes a sticker");
        let log = state.store.get_backlog(&ours.id, 0, 50).unwrap();
        println!("logged after the sticker: {:?}", log.last().map(|m| m.body.clone()));
        assert!(log.len() > before);
        within(30, "the other side to hear it", || state.store.get_backlog(&conversation(&receiver_id, sender.steamid).unwrap().id, 0, 5).unwrap().iter().any(|m| m.body.contains("sticker"))).await;
    } else {
        println!("the sender has no stickers to send");
    }
    for id in [&sender_id, &receiver_id] {
        if let Some(link) = state.runtime.take_steam_link(id) {
            crate::backend::steam::sign_off(link);
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// Files between two real accounts: a picture, a spoilered picture and a short
/// video go through Steam's website, and the other account hears each as a
/// message. Prints what Steam wrote into the conversation, which is the one
/// thing nothing else can say. Needs `ffmpeg` to make the video.
#[tokio::test]
#[ignore = "needs two real Steam accounts that are friends; sends real files"]
async fn pictures_and_videos_are_sent_and_heard() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam::connect=debug,nobilis::backend::steam::web=debug").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-files");
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
    within(60, "the friend", || conversation(&sender_id, receiver.steamid).is_some() && conversation(&receiver_id, sender.steamid).is_some()).await;

    let dir = std::env::temp_dir().join(format!("moho-steam-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("probe.png");
    image::RgbaImage::from_pixel(96, 64, image::Rgba([200, 60, 60, 255])).save(&png).unwrap();
    let video = dir.join("probe.webm");
    let made = std::process::Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=128x96:rate=10", "-t", "1", video.to_str().unwrap()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    let pdf = dir.join("paper.pdf");
    std::fs::write(&pdf, b"%PDF-1.4").unwrap();

    let outgoing = conversation(&sender_id, receiver.steamid).unwrap();
    let path = |p: &std::path::Path| p.to_str().unwrap().to_string();
    let refused = crate::backend::steam::send_files(&state, &outgoing, "", &[(path(&pdf), false)], None).await;
    println!("a pdf: {refused:?}");
    assert!(refused.is_err());

    let mut files = vec![(path(&png), false), (path(&png), true)];
    if made {
        files.push((path(&video), false));
    }
    let sent = crate::backend::steam::send_files(&state, &outgoing, "moho files probe", &files, None).await;
    println!("sent: {sent:?}");
    sent.expect("Steam takes the files");

    let incoming = conversation(&receiver_id, sender.steamid).unwrap();
    let wanted = files.len() + 1;
    within(60, "the receiver to hear them all", || state.store.get_backlog(&incoming.id, 0, 50).map(|m| m.iter().filter(|m| !m.is_own).count() >= wanted).unwrap_or(false)).await;
    for m in state.store.get_backlog(&incoming.id, 0, 50).unwrap().iter().filter(|m| !m.is_own) {
        println!("RECEIVED {:?}", m.body);
    }
    for m in state.store.get_backlog(&outgoing.id, 0, 50).unwrap().iter().filter(|m| m.is_own) {
        println!("SENDER HAS {:?}", m.body);
    }
    std::fs::remove_dir_all(&dir).ok();
    for id in [&sender_id, &receiver_id] {
        if let Some(link) = state.runtime.take_steam_link(id) {
            crate::backend::steam::sign_off(link);
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// Whether Steam would take a file from an account, asked and not sent: the
/// first step of an upload is a question and leaves nothing behind.
#[tokio::test]
#[ignore = "needs a real Steam account; sends no message"]
async fn steam_says_whether_an_account_may_upload() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam::web=debug").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-may-upload");
    for role in ["SENDER", "RECEIVER"] {
        let config = account(role);
        let id = config.account_id();
        state.accounts.add_steam(config.clone()).unwrap();
        connect::spawn(state.clone(), config.clone());
        within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
        let link = state.runtime.steam_link(&id).unwrap();
        println!("{role}: {:?}", super::web::may_upload(&link, &id, config.steamid, &config.refresh_token).await);
        if let Some(link) = state.runtime.take_steam_link(&id) {
            crate::backend::steam::sign_off(link);
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// Prints the newest messages of a conversation as Steam holds them, tags and
/// all, for finding out how it writes something nothing has described.
#[tokio::test]
#[ignore = "needs a real Steam account and its friend; sends nothing"]
async fn the_newest_messages_as_steam_writes_them() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam::history=debug").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-raw");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    let link = state.runtime.steam_link(&id).unwrap();
    let _ = super::history::fetch(&link, sender.steamid, receiver.steamid, None, 6).await;
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// Whether a session stays up: signs the sender in and watches it for as long
/// as `MOHO_STEAM_SOAK_SECS` says (five minutes by default), counting how
/// often it is lost. What the daemon logs about each ending is printed.
#[tokio::test]
#[ignore = "needs a real Steam account; sends nothing, and takes minutes"]
async fn a_session_stays_connected() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam=info").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-soak");
    let sender = account("SENDER");
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    let secs: u64 = std::env::var("MOHO_STEAM_SOAK_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    let (mut lost, mut last) = (0, state.runtime.conn_state(&id));
    for _ in 0..secs * 2 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = state.runtime.conn_state(&id);
        if now != last {
            println!("{:?} -> {:?}", last, now);
            if last == Some(ConnState::Connected) {
                lost += 1;
            }
            last = now;
        }
    }
    println!("lost the connection {lost} time(s) in {secs}s; now {last:?}");
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// How Steam stores a link a person sends: as written, or made into a link tag,
/// or a card. One message to the friend for each, from the sender alone.
#[tokio::test]
#[ignore = "needs a real Steam account and its friend; sends a few real messages"]
async fn links_are_stored_by_steam_as_these() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam::chat=debug").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-links");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    within(60, "the friend", || state.runtime.list_buffers().iter().any(|b| b.account_id == id && b.remote_id.as_deref() == Some(&receiver.steamid.to_string()))).await;
    let link = state.runtime.steam_link(&id).unwrap();
    for text in [
        "https://upload.wikimedia.org/wikipedia/commons/4/47/PNG_transparency_demonstration_1.png",
        "https://x.salastil.com/nico_avchd/status/2109815235469082675#m",
    ] {
        println!("SENT {text:?} -> {:?}", super::chat::send(&link, &receiver.steamid.to_string(), text).await.map(|s| s.shown));
        tokio::time::sleep(Duration::from_secs(20)).await;
    }
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// Our own presence against real Steam, through the same calls the window makes:
/// the account is set Online, Idle, Invisible and Online, then moved to idle
/// and back by the idle detection. Prints a marker at each step, so another
/// session can be watched for what it sees. Nothing is sent to anyone.
#[tokio::test]
#[ignore = "needs a real Steam account; changes only its presence; takes a minute"]
async fn our_presence_is_set_and_idle_detection_moves_it() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe-presence");
    let sender = account("SENDER");
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    let subscriptions: crate::rpc::Subscriptions = Default::default();
    let call = |method: &'static str, params: serde_json::Value| {
        let (state, subscriptions) = (state.clone(), &subscriptions);
        async move { crate::rpc::methods::dispatch(&state, method, &params, subscriptions).await }
    };
    let wait = std::env::var("MOHO_STEP_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(10u64);
    let stamp = |what: &str| println!("STEP {} {what}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() % 86400);
    tokio::time::sleep(Duration::from_secs(wait)).await;
    for status in ["online", "idle", "invisible", "online"] {
        stamp(&format!("set {status}"));
        let (result, error) = call("setAccountStatus", serde_json::json!({ "accountId": id, "status": status })).await;
        println!("  -> {result:?} {error:?}");
        tokio::time::sleep(Duration::from_secs(wait)).await;
    }
    stamp("idle detection: the person has been away");
    let (result, error) = call("setUserIdle", serde_json::json!({ "idle": true })).await;
    println!("  -> {result:?} {error:?}");
    tokio::time::sleep(Duration::from_secs(wait)).await;
    stamp("idle detection: the person is back");
    let (result, error) = call("setUserIdle", serde_json::json!({ "idle": false })).await;
    println!("  -> {result:?} {error:?}");
    tokio::time::sleep(Duration::from_secs(wait)).await;
    // And a status chosen by the person is not moved by being away.
    stamp("set invisible, then away: should stay invisible");
    let _ = call("setAccountStatus", serde_json::json!({ "accountId": id, "status": "invisible" })).await;
    tokio::time::sleep(Duration::from_secs(wait)).await;
    let _ = call("setUserIdle", serde_json::json!({ "idle": true })).await;
    tokio::time::sleep(Duration::from_secs(wait)).await;
    let _ = call("setUserIdle", serde_json::json!({ "idle": false })).await;
    let _ = call("setAccountStatus", serde_json::json!({ "accountId": id, "status": "online" })).await;
    stamp("back online; done");
    tokio::time::sleep(Duration::from_secs(wait)).await;
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// A friend's name history, read through the website as the signed-in account.
/// Reads only.
#[tokio::test]
#[ignore = "needs a real Steam account and its friend; sends nothing"]
async fn a_friends_name_history_is_read() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe-aliases");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    let link = state.runtime.steam_link(&id).unwrap();
    let got = super::web::aliases(&link, &id, sender.steamid, &sender.refresh_token, receiver.steamid).await;
    println!("ALIASES {got:?}");
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// The account's friend groups against real Steam: what it says at sign-in, and
/// a group made, filled, renamed, emptied and deleted - one of the sender's own,
/// made for this and gone at the end.
#[tokio::test]
#[ignore = "needs a real Steam account and its friend; changes only that account's friend groups"]
async fn friend_groups_are_read_and_changed() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let _ = tracing_subscriber::fmt().with_env_filter("nobilis::backend::steam=info").with_test_writer().try_init();
    let state = simulated_daemon("steam-probe-groups");
    let (sender, receiver) = (account("SENDER"), account("RECEIVER"));
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    tokio::time::sleep(Duration::from_secs(6)).await;
    println!("AT SIGN-IN {}", super::groups::held(&id).to_json());
    let link = state.runtime.steam_link(&id).unwrap();
    let friend = receiver.steamid;
    use super::groups::{change, Change};
    // Between two sign-ins: leave a group behind, or take the ones this left.
    if std::env::var("MOHO_GROUP_CLEAN").is_ok() {
        for (group, name) in super::groups::held(&id).names_for_probe() {
            if name.starts_with("moho probe") {
                println!("CLEAN {name} {:?}", change(&state, &link, &id, sender.steamid, Change::Delete { group }).await);
            }
        }
        if let Some(link) = state.runtime.take_steam_link(&id) {
            crate::backend::steam::sign_off(link);
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        return;
    }
    if std::env::var("MOHO_GROUP_KEEP").is_ok() {
        let group = change(&state, &link, &id, sender.steamid, Change::Create { name: "moho probe kept".into(), friends: vec![friend] }).await.unwrap().unwrap();
        println!("KEPT {group}");
        tokio::time::sleep(Duration::from_secs(3)).await;
        if let Some(link) = state.runtime.take_steam_link(&id) {
            crate::backend::steam::sign_off(link);
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        return;
    }
    let made = change(&state, &link, &id, sender.steamid, Change::Create { name: "moho probe".into(), friends: vec![] }).await;
    println!("CREATE {made:?}");
    let group = made.unwrap().expect("a group number");
    println!("ADD {:?}", change(&state, &link, &id, sender.steamid, Change::Add { group, friend }).await);
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("HELD {}", super::groups::held(&id).to_json());
    println!("RENAME {:?}", change(&state, &link, &id, sender.steamid, Change::Rename { group, name: "moho probe 2".into() }).await);
    println!("REMOVE {:?}", change(&state, &link, &id, sender.steamid, Change::Remove { group, friend }).await);
    println!("DELETE {:?}", change(&state, &link, &id, sender.steamid, Change::Delete { group }).await);
    tokio::time::sleep(Duration::from_secs(3)).await;
    println!("AT END {}", super::groups::held(&id).to_json());
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// What Steam's website says about the signed-in account's invite links and
/// session: the shape only, never a token. Reads only.
#[tokio::test]
#[ignore = "needs a real Steam account; changes nothing"]
async fn the_websites_account_answers_look_like_this() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe-web-shape");
    let sender = account("SENDER");
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    let link = state.runtime.steam_link(&id).unwrap();
    let (http, cookie, session_id) = super::web::login(&link, &id, sender.steamid, &sender.refresh_token).await.unwrap();
    fn shape(v: &serde_json::Value, depth: usize) -> String {
        match v {
            serde_json::Value::Object(o) if depth < 3 => format!("{{{}}}", o.iter().map(|(k, v)| format!("{k}:{}", shape(v, depth + 1))).collect::<Vec<_>>().join(", ")),
            serde_json::Value::Array(a) => format!("[{} x {}]", a.len(), a.first().map(|f| shape(f, depth + 1)).unwrap_or_default()),
            serde_json::Value::String(s) => format!("str({})", s.len()),
            serde_json::Value::Number(_) => "num".into(),
            serde_json::Value::Bool(b) => format!("{b}"),
            _ => "null".into(),
        }
    }
    // One link, made and looked at, on the sender's own account.
    let form = reqwest::multipart::Form::new().text("sessionid", session_id.clone()).text("steamid_user", sender.steamid.to_string()).text("duration", "2592000");
    let made = http.post("https://steamcommunity.com/invites/ajaxcreate").header("Cookie", &cookie).header("Accept-Encoding", "identity").multipart(form).send().await.unwrap();
    let made_text = made.text().await.unwrap_or_default();
    let made_value: serde_json::Value = serde_json::from_str(&made_text).unwrap_or(serde_json::Value::Null);
    println!("SHAPE create -> {}", shape(&made_value, 0));
    println!("INVITE keys {:?}", made_value["invite"].as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()));
    println!("INVITE url {:?} token_len {:?}", made_value["invite"]["invite_url"].as_str().map(|u| u.split("/p/").next().unwrap_or("").to_string()), made_value["invite"]["invite_token"].as_str().map(str::len));
    let token = made_value["invite"]["invite_token"].as_str().unwrap_or_default().to_string();
    for path in ["invites/ajaxrevoke", "invites/ajaxdelete", "invites/ajaxinvalidate"] {
        let form = reqwest::multipart::Form::new().text("sessionid", session_id.clone()).text("invite_token", token.clone()).text("steamid_user", sender.steamid.to_string());
        let r = http.post(format!("https://steamcommunity.com/{path}")).header("Cookie", &cookie).header("Accept-Encoding", "identity").multipart(form).send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        println!("REVOKE {path} -> {status} {}", text.chars().take(120).collect::<String>().replace(&token, "TOKEN"));
        if status.is_success() && text.contains("\"success\":1") {
            break;
        }
    }
    for url in [
        "https://steamcommunity.com/chat/clientjstoken".to_string(),
        format!("https://steamcommunity.com/invites/ajaxgetall?sessionid={session_id}"),
    ] {
        let r = http.get(&url).header("Cookie", &cookie).header("Accept-Encoding", "identity").send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap_or_default();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        println!("SHAPE {} -> {status} {}", url.split('?').next().unwrap(), shape(&value, 0));
    }
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// The account's own invite links against real Steam: listed, one made, listed,
/// and every one revoked at the end, so none is left behind. Nothing is sent to anybody.
#[tokio::test]
#[ignore = "needs a real Steam account; makes and revokes invite links on it"]
async fn invite_links_are_listed_made_and_revoked() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe-invites");
    let sender = account("SENDER");
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    let link = state.runtime.steam_link(&id).unwrap();
    use super::invites::*;
    let before = list(&link, &id, sender.steamid, &sender.refresh_token).await;
    println!("INVITES before {:?}", before.as_ref().map(|l| l.len()));
    let made = create(&link, &id, sender.steamid, &sender.refresh_token).await.unwrap();
    println!("INVITES made, link shape {} and the code is for this account: {}", made.link.replace(&made.token, "TOKEN"), account_of_link(&made.link) == super::steamid::account_of(sender.steamid));
    let after = list(&link, &id, sender.steamid, &sender.refresh_token).await.unwrap();
    println!("INVITES after {} (the new one is there: {}); tokens as first-3+len: made {}{} list {:?}", after.len(), after.iter().any(|l| l.token.eq_ignore_ascii_case(&made.token)), &made.token[..3], made.token.len(), after.iter().map(|l| format!("{}{}", &l.token[..3], l.token.len())).collect::<Vec<_>>());
    for l in &after {
        println!("INVITES revoke {:?}", revoke(&link, &id, sender.steamid, &sender.refresh_token, &l.token).await);
    }
    let end = list(&link, &id, sender.steamid, &sender.refresh_token).await.unwrap();
    println!("INVITES at the end {}", end.len());
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// Scan-to-sign-in against real Steam, as far as it can go without a phone: the
/// session begins, Steam gives the address a code carries, a picture of it is
/// written, and polling it is answered "not yet". Then the sign-in is cancelled.
/// Needs no account.
#[tokio::test]
#[ignore = "needs the network; signs nothing in"]
async fn a_scan_to_sign_in_begins_and_waits_for_a_phone() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe-qr");
    let key = crate::net::route::pending_key("steam");
    let servers = super::cm::discover(&key, 0).await.expect("Steam's server list");
    let urls: Vec<String> = servers.iter().take(4).map(|s| s.url()).collect();
    let (cancel, cancelled) = tokio::sync::mpsc::unbounded_channel::<String>();
    let task = tokio::spawn({
        let state = state.clone();
        async move { super::login::flow_qr(&state, "probe-qr", &urls, cancelled).await }
    });
    tokio::time::sleep(Duration::from_secs(8)).await;
    let path = crate::media_cache::transient_dir().join("moho-steam-qr-probe-qr.png");
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("QR picture written: {} bytes", size);
    drop(cancel);
    let ended = tokio::time::timeout(Duration::from_secs(40), task).await;
    println!("QR ended: {:?}", ended.map(|r| r.map(|o| o.map(|c| c.is_some()))));
    let _ = std::fs::remove_file(path);
    assert!(size > 500, "a picture was written");
}

/// What the account is, as Steam says it, and where it is signed in. Reads only.
#[tokio::test]
#[ignore = "needs a real Steam account; changes nothing"]
async fn the_account_facts_and_sessions_are_read() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe-facts");
    for role in ["SENDER"] {
        let config = account(role);
        let id = config.account_id();
        state.accounts.add_steam(config.clone()).unwrap();
        connect::spawn(state.clone(), config.clone());
        within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
        within(30, "the facts", || super::account::held(&id).privacy.is_some() || super::account::held(&id).family_view.is_some() || super::account::held(&id).filter.is_some()).await;
        println!("FACTS {role} {}", super::account::held(&id).to_json());
        let link = state.runtime.steam_link(&id).unwrap();
        let sessions = super::account::sessions(&link).await;
        println!("SESSIONS {role} {:?}", sessions.as_ref().map(|s| s.iter().map(|x| (x.description.clone(), x.platform, x.current, x.place.clone())).collect::<Vec<_>>()));
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let exp = super::account::expires_at(&config.refresh_token);
        println!("TOKEN expires in {:?} days; due for renewal: {}", exp.map(|e| (e as i64 - now as i64) / 86400), super::account::due_for_renewal(&config.refresh_token, now));
        if let Some(link) = state.runtime.take_steam_link(&id) {
            crate::backend::steam::sign_off(link);
        }
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

/// The chat groups an account is in, as Steam lists them, and what making one is answered with.
/// Run as the throwaway account only: it makes a group and leaves it again.
#[tokio::test]
#[ignore = "needs a real Steam account; makes and leaves a chat group on it"]
async fn chat_groups_are_listed_and_one_is_made() {
    use super::proto::*;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe-chatgroups");
    let sender = account("SENDER");
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    let link = state.runtime.steam_link(&id).unwrap();
    let mine: Result<GetMyChatRoomGroupsResponse, _> = link.call("ChatRoom.GetMyChatRoomGroups#1", &GetMyChatRoomGroupsRequest {}, true).await;
    println!("GROUPS mine {:#?}", mine);
    let made: Result<CreateChatGroupResponse, _> = link
        .call("ChatRoom.CreateChatRoomGroup#1", &CreateChatGroupRequest { name: Some("moho probe".into()), ..Default::default() }, true)
        .await;
    println!("GROUPS made {:#?}", made);
    if let Ok(made) = made {
        if let Some(group) = made.chat_group_id {
            let msg: Result<ChatSendResponse, _> = link
                .call("ChatRoom.SendChatMessage#1", &ChatSendRequest { chat_group_id: Some(group), chat_id: made.state.as_ref().and_then(|s| s.default_chat_id), message: Some("hello from the probe".into()), echo_to_sender: Some(true) }, true)
                .await;
            println!("GROUPS sent {:#?}", msg);
            let hist: Result<ChatHistoryResponse, _> = link
                .call("ChatRoom.GetMessageHistory#1", &ChatHistoryRequest { chat_group_id: Some(group), chat_id: made.state.as_ref().and_then(|s| s.default_chat_id), max_count: Some(20), ..Default::default() }, true)
                .await;
            println!("GROUPS history {:#?}", hist);
            let links: Result<CreateInviteLinkResponse, _> = link.call("ChatRoom.CreateInviteLink#1", &CreateInviteLinkRequest { chat_group_id: Some(group), seconds_valid: Some(3600), chat_id: None }, true).await;
            println!("GROUPS invite {:#?}", links);
            let left: Result<LeaveChatGroupResponse, _> = link.call("ChatRoom.LeaveChatRoomGroup#1", &LeaveChatGroupRequest { chat_group_id: Some(group) }, true).await;
            println!("GROUPS left {:?}", left);
        }
    }
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}

fn link_for_raw(state: &crate::state::AppState, id: &str) -> super::link::Link {
    state.runtime.steam_link(id).unwrap()
}

/// Chat groups end to end against real Steam, as the throwaway account: a group is made,
/// shows up as a server with a channel, is spoken in, gains a room, is renamed, has an
/// invite made and revoked, and is left. Every step is read back from what the window would see.
#[tokio::test]
#[ignore = "needs a real Steam account; makes, uses and leaves a chat group on it"]
async fn a_chat_group_is_made_used_and_left() {
    use super::chatgroups::{self as groups, Action};
    use super::proto::*;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let state = simulated_daemon("steam-probe-groups-e2e");
    let sender = account("SENDER");
    let id = sender.account_id();
    state.accounts.add_steam(sender.clone()).unwrap();
    connect::spawn(state.clone(), sender.clone());
    within(60, "the account to be signed in", || state.runtime.conn_state(&id) == Some(ConnState::Connected)).await;
    let channels = |state: &crate::state::AppState| -> Vec<String> { state.runtime.list_buffers().into_iter().filter(|b| b.account_id == id && b.kind == "channel").map(|b| b.name).collect() };
    // Left over from an earlier run: leave them first.
    tokio::time::sleep(Duration::from_secs(4)).await;
    println!("E2E at the start: channels {:?}", channels(&state));

    let made = groups::act(&state, &id, Action::Create { name: "moho e2e".into() }).await;
    println!("E2E create {made:?}");
    let rail = made.unwrap()["groupId"].as_str().unwrap().to_string();
    let (_, group) = groups::group_of_rail_id(&rail).map(|(a, g)| (a.to_string(), g)).unwrap();
    within(10, "the group's channel to appear", || channels(&state).iter().any(|n| n == "moho e2e/#General")).await;
    println!("E2E channels now {:?}; rail groups {:?}", channels(&state), state.runtime.list_buffer_groups().into_iter().filter(|g| g.account_id == id).map(|g| (g.id, g.name, g.kind)).collect::<Vec<_>>());

    let buffer = state.runtime.get_buffer(&crate::model::buffer_id(&id, "moho e2e/#General")).unwrap();
    println!("E2E remote id {:?}, group {:?}, read-only {:?}", buffer.remote_id, buffer.group_id, buffer.read_only);
    println!("E2E members shown {:?}", state.runtime.get_presence(&buffer.id));

    println!("E2E send {:?}", super::send_message(&state, &buffer, "hello [b]world[/b] :steamhappy:").await);
    println!("E2E mention send {:?}", super::send_message(&state, &buffer, "@Mohotester ping").await);
    let raw: Result<ChatHistoryResponse, _> = link_for_raw(&state, &id).call("ChatRoom.GetMessageHistory#1", &ChatHistoryRequest { chat_group_id: Some(group), chat_id: groups::ids_of(&buffer).map(|(_, r)| r), max_count: Some(5), ..Default::default() }, true).await;
    println!("E2E raw history {:?}", raw.map(|h| h.messages.iter().map(|m| m.message.clone()).collect::<Vec<_>>()));
    let stored = state.store.get_backlog(&buffer.id, 0, 10).unwrap();
    println!("E2E stored {:?}", stored.iter().map(|m| (m.id.clone(), m.from.clone(), m.body.clone(), m.ts)).collect::<Vec<_>>());

    // Our own message echoed back as a notification, to see the live path decode and land.
    let link = state.runtime.steam_link(&id).unwrap();
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let echoed: Result<ChatSendResponse, _> = link
        .call("ChatRoom.SendChatMessage#1", &ChatSendRequest { chat_group_id: Some(group), chat_id: groups::ids_of(&buffer).map(|(_, r)| r), message: Some("echo please".into()), echo_to_sender: Some(true) }, true)
        .await;
    println!("E2E raw send {echoed:?}");
    tokio::time::sleep(Duration::from_secs(2)).await;
    println!("E2E after the echo {:?}", state.store.get_backlog(&buffer.id, 0, 10).unwrap().iter().map(|m| (m.id.clone(), m.body.clone())).collect::<Vec<_>>());
    println!("E2E history adds (0 means live and history agree on the id): {:?}", groups::history(&state, &buffer, None).await);

    if let Some(first) = stored.first() {
        println!("E2E react {:?}", groups::react(&state, &buffer, &first.id, "[steam:emote:steamhappy]", true).await);
        tokio::time::sleep(Duration::from_millis(800)).await;
        println!("E2E reactions {:?}", state.store.get_message(&buffer.id, &first.id).ok().flatten().map(|m| m.reactions));
        println!("E2E mark read {:?}", groups::mark_read(&state, &buffer));
        println!("E2E history {:?}", groups::history(&state, &buffer, None).await);
        println!("E2E delete {:?}", groups::delete_message(&state, &buffer, &first.id).await);
    }

    println!("E2E room {:?}", groups::act(&state, &id, Action::CreateRoom { group, name: "second".into() }).await);
    within(10, "the second room's channel", || channels(&state).iter().any(|n| n == "moho e2e/#second")).await;
    let second = groups::held(&id, group).unwrap().rooms.iter().find(|r| r.name == "second").map(|r| r.id).unwrap();
    println!("E2E rename room {:?}", groups::act(&state, &id, Action::RenameRoom { group, room: second, name: "third".into() }).await);
    within(10, "the renamed room's channel", || channels(&state).iter().any(|n| n == "moho e2e/#third") && !channels(&state).iter().any(|n| n == "moho e2e/#second")).await;
    println!("E2E rename group {:?}", groups::act(&state, &id, Action::Rename { group, name: "moho e2e renamed".into() }).await);
    within(10, "the renamed group's channels", || channels(&state).iter().any(|n| n == "moho e2e renamed/#third")).await;
    println!("E2E tagline {:?}", groups::act(&state, &id, Action::Tagline { group, text: "a tagline".into() }).await);
    let made = groups::act(&state, &id, Action::MakeInvite { group, seconds: 3600 }).await;
    println!("E2E invite {made:?}");
    println!("E2E links {:?}", groups::act(&state, &id, Action::Links { group }).await);
    if let Some(code) = made.ok().and_then(|v| v["code"].as_str().map(str::to_string)) {
        println!("E2E revoke {:?}", groups::act(&state, &id, Action::RevokeInvite { group, code }).await);
    }
    println!("E2E bans {:?}", groups::act(&state, &id, Action::Bans { group }).await);
    println!("E2E create role {:?}", groups::act(&state, &id, Action::CreateRole { group, name: "Raiders".into() }).await);
    let role = groups::held(&id, group).unwrap().roles.iter().find(|(_, n)| n == "Raiders").map(|(r, _)| *r);
    println!("E2E role found {role:?}");
    if let Some(role) = role {
        println!("E2E rename role {:?}", groups::act(&state, &id, Action::RenameRole { group, role, name: "Raid leaders".into() }).await);
        println!("E2E role actions {:?}", groups::act(&state, &id, Action::RoleActions { group, role }).await);
        let mut a = groups::actions_of(role, &serde_json::json!({ "chat": true, "kick": true }));
        a.can_ban = Some(true);
        println!("E2E set role actions {:?}", groups::act(&state, &id, Action::SetRoleActions { group, role, actions: a }).await);
        println!("E2E role actions after {:?}", groups::act(&state, &id, Action::RoleActions { group, role }).await);
        println!("E2E give role {:?}", groups::act(&state, &id, Action::GiveRole { group, who: sender.steamid, role }).await);
        println!("E2E members with roles {:?}", groups::held(&id, group).map(|g| g.members.iter().map(|m| (m.account, m.roles.clone())).collect::<Vec<_>>()));
        println!("E2E take role {:?}", groups::act(&state, &id, Action::TakeRole { group, who: sender.steamid, role }).await);
        println!("E2E delete role {:?}", groups::act(&state, &id, Action::DeleteRole { group, role }).await);
    }
    println!("E2E json {}", groups::to_json(&id, Some(group)));
    println!("E2E delete room {:?}", groups::act(&state, &id, Action::DeleteRoom { group, room: second }).await);
    within(10, "the deleted room's channel to go", || !channels(&state).iter().any(|n| n.ends_with("/#third"))).await;
    println!("E2E leave {:?}", groups::act(&state, &id, Action::Leave { group }).await);
    within(10, "the group's channels to go", || !channels(&state).iter().any(|n| n.starts_with("moho e2e"))).await;
    println!("E2E at the end: channels {:?}, rail groups {:?}", channels(&state), state.runtime.list_buffer_groups().into_iter().filter(|g| g.account_id == id).count());
    if let Some(link) = state.runtime.take_steam_link(&id) {
        crate::backend::steam::sign_off(link);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
}
