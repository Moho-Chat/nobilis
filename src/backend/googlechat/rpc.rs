//! Google Chat's own daemon methods.
//!
//! `addGoogleChatAccount` takes what the sign-in window captured - the cookies Google set for chat.google.com, and the
//! window's user agent, so the daemon's requests look like the browser that made the session - checks that they sign
//! somebody in, and keeps the account. Signing in again to an account that is already here replaces its session and
//! keeps everything else about it.

use super::api::Api;
use super::session::{self, Session};
use crate::accounts::GoogleChatAccountConfig;
use crate::state::AppState;
use serde_json::{json, Value};

fn p_str<'a>(params: &'a Value, key: &str) -> Option<&'a str> {
    params.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

pub async fn dispatch(state: &AppState, method: &str, params: &Value) -> Option<(Option<Value>, Option<String>)> {
    match method {
        "addGoogleChatAccount" => Some(match add_account(state, params).await {
            Ok(id) => (Some(json!({ "ok": true, "accountId": id })), None),
            Err(e) => (None, Some(e)),
        }),
        _ => None,
    }
}

async fn add_account(state: &AppState, params: &Value) -> Result<String, String> {
    let cookies = session::parse_cookies(p_str(params, "cookies").ok_or("addGoogleChatAccount requires \"cookies\"")?);
    let missing = session::missing_cookies(&cookies);
    if !missing.is_empty() {
        return Err(format!("The Google sign-in did not finish: the session is missing {}. Try signing in again", missing.join(", ")));
    }
    let user_agent = p_str(params, "userAgent").unwrap_or(session::DEFAULT_USER_AGENT).to_string();
    let use_tor = params.get("useTor").and_then(Value::as_bool).unwrap_or(false);

    let key = crate::net::route::pending_key("googlechat");
    let router = crate::net::route::router();
    router.mark(&key, use_tor);
    if use_tor || router.tunnel_all() {
        router.ready(|_| {}).await.map_err(|e| format!("Tor or the proxy is not ready: {e:#}"))?;
    }
    let api = Api::new(Session::new(&key, cookies, &user_agent));
    let status = api.self_user_status().await.map_err(|e| e.to_string())?;
    let gaia = status.user_status.and_then(|s| s.user_id).map(|u| u.id().to_string()).filter(|s| !s.is_empty()).ok_or("Google Chat did not say whose session this is")?;
    // Re-signing an account from its menu must be the same Google account, or it would quietly become another one.
    if let Some(expected) = p_str(params, "accountId") {
        if expected != super::account_id(&gaia) {
            return Err("That sign-in was a different Google account from this one. Add it as a new account instead".into());
        }
    }
    let (mut name, mut email) = (String::new(), String::new());
    if let Ok(response) = api.members(std::slice::from_ref(&gaia)).await {
        for member in &response.members {
            if let Some(super::pb::member::Profile::User(user)) = &member.profile {
                name = user.name().to_string();
                email = user.email().to_string();
            }
        }
    }
    let config = GoogleChatAccountConfig {
        user_id: gaia,
        email,
        name,
        cookies: api.session.cookies(),
        user_agent,
        use_tor,
        ..Default::default()
    };
    let saved = state.accounts.add_googlechat(config).map_err(|e| format!("{e:#}"))?;
    let id = saved.account_id();
    state.accounts.set_disconnected(&id, false).ok();
    super::spawn(state.clone(), saved);
    state.events.emit("accountsChanged", json!({}));
    Ok(id)
}
