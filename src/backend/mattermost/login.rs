//! Adding an account: the sign-in a person takes part in.
//!
//! Three ways, all to a server the person names:
//!
//! - a username or email and a password; if the account has a second factor the server says so, and the six-digit code
//!   of the authenticator app is asked for next;
//! - a personal access token made in the account's own settings (Profile → Security), pasted;
//! - the server's own sign-in page in a window (for single sign-on: GitLab, SAML, OpenID, LDAP), which leaves the
//!   session behind as the `MMAUTHTOKEN` cookie; the window hands that over as a token made here.
//!
//! Neither the password nor the code is kept: what the account keeps is the session token the server issued. It runs as
//! its own task and talks to the window through events:
//!
//! - `mattermostLoginStatus` - what it is doing, for a line of text
//! - `mattermostLoginNeeds` - `step` is `mfa`, and `error` says why the last code was refused
//! - `mattermostLoginResult` - done, with the account, or the reason it failed
//!
//! and the window answers with `submitMattermostCode`. An account is never made here.

use super::api::{Api, ApiError};
use crate::accounts::MattermostAccountConfig;
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;

/// How long a sign-in may wait for a person.
const PATIENCE: Duration = Duration::from_secs(5 * 60);

fn waiting() -> &'static Mutex<HashMap<String, mpsc::UnboundedSender<String>>> {
    static WAITING: std::sync::OnceLock<Mutex<HashMap<String, mpsc::UnboundedSender<String>>>> = std::sync::OnceLock::new();
    WAITING.get_or_init(Default::default)
}

/// Gives a sign-in the code the server asked for.
pub fn submit_code(login_id: &str, code: &str) -> Result<()> {
    if code.trim().is_empty() {
        bail!("type the code first");
    }
    let sender = waiting().lock().unwrap().get(login_id).cloned().ok_or_else(|| anyhow!("that sign-in is not waiting for anything any more"))?;
    sender.send(code.chars().filter(|c| !c.is_whitespace()).collect()).map_err(|_| anyhow!("that sign-in has ended"))
}

/// Stops a sign-in that is waiting, because the form was closed.
pub fn cancel(login_id: &str) {
    waiting().lock().unwrap().remove(login_id);
}

/// The server's address as it was typed: a scheme added where there is none, and anything after the host that is the
/// web app's own (a team's page, `/login`) taken off. A server served under a path (`example.org/chat`) keeps it.
pub fn normalize_server(typed: &str) -> Option<String> {
    let text = typed.trim().trim_end_matches('/');
    if text.is_empty() {
        return None;
    }
    let with_scheme = if text.contains("://") { text.to_string() } else { format!("https://{text}") };
    let mut url = url::Url::parse(&with_scheme).ok()?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/').to_string();
    // A team's page (`/<team>/channels/...`, `/<team>/messages/...`, `/<team>/pl/...`) is cut with its team.
    let team_cut = ["/channels/", "/messages/", "/pl/", "/threads"].iter().filter_map(|m| path.find(m)).min().map(|at| path[..at].rfind('/').unwrap_or(0));
    let cut = ["/login", "/signup_user_complete", "/api/v4"].iter().filter_map(|m| path.find(m)).chain(team_cut).min();
    let kept = match cut {
        Some(at) => path[..at].to_string(),
        None => path,
    };
    url.set_path(&kept);
    Some(url.to_string().trim_end_matches('/').to_string())
}

/// How the person is signing in.
enum Method {
    Password { login: String, password: String },
    /// A token, and whether it is a session made here (the window's sign-in page) rather than one belonging elsewhere.
    Token { token: String, made_here: bool },
}

/// Starts a sign-in by password. Answers at once; the rest is events.
pub fn start(state: AppState, login_id: String, server: String, login: String, password: String) {
    begin(state, login_id, server, Method::Password { login, password });
}

/// Starts a sign-in with a token: a personal access token pasted, or a session the sign-in page left.
pub fn start_token(state: AppState, login_id: String, server: String, token: String, made_here: bool) {
    begin(state, login_id, server, Method::Token { token, made_here });
}

fn begin(state: AppState, login_id: String, server: String, method: Method) {
    let (tx, rx) = mpsc::unbounded_channel();
    waiting().lock().unwrap().insert(login_id.clone(), tx);
    tokio::spawn(async move {
        let outcome = flow(&state, &login_id, &server, method, rx).await;
        waiting().lock().unwrap().remove(&login_id);
        match outcome {
            Ok(Some(saved)) => {
                let account = crate::accounts::mattermost_account_to_json(&saved, "connecting");
                super::connect::spawn(state.clone(), saved);
                state.events.emit("mattermostLoginResult", json!({ "loginId": login_id, "success": true, "account": account }));
            }
            Ok(None) => {}
            Err(e) => {
                let error = format!("{e:#}");
                tracing::info!("mattermost: sign-in {login_id}: {error}");
                state.events.emit("mattermostLoginResult", json!({ "loginId": login_id, "success": false, "error": error }));
            }
        }
    });
}

fn status(state: &AppState, login_id: &str, text: &str) {
    state.events.emit("mattermostLoginStatus", json!({ "loginId": login_id, "text": text }));
}

fn needs(state: &AppState, login_id: &str, error: Option<&str>) {
    state.events.emit("mattermostLoginNeeds", json!({ "loginId": login_id, "step": "mfa", "error": error }));
}

/// Whether a refusal of a password sign-in means a code is wanted (none was given) or the one given was wrong.
pub fn wants_code(e: &ApiError) -> bool {
    matches!(e.id(), "mfa.validate_token.authenticate.app_error" | "api.user.check_user_mfa.bad_code.app_error")
}

/// What the server says of itself, and `Err` if what answered is not Mattermost.
pub async fn ping(api: &Api) -> Result<Value> {
    let ping = api.get("/api/v4/system/ping", &[]).await.map_err(|e| match e {
        ApiError::Refused { status: 404, .. } => anyhow!("that address did not answer like a Mattermost server"),
        other => anyhow!("{other}"),
    })?;
    if ping["status"].as_str() != Some("OK") {
        bail!("that address did not answer like a Mattermost server");
    }
    Ok(ping)
}

async fn flow(state: &AppState, login_id: &str, server: &str, method: Method, mut rx: mpsc::UnboundedReceiver<String>) -> Result<Option<MattermostAccountConfig>> {
    let server = normalize_server(server).ok_or_else(|| anyhow!("that does not look like the address of a Mattermost server"))?;
    status(state, login_id, "Reaching the server");
    let key = crate::net::route::pending_key("mattermost");
    let anonymous = Api::new(&key, &server, None);
    ping(&anonymous).await?;

    let (token, made_here, method_name) = match method {
        Method::Token { token, made_here } => (token.trim().to_string(), made_here, if made_here { "sso" } else { "token" }),
        Method::Password { login, password } => {
            if login.trim().is_empty() || password.is_empty() {
                bail!("a username or email and a password are needed");
            }
            status(state, login_id, "Signing in");
            let mut body = json!({ "login_id": login.trim(), "password": password, "token": "" });
            let token = loop {
                match anonymous.request(reqwest::Method::POST, "/api/v4/users/login", &[], Some(&body)).await {
                    Ok(answer) => break answer.headers.get("token").and_then(|v| v.to_str().ok()).map(str::to_string).ok_or_else(|| anyhow!("the server did not give a session"))?,
                    Err(e) if wants_code(&e) => {
                        let first = body["token"].as_str().is_some_and(str::is_empty);
                        needs(state, login_id, (!first).then_some("That code is not right. Check it and try again"));
                        let Some(code) = tokio::time::timeout(PATIENCE, rx.recv()).await.ok().flatten() else { return Ok(None) };
                        body["token"] = json!(code);
                        status(state, login_id, "Checking the code");
                    }
                    Err(e) => bail!("{e}"),
                }
            };
            (token, true, "password")
        }
    };
    if token.is_empty() {
        bail!("a token is needed");
    }
    // Who it is, which is also the proof the token is good.
    let session = Api::new(&key, &server, Some(token.clone()));
    let me = session.get("/api/v4/users/me", &[]).await.map_err(|e| match e {
        ApiError::Unauthorized => anyhow!("The server did not accept that token"),
        other => anyhow!("{other}"),
    })?;
    let user_id = me["id"].as_str().ok_or_else(|| anyhow!("the server did not say who this is"))?.to_string();
    let config = MattermostAccountConfig {
        server,
        user_id,
        username: me["username"].as_str().unwrap_or("").to_string(),
        token,
        made_here,
        method: method_name.to_string(),
        use_tor: crate::net::route::router().wanted(&key),
        ..Default::default()
    };
    Ok(Some(state.accounts.add_mattermost(config)?))
}

/// Ends a session this client made, when its account is removed: a token pasted belongs to the person's other uses, and
/// is left alone.
pub async fn end_session(config: &MattermostAccountConfig) {
    if !config.made_here || config.token.is_empty() {
        return;
    }
    let api = Api::new(&config.account_id(), &config.server, Some(config.token.clone()));
    if let Err(e) = api.post("/api/v4/users/logout", &json!({})).await {
        tracing::info!("mattermost: ending the session at the server: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_address_of_a_server_is_made_from_what_was_typed() {
        assert_eq!(normalize_server("chat.example.org").as_deref(), Some("https://chat.example.org"));
        assert_eq!(normalize_server("https://chat.example.org/").as_deref(), Some("https://chat.example.org"));
        assert_eq!(normalize_server("https://chat.example.org/alpha/channels/town-square").as_deref(), Some("https://chat.example.org"));
        assert_eq!(normalize_server("https://chat.example.org/login?redirect_to=x").as_deref(), Some("https://chat.example.org"));
        assert_eq!(normalize_server("https://example.org/chat/").as_deref(), Some("https://example.org/chat"), "a server under a path keeps it");
        assert_eq!(normalize_server("http://127.0.0.1:8065").as_deref(), Some("http://127.0.0.1:8065"));
        assert_eq!(normalize_server(""), None);
        assert_eq!(normalize_server("ftp://example.org"), None);
    }

    #[test]
    fn a_code_is_asked_for_when_the_server_says_one_is_wanted_or_was_wrong() {
        let refused = |id: &str| ApiError::Refused { status: 401, id: id.into(), message: String::new() };
        assert!(wants_code(&refused("mfa.validate_token.authenticate.app_error")));
        assert!(wants_code(&refused("api.user.check_user_mfa.bad_code.app_error")));
        assert!(!wants_code(&refused("api.user.login.invalid_credentials_email_username")));
    }

    #[test]
    fn answers_for_a_sign_in_that_is_not_waiting_are_refused_in_words() {
        assert!(submit_code("mm-none", "123456").unwrap_err().to_string().contains("not waiting"));
        let (tx, mut rx) = mpsc::unbounded_channel();
        waiting().lock().unwrap().insert("mm-t".into(), tx);
        submit_code("mm-t", " 123 456 ").unwrap();
        assert_eq!(rx.try_recv().unwrap(), "123456");
        cancel("mm-t");
        assert!(submit_code("mm-t", "1").is_err());
    }
}
