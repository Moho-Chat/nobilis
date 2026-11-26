//! Adding an account: the sign-in a person takes part in.
//!
//! Two ways in, both ending in a session token Fluxer issued (which is what the account keeps; a password is never
//! kept):
//!
//! - **username or email and password**, then the six-digit code of an authenticator (or a backup code) if the account
//!   has one. Fluxer gates the password request with its ALTCHA proof of work, which is solved here (see `altcha`). A
//!   sign-in from a new place on an account without a second factor waits for the link Fluxer emails.
//! - **a pasted token**.
//!
//! It runs as its own task and talks to the window through events:
//!
//! - `fluxerLoginStatus` - what it is doing, for a line of text
//! - `fluxerLoginNeeds` - what is wanted: `step` is `mfa` (with `methods`) or `email` (the emailed link); `error` says
//!   why the last answer was refused
//! - `fluxerLoginResult` - done, with the account, or the reason it failed
//!
//! and the window answers with `submitFluxerCode`. An account is never made here.

use super::api::{Api, ApiError};
use super::discovery::{self, Instance};
use crate::accounts::FluxerAccountConfig;
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// How long a sign-in may wait for a person.
const PATIENCE: Duration = Duration::from_secs(5 * 60);
/// How long the emailed link is waited for, polled every few seconds (the poll's allowance is 60 a minute).
const EMAIL_PATIENCE: Duration = Duration::from_secs(10 * 60);
const POLL_EVERY: Duration = Duration::from_secs(3);

enum Answer {
    Code(String),
}

fn waiting() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<Answer>>> {
    static WAITING: std::sync::OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<Answer>>>> = std::sync::OnceLock::new();
    WAITING.get_or_init(Default::default)
}

/// Gives a sign-in the second factor Fluxer asked for.
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

/// How the person is signing in.
enum Way {
    Password { login: String, password: String },
    Token(String),
}

pub fn start(state: AppState, login_id: String, instance: String, login: String, password: String) {
    begin(state, login_id, instance, Way::Password { login, password });
}

pub fn start_token(state: AppState, login_id: String, instance: String, token: String) {
    begin(state, login_id, instance, Way::Token(token));
}

fn begin(state: AppState, login_id: String, instance: String, way: Way) {
    let (tx, rx) = mpsc::unbounded_channel();
    waiting().lock().unwrap().insert(login_id.clone(), tx);
    tokio::spawn(async move {
        let outcome = flow(&state, &login_id, &instance, way, rx).await;
        waiting().lock().unwrap().remove(&login_id);
        match outcome {
            Ok(Some(saved)) => {
                let account = crate::accounts::fluxer_account_to_json(&saved, "connecting");
                super::connect::spawn(state.clone(), saved);
                state.events.emit("fluxerLoginResult", json!({ "loginId": login_id, "success": true, "account": account }));
            }
            Ok(None) => {}
            Err(e) => {
                let error = format!("{e:#}");
                tracing::info!("fluxer: sign-in {login_id}: {error}");
                state.events.emit("fluxerLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
            }
        }
    });
}

fn status(state: &AppState, login_id: &str, text: &str) {
    state.events.emit("fluxerLoginStatus", json!({ "loginId": login_id, "text": text }));
}

fn needs(state: &AppState, login_id: &str, step: &str, extra: Value, error: Option<&str>) {
    let mut event = json!({ "loginId": login_id, "step": step, "error": error });
    if let (Some(event), Some(extra)) = (event.as_object_mut(), extra.as_object()) {
        event.extend(extra.clone());
    }
    state.events.emit("fluxerLoginNeeds", event);
}

/// Whether the form was closed: the sign-in is no longer in the waiting list.
fn closed(login_id: &str) -> bool {
    !waiting().lock().unwrap().contains_key(login_id)
}

async fn flow(state: &AppState, login_id: &str, typed: &str, way: Way, mut rx: mpsc::UnboundedReceiver<Answer>) -> Result<Option<FluxerAccountConfig>> {
    let origin = discovery::normalize_origin(typed).ok_or_else(|| anyhow!("that does not look like the address of a Fluxer instance"))?;
    status(state, login_id, "Reaching the instance");
    let key = crate::net::route::pending_key("fluxer");
    let router = crate::net::route::router();
    if router.routed(&key) {
        router.ready(|msg| status(state, login_id, msg)).await.map_err(|e| anyhow!("starting Tor or reaching the proxy: {e:#}"))?;
    }
    let instance = discovery::fetch(&key, &origin).await?;
    let anonymous = Api::new(&key, &instance.api, None);
    let made_here = !matches!(way, Way::Token(_));
    let token = match way {
        Way::Token(token) => token.trim().trim_start_matches("Bearer ").to_string(),
        Way::Password { login, password } => {
            if login.trim().is_empty() || password.is_empty() {
                bail!("a username or email, and a password, are needed");
            }
            status(state, login_id, if instance.altcha { "Working out Fluxer's proof of work, then signing in" } else { "Signing in" });
            match password_flow(state, login_id, &anonymous, login.trim(), &password, &mut rx).await? {
                Some(token) => token,
                None => return Ok(None),
            }
        }
    };
    if token.is_empty() {
        bail!("there is no token");
    }
    // Who it is, which is also the proof the token is good.
    let session = Api::new(&key, &instance.api, Some(token.clone()));
    let me = session.get("/users/@me", &[]).await.map_err(|e| match e {
        ApiError::Unauthorized => anyhow!("Fluxer did not accept that token"),
        other => anyhow!("{other}"),
    })?;
    let user_id = me["id"].as_str().ok_or_else(|| anyhow!("Fluxer did not say who this is"))?.to_string();
    let config = FluxerAccountConfig { instance: origin, user_id, username: username_of(&me, &instance), token, made_here, use_tor: crate::net::route::router().wanted(&key), ..Default::default() };
    Ok(Some(state.accounts.add_fluxer(config)?))
}

/// The name an account goes by: `name#0001`, or just `name` where the instance has no tags.
pub fn username_of(user: &Value, _instance: &Instance) -> String {
    let name = user["username"].as_str().unwrap_or("");
    match user["discriminator"].as_str().filter(|d| !d.is_empty() && d.chars().any(|c| c != '0')) {
        Some(tag) => format!("{name}#{tag}"),
        None => name.to_string(),
    }
}

/// The password request, then whatever Fluxer asks for next. Answers the session token.
async fn password_flow(state: &AppState, login_id: &str, api: &Api, login: &str, password: &str, rx: &mut mpsc::UnboundedReceiver<Answer>) -> Result<Option<String>> {
    let body = json!({ "login": login, "password": password });
    let reply = match api.gated(Method::POST, "/auth/login", &body).await {
        Ok(reply) => reply,
        Err(ApiError::Refused { code, detail, .. }) if code == "IP_AUTHORIZATION_REQUIRED" => {
            let ticket = detail["ticket"].as_str().ok_or_else(|| anyhow!("Fluxer wants this sign-in approved by email, but gave nothing to wait on"))?.to_string();
            return email_flow(state, login_id, api, &ticket).await;
        }
        Err(e) => bail!("{e}"),
    };
    if let Some(token) = reply["token"].as_str() {
        return Ok(Some(token.to_string()));
    }
    if reply["mfa"].as_bool() != Some(true) {
        bail!("Fluxer answered something moho does not know how to follow");
    }
    let ticket = reply["ticket"].as_str().ok_or_else(|| anyhow!("Fluxer did not give a ticket"))?.to_string();
    let methods = reply["allowed_methods"].clone();
    if !methods.as_array().is_some_and(|m| m.iter().any(|x| x == "totp" || x == "backup_codes")) {
        bail!("This account's second factor is a passkey, which moho cannot use. Sign in with a code from another Fluxer app instead");
    }
    needs(state, login_id, "mfa", json!({ "methods": methods }), None);
    loop {
        let Ok(Some(Answer::Code(code))) = tokio::time::timeout(PATIENCE, rx.recv()).await else { return Ok(None) };
        match api.post("/auth/login/mfa/totp", &json!({ "code": code.replace([' ', '-'], ""), "ticket": ticket })).await {
            Ok(done) => return done["token"].as_str().map(|t| Some(t.to_string())).ok_or_else(|| anyhow!("Fluxer did not give a session")),
            Err(e) if e.field_code() == Some("INVALID_CODE") => needs(state, login_id, "mfa", json!({ "methods": methods }), Some("That code is not right. Check it and try again")),
            Err(e) => bail!("{e}"),
        }
    }
}

/// Waits for the link Fluxer emailed to approve a sign-in from here.
async fn email_flow(state: &AppState, login_id: &str, api: &Api, ticket: &str) -> Result<Option<String>> {
    needs(state, login_id, "email", json!({}), None);
    let started = Instant::now();
    while started.elapsed() < EMAIL_PATIENCE {
        tokio::time::sleep(POLL_EVERY).await;
        if closed(login_id) {
            return Ok(None);
        }
        let poll = api.get("/auth/ip-authorization/poll", &[("ticket", ticket.to_string())]).await.map_err(|e| anyhow!("{e}"))?;
        if poll["completed"].as_bool() == Some(true) {
            return poll["token"].as_str().map(|t| Some(t.to_string())).ok_or_else(|| anyhow!("Fluxer approved the sign-in but gave no session"));
        }
    }
    bail!("The emailed link was not used in time. Start the sign-in again")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_username_has_its_tag_only_where_it_has_one() {
        let instance = Instance::default();
        assert_eq!(username_of(&json!({ "username": "ada", "discriminator": "0001" }), &instance), "ada#0001");
        assert_eq!(username_of(&json!({ "username": "ada", "discriminator": "0000" }), &instance), "ada");
        assert_eq!(username_of(&json!({ "username": "ada" }), &instance), "ada");
    }

    #[test]
    fn answers_for_a_sign_in_that_is_not_waiting_are_refused_in_words() {
        assert!(submit_code("fluxer-none", "123456").unwrap_err().to_string().contains("not waiting"));
        assert!(submit_code("fluxer-none", " ").unwrap_err().to_string().contains("type the code"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        waiting().lock().unwrap().insert("fluxer-t".into(), tx);
        submit_code("fluxer-t", " 123456 ").unwrap();
        assert!(matches!(rx.try_recv(), Ok(Answer::Code(c)) if c == "123456"));
        cancel("fluxer-t");
        assert!(closed("fluxer-t"));
    }
}
