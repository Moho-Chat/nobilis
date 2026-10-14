//! An account's connection to Twitch.
//!
//! For now this is the part every later round stands on: keeping the
//! account's sign-in good. The access token lasts four hours; this renews it
//! before it stops, with the refresh token, which is single-use - so what
//! comes back is written down before anything else is done with it - and
//! checks who it belongs to when it starts and every so often after. A token
//! Twitch will not accept any more (revoked, or thirty days unused) stops the
//! account in the one state that says "sign in again", where retrying could
//! never help. Reading and sending chat are added to this loop as the plan's
//! later rounds are built.
//!
//! An anonymous account has nothing to keep good and is connected at once.

use super::auth::{self, Renewal};
use crate::accounts::TwitchAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::Result;
use std::time::Duration;

/// How often a good token is checked again with Twitch, in seconds. Twitch
/// asks apps to validate from time to time, and it is how a token revoked on
/// twitch.tv is noticed before it is next needed.
const VALIDATE_EVERY: i64 = 60 * 60;

pub fn spawn(state: AppState, config: TwitchAccountConfig) {
    let account_id = config.account_id();
    let routed = state.accounts.all_twitch().iter().any(|a| a.use_tor) || config.use_tor;
    crate::net::route::router().mark(super::SHARED_KEY, routed);
    state.runtime.reset_connection(&account_id);
    let join_handle = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run(&state, &account_id, auth::ID_ROOT).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, join_handle.abort_handle());
}

/// What keeping a sign-in good came to.
#[derive(Debug, PartialEq)]
pub enum Kept {
    /// Good until the given time (Unix seconds); look again then.
    Good(i64),
    /// Twitch will not accept it. Sign in again.
    SignInAgain(String),
}

/// Makes the account's token good, renewing it if it is due or Twitch has
/// stopped accepting it, and saves what it learned.
pub async fn keep_good(state: &AppState, account_id: &str, root: &str) -> Result<Kept> {
    let http = super::client()?;
    let Some(cfg) = state.accounts.get_twitch(account_id) else {
        return Ok(Kept::SignInAgain("this account was removed".into()));
    };
    let now = chrono::Utc::now().timestamp();
    let mut token = cfg.access_token.clone();
    let mut expires_at = cfg.expires_at;

    // Due: renewed before anything is asked of the old one.
    let mut due = now >= expires_at - auth::RENEW_BEFORE;
    if !due {
        // Not due by the clock. Twitch may still have ended it - asked now, so that is found here.
        match auth::validate(&http, root, &token).await? {
            Some(who) => {
                state.accounts.mutate_twitch(account_id, |a| {
                    a.user_id = who.user_id.clone();
                    if !who.scopes.is_empty() {
                        a.scopes = who.scopes.clone();
                    }
                })?;
                let renew = auth::renew_at(now, who.expires_in);
                return Ok(Kept::Good(renew.min(now + VALIDATE_EVERY)));
            }
            None => due = true,
        }
    }
    if due {
        match auth::refresh(&http, root, &cfg.refresh_token).await {
            Ok(tokens) => {
                token = tokens.access_token.clone();
                expires_at = now + tokens.expires_in;
                // Written before it is used: the old refresh token is spent, and
                // a crash between here and the write would lose the account.
                state.accounts.mutate_twitch(account_id, |a| {
                    a.access_token = tokens.access_token.clone();
                    a.refresh_token = tokens.refresh_token.clone();
                    a.expires_at = expires_at;
                    if !tokens.scope.is_empty() {
                        a.scopes = tokens.scope.clone();
                    }
                })?;
            }
            Err(Renewal::Refused(why)) => return Ok(Kept::SignInAgain(if why.is_empty() { "Twitch no longer accepts this sign-in".into() } else { why })),
            Err(Renewal::Failed(e)) => return Err(e),
        }
        // And who it is, now that it is new.
        if let Some(who) = auth::validate(&http, root, &token).await? {
            state.accounts.mutate_twitch(account_id, |a| a.user_id = who.user_id.clone())?;
        }
    }
    Ok(Kept::Good(auth::renew_at(now, (expires_at - now).max(0)).min(now + VALIDATE_EVERY)))
}

async fn run(state: &AppState, account_id: &str, root: &str) {
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    let mut wait = 5u64;
    loop {
        let Some(cfg) = state.accounts.get_twitch(account_id) else { return };
        if !cfg.is_signed_in() {
            // Anonymous: nothing to keep good. Reading is added in round 2.
            state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
            std::future::pending::<()>().await;
        }
        match keep_good(state, account_id, root).await {
            Ok(Kept::Good(until)) => {
                wait = 5;
                state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
                let now = chrono::Utc::now().timestamp();
                tokio::time::sleep(Duration::from_secs((until - now).max(30) as u64)).await;
            }
            Ok(Kept::SignInAgain(why)) => {
                state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&format!("{why}. Sign in to Twitch again from Accounts.")));
                return;
            }
            // The network, or Twitch having a bad moment: the token may be fine.
            Err(e) => {
                tracing::debug!("twitch[{account_id}]: {e:#}");
                state.runtime.report_progress(state, account_id, "Cannot reach Twitch - trying again");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                wait = (wait * 2).min(300);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A Twitch that answers requests by what they are, with a count of each.
    async fn twitch(refresh: (u16, &'static str), valid: bool) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<String>>> = Default::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let text = String::from_utf8_lossy(&buf[..n]).to_string();
                    let line = text.lines().next().unwrap_or("").to_string();
                    log.lock().unwrap().push(format!("{line} | {}", text.split("\r\n\r\n").nth(1).unwrap_or("")));
                    let (status, body) = if line.starts_with("POST /oauth2/token") {
                        (refresh.0, refresh.1.to_string())
                    } else if line.starts_with("GET /oauth2/validate") {
                        if valid {
                            (200, r#"{"client_id":"c","login":"someone","scopes":["chat:read","chat:edit"],"user_id":"42","expires_in":10000}"#.to_string())
                        } else {
                            (401, r#"{"message":"invalid access token"}"#.to_string())
                        }
                    } else {
                        (404, "{}".to_string())
                    };
                    let reply = format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        (root, seen)
    }

    const NEW: &str = r#"{"access_token":"new-acc","refresh_token":"new-ref","expires_in":14000,"scope":["chat:read"],"token_type":"bearer"}"#;

    fn signed_in(state: &AppState, expires_in: i64) {
        state
            .accounts
            .add_twitch(TwitchAccountConfig {
                login: "someone".into(),
                user_id: "42".into(),
                access_token: "old-acc".into(),
                refresh_token: "old-ref".into(),
                expires_at: chrono::Utc::now().timestamp() + expires_in,
                ..Default::default()
            })
            .unwrap();
    }

    #[tokio::test]
    async fn a_token_with_time_left_is_checked_and_left_alone() {
        let state = testkit::simulated_daemon("twitch-keep-good");
        signed_in(&state, 10_000);
        let (root, seen) = twitch((500, "{}"), true).await;
        let kept = keep_good(&state, "twitch:someone", &root).await.unwrap();
        assert!(matches!(kept, Kept::Good(_)));
        assert_eq!(state.accounts.get_twitch("twitch:someone").unwrap().access_token, "old-acc");
        assert!(seen.lock().unwrap().iter().all(|l| !l.contains("/oauth2/token")), "no renewal when none is due");
    }

    #[tokio::test]
    async fn a_token_that_is_nearly_out_is_renewed_and_the_new_pair_is_kept() {
        let state = testkit::simulated_daemon("twitch-renew");
        signed_in(&state, 100);
        let (root, seen) = twitch((200, NEW), true).await;
        let kept = keep_good(&state, "twitch:someone", &root).await.unwrap();
        assert!(matches!(kept, Kept::Good(_)));
        let saved = state.accounts.get_twitch("twitch:someone").unwrap();
        assert_eq!((saved.access_token.as_str(), saved.refresh_token.as_str()), ("new-acc", "new-ref"));
        assert!(saved.expires_at > chrono::Utc::now().timestamp() + 13_000);
        assert!(seen.lock().unwrap().iter().any(|l| l.contains("refresh_token=old-ref")));
    }

    #[tokio::test]
    async fn a_token_twitch_has_ended_is_renewed_before_its_time_and_a_dead_refresh_means_sign_in_again() {
        let state = testkit::simulated_daemon("twitch-revoked");
        signed_in(&state, 10_000);
        // Not due by the clock, but Twitch will not have it: renewed, and the renewal is refused too.
        let (root, _) = twitch((400, r#"{"status":400,"message":"Invalid refresh token"}"#), false).await;
        match keep_good(&state, "twitch:someone", &root).await.unwrap() {
            Kept::SignInAgain(why) => assert_eq!(why, "Invalid refresh token"),
            other => panic!("{other:?}"),
        }
        // The account is kept: it is the sign-in that has to be redone, not the account.
        assert!(state.accounts.get_twitch("twitch:someone").is_some());
    }

    #[tokio::test]
    async fn twitch_being_unreachable_is_an_error_to_retry_and_not_a_verdict() {
        let state = testkit::simulated_daemon("twitch-down");
        signed_in(&state, 100);
        assert!(keep_good(&state, "twitch:someone", "http://127.0.0.1:9").await.is_err());
        assert_eq!(state.accounts.get_twitch("twitch:someone").unwrap().refresh_token, "old-ref", "nothing was spent");
    }

    #[tokio::test]
    async fn an_account_that_was_removed_is_not_kept() {
        let state = testkit::simulated_daemon("twitch-gone");
        assert!(matches!(keep_good(&state, "twitch:nobody", "http://127.0.0.1:9").await.unwrap(), Kept::SignInAgain(_)));
    }
}
