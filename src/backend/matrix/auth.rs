//! `m.login.password` authentication + device_id persistence across
//! restarts.
//!
//! Reusing the same device_id on every login is not optional bookkeeping -
//! it's what keeps this account's Olm identity keys (and therefore every
//! Megolm session anyone has ever shared with this device) alive across
//! restarts. A fresh device_id each time is indistinguishable, from every
//! other device's perspective, from a brand new device nobody has ever
//! established a session with: encrypted history received before that
//! point becomes permanently undecryptable, and other users' clients have
//! to re-establish sessions (and often re-prompt for verification) with
//! what looks like a stranger.

use super::http::post_json;
use anyhow::{Context, Result};
use serde_json::json;

pub struct LoginResult {
    pub user_id: String,
    pub access_token: String,
    pub device_id: String,
}

/// Logs in via `m.login.password`. `device_id`, when `Some`, asks the
/// homeserver to reuse (or create-with-this-id) that specific device
/// rather than minting a fresh one - see the module doc above for why this
/// matters for E2EE continuity.
pub async fn login(homeserver_url: &str, username: &str, password: &str, device_id: Option<&str>) -> Result<LoginResult> {
    let url = format!("{}/_matrix/client/v3/login", homeserver_url.trim_end_matches('/'));
    let mut body = json!({
        "type": "m.login.password",
        "identifier": { "type": "m.id.user", "user": username },
        "password": password,
        "initial_device_display_name": "nobilis",
    });
    if let Some(device_id) = device_id {
        body["device_id"] = json!(device_id);
    }

    let resp = post_json(&url, None, body).await.context("login request failed")?;
    let user_id = resp["user_id"].as_str().context("login response missing user_id")?.to_string();
    let access_token = resp["access_token"].as_str().context("login response missing access_token")?.to_string();
    let device_id = resp["device_id"].as_str().context("login response missing device_id")?.to_string();

    Ok(LoginResult { user_id, access_token, device_id })
}

/// Which ways a homeserver will let somebody sign in.
///
/// Asked before anything is typed, because the answer decides what to ask
/// for: a server with only `m.login.sso` has no password to take, and a form
/// that demands one there is a form nobody can complete.
pub async fn login_flows(homeserver_url: &str) -> Result<Vec<String>> {
    let url = format!("{}/_matrix/client/v3/login", homeserver_url.trim_end_matches('/'));
    let resp = super::http::get_json_anonymous(&url).await.context("asking how to sign in")?;
    Ok(resp["flows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|flow| flow["type"].as_str().map(|s| s.to_string()))
        .collect())
}

/// Finishes an SSO sign-in with the one-time token the browser came back
/// with.
///
/// The same endpoint as a password login, with the type that says a third
/// party already did the identifying. `device_id` is reused for exactly the
/// reason the module doc gives - the identity has to survive.
pub async fn login_with_token(homeserver_url: &str, token: &str, device_id: Option<&str>) -> Result<LoginResult> {
    let url = format!("{}/_matrix/client/v3/login", homeserver_url.trim_end_matches('/'));
    let mut body = json!({
        "type": "m.login.token",
        "token": token,
        "initial_device_display_name": "nobilis",
    });
    if let Some(device_id) = device_id {
        body["device_id"] = json!(device_id);
    }
    let resp = post_json(&url, None, body).await.context("finishing the sign-in")?;
    Ok(LoginResult {
        user_id: resp["user_id"].as_str().context("login response missing user_id")?.to_string(),
        access_token: resp["access_token"].as_str().context("login response missing access_token")?.to_string(),
        device_id: resp["device_id"].as_str().context("login response missing device_id")?.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase-1 live check: a real `m.login.password` against a real
    /// homeserver. Reads credentials from the environment rather than
    /// taking them as literals anywhere in this repo or conversation - set
    /// them in your own shell before running:
    ///
    ///   MATRIX_USERNAME=... MATRIX_PASSWORD=... \
    ///     cargo test --release -- --ignored --nocapture matrix_login_probe
    ///
    /// MATRIX_HOMESERVER defaults to https://matrix.org if unset. Skips
    /// (rather than failing) if MATRIX_USERNAME/PASSWORD aren't set, so
    /// this is safe to leave in the suite without real credentials present
    /// in CI or anyone else's environment.
    #[tokio::test]
    #[ignore]
    async fn matrix_login_probe() {
        let Ok(username) = std::env::var("MATRIX_USERNAME") else {
            println!("MATRIX_USERNAME not set, skipping");
            return;
        };
        let Ok(password) = std::env::var("MATRIX_PASSWORD") else {
            println!("MATRIX_PASSWORD not set, skipping");
            return;
        };
        let homeserver = std::env::var("MATRIX_HOMESERVER").unwrap_or_else(|_| "https://matrix.org".to_string());

        let _ = rustls::crypto::ring::default_provider().install_default();

        println!("logging in to {homeserver} as {username}...");
        let result = login(&homeserver, &username, &password, None).await.expect("login failed");
        println!("logged in. user_id={} device_id={}", result.user_id, result.device_id);
        assert!(!result.access_token.is_empty(), "access_token was empty");

        println!("logging in again with the same device_id to confirm reuse...");
        let second = login(&homeserver, &username, &password, Some(&result.device_id)).await.expect("second login failed");
        assert_eq!(second.device_id, result.device_id, "homeserver did not reuse the supplied device_id");
        println!("device_id reuse confirmed.");
    }
}
