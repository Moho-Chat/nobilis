//! Thin reqwest-based Matrix Client-Server API HTTP client. Plain
//! rustls-backed reqwest (already `ring`-pinned by main.rs) - Matrix
//! homeservers are ordinary clearnet HTTPS, unlike Sneedchat's Tor-tunneled
//! transport, so there's no need for that backend's hand-rolled hyper
//! client over a custom `Transport`.

use anyhow::{bail, Context, Result};
use serde_json::Value;

/// The client for requests made with `key` - an account's access token -
/// routed through Tor or the configured proxy when that account is.
pub fn http_client_for(key: &str) -> reqwest::Client {
    let router = crate::net::route::router();
    matrix_client(router.routed(key))
}

fn matrix_client(routed: bool) -> reqwest::Client {
    crate::net::route::router().client_if("matrix", routed, |builder| {
        // Pinned to HTTP/1.1. Enabling reqwest's http2 feature (which the
        // file-upload path needs - see upload::http_client) would otherwise
        // let every client here negotiate h2 as a side effect, changing the
        // transport under a backend that works and is tested as it stands.
        // Nothing here wants h2; if it ever does, that is its own change.
        //
        // A user agent, because otherwise there is none. Homeservers log it,
        // rate limiters key on it, and an account provider naming a new
        // device guesses from it - which is where "moho on Unknown device"
        // came from in a matrix.org session list. The device's name is now
        // set outright (see auth::DEVICE_DISPLAY_NAME), so this is no longer
        // load-bearing for that; it is simply what a well-behaved client
        // says about itself.
        builder.http1_only().user_agent(concat!("moho/", env!("CARGO_PKG_VERSION")))
    })
}

/// The client for requests that carry no account's token: discovery, the
/// login and registration flows, the sign-in itself, and the public reads -
/// room directories, peeks - a signed-in account makes too.
///
/// Routed when the add form's switch says so, and whenever any Matrix account
/// is routed: a request with no token cannot say whose it is, and guessing
/// "nobody's" would send a routed account's room search out directly.
pub fn anonymous_client() -> reqwest::Client {
    let router = crate::net::route::router();
    matrix_client(router.routed(&crate::net::route::pending_key("matrix")) || router.wanted(SHARED_KEY))
}

/// Marked while any Matrix account is routed.
pub const SHARED_KEY: &str = "matrix:any-routed";

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
    let resp = anonymous_client().get(&url).send().await.ok()?;
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

/// What a user-interactive endpoint said.
///
/// A 401 here is not a failure: it is the server listing the stages it wants
/// and handing back a session to carry between them. Treating it as an error -
/// which `handle_response` rightly does everywhere else - throws away the one
/// part of the answer that says how to continue.
pub enum Attempt {
    Done(Value),
    NeedsAuth(Value),
}

/// A POST whose 401 is an answer rather than a failure.
pub async fn post_json_uia(url: &str, body: Value) -> Result<Attempt> {
    let resp = anonymous_client().post(url).json(&body).send().await.context("request failed")?;
    if resp.status().as_u16() == 401 {
        let challenge: Value = resp.json().await.context("invalid JSON in the authentication challenge")?;
        return Ok(Attempt::NeedsAuth(challenge));
    }
    handle_response(resp).await.map(Attempt::Done)
}

pub async fn post_json(url: &str, token: Option<&str>, body: Value) -> Result<Value> {
    let client = match token {
        Some(token) => http_client_for(token),
        None => anonymous_client(),
    };
    let mut req = client.post(url).json(&body);
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
    let resp = anonymous_client().get(url).send().await.context("request failed")?;
    handle_response(resp).await
}

/// A POST with no credential, for the same reason as the GET above: an
/// account provider is asked to register this client before there is any
/// account to act as.
pub async fn post_json_anonymous(url: &str, body: Value) -> Result<Value> {
    let resp = anonymous_client().post(url).json(&body).send().await.context("request failed")?;
    handle_response(resp).await
}

/// A form POST, which is what OAuth 2.0 endpoints take rather than JSON.
pub async fn post_form_anonymous(url: &str, fields: &[(&str, &str)]) -> Result<Value> {
    let resp = anonymous_client().post(url).form(fields).send().await.context("request failed")?;
    handle_response(resp).await
}

/// The same, keeping the status and the body whatever the status is.
///
/// The device grant answers "not yet" with a 403 and a JSON body saying so -
/// a perfectly ordinary step in a sign-in that has not finished - and
/// `handle_response` correctly turns a 403 into an error. Polling needs to
/// read the body either way, so it asks for both and decides for itself.
pub async fn post_form_anonymous_raw(url: &str, fields: &[(&str, &str)]) -> Result<(u16, Value)> {
    let resp = anonymous_client().post(url).form(fields).send().await.context("request failed")?;
    let status = resp.status().as_u16();
    let text = resp.text().await.context("reading the reply")?;
    let body = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    Ok((status, body))
}

pub async fn get_json(url: &str, token: &str) -> Result<Value> {
    let resp = http_client_for(token).get(url).bearer_auth(token).send().await.context("request failed")?;
    handle_response(resp).await
}

pub async fn delete_json(url: &str, token: &str) -> Result<Value> {
    let resp = http_client_for(token).delete(url).bearer_auth(token).send().await.context("request failed")?;
    handle_response(resp).await
}

/// The server is limiting how often this may be done, and says when to try again.
#[derive(Debug)]
pub struct RateLimited(pub std::time::Duration);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "M_LIMIT_EXCEEDED: the server asks for {} seconds before this is done again", self.0.as_secs().max(1))
    }
}

impl std::error::Error for RateLimited {}

/// The longest a request waits out a limit by itself before handing it back.
const LIMIT_WAIT_MAX: std::time::Duration = std::time::Duration::from_secs(10);
const LIMIT_TRIES: usize = 3;

/// How long a homeserver asked for, from the body of its 429: `retry_after_ms`,
/// or a couple of seconds where it did not say.
fn limit_wait(body: &Value) -> std::time::Duration {
    std::time::Duration::from_millis(body["retry_after_ms"].as_u64().unwrap_or(2000).max(100))
}

/// PUTs, and where the server says it is being asked too often, waits as long
/// as it asks - up to a few seconds - and asks again. Presence, state events
/// and sends are all PUTs, and a server's limit on any of them is a delay, not
/// a refusal. A wait longer than that is returned as `RateLimited` for the
/// caller to decide about.
pub async fn put_json(url: &str, token: &str, body: Value) -> Result<Value> {
    for attempt in 1..=LIMIT_TRIES {
        let resp = http_client_for(token).put(url).bearer_auth(token).json(&body).send().await.context("request failed")?;
        if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
            return handle_response(resp).await;
        }
        let wait = limit_wait(&resp.json::<Value>().await.unwrap_or(Value::Null));
        if wait > LIMIT_WAIT_MAX || attempt == LIMIT_TRIES {
            return Err(RateLimited(wait).into());
        }
        tokio::time::sleep(wait).await;
    }
    unreachable!("the last try returns")
}

/// Raw bytes, not JSON - for media downloads (see mod.rs's media caching).
/// Returns the HTTP status and body regardless of success, same shape as
/// backend/sneedchat/http.rs's own get_bytes, so callers can distinguish
/// "fetch itself failed" from "server returned a non-2xx".
pub async fn get_bytes(url: &str, token: &str) -> Result<(u16, Vec<u8>)> {
    let resp = http_client_for(token).get(url).bearer_auth(token).send().await.context("request failed")?;
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
    let resp = http_client_for(token).delete(url).bearer_auth(token).json(&serde_json::json!({})).send().await.context("request failed")?;
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
    let resp = http_client_for(token).delete(url).bearer_auth(token).json(&auth_body).send().await.context("request failed")?;
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
    let resp = http_client_for(token).post(url).bearer_auth(token).json(&body).send().await.context("request failed")?;
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
    let resp = http_client_for(token).post(url).bearer_auth(token).json(&authed).send().await.context("request failed")?;
    handle_response(resp).await
}

/// Some homeservers put the errcode at the front of the human-readable error
/// as well, so printing both verbatim gives "M_USER_IN_USE: M_USER_IN_USE:
/// User ID is not available." Tuwunel does this to every error it sends.
fn without_errcode<'a>(errcode: &str, error: &'a str) -> &'a str {
    error.strip_prefix(errcode).map(|rest| rest.trim_start_matches([':', ' '])).filter(|rest| !rest.is_empty()).unwrap_or(error)
}

async fn handle_response(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let body: Value = resp.json().await.context("invalid JSON response")?;
    if !status.is_success() {
        if let Ok(err) = serde_json::from_value::<MatrixError>(body.clone()) {
            bail!("{}: {}", err.errcode, without_errcode(&err.errcode, &err.error));
        }
        bail!("HTTP {status}: {body}");
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_errcode_is_printed_once_however_the_server_words_it() {
        assert_eq!(without_errcode("M_USER_IN_USE", "M_USER_IN_USE: User ID is not available."), "User ID is not available.");
        assert_eq!(without_errcode("M_FORBIDDEN", "Registration has been disabled."), "Registration has been disabled.");
        // Nothing left once the prefix goes: keep what there was, because an
        // errcode alone still says more than an empty string.
        assert_eq!(without_errcode("M_LIMIT_EXCEEDED", "M_LIMIT_EXCEEDED"), "M_LIMIT_EXCEEDED");
    }

    #[test]
    fn a_limit_is_waited_out_as_long_as_the_server_asks() {
        assert_eq!(limit_wait(&serde_json::json!({ "errcode": "M_LIMIT_EXCEEDED", "retry_after_ms": 4500 })), std::time::Duration::from_millis(4500));
        // Said nothing, or said zero: a short pause, not a spin.
        assert_eq!(limit_wait(&serde_json::json!({})), std::time::Duration::from_secs(2));
        assert_eq!(limit_wait(&serde_json::json!({ "retry_after_ms": 0 })), std::time::Duration::from_millis(100));
    }

    /// A server that answers each connection with the next canned reply.
    async fn server_saying(replies: Vec<&'static str>) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for reply in replies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 8192];
                let _ = socket.read(&mut buf).await;
                socket.write_all(reply.as_bytes()).await.unwrap();
                let _ = socket.shutdown().await;
            }
        });
        format!("http://{addr}/presence")
    }

    const LIMITED: &str = "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 63\r\n\r\n{\"errcode\":\"M_LIMIT_EXCEEDED\",\"error\":\"x\",\"retry_after_ms\":100}  ";
    const OK: &str = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{}";

    #[tokio::test]
    async fn a_put_the_server_limits_is_asked_again_after_the_wait() {
        let url = server_saying(vec![LIMITED, OK]).await;
        let started = std::time::Instant::now();
        let answer = put_json(&url, "token", serde_json::json!({ "presence": "online" })).await;
        assert!(answer.is_ok(), "{answer:?}");
        assert!(started.elapsed() >= std::time::Duration::from_millis(100), "it waited as asked");
    }

    #[tokio::test]
    async fn a_wait_too_long_to_sit_through_is_handed_back_as_the_wait() {
        const LONG: &str = "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 66\r\n\r\n{\"errcode\":\"M_LIMIT_EXCEEDED\",\"error\":\"x\",\"retry_after_ms\":60000}  ";
        let url = server_saying(vec![LONG]).await;
        let err = put_json(&url, "token", serde_json::json!({})).await.unwrap_err();
        let limited = err.downcast_ref::<RateLimited>().expect("the wait, to be decided about");
        assert_eq!(limited.0, std::time::Duration::from_secs(60));
    }

    #[test]
    fn a_limit_reads_as_the_wait_and_not_a_bare_code() {
        let text = RateLimited(std::time::Duration::from_millis(45_000)).to_string();
        assert!(text.contains("45 seconds"), "{text}");
    }

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
