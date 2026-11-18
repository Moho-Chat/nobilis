//! Fluxer: an open-source chat service shaped like Discord - guilds with categories, channels and roles, direct
//! messages and groups, replies, reactions, custom emoji, stickers, embeds, threads and LiveKit voice - over a REST API
//! and a gateway that are both documented in full (https://docs.fluxer.com; the source is `fluxer_docs/` in
//! https://github.com/fluxerapp/fluxer).
//!
//! The plan, and what has been checked, is issue #57. In short: the official instance is fluxer.com, but anybody can
//! host one, so an account names its instance's origin (as a Matrix account names its homeserver), and every address
//! is read from that instance's `/.well-known/fluxer`; the session token is what the account is.
//!
//! # How this folder is arranged
//!
//! - `discovery` - the instance's own description of itself: its addresses, features and limits
//! - `altcha` - the proof of work Fluxer puts in front of the password sign-in
//! - `api` - the REST client: the token, pacing well under the global limit, and what an error means
//! - `login` - adding an account: a code approved in another Fluxer app, a password (and second factor), or a token
//! - `world` - what the account can see: users, guilds, channels, roles, and what each lets it do
//! - `connect` - the gateway: identified, heartbeaten, resumed
//! - `events` - what the gateway says as it happens
//! - `messages` - a message as moho's, and a channel's history
//! - `send` - saying things: send, edit, delete, react, typing, read
//! - `media` - avatars, icons, emoji and files, fetched from the media proxy when looked at
//!
//! # What moho tells Fluxer about itself
//!
//! The user agent is `moho/<version>`, and Identify says `browser: "moho"` with the real operating system. One gateway
//! connection is kept per account; a dropped one is resumed, and a session Fluxer has ended stops the account instead
//! of signing in again to get over it.

// What later rounds of issue #57 use (moderation, guild settings, uploads planned ahead, voice) is written down already
// and not yet read; the allowance goes when they are.
#![allow(dead_code)]

pub mod altcha;
pub mod api;
pub mod connect;
pub mod discovery;
pub mod events;
pub mod invites;
pub mod login;
pub mod manage;
pub mod media;
pub mod members;
pub mod messages;
pub mod moderation;
pub mod own;
pub mod people;
pub mod profile;
pub mod search;
pub mod send;
pub mod settings;
pub mod world;

pub use connect::spawn;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// What moho calls itself to Fluxer's servers.
pub fn user_agent() -> String {
    concat!("moho/", env!("CARGO_PKG_VERSION")).to_string()
}

/// What the rest of the daemon needs to act on a connected account.
#[derive(Clone)]
pub struct Handle {
    pub account_id: String,
    pub api: Arc<api::Api>,
    pub world: Arc<Mutex<world::World>>,
    /// What the instance said about itself when the connection was made.
    pub instance: Arc<discovery::Instance>,
    /// Lines for the gateway, written by the connection that owns it.
    pub gateway: tokio::sync::mpsc::UnboundedSender<String>,
    /// The guilds made active for this connection (see `connect::activate_for`).
    pub active: Arc<Mutex<HashSet<String>>>,
    /// The limits this account is held to.
    pub limits: Arc<discovery::Limits>,
    /// When each channel's slowmode lets the account send again, in Unix seconds.
    pub slow: Arc<Mutex<HashMap<String, i64>>>,
    /// The account's notification settings by guild (`@me` for direct messages), as Fluxer last sent them.
    pub notify: Arc<Mutex<HashMap<String, serde_json::Value>>>,
}

impl Handle {
    pub fn slow_until(&self, channel: &str) -> i64 {
        self.slow.lock().unwrap_or_else(|e| e.into_inner()).get(channel).copied().unwrap_or(0)
    }

    pub fn set_slow_until(&self, channel: &str, at: i64) {
        self.slow.lock().unwrap_or_else(|e| e.into_inner()).insert(channel.to_string(), at);
    }
}
