//! What the tests in this folder need in common.
//!
//! A daemon with a store and no network, and the two lines of ceremony for
//! feeding it a frame. Here rather than in whichever test file wrote it first,
//! because three of them want it and none of them owns it.
#![cfg(test)]

use super::*;

/// A whole daemon in a temporary directory, so a stream event can be fed
/// in and the message it produces read back out.
///
/// Nothing about a poll or a prediction can be produced on demand from a
/// real channel - they happen when a streamer decides to run one - so the
/// only way to know this code works is to hand it the payload and look at
/// what lands in the store. That is what these do.
pub(super) fn simulated_daemon(name: &str) -> AppState {
    // The same convention the audio and Tor probes use for a scratch
    // directory - a dependency for one test would be a poor trade.
    let dir = std::env::temp_dir().join(format!("nobilis-kick-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    AppState {
        store: std::sync::Arc::new(crate::store::Store::open(&dir.join("scrollback.db")).expect("store")),
        accounts: std::sync::Arc::new(crate::accounts::AccountStore::open(dir.join("accounts.toml")).expect("accounts")),
        events: crate::events::EventBus::new(),
        runtime: std::sync::Arc::new(crate::runtime::Runtime::new()),
        tor: std::sync::Arc::new(crate::net::tor::TorManager::new(&dir)),
        shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
        voice: std::sync::Arc::new(crate::backend::discord::voice::VoiceState::new()),
        voice_prefs: std::sync::Arc::new(crate::audio::VoicePrefsStore::open(dir.join("voice.toml"))),
        dcc_prefs: std::sync::Arc::new(crate::backend::irc::dcc::DccPrefsStore::open(dir.join("dcc.toml"))),
        highlights: std::sync::Arc::new(crate::highlights::HighlightStore::open(dir.join("highlights.toml"))),
        ignores: std::sync::Arc::new(crate::ignores::IgnoreStore::open(dir.join("ignores.toml"))),
    }
}

/// The messages a channel's buffer holds, oldest first.
pub(super) fn lines(state: &AppState, slug: &str) -> Vec<String> {
    state
        .store
        .get_backlog(&crate::model::buffer_id("kick:tester", slug), 0, 50)
        .expect("backlog")
        .into_iter()
        .map(|m| m.body)
        .collect()
}

pub(super) fn feed(state: &AppState, watched: &mut Watched, event: &str, payload: serde_json::Value) {
    // odablock's own chatroom, as the fixture below sets it up.
    handle_event(state, "kick:tester", watched, event, Some("chatrooms.2393554.v2"), payload).expect("handled");
}

/// odablock's real numbers: the chatroom and the channel differ, which is
/// the ordinary case and the one a single map gets wrong.
pub(super) fn watching_odablock() -> Watched {
    let mut w = Watched::default();
    w.add(&api::Channel {
        id: 2401072,
        chatroom_id: 2393554,
        slug: "odablock".into(),
        username: "odablock".into(),
        avatar_url: None,
        subscribers_only: false,
        followers_only: false,
        live: None,
        followers: None,
        playback_url: None,
    });
    w
}
