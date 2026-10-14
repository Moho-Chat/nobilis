//! Adding an account: the sign-in a person takes part in.
//!
//! Twitch's sign-in is a conversation held in two places. Moho asks for a
//! code and shows it; the person types it (or follows the link, which has it
//! in) on twitch.tv, signed in however they sign in, and says yes; moho,
//! meanwhile, asks every few seconds whether that has happened yet. It can
//! take minutes, so it runs as its own task and talks to the window through
//! events rather than answering one request:
//!
//! - `twitchLoginCode` - the code to show, where to enter it, and how long it
//!   is good for
//! - `twitchLoginStatus` - what it is doing, for a line of text
//! - `twitchLoginResult` - done, with the account, or the reason it failed
//!
//! The window closing the form ends the wait with `cancelTwitchLogin`.

use super::auth::{self, Poll};
use crate::accounts::TwitchAccountConfig;
use crate::state::AppState;
use anyhow::{bail, Result};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

/// The sign-ins waiting on a person, by id, and the way to end each one.
fn waiting() -> &'static Mutex<HashMap<String, Arc<Notify>>> {
    static WAITING: std::sync::OnceLock<Mutex<HashMap<String, Arc<Notify>>>> = std::sync::OnceLock::new();
    WAITING.get_or_init(Default::default)
}

/// Ends a sign-in that is waiting, because the form was closed.
pub fn cancel(login_id: &str) {
    if let Some(stop) = waiting().lock().unwrap().remove(login_id) {
        stop.notify_one();
    }
}

/// Starts a sign-in. Answers at once; the rest is events.
pub fn start(state: AppState, login_id: String) {
    start_at(state, login_id, auth::ID_ROOT.to_string(), true)
}

/// The same against a given server, and with or without starting the
/// connection once it is done - which is what lets a test do it without one.
pub fn start_at(state: AppState, login_id: String, root: String, connect: bool) {
    let stop = Arc::new(Notify::new());
    waiting().lock().unwrap().insert(login_id.clone(), stop.clone());
    tokio::spawn(async move {
        let outcome = flow(&state, &login_id, &root, stop).await;
        waiting().lock().unwrap().remove(&login_id);
        match outcome {
            Ok(Some(saved)) => {
                if connect {
                    super::spawn(state.clone(), saved.clone());
                }
                let account = crate::accounts::twitch_account_to_json(&saved, if connect { "connecting" } else { "disconnected" });
                state.events.emit("twitchLoginResult", json!({ "loginId": login_id, "success": true, "account": account }));
            }
            // The form was closed: nobody is waiting to be told.
            Ok(None) => {}
            Err(e) => {
                let error = format!("{e:#}");
                tracing::warn!("twitch sign-in {login_id}: {error}");
                state.events.emit("twitchLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
            }
        }
    });
}

/// The sign-in itself. `None` when it was cancelled.
pub async fn flow(state: &AppState, login_id: &str, root: &str, stop: Arc<Notify>) -> Result<Option<TwitchAccountConfig>> {
    let status = |detail: &str| state.events.emit("twitchLoginStatus", json!({ "loginId": login_id, "detail": detail }));
    let http = super::client()?;

    status("Asking Twitch for a code");
    let code = auth::start(&http, root).await?;
    state.events.emit(
        "twitchLoginCode",
        json!({
            "loginId": login_id,
            "userCode": code.user_code,
            "verificationUri": code.verification_uri,
            "expiresIn": code.expires_in,
            "scopes": auth::SCOPES.iter().map(|(s, why)| json!({ "scope": s, "why": why })).collect::<Vec<_>>(),
        }),
    );
    status("Waiting for you to approve it on twitch.tv");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(code.expires_in);
    let mut interval = Duration::from_secs(code.interval.max(1));
    let tokens = loop {
        tokio::select! {
            _ = stop.notified() => return Ok(None),
            _ = tokio::time::sleep(interval) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("The code ran out before it was approved. Start again for a new one.");
        }
        match auth::poll(&http, root, &code.device_code).await {
            Ok(Poll::Pending) => {}
            Ok(Poll::SlowDown) => interval += Duration::from_secs(5),
            Ok(Poll::Denied) => bail!("The sign-in was refused on twitch.tv."),
            Ok(Poll::Expired) => bail!("The code ran out before it was approved. Start again for a new one."),
            Ok(Poll::Approved(tokens)) => break tokens,
            // A network blip while waiting is not the end of a sign-in the
            // person may be halfway through approving; the deadline is.
            Err(e) => tracing::debug!("twitch sign-in {login_id}: {e:#}"),
        }
    };

    status("Checking who signed in");
    let Some(who) = auth::validate(&http, root, &tokens.access_token).await? else {
        bail!("Twitch gave a token and then would not accept it. Try again.");
    };
    let now = chrono::Utc::now().timestamp();
    let existing = state.accounts.get_twitch(&format!("twitch:{}", who.login.to_lowercase()));
    let saved = state.accounts.add_twitch(TwitchAccountConfig {
        login: who.login,
        user_id: who.user_id,
        display_name: existing.as_ref().and_then(|e| e.display_name.clone()),
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at: now + tokens.expires_in,
        scopes: if who.scopes.is_empty() { tokens.scope } else { who.scopes },
        // Signing in again keeps the channels that were open: a renewed
        // sign-in is not a reason to forget where the person was.
        channels: existing.map(|e| e.channels).unwrap_or_default(),
        anonymous: false,
        use_tor: crate::net::route::router().wanted(&crate::net::route::pending_key("twitch")),
        // Never strict at sign-in; the account store keeps an existing
        // account's strict routing across a sign-in.
        strict_route: false,
    })?;
    Ok(Some(saved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A Twitch whose answers depend on what is asked and when: the code at
    /// once, "pending" a couple of times, then the tokens, then who they are.
    async fn twitch(approve_after: usize) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let polls = Arc::new(Mutex::new(0usize));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let polls = polls.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let head = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = head.lines().next().unwrap_or("").to_string();
                    let (status, body) = if line.starts_with("POST /oauth2/device") {
                        (200, r#"{"device_code":"dev","expires_in":60,"interval":1,"user_code":"WXYZ1234","verification_uri":"https://www.twitch.tv/activate?device-code=WXYZ1234"}"#.to_string())
                    } else if line.starts_with("POST /oauth2/token") {
                        let mut p = polls.lock().unwrap();
                        *p += 1;
                        if *p > approve_after {
                            (200, r#"{"access_token":"acc","refresh_token":"ref","expires_in":14000,"scope":["chat:read"],"token_type":"bearer"}"#.to_string())
                        } else {
                            (400, r#"{"status":400,"message":"authorization_pending"}"#.to_string())
                        }
                    } else if line.starts_with("GET /oauth2/validate") {
                        (200, r#"{"client_id":"c","login":"Someone","scopes":["chat:read"],"user_id":"42","expires_in":14000}"#.to_string())
                    } else {
                        (404, "{}".to_string())
                    };
                    let reply = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        root
    }

    async fn next_event(rx: &mut tokio::sync::broadcast::Receiver<crate::events::Event>, name: &str) -> serde_json::Value {
        loop {
            let event = tokio::time::timeout(Duration::from_secs(15), rx.recv()).await.expect("an event in time").expect("the bus is open");
            if event.name == name {
                return event.data;
            }
        }
    }

    #[tokio::test]
    async fn a_person_is_shown_a_code_approves_it_and_has_an_account() {
        let state = testkit::simulated_daemon("twitch-login");
        let mut events = state.events.subscribe();
        let root = twitch(1).await;
        start_at(state.clone(), "login-1".into(), root, false);

        let code = next_event(&mut events, "twitchLoginCode").await;
        assert_eq!(code["userCode"], "WXYZ1234");
        assert!(code["verificationUri"].as_str().unwrap().contains("device-code=WXYZ1234"));
        assert!(code["scopes"].as_array().unwrap().iter().any(|s| s["scope"] == "chat:send" || s["scope"] == "chat:edit"));

        let result = next_event(&mut events, "twitchLoginResult").await;
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["account"]["id"], "twitch:someone", "the login is read from Twitch and lower-cased");
        assert_eq!(result["account"]["service"], "twitch");
        let saved = state.accounts.get_twitch("twitch:someone").expect("saved");
        assert_eq!((saved.user_id.as_str(), saved.access_token.as_str(), saved.refresh_token.as_str()), ("42", "acc", "ref"));
        assert!(saved.expires_at > chrono::Utc::now().timestamp() + 13000);
    }

    #[tokio::test]
    async fn closing_the_form_ends_the_wait_without_saving_anything() {
        let state = testkit::simulated_daemon("twitch-login-cancel");
        let mut events = state.events.subscribe();
        let root = twitch(1000).await;
        start_at(state.clone(), "login-2".into(), root, false);
        next_event(&mut events, "twitchLoginCode").await;
        cancel("login-2");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(state.accounts.all_twitch().is_empty());
        assert!(waiting().lock().unwrap().get("login-2").is_none());
    }

    #[tokio::test]
    async fn an_unreachable_twitch_is_said_by_the_form() {
        let state = testkit::simulated_daemon("twitch-login-down");
        let mut events = state.events.subscribe();
        start_at(state.clone(), "login-3".into(), "http://127.0.0.1:9".into(), false);
        let result = next_event(&mut events, "twitchLoginResult").await;
        assert_eq!(result["success"], false);
        assert!(result["error"].as_str().unwrap().to_lowercase().contains("twitch"), "{result}");
    }
}
