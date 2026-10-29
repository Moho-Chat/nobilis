//! Adding an account: the sign-in a person takes part in.
//!
//! The person types the instance (the official one unless they run their own), their email and password; or pastes a
//! session token. If the account has a second factor Stoat asks for it next: the six-digit code of an authenticator
//! app, or a recovery code. Neither the password nor the code is kept: what the account keeps is the session token
//! Stoat issued, which is as private as a Discord token and handled the same way.
//!
//! It is a conversation that can take minutes, so it runs as its own task and talks to the window through events:
//!
//! - `stoatLoginStatus` - what it is doing, for a line of text
//! - `stoatLoginNeeds` - what Stoat is asking for: `step` is `mfa`, with the `methods` it accepts, and `error` says why
//!   the last answer was refused
//! - `stoatLoginResult` - done, with the account, or the reason it failed
//!
//! and the window answers with `submitStoatCode`. An account is never made here, and a captcha or an unverified email
//! is said and left to Stoat's own app: answering a captcha for a person is what gets an account flagged.

use super::api::{self, Api, ApiError};
use crate::accounts::StoatAccountConfig;
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

/// How long a sign-in may wait for a person.
const PATIENCE: Duration = Duration::from_secs(5 * 60);

/// What the window can hand to a sign-in that is waiting.
enum Answer {
    Code(String),
}

fn waiting() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<Answer>>> {
    static WAITING: std::sync::OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<Answer>>>> = std::sync::OnceLock::new();
    WAITING.get_or_init(Default::default)
}

/// Gives a sign-in the second factor Stoat asked for.
pub fn submit_code(login_id: &str, code: &str) -> Result<()> {
    if code.trim().is_empty() {
        bail!("type the code first");
    }
    let sender = waiting().lock().unwrap().get(login_id).cloned().ok_or_else(|| anyhow!("that sign-in is not waiting for anything any more"))?;
    sender.send(Answer::Code(code.trim().to_string())).map_err(|_| anyhow!("that sign-in has ended"))
}

/// Stops a sign-in that is waiting, because the form was closed.
pub fn cancel(login_id: &str) {
    waiting().lock().unwrap().remove(login_id);
}

/// How a second factor is said to Stoat: an authenticator's code is digits, and a recovery code is not.
pub fn mfa_response(code: &str) -> Value {
    let digits: String = code.chars().filter(|c| c.is_ascii_digit()).collect();
    let spaced = code.chars().all(|c| c.is_ascii_digit() || c == ' ' || c == '-');
    if spaced && digits.len() == 6 {
        json!({ "totp_code": digits })
    } else {
        json!({ "recovery_code": code.trim() })
    }
}

/// The address of an instance's API as it was typed: nothing means the official one, and the address of the official
/// web app (which is not the API) is turned into the API's.
pub fn normalize_instance(typed: &str) -> Option<String> {
    let text = typed.trim().trim_end_matches('/');
    if text.is_empty() {
        return Some(api::DEFAULT_INSTANCE.to_string());
    }
    let with_scheme = if text.contains("://") { text.to_string() } else { format!("https://{text}") };
    let url = url::Url::parse(&with_scheme).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?;
    if matches!(host, "stoat.chat" | "app.stoat.chat" | "revolt.chat" | "app.revolt.chat") {
        return Some(api::DEFAULT_INSTANCE.to_string());
    }
    Some(with_scheme)
}

/// How the person is signing in.
enum Method {
    Password { email: String, password: String },
    Token(String),
}

/// Starts a sign-in by email and password. Answers at once; the rest is events.
pub fn start(state: AppState, login_id: String, instance: String, email: String, password: String) {
    begin(state, login_id, instance, Method::Password { email, password });
}

/// Starts a sign-in with a session token the person already has.
pub fn start_token(state: AppState, login_id: String, instance: String, token: String) {
    begin(state, login_id, instance, Method::Token(token));
}

fn begin(state: AppState, login_id: String, instance: String, method: Method) {
    let (tx, rx) = mpsc::unbounded_channel();
    waiting().lock().unwrap().insert(login_id.clone(), tx);
    tokio::spawn(async move {
        let outcome = flow(&state, &login_id, &instance, method, rx).await;
        waiting().lock().unwrap().remove(&login_id);
        match outcome {
            Ok(Some(saved)) => {
                let account = crate::accounts::stoat_account_to_json(&saved, "connecting");
                super::connect::spawn(state.clone(), saved);
                state.events.emit("stoatLoginResult", json!({ "loginId": login_id, "success": true, "account": account }));
            }
            // The form was closed; nobody is waiting for an answer.
            Ok(None) => {}
            Err(e) => {
                let error = format!("{e:#}");
                tracing::info!("stoat: sign-in {login_id}: {error}");
                state.events.emit("stoatLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
            }
        }
    });
}

fn status(state: &AppState, login_id: &str, text: &str) {
    state.events.emit("stoatLoginStatus", json!({ "loginId": login_id, "text": text }));
}

fn needs(state: &AppState, login_id: &str, methods: &Value, error: Option<&str>) {
    state.events.emit("stoatLoginNeeds", json!({ "loginId": login_id, "step": "mfa", "methods": methods, "error": error }));
}

async fn answer(rx: &mut mpsc::UnboundedReceiver<Answer>) -> Option<Answer> {
    tokio::time::timeout(PATIENCE, rx.recv()).await.ok().flatten()
}

async fn flow(state: &AppState, login_id: &str, instance: &str, method: Method, mut rx: mpsc::UnboundedReceiver<Answer>) -> Result<Option<StoatAccountConfig>> {
    let instance = normalize_instance(instance).ok_or_else(|| anyhow!("that does not look like the address of a Stoat instance"))?;
    status(state, login_id, "Reaching the instance");
    let key = crate::net::route::pending_key("stoat");
    let anonymous = Api::new(&key, &instance, None);
    api::instance_info(&anonymous).await?;

    let (token, session_id) = match &method {
        Method::Token(token) => (token.trim().to_string(), String::new()),
        Method::Password { email, password } => {
            if email.trim().is_empty() || password.is_empty() {
                bail!("an email and a password are needed");
            }
            status(state, login_id, "Signing in");
            let first = json!({ "email": email.trim(), "password": password, "friendly_name": super::session_name() });
            match signed_in(&anonymous, state, login_id, first, &mut rx).await? {
                Some(session) => session,
                None => return Ok(None),
            }
        }
    };
    // Who it is, which is also the proof the token is good.
    let session = Api::new(&key, &instance, Some(token.clone()));
    let me = session.get("/users/@me", &[]).await.map_err(|e| match e {
        ApiError::Unauthorized => anyhow!("Stoat did not accept that token"),
        other => anyhow!("{other}"),
    })?;
    let user_id = me["_id"].as_str().ok_or_else(|| anyhow!("Stoat did not say who this is"))?.to_string();
    let config = StoatAccountConfig {
        instance,
        user_id,
        username: format!("{}#{}", me["username"].as_str().unwrap_or(""), me["discriminator"].as_str().unwrap_or("")),
        token,
        session_id,
        use_tor: crate::net::route::router().wanted(&key),
        ..Default::default()
    };
    Ok(Some(state.accounts.add_stoat(config)?))
}

/// The login request, and the second factor if Stoat wants one. Answers the session token and its id.
async fn signed_in(api: &Api, state: &AppState, login_id: &str, first: Value, rx: &mut mpsc::UnboundedReceiver<Answer>) -> Result<Option<(String, String)>> {
    let mut reply = api.post("/auth/session/login", &first).await.map_err(|e| anyhow!("{e}"))?;
    loop {
        match reply["result"].as_str() {
            Some("Success") => {
                let token = reply["token"].as_str().ok_or_else(|| anyhow!("Stoat did not give a session"))?.to_string();
                return Ok(Some((token, reply["_id"].as_str().unwrap_or("").to_string())));
            }
            Some("Disabled") => bail!("This account is disabled"),
            Some("MFA") => {
                let ticket = reply["ticket"].as_str().ok_or_else(|| anyhow!("Stoat did not give a ticket"))?.to_string();
                let methods = reply["allowed_methods"].clone();
                needs(state, login_id, &methods, None);
                loop {
                    let Some(Answer::Code(code)) = answer(rx).await else { return Ok(None) };
                    let body = json!({ "mfa_ticket": ticket, "mfa_response": mfa_response(&code), "friendly_name": super::session_name() });
                    match api.post("/auth/session/login", &body).await {
                        Ok(next) => {
                            reply = next;
                            break;
                        }
                        Err(ApiError::Refused { kind, .. }) if matches!(kind.as_str(), "InvalidToken" | "InvalidOneTimeCode" | "InvalidCredentials") => {
                            needs(state, login_id, &methods, Some("That code is not right. Check it and try again"));
                        }
                        Err(e) => bail!("{e}"),
                    }
                }
            }
            _ => bail!("Stoat answered something moho does not know how to follow"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_of_an_instance_is_made_from_what_was_typed() {
        assert_eq!(normalize_instance("").as_deref(), Some("https://api.stoat.chat"));
        assert_eq!(normalize_instance("  ").as_deref(), Some("https://api.stoat.chat"));
        assert_eq!(normalize_instance("https://stoat.chat").as_deref(), Some("https://api.stoat.chat"), "the web app is not the API");
        assert_eq!(normalize_instance("app.revolt.chat/").as_deref(), Some("https://api.stoat.chat"));
        assert_eq!(normalize_instance("chat.example.org/api/").as_deref(), Some("https://chat.example.org/api"));
        assert_eq!(normalize_instance("http://localhost:8000").as_deref(), Some("http://localhost:8000"));
        assert_eq!(normalize_instance("ftp://example.org"), None);
        assert_eq!(normalize_instance("not a host"), None);
    }

    #[test]
    fn a_six_digit_code_is_an_authenticators_and_anything_else_is_a_recovery_code() {
        assert_eq!(mfa_response("123456"), json!({ "totp_code": "123456" }));
        assert_eq!(mfa_response("123 456"), json!({ "totp_code": "123456" }));
        assert_eq!(mfa_response("abcd-1234-efgh"), json!({ "recovery_code": "abcd-1234-efgh" }));
        assert_eq!(mfa_response("12345"), json!({ "recovery_code": "12345" }));
    }

    #[test]
    fn answers_for_a_sign_in_that_is_not_waiting_are_refused_in_words() {
        assert!(submit_code("stoat-none", "123456").unwrap_err().to_string().contains("not waiting"));
        assert!(submit_code("stoat-none", " ").unwrap_err().to_string().contains("type the code"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        waiting().lock().unwrap().insert("stoat-t".into(), tx);
        submit_code("stoat-t", " 123456 ").unwrap();
        assert!(matches!(rx.try_recv(), Ok(Answer::Code(c)) if c == "123456"));
        cancel("stoat-t");
        assert!(submit_code("stoat-t", "123456").is_err());
    }
}
