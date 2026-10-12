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
