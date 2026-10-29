//! Stoat (formerly Revolt): a chat service shaped like Discord - servers with categories, channels and roles, direct
//! messages and groups, replies, reactions, custom emoji, embeds - over a REST API and an events websocket, both open
//! and documented (https://developers.stoat.chat).
//!
//! The plan, and what has been checked, is Gitea issue #32. In short: the official instance is `api.stoat.chat`, but
//! anybody can host one, so an account names its instance (as a Matrix account names its homeserver); the session
//! token is what the account is.
//!
//! # How this folder is arranged
//!
//! - `api` - the REST client: the token, the instance's rate limits kept to as its headers say, and what an error means
//! - `login` - adding an account: email and password, a second factor if there is one, or a pasted token
//! - `world` - what the account can see: users, servers, channels, roles, and what each lets it do
//! - `connect` - an account's connection: the events websocket, kept up and reconnected
//! - `events` - what the websocket says as it happens
//! - `messages` - a message as moho's, and a channel's history
//! - `send` - saying things: send, edit, delete, typing, read
//! - `invites` - a link to a server drawn as a card to join it
//! - `limits` - the instance's stated limits, kept to before a request is made
//! - `manage` - looking after a server: channels, roles, permissions, webhooks, the audit log
//! - `members` - who is in a conversation, and who is online
//! - `profile` - the card for a person
//! - `own` - the account's own profile and presence
//! - `settings` - what the account has muted, kept on the account
//! - `search` - searching a channel, its pins, and reading around a message
//! - `media` - avatars, icons, pictures and files, fetched from the file server when looked at
//!
//! # What moho tells Stoat about itself
//!
//! The user agent is `moho/<version>` and the session is named `moho (<os>)`, which is what Stoat's list of sessions
//! shows. One connection is kept per account, and a session Stoat has ended stops the account instead of signing in
//! again to get over it.

pub mod api;
pub mod connect;
pub mod events;
pub mod invites;
pub mod limits;
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
pub mod settings;
pub mod send;
pub mod voice;
pub mod world;

#[cfg(test)]
mod tests;

pub use connect::spawn;

use std::sync::{Arc, Mutex};

/// What the rest of the daemon needs to act on a connected account.
#[derive(Clone)]
pub struct Handle {
    pub account_id: String,
    pub api: Arc<api::Api>,
    pub world: Arc<Mutex<world::World>>,
    /// What the instance said about itself when the connection was made: where its files and previews are, and its limits.
    pub instance: Arc<world::Instance>,
    /// Lines for the websocket (typing notices), written by the connection that owns it.
    pub socket: tokio::sync::mpsc::UnboundedSender<String>,
}

/// The name Stoat shows for this app's sessions.
pub fn session_name() -> String {
    format!("moho ({})", std::env::consts::OS)
}

impl Handle {
    /// Whether the account is still held to the tighter limits of a new one: its id says when it was made.
    pub fn is_new(&self, now_ms: i64) -> bool {
        let own = self.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
        let made = messages::ulid_ms(&own).unwrap_or(0);
        made > 0 && now_ms - made < (self.instance.new_user_hours as i64) * 3_600_000
    }

    /// What this account may do now, by its standing.
    pub fn limits(&self) -> &world::UserLimits {
        if self.is_new(chrono::Utc::now().timestamp_millis()) {
            &self.instance.fresh
        } else {
            &self.instance.standing
        }
    }
}
