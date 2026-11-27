//! Mattermost: a team chat server anybody can host, over its REST API (v4) and its websocket, both documented
//! (https://developers.mattermost.com/api-documentation/).
//!
//! The plan, and what has been checked, is issue #25. In short: an account is a person on one server, which the
//! account names (as a Matrix account names its homeserver); a server holds teams, each team holds channels (open and
//! private), and direct messages and group messages belong to no team. Here a team is a group on the rail and its
//! channels are filed under the team's own sidebar categories; direct and group messages are under Direct Messages.
//! Replies form threads, which open in the thread panel beside the channel.
//!
//! # How this folder is arranged
//!
//! - `api` - the REST client: the token, the server's rate limits kept to as its headers say, and what an error means
//! - `login` - adding an account: username or email and password with a second factor, a personal access token, or the
//!   session a single sign-on page in the window leaves behind
//! - `world` - what the account can see: users, teams, channels, its memberships and categories
//! - `connect` - an account's connection: the websocket, resumed where it left off, and reconnected
//! - `events` - the first load that makes the rail, and what the websocket says after it
//! - `messages` - a post as moho's message, a channel's history, a thread
//! - `send` - saying things: send, files, edit, delete, react, typing, read
//! - `media` - avatars, team icons, files and custom emoji: fetched with the account's token when looked at
//! - `emoji` - Mattermost's names for emoji, and the pictures of a server's own
//! - `people` - profiles, presence, the account's own status and profile, direct messages opened by name
//! - `extra` - pins, saved posts, search, threads followed, drafts, scheduled posts, bookmarks, slash commands
//! - `manage` - looking after a channel or team: settings, members and their roles, invites, archive
//! - `calls` - the Calls plugin: who is in a call, and the window's call joined through the plugin's signalling
//!
//! # What moho tells Mattermost about itself
//!
//! The user agent is `moho/<version>`; a session made here by password carries the device id `moho`, which is what
//! Mattermost's list of sessions shows. One websocket is kept per account; a session the server has ended (a 401, or the
//! account's sessions revoked) stops the account instead of signing in again to get over it.

pub mod api;
pub mod calls;
pub mod connect;
pub mod emoji;
pub mod events;
pub mod extra;
pub mod login;
pub mod manage;
pub mod media;
pub mod messages;
pub mod people;
pub mod send;
pub mod world;

pub use connect::spawn;

use std::sync::{Arc, Mutex};

/// What the rest of the daemon needs to act on a connected account.
#[derive(Clone)]
pub struct Handle {
    pub account_id: String,
    pub api: Arc<api::Api>,
    pub world: Arc<Mutex<world::World>>,
    /// What the server said about itself when the connection was made.
    pub server: Arc<world::ServerInfo>,
    /// Lines for the websocket (typing notices, the Calls plugin's signalling), written by the connection that owns it.
    pub socket: tokio::sync::mpsc::UnboundedSender<String>,
}

impl Handle {
    /// The account's own user id.
    pub fn own_id(&self) -> String {
        self.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone()
    }
}

/// The account id for a person on a server: `mattermost:<host>:<user id>`, the host with its port where it has one.
pub fn account_id(server_url: &str, user_id: &str) -> String {
    let host = url::Url::parse(server_url).ok().map(|u| match u.port() {
        Some(port) => format!("{}:{port}", u.host_str().unwrap_or("")),
        None => u.host_str().unwrap_or("").to_string(),
    });
    format!("mattermost:{}:{user_id}", host.unwrap_or_default())
}

/// Mattermost's ids: 26 lowercase letters and digits.
pub fn is_id(text: &str) -> bool {
    text.len() == 26 && text.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_is_named_by_its_server_and_its_user() {
        assert_eq!(account_id("https://chat.example.org", "bx955xywntf83q76gx1ost4pkr"), "mattermost:chat.example.org:bx955xywntf83q76gx1ost4pkr");
        assert_eq!(account_id("http://127.0.0.1:8065", "bx955xywntf83q76gx1ost4pkr"), "mattermost:127.0.0.1:8065:bx955xywntf83q76gx1ost4pkr");
        assert_eq!(crate::model::service_of("mattermost:chat.example.org:x"), "mattermost");
    }

    #[test]
    fn an_id_is_twenty_six_lowercase_letters_and_digits() {
        assert!(is_id("bx955xywntf83q76gx1ost4pkr"));
        assert!(!is_id("BX955XYWNTF83Q76GX1OST4PKR"));
        assert!(!is_id("short"));
    }
}
