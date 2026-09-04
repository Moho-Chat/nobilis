//! Thin reqwest-based Matrix Client-Server API HTTP client. Plain
//! rustls-backed reqwest (already `ring`-pinned by main.rs) - Matrix
//! homeservers are ordinary clearnet HTTPS, unlike Sneedchat's Tor-tunneled
//! transport, so there's no need for that backend's hand-rolled hyper
//! client over a custom `Transport`.

use anyhow::{bail, Context, Result};
use serde_json::Value;

pub fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        // Pinned to HTTP/1.1. Enabling reqwest's http2 feature (which the
        // file-upload path needs - see upload::http_client) would otherwise
        // let every client here negotiate h2 as a side effect, changing the
        // transport under a backend that works and is tested as it stands.
        // Nothing here wants h2; if it ever does, that is its own change.
        reqwest::Client::builder().http1_only().build().unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// Turns whatever somebody typed into a base URL the client API answers on.
///
/// Three things happen here, and each is a way people actually get this wrong.
///
/// **A missing scheme.** `matrix.org` is not a URL, and `reqwest` refuses it
/// with a builder error that surfaces only as a reconnect banner. Assumed
/// https, since a homeserver that is not is not one to send a password to.
///
/// **Delegation.** A homeserver is named by the domain in a user id, and that
/// domain need not be where the client API lives - `@you:example.com` is
/// commonly served from `matrix.example.com`. The server publishes where, at
/// `/.well-known/matrix/client`, and a client that does not ask has to be told
/// by hand. This asks.
///
/// **Port 8448.** That is the *federation* port. It does not serve
/// `/_matrix/client/v3/*` at all, so pointing at it fails with a bare 404 and
/// no hint what is wrong - and the reconnect loop then retries it forever.
/// It is a common and reasonable-looking mistake, so it is named rather than
/// silently rewritten: guessing that somebody meant 443 would be right most
/// of the time and wrong on the servers that really do run the client API on
/// an odd port.
///
/// A server that publishes nothing keeps the address as given, which is the
/// right answer for a homeserver on its own domain.
pub async fn resolve_homeserver(typed: &str) -> Result<String> {
    let with_scheme = normalise_homeserver(typed)?;
    // Best-effort by design: a server with no well-known is the ordinary case,
    // and so is one that answers with something that is not JSON. Either way
    // the address as given is the answer.
    match discover(&with_scheme).await {
        Some(delegated) => Ok(delegated),
        None => Ok(with_scheme),
    }
}

/// The part of the above that needs no network: a scheme, and a refusal.
fn normalise_homeserver(typed: &str) -> Result<String> {
    let typed = typed.trim().trim_end_matches('/');
    if typed.is_empty() {
        bail!("no homeserver address given");
    }
    let with_scheme = if typed.contains("://") { typed.to_string() } else { format!("https://{typed}") };

    let parsed = url::Url::parse(&with_scheme).with_context(|| format!("{typed} is not an address"))?;
    if parsed.port() == Some(8448) {
        bail!(
            "{typed} is the federation port, which does not serve the client API - use the address without :8448, or whatever your server's own /.well-known/matrix/client points at"
        );
    }
    Ok(with_scheme)
}

async fn discover(base: &str) -> Option<String> {
    let url = format!("{}/.well-known/matrix/client", base.trim_end_matches('/'));
    let resp = http_client().get(&url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body: Value = resp.json().await.ok()?;
    let delegated = body
        .get("m.homeserver")
        .and_then(|h| h.get("base_url"))
        .and_then(|u| u.as_str())?
        .trim()
        .trim_end_matches('/');
    // A well-known naming something that is not a URL is worse than one that
    // names nothing: it would replace a working address with a broken one.
    if delegated.is_empty() || url::Url::parse(delegated).is_err() {
        return None;
    }
    Some(delegated.to_string())
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

/// A GET with no credential, for the handful of endpoints that take none -
/// asking a homeserver how it lets people sign in is the first thing that
/// happens, before there is anybody to be.
pub async fn get_json_anonymous(url: &str) -> Result<Value> {
    let resp = http_client().get(url).send().await.context("request failed")?;
    handle_response(resp).await
}

pub async fn get_json(url: &str, token: &str) -> Result<Value> {
    let resp = http_client().get(url).bearer_auth(token).send().await.context("request failed")?;
    handle_response(resp).await
}

pub async fn delete_json(url: &str, token: &str) -> Result<Value> {
    let resp = http_client().delete(url).bearer_auth(token).send().await.context("request failed")?;
    handle_response(resp).await
}

pub async fn put_json(url: &str, token: &str, body: Value) -> Result<Value> {
    let resp = http_client().put(url).bearer_auth(token).json(&body).send().await.context("request failed")?;
    handle_response(resp).await
}

/// Raw bytes, not JSON - for media downloads (see mod.rs's media caching).
/// Returns the HTTP status and body regardless of success, same shape as
/// backend/sneedchat/http.rs's own get_bytes, so callers can distinguish
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

/// The same dance for a POST.
///
/// Uploading cross-signing keys is user-interactive-auth gated exactly the way
/// deleting a device is: the first attempt is refused with a 401 carrying a
/// session, and the second one repeats the body with the password attached.
/// The server decides which flows it accepts; a homeserver that will not take
/// a password says so in its own words rather than being guessed at here.
pub async fn post_with_password_uia(
    url: &str,
    token: &str,
    user_id: &str,
    password: &str,
    body: Value,
) -> Result<Value> {
    let resp = http_client().post(url).bearer_auth(token).json(&body).send().await.context("request failed")?;
    if resp.status().as_u16() != 401 {
        return handle_response(resp).await;
    }
    let challenge: Value = resp.json().await.context("invalid JSON response")?;
    let session = challenge["session"].as_str().context("no UIA session in 401 response")?.to_string();
    let mut authed = body;
    if let Some(object) = authed.as_object_mut() {
        object.insert(
            "auth".to_string(),
            serde_json::json!({
                "type": "m.login.password",
                "identifier": { "type": "m.id.user", "user": user_id },
                "password": password,
                "session": session,
            }),
        );
    }
    let resp = http_client().post(url).bearer_auth(token).json(&authed).send().await.context("request failed")?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assumes_https_where_no_scheme_was_typed() {
        // Without this, reqwest refuses the address with a builder error that
        // surfaces only as a reconnect banner saying nothing useful.
        assert_eq!(normalise_homeserver("matrix.org").unwrap(), "https://matrix.org");
        assert_eq!(normalise_homeserver("  matrix.org/  ").unwrap(), "https://matrix.org");
        // Something already carrying one is left exactly as written, including
        // a plain-http server somebody is deliberately running locally.
        assert_eq!(normalise_homeserver("https://matrix.example.com").unwrap(), "https://matrix.example.com");
        assert_eq!(normalise_homeserver("http://localhost:8008").unwrap(), "http://localhost:8008");
    }

    #[test]
    fn refuses_the_federation_port_by_name() {
        // 8448 does not serve the client API at all, so this fails with a bare
        // 404 and a reconnect loop that retries it forever. Named rather than
        // rewritten: guessing at 443 would be right most of the time and wrong
        // on a server genuinely running the client API on an odd port.
        let err = normalise_homeserver("https://example.com:8448").unwrap_err().to_string();
        assert!(err.contains("federation port"), "{err}");
        assert!(err.contains("well-known"), "{err}");
        // Any other port is somebody's own arrangement and is left alone.
        assert!(normalise_homeserver("https://example.com:8008").is_ok());
    }

    #[test]
    fn says_so_when_there_is_nothing_to_resolve() {
        assert!(normalise_homeserver("").is_err());
        assert!(normalise_homeserver("   ").is_err());
    }
}
