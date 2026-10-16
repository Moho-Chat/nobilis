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
pub mod link;
pub mod login;
pub mod session;

use grammers_mtsender::ConnectionParams;

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
