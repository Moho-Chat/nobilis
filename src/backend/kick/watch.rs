//! Telling Kick we are watching, so watch time is earned.
//!
//! ## What was wrong
//!
//! Kick gives 10 points per five minutes of watching (20 to subscribers), and
//! moho earned none of it. Measured rather than assumed: nine minutes on a
//! live channel with 565 viewers, balance unchanged throughout.
//!
//! The reason is in `socket.rs`'s own words - "Empty auth: all four are
//! public, and this backend subscribes to nothing that is not". Kick was told
//! nothing about who was watching, so it had nothing to credit. The video is a
//! signed CDN playlist fetched anonymously, so that earned nothing either.
//!
//! ## How Kick counts a viewer
//!
//! There is no points call. Nothing asks for them and nothing reports watch
//! time; it is entirely server-side, off the presence of an authenticated
//! realtime subscription. Kick has moved from Pusher to Centrifugo for this,
//! and the new path is *identified*, which is the whole point.
//!
//! Three steps, from a capture of a logged-in session:
//!
//! 1. `POST /api/v1/realtime/connection` with this account's Kick user id,
//!    which answers with the websocket to use.
//! 2. `POST /api/v1/realtime/auth/channel` for `private-livestream.<id>`,
//!    which answers with a JWT for that one subscription.
//! 3. Connect and subscribe with it. The connection itself carries no token -
//!    only a URL came back - so it is anonymous, and the per-channel JWT is
//!    what says who is watching.
//!
//! It is the **livestream** channel that earns time, not the chat one, which
//! is right: this is watch time rather than chat presence. And it is keyed by
//! *livestream* id, which changes every broadcast - so it is read when a
//! channel goes live rather than cached alongside the channel.
//!
//! ## Scope, deliberately
//!
//! Only what is actually on screen: a chat buffer somebody has open, or a
//! channel whose video is playing. Not every followed channel - a background
//! subscription to twenty of them is claiming to watch twenty streams at once,
//! which is both untrue and exactly the shape of thing that gets an account
//! looked at.
//!
//! This is also the one place moho stops reading Kick anonymously. That is the
//! trade for points and it was made deliberately; keeping it to what is
//! displayed is what keeps the claim honest.

use anyhow::{bail, Context, Result};

/// Where the realtime endpoints live. Not `API_ROOT` - these are on a
/// different host from the rest of Kick's API.
const WEB_ROOT: &str = "https://web.kick.com";

/// What Kick's own client calls itself. Sent because the endpoint expects it.
const PLATFORM: &str = "web";

/// The websocket to use, and it is asked for rather than hardcoded: the
/// answer names a region, and a client that pinned one would break for
/// everybody the day Kick moved it.
pub async fn negotiate(http: &reqwest::Client, token: &str, user_id: &str) -> Result<String> {
    let body = serde_json::json!({
        "client": { "id": user_id, "type": PLATFORM },
        // Both, the way Kick's own client asks. It still offers pusher, which
        // is presumably why the chat socket in socket.rs keeps working.
        "capabilities": { "accepted_providers": [ { "provider": "pusher" }, { "provider": "centrifugo" } ] }
    });
    let resp = post(http, token, &format!("{WEB_ROOT}/api/v1/realtime/connection"), body)
        .await
        .context("asking Kick where its realtime service is")?;
    resp["data"]["connections"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["provider"].as_str() == Some("centrifugo"))
        .and_then(|c| c["credentials"]["url"].as_str())
        .map(str::to_string)
        .context("Kick named no centrifugo connection")
}

/// A token for one livestream's channel - what actually says who is watching.
pub async fn channel_token(http: &reqwest::Client, token: &str, livestream_id: u64) -> Result<String> {
    let body = serde_json::json!({ "channel": channel_name(livestream_id) });
    let resp = post(http, token, &format!("{WEB_ROOT}/api/v1/realtime/auth/channel"), body)
        .await
        .context("asking Kick to let us watch")?;
    resp["data"]["token"].as_str().map(str::to_string).context("Kick sent no subscription token")
}

/// The channel watch time is counted on.
pub fn channel_name(livestream_id: u64) -> String {
    format!("private-livestream.{livestream_id}")
}

async fn post(http: &reqwest::Client, token: &str, url: &str, body: serde_json::Value) -> Result<serde_json::Value> {
    let resp = http
        .post(url)
        .header("Accept", "application/json")
        .header("x-app-platform", PLATFORM)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await
        .context("request failed")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("Kick answered {status}");
    }
    resp.json().await.context("unreadable answer")
}

/// Centrifugo's own frames, as far as watching needs them.
///
/// Deliberately the smallest possible client. This subscribes and then listens
/// to nothing: the point is to be present, not to read what the channel
/// publishes - chat already arrives on the Pusher socket, and duplicating it
/// here would mean two sources for one conversation.
pub mod frames {
    use serde_json::{json, Value};

    /// Opens the connection. No token: the negotiation answers with a URL and
    /// nothing else, so the connection is anonymous and the per-channel token
    /// below is what identifies anybody.
    pub fn connect(id: u64) -> Value {
        json!({ "id": id, "connect": {} })
    }

    pub fn subscribe(id: u64, channel: &str, token: &str) -> Value {
        json!({ "id": id, "subscribe": { "channel": channel, "token": token } })
    }

    pub fn unsubscribe(id: u64, channel: &str) -> Value {
        json!({ "id": id, "unsubscribe": { "channel": channel } })
    }

    /// Centrifugo's keepalive is an empty object in both directions: the
    /// server sends `{}` and expects `{}` back. A client that does not answer
    /// is dropped, which for this would mean silently ceasing to be a viewer.
    pub fn is_ping(v: &Value) -> bool {
        v.as_object().is_some_and(|o| o.is_empty())
    }

    pub fn pong() -> Value {
        json!({})
    }

    /// Whether a reply to one of the above refused it, and why.
    pub fn error_in(v: &Value) -> Option<String> {
        let e = v.get("error")?;
        let message = e["message"].as_str().unwrap_or("refused");
        Some(match e["code"].as_u64() {
            Some(code) => format!("{message} ({code})"),
            None => message.to_string(),
        })
    }
}

/// Which livestreams this account is currently claiming to watch.
///
/// Held per account rather than globally: two Kick accounts in one client are
/// two viewers, and merging them would credit one for the other's time.
pub type Watching = std::collections::BTreeMap<String, u64>;

/// Keeps one connection open for as long as anything is being watched.
///
/// Restarted rather than repaired when the socket dies: a subscription is
/// cheap to make again and the alternative is tracking which of them survived
/// a reconnect. Backs off so a service that is down is not hammered by a
/// client that wants points.
pub async fn run(state: crate::state::AppState, account_id: String, user_id: String, token: String) {
    let mut backoff = std::time::Duration::from_secs(3);
    loop {
        if state.runtime.kick_watching(&account_id).is_empty() {
            // Nothing on screen. Wait to be told there is rather than holding
            // a connection open to say nothing - see the module comment on
            // why this is scoped to what is displayed.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            continue;
        }
        match session(&state, &account_id, &user_id, &token).await {
            Ok(()) => backoff = std::time::Duration::from_secs(3),
            Err(e) => {
                tracing::debug!("kick[{account_id}]: watch session ended: {e:#}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(std::time::Duration::from_secs(120));
            }
        }
    }
}

async fn session(state: &crate::state::AppState, account_id: &str, user_id: &str, token: &str) -> Result<()> {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let http = super::api::client()?;
    let url = negotiate(&http, token, user_id).await?;
    let (mut socket, _) = tokio_tungstenite::connect_async(&url).await.context("connecting to Kick's realtime service")?;

    let mut next_id = 1u64;
    let mut id = || {
        next_id += 1;
        next_id
    };
    socket.send(Message::Text(frames::connect(id()).to_string())).await.context("opening the connection")?;

    // What is subscribed now, so the set on screen can be followed as it
    // changes without tearing the connection down.
    let mut subscribed: std::collections::BTreeMap<String, u64> = Default::default();

    loop {
        let wanted = state.runtime.kick_watching(account_id);
        if wanted.is_empty() {
            return Ok(());
        }

        // Newly on screen.
        for (buffer_id, livestream) in wanted.iter() {
            if subscribed.contains_key(buffer_id) {
                continue;
            }
            let channel = channel_name(*livestream);
            match channel_token(&http, token, *livestream).await {
                Ok(jwt) => {
                    socket.send(Message::Text(frames::subscribe(id(), &channel, &jwt).to_string())).await?;
                    subscribed.insert(buffer_id.clone(), *livestream);
                    tracing::info!("kick[{account_id}]: watching {channel}");
                }
                // A stream that ended between being displayed and being asked
                // about is the ordinary case, not a failure.
                Err(e) => tracing::debug!("kick[{account_id}]: cannot watch {channel}: {e:#}"),
            }
        }

        // No longer on screen. Unsubscribed rather than left running, which is
        // the whole of the scoping promise: moho claims to be watching exactly
        // what is being looked at.
        let gone: Vec<String> = subscribed.keys().filter(|b| !wanted.contains_key(*b)).cloned().collect();
        for buffer_id in gone {
            if let Some(livestream) = subscribed.remove(&buffer_id) {
                let _ = socket.send(Message::Text(frames::unsubscribe(id(), &channel_name(livestream)).to_string())).await;
                tracing::info!("kick[{account_id}]: stopped watching {}", channel_name(livestream));
            }
        }

        // Answer the keepalive and notice refusals. A second at a time so a
        // change to what is on screen is acted on promptly rather than
        // whenever the server next says something.
        match tokio::time::timeout(std::time::Duration::from_secs(1), socket.next()).await {
            Err(_) => continue,
            Ok(None) => bail!("the connection closed"),
            Ok(Some(frame)) => match frame.context("reading from the connection")? {
                Message::Text(text) => {
                    for line in text.lines().filter(|l| !l.trim().is_empty()) {
                        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else { continue };
                        if frames::is_ping(&value) {
                            socket.send(Message::Text(frames::pong().to_string())).await?;
                        } else if let Some(why) = frames::error_in(&value) {
                            // Said out loud: a refused subscription is the
                            // difference between earning watch time and
                            // quietly not, and it looks like nothing.
                            tracing::warn!("kick[{account_id}]: realtime refused: {why}");
                        }
                    }
                }
                Message::Close(_) => bail!("the server closed the connection"),
                _ => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Watch time is counted on the livestream, not the chat room - and by
    /// livestream id, which is not the channel id and changes every broadcast.
    #[test]
    fn the_channel_is_the_livestreams_own() {
        assert_eq!(channel_name(126952428), "private-livestream.126952428");
    }

    /// Centrifugo's keepalive is an empty object, and answering it is what
    /// keeps us a viewer - a client that does not is dropped.
    #[test]
    fn an_empty_object_is_the_keepalive() {
        assert!(frames::is_ping(&json!({})));
        assert!(!frames::is_ping(&json!({ "id": 1 })));
        assert!(!frames::is_ping(&json!([])));
        assert!(!frames::is_ping(&json!(null)));
        assert_eq!(frames::pong(), json!({}));
    }

    #[test]
    fn a_subscription_carries_the_token_that_names_the_watcher() {
        let f = frames::subscribe(2, "private-livestream.1", "jwt");
        assert_eq!(f["subscribe"]["channel"], json!("private-livestream.1"));
        assert_eq!(f["subscribe"]["token"], json!("jwt"));
        assert_eq!(f["id"], json!(2));
    }

    /// The connection itself is anonymous - the negotiation answers with a URL
    /// and no token, so nothing is sent here.
    #[test]
    fn the_connection_carries_no_token() {
        assert_eq!(frames::connect(1), json!({ "id": 1, "connect": {} }));
    }

    #[test]
    fn a_refusal_is_read_with_its_reason() {
        assert_eq!(
            frames::error_in(&json!({ "id": 2, "error": { "code": 103, "message": "permission denied" } })).as_deref(),
            Some("permission denied (103)")
        );
        assert!(frames::error_in(&json!({ "id": 2, "subscribe": {} })).is_none());
    }
}
