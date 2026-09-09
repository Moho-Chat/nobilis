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

use super::*;
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
pub(super) async fn flows_at(homeserver_url: &str) -> Result<Vec<String>> {
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

/// Kicks off a login attempt in the background - see backend/discord.rs's
/// start_qr_login/backend/sneedchat's start_login for the identical
/// async-kickoff shape. Progress/result arrive via matrixLoginStatus/
/// matrixLoginResult events tagged with `login_id`, not the RPC response
/// (see rpc/methods.rs's addMatrixAccount).
/// Which ways a homeserver will let somebody sign in, resolving delegation
/// first - the address somebody types is often not where the API lives.
pub async fn login_flows(homeserver_url: &str) -> Result<Vec<String>> {
    let resolved = http::resolve_homeserver(homeserver_url).await?;
    flows_at(&resolved).await
}

/// Signs in through the homeserver's own web login.
///
/// The shape is fixed by the spec and by what a desktop app can do: the
/// homeserver sends the browser back to a URL of our choosing with a one-time
/// token, and that token is exchanged for a real one. So this listens on
/// loopback for exactly one request, hands the browser a page saying it can
/// be closed, and finishes the login with what it carried.
///
/// Loopback rather than a custom scheme because it needs no registration with
/// the desktop, works the same on every platform, and cannot be claimed by
/// another application: the port is chosen by the kernel a moment before it
/// is used, and nothing else knows it.
pub fn start_sso_login(state: AppState, login_id: String, homeserver_url: String, provider: Option<String>) {
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(try_sso_login(&state, &login_id, &homeserver_url, provider.as_deref())).catch_unwind().await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("matrix sso login[{login_id}]: {error}");
        state.events.emit("matrixLoginResult", serde_json::json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

/// How long to hold the loopback listener open waiting for the browser.
pub(super) const SSO_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) async fn try_sso_login(state: &AppState, login_id: &str, homeserver_url: &str, provider: Option<&str>) -> Result<()> {
    state.events.emit("matrixLoginStatus", serde_json::json!({ "loginId": login_id, "detail": "finding the server..." }));
    let homeserver_url = http::resolve_homeserver(homeserver_url).await?;
    let base = homeserver_url.trim_end_matches('/');

    let flows = auth::login_flows(&homeserver_url).await.unwrap_or_default();
    if !flows.iter().any(|flow| flow == "m.login.sso" || flow == "m.login.token") {
        anyhow::bail!("this homeserver does not offer single sign-on");
    }

    // Bound before the URL is built, because the URL has to contain the port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("opening a port for the sign-in to come back to")?;
    let port = listener.local_addr().context("reading the port")?.port();
    let redirect = format!("http://127.0.0.1:{port}/");
    let url = match provider.filter(|p| !p.is_empty()) {
        Some(provider) => format!(
            "{base}/_matrix/client/v3/login/sso/redirect/{}?redirectUrl={}",
            url::form_urlencoded::byte_serialize(provider.as_bytes()).collect::<String>(),
            url::form_urlencoded::byte_serialize(redirect.as_bytes()).collect::<String>()
        ),
        None => format!(
            "{base}/_matrix/client/v3/login/sso/redirect?redirectUrl={}",
            url::form_urlencoded::byte_serialize(redirect.as_bytes()).collect::<String>()
        ),
    };

    // The client opens this; the daemon has no browser and should not pretend
    // to. Sent as a status rather than returned, because the RPC that started
    // this answered long before the person had finished typing a password
    // into somebody else's login page.
    state.events.emit(
        "matrixLoginStatus",
        serde_json::json!({ "loginId": login_id, "detail": "waiting for the browser...", "url": url }),
    );

    let token = tokio::time::timeout(SSO_TIMEOUT, wait_for_sso_token(listener))
        .await
        .map_err(|_| anyhow::anyhow!("the sign-in was not finished in time"))?
        .context("waiting for the browser to come back")?;

    state.events.emit("matrixLoginStatus", serde_json::json!({ "loginId": login_id, "detail": "logging in..." }));
    let login = auth::login_with_token(&homeserver_url, &token, None).await.context("finishing the sign-in")?;

    let config = MatrixAccountConfig {
        homeserver_url: homeserver_url.to_string(),
        user_id: login.user_id,
        // No password to keep, and that is a real difference rather than an
        // omission: anything that needs one - publishing cross-signing keys,
        // removing a device - will have to ask the homeserver for its own
        // interactive auth instead.
        password: String::new(),
        access_token: login.access_token,
        device_id: login.device_id,
        next_batch: None,
        display_name: None,
            rtc_focus_url: None,
        };
    let saved = state.accounts.add_matrix(config)?;
    let account = crate::accounts::matrix_account_to_json(&saved, "connecting", false);
    spawn(state.clone(), saved);
    state.events.emit("matrixLoginResult", serde_json::json!({ "loginId": login_id, "success": true, "account": account }));
    Ok(())
}

/// Waits for the browser to arrive with a token, and tells the person they
/// can close the tab.
pub(super) async fn wait_for_sso_token(listener: tokio::net::TcpListener) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    loop {
        let (mut socket, _) = listener.accept().await.context("accepting the browser's request")?;
        let mut buffer = [0u8; 2048];
        let read = socket.read(&mut buffer).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buffer[..read]).to_string();
        let token = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|target| {
                let query = target.split_once('?')?.1;
                url::form_urlencoded::parse(query.as_bytes())
                    .find(|(key, _)| key == "loginToken")
                    .map(|(_, value)| value.into_owned())
            });

        let page = match &token {
            Some(_) => "<!doctype html><meta charset=utf-8><title>Signed in</title><body style=\"font-family:sans-serif;padding:2rem\"><h1>Signed in</h1><p>You can close this tab and go back to moho.</p>",
            None => "<!doctype html><meta charset=utf-8><title>Nothing to sign in with</title><body style=\"font-family:sans-serif;padding:2rem\"><h1>Nothing to sign in with</h1><p>The homeserver sent the browser back without a login token.</p>",
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
            page.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.shutdown().await;

        // A browser asking for /favicon.ico is not the answer; keep waiting
        // for the one that carries a token.
        if let Some(token) = token {
            return Ok(token);
        }
    }
}

pub fn start_login(state: AppState, login_id: String, homeserver_url: String, username: String, password: String) {
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(try_login(&state, &login_id, &homeserver_url, &username, &password)).catch_unwind().await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("matrix login[{login_id}]: {error}");
        state.events.emit("matrixLoginResult", serde_json::json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

pub(super) async fn try_login(state: &AppState, login_id: &str, homeserver_url: &str, username: &str, password: &str) -> Result<()> {
    // Where the client API actually is, before anything is stored or tried.
    //
    // Resolved here rather than at every call site, and the *resolved* address
    // is what gets saved - so a delegated server is asked once at sign-in
    // rather than on every request forever, and an account that works keeps
    // working if the well-known later goes away.
    state.events.emit("matrixLoginStatus", serde_json::json!({ "loginId": login_id, "detail": "finding the server..." }));
    let homeserver_url = &http::resolve_homeserver(homeserver_url).await?;
    state.events.emit("matrixLoginStatus", serde_json::json!({ "loginId": login_id, "detail": "logging in..." }));

    // Saved before the login is attempted, so a failure leaves an account to
    // correct and retry rather than an empty form. Same reasoning as the
    // Sneedchat path: the moment somebody is told their password might be
    // wrong is the worst moment to also make them retype the server address.
    //
    // An empty access token is already a state this backend understands -
    // ensure_login treats it as "never successfully logged in" and
    // authenticates from the stored password - so a retry needs nothing that
    // is not here, and a corrected password overwrites in place because the
    // account id comes from the user id.
    let pending = MatrixAccountConfig {
        homeserver_url: homeserver_url.to_string(),
        user_id: username.to_string(),
        password: password.to_string(),
        access_token: String::new(),
        device_id: String::new(),
        next_batch: None,
        display_name: None,
            rtc_focus_url: None,
        };
    state.accounts.add_matrix(pending.clone())?;

    let login = auth::login(homeserver_url, username, password, None).await.context("logging in")?;

    let config = MatrixAccountConfig {
        homeserver_url: homeserver_url.to_string(),
        user_id: login.user_id,
        password: password.to_string(),
        access_token: login.access_token,
        device_id: login.device_id,
        next_batch: None,
        display_name: None,
            rtc_focus_url: None,
        };
    let saved = state.accounts.add_matrix(config)?;
    // The placeholder above was keyed on whatever was typed, and the server
    // answers with the canonical user id - "salastil" against "@salastil:
    // poa.st". When they differ, the placeholder is a second account for the
    // same person, so it goes now that the real one exists.
    let pending_id = pending.account_id();
    if pending_id != saved.account_id() {
        // No event: removeAccount does not emit one either, and the frontend
        // re-reads the account list when the login result arrives a few lines
        // below. Inventing a name nothing listens for would only look like it
        // did something.
        let _ = state.accounts.remove(&pending_id);
    }
    let account = crate::accounts::matrix_account_to_json(&saved, "connecting", false);
    spawn(state.clone(), saved);

    state.events.emit("matrixLoginResult", serde_json::json!({ "loginId": login_id, "success": true, "account": account }));
    Ok(())
}

#[cfg(test)]
mod sso_tests {
    /// The browser comes back to a port this process opened a moment ago,
    /// carrying a one-time token in the query string. Everything about the
    /// sign-in hangs off reading that correctly, and a hand-rolled HTTP
    /// handler is exactly the place to get it wrong.
    #[tokio::test]
    async fn reads_the_token_the_browser_brings_back() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let waiting = tokio::spawn(super::wait_for_sso_token(listener));

        // A browser asking for the icon first, which must not be mistaken for
        // the answer - this is why the handler loops rather than taking the
        // first request it sees.
        let mut noise = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        noise.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: localhost\r\n\r\n").await.expect("write");
        drop(noise);

        let mut browser = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.expect("connect");
        browser
            .write_all(b"GET /?loginToken=syt_abc123 HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .expect("write");

        // And the page it is answered with, so nobody is left looking at a
        // browser error after a sign-in that worked.
        let mut answer = Vec::new();
        browser.read_to_end(&mut answer).await.expect("read");
        let answer = String::from_utf8_lossy(&answer);
        assert!(answer.starts_with("HTTP/1.1 200 OK"), "{answer}");
        assert!(answer.contains("You can close this tab"), "{answer}");

        let token = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
            .await
            .expect("in time")
            .expect("task")
            .expect("token");
        assert_eq!(token, "syt_abc123");
    }
}
