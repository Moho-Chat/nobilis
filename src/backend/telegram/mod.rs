//! Telegram, over MTProto, in pure Rust (the `grammers` crates).
//!
//! # How this folder is arranged
//!
//! Built in rounds, as Twitch was (see Gitea issue #26 for the checklist and what has been verified):
//!
//! - this file - the identity moho gives Telegram, and what every other file shares
//! - `session` - what the library remembers between runs, in a private file of its own per account
//! - `link` - one connection to Telegram: the sender pool, the client and the stream of updates
//! - `login` - adding an account: phone, code, cloud password
//! - `connect` - an account's connection, kept up and reconnected
//! - `dialogs` - the account's conversations, read when it connects
//! - `history` - a conversation's history, read when it is opened and scrolled back
//! - `markup` - what the message box holds, as plain text and entities
//! - `updates` - what Telegram sends that is not a message: typing, reads on other apps
//! - `send` - saying things: send, answer, edit, delete, typing, read
//! - `peers` - a conversation as a buffer: its name, its kind, its heading in the list
//! - `messages` - a message as moho's, with its entities as html
//!
//! # The api_id
//!
//! Telegram gives one `api_id` and `api_hash` to a phone number's owner, and a client sends them with every
//! connection. Moho does not embed one: each account carries its own, typed into the sign-in form, and they are
//! kept with the session like a password. Nothing here logs them.
//!
//! # What moho tells Telegram about itself
//!
//! Telegram lists every session of an account (Settings, Devices) with the app and device each one reported, and
//! treats a stream of unfamiliar sessions as a risk. So the identity is set once, is the same on every connection,
//! and says what this is: the device is "moho" on this operating system, the version is moho's own. A session is
//! saved and reused; nothing here signs in afresh to get over a hiccup.

pub mod connect;
pub mod dialogs;
pub mod history;
pub mod link;
pub mod login;
pub mod markup;
pub mod messages;
pub mod peers;
pub mod send;
pub mod updates;
pub mod session;

use grammers_mtsender::ConnectionParams;
use std::sync::Arc;

/// What the rest of the daemon needs to act on a connected account: its client, its session (to turn a conversation
/// into something Telegram will accept, an id with the authority to use it) and its own name.
#[derive(Clone)]
pub struct Handle {
    pub client: grammers_client::Client,
    pub session: Arc<session::FileSession>,
    pub own: messages::Own,
    /// What people are called, by Telegram's number for them, so a typing notice can say who. Filled as people are
    /// met in the conversation list and in messages.
    pub names: Arc<std::sync::Mutex<std::collections::HashMap<i64, String>>>,
}

/// The name Telegram shows for this app's sessions: `moho` and the kind of machine it runs on.
pub fn device_model() -> String {
    format!("moho ({} {})", std::env::consts::OS, std::env::consts::ARCH)
}

/// How every connection of an account introduces itself.
///
/// `proxy` is the SOCKS5 address of the account's route (Tor or the proxy in the network settings), if it has one.
pub fn connection_params(proxy: Option<&str>) -> ConnectionParams {
    ConnectionParams {
        device_model: device_model(),
        // Moho's own version, which is the daemon's.
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        proxy_url: proxy.map(str::to_string),
        ..ConnectionParams::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_identity_says_what_this_is_and_is_the_same_each_time() {
        let a = connection_params(None);
        let b = connection_params(None);
        assert!(a.device_model.starts_with("moho ("), "{}", a.device_model);
        assert_eq!((a.device_model.as_str(), a.app_version.as_str()), (b.device_model.as_str(), b.app_version.as_str()));
        assert_eq!(a.app_version, env!("CARGO_PKG_VERSION"));
        assert!(!a.system_version.is_empty() && !a.lang_code.is_empty());
    }
}
