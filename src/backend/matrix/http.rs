//! Thin reqwest-based Matrix Client-Server API HTTP client. Plain
//! rustls-backed reqwest (already `ring`-pinned by main.rs) - Matrix
//! homeservers are ordinary clearnet HTTPS, unlike Sneedchat's Tor-tunneled
//! transport, so there's no need for that backend's hand-rolled hyper
//! client over a custom `Transport`.

use anyhow::{bail, Context, Result};
use serde_json::Value;

pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// The Matrix C-S API's standard error body shape (`{"errcode": "M_...",
/// "error": "..."}`) - surfaced verbatim as the error message on a
/// non-2xx response, since these are meant to be shown to the user
/// (e.g. "M_FORBIDDEN: Invalid password").
#[derive(serde::Deserialize)]
struct MatrixError {
    errcode: String,
    error: String,
}

pub async fn post_json(url: &str, token: Option<&str>, body: Value) -> Result<Value> {
    let mut req = http_client().post(url).json(&body);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.context("request failed")?;
    handle_response(resp).await
}

pub async fn get_json(url: &str, token: &str) -> Result<Value> {
    let resp = http_client().get(url).bearer_auth(token).send().await.context("request failed")?;
    handle_response(resp).await
}

pub async fn put_json(url: &str, token: &str, body: Value) -> Result<Value> {
    let resp = http_client().put(url).bearer_auth(token).json(&body).send().await.context("request failed")?;
    handle_response(resp).await
}

/// Raw bytes, not JSON - for media downloads (see mod.rs's media caching).
/// Returns the HTTP status and body regardless of success, same shape as
/// backend/sockchat/http.rs's own get_bytes, so callers can distinguish
/// "fetch itself failed" from "server returned a non-2xx".
pub async fn get_bytes(url: &str, token: &str) -> Result<(u16, Vec<u8>)> {
    let resp = http_client().get(url).bearer_auth(token).send().await.context("request failed")?;
    let status = resp.status().as_u16();
    let bytes = resp.bytes().await.context("reading response body")?;
    Ok((status, bytes.to_vec()))
}

/// DELETEs a UIA (User-Interactive Auth)-gated endpoint using password
/// auth, handling the spec's standard two-step dance: an unauthenticated
/// attempt to learn the session id from the 401, then a retry with
/// `m.login.password` auth attached. Deleting a *device* requires this
/// (unlike most C-S API calls, a valid access token alone isn't enough -
/// removing a login session is exactly the kind of sensitive action UIA
/// exists to re-gate behind the account password) - see
/// backend/matrix/verification.rs's delete_device, the only caller today.
pub async fn delete_with_password_uia(url: &str, token: &str, user_id: &str, password: &str) -> Result<Value> {
    let resp = http_client().delete(url).bearer_auth(token).json(&serde_json::json!({})).send().await.context("request failed")?;
    if resp.status().as_u16() != 401 {
        return handle_response(resp).await;
    }
    let body: Value = resp.json().await.context("invalid JSON response")?;
    let session = body["session"].as_str().context("no UIA session in 401 response")?.to_string();
    let auth_body = serde_json::json!({
        "auth": {
            "type": "m.login.password",
            "identifier": { "type": "m.id.user", "user": user_id },
            "password": password,
            "session": session,
        }
    });
    let resp = http_client().delete(url).bearer_auth(token).json(&auth_body).send().await.context("request failed")?;
    handle_response(resp).await
}

async fn handle_response(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body: Value = resp.json().await.context("invalid JSON response")?;
    if !status.is_success() {
        if let Ok(err) = serde_json::from_value::<MatrixError>(body.clone()) {
            bail!("{}: {}", err.errcode, err.error);
        }
        bail!("HTTP {status}: {body}");
    }
    Ok(body)
}
