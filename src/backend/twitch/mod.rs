//! Twitch - chat and live video, over Twitch's own IRC service and the same
//! playlists its website plays.
//!
//! Twitch is the plan in Gitea issue #55, read before anything here. In short:
//! chat is IRC over a websocket (anonymously readable, signed in to send),
//! the official REST API is Helix, and the video comes from the playlist
//! service twitch.tv's own player uses, so the stream can be drawn in the
//! window's player the way Kick's is.
//!
//! # How this folder is arranged
//!
//! - `auth` - Twitch's sign-in (the device code flow) and what comes of it:
//!   tokens, renewing them, finding out who they belong to
//! - `login` - adding an account: the sign-in a person takes part in
//! - `connect` - an account's connection: keeping its token good, and (as the
//!   rounds in the plan are built) listening
//!
//! As in the other backends, `spawn` is re-exported so callers say
//! `backend::twitch::spawn` without knowing which file that is.

pub mod auth;
pub mod connect;
pub mod login;

pub use connect::spawn;

/// What routes this service's requests when any Twitch account goes through
/// Tor or the proxy. One key for the whole service, as Kick's is: the
/// requests that are made before an account exists (the sign-in) have no
/// account to name.
pub const SHARED_KEY: &str = "twitch:any-routed";

/// An HTTP client for Twitch's servers, through the route.
pub fn client() -> anyhow::Result<reqwest::Client> {
    let router = crate::net::route::router();
    let routed = router.routed(SHARED_KEY) || router.routed(&crate::net::route::pending_key("twitch"));
    Ok(router.client_if("twitch", routed, |builder| builder.user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"))).timeout(std::time::Duration::from_secs(20))))
}
