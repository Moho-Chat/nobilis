//! The HTTP side of Discord: one client, and the manners it uses.
//!
//! Every write in this backend goes out through `send_write`, which paces
//! them and obeys a rate-limit answer rather than arguing with it. That is
//! here, alone, because it is the one thing every other module in this folder
//! needs and none of them should be reimplementing.

use super::*;

pub(super) const API_BASE: &str = "https://discord.com/api/v10";

pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Says who is calling, honestly.
///
/// reqwest sends no User-Agent whatsoever unless told to, and every request
/// this backend made went out without one. That is worth fixing on its own
/// terms - an API is entitled to know what is talking to it - and it is also
/// the single worst thing a request can look like at a credential endpoint,
/// where "no user agent" is the signature of a script trying passwords.
///
/// Deliberately names this client rather than claiming to be Discord's own.
/// Being identifiable is the point; a request that lies about what it is has
/// nothing to fall back on when the lie is spotted.
pub(super) const USER_AGENT: &str = concat!("moho/", env!("CARGO_PKG_VERSION"), " (nobilis)");

/// The shortest gap between two things this client *does* on Discord.
///
/// Reading is not paced - a client that could not fetch history quickly would
/// be a slow client - but writing is, because Discord judges accounts on it.
/// This project has already lost one to a spam flag, and while the messages
/// in question were seconds apart rather than milliseconds, a client with no
/// pacing at all is a client that will eventually send a burst.
pub(super) const WRITE_GAP: Duration = Duration::from_millis(400);

/// How many times to wait out a rate limit before giving up on a request.
pub(super) const RATE_LIMIT_RETRIES: usize = 3;

/// When the last write went out, so the next one can wait its turn.
pub(super) static LAST_WRITE: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// Waits for this write's turn, and takes it.
async fn pace() {
    // The gap is held across every account, because the traffic Discord sees
    // is this machine's rather than one account's.
    let wait = {
        let mut last = LAST_WRITE.lock().unwrap();
        let wait = last.map(|at| WRITE_GAP.saturating_sub(at.elapsed())).unwrap_or_default();
        *last = Some(std::time::Instant::now() + wait);
        wait
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

/// Like `send_write`, for a request whose body cannot be copied - a file
/// streamed from disk - and so is built again for every try.
pub(super) async fn send_write_rebuilt<F>(mut build: F) -> Result<reqwest::Response>
where
    F: FnMut() -> Result<reqwest::RequestBuilder>,
{
    pace().await;
    for attempt in 0..=RATE_LIMIT_RETRIES {
        let resp = build()?.send().await.context("talking to Discord")?;
        if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS || attempt == RATE_LIMIT_RETRIES {
            return Ok(resp);
        }
        let pause = retry_after(&resp);
        tracing::debug!("discord: rate limited, waiting {}ms", pause.as_millis());
        tokio::time::sleep(pause).await;
    }
    bail!("Discord kept rate-limiting that request")
}

/// Sends something that changes state, at a civilised pace, and waits out a
/// rate limit rather than reporting it.
///
/// Discord answers a rate limit with 429 and says in the response how long to
/// wait; nothing here read that, so every one became an error shown to
/// somebody who could do nothing about it but try again - the worst possible
/// answer to being told to slow down.
///
/// Reads deliberately do not go through this. A client that could not fetch
/// history quickly would be a slow client, and what Discord judges an account
/// on is what it sends.
pub(super) async fn send_write(request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    pace().await;

    let mut pending = Some(request);
    for attempt in 0..=RATE_LIMIT_RETRIES {
        let Some(current) = pending.take() else { break };
        // A retry needs its own copy, taken before the body is consumed. One
        // that cannot be cloned is one this cannot retry, and is sent once.
        let again = current.try_clone();
        let resp = current.send().await.context("talking to Discord")?;
        if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS || attempt == RATE_LIMIT_RETRIES {
            return Ok(resp);
        }
        let Some(again) = again else { return Ok(resp) };
        let pause = retry_after(&resp);
        tracing::debug!("discord: rate limited, waiting {}ms", pause.as_millis());
        tokio::time::sleep(pause).await;
        pending = Some(again);
    }
    bail!("Discord kept rate-limiting that request")
}

/// How long Discord asked this client to wait.
///
/// Seconds, as a decimal, in a header - clamped at both ends: a limit with no
/// number is not a reason to hammer, and one asking for half an hour is not a
/// wait anybody would sit through inside a request.
pub(super) fn retry_after(resp: &reqwest::Response) -> Duration {
    let seconds = resp
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(1.0);
    Duration::from_millis(((seconds.max(0.0) * 1000.0) as u64).clamp(200, 30_000))
}

/// The client for requests made under `key` - the account's token, or its
/// id - routed through Tor or the configured proxy when that account is.
pub(super) fn http_client_for(key: &str) -> reqwest::Client {
    discord_client(crate::net::route::router().routed(key))
}

/// The client for requests that belong to no account yet - the sign-in - or
/// that cannot say whose they are, such as a thumbnail off the CDN. Routed
/// when the add form's switch says so, or when any Discord account is.
pub(super) fn anonymous_client() -> reqwest::Client {
    let router = crate::net::route::router();
    discord_client(router.routed(&crate::net::route::pending_key("discord")) || router.wanted(SHARED_KEY))
}

/// Marked while any Discord account is routed.
pub(super) const SHARED_KEY: &str = "discord:any-routed";

fn discord_client(routed: bool) -> reqwest::Client {
    crate::net::route::router().client_if("discord", routed, |builder| {
        // Pinned to HTTP/1.1. Enabling reqwest's http2 feature (which the
        // file-upload path needs - see upload::http_client) would otherwise
        // let every client here negotiate h2 as a side effect, changing the
        // transport under a backend that works and is tested as it stands.
        // Nothing here wants h2; if it ever does, that is its own change.
        // A request that hears nothing back is given up on. Without this one
        // sent down a connection that had silently died waited for ever: the
        // gateway handler awaiting it stopped handling messages, and a history
        // fetch holding its in-flight mark never released it. Reads, not the
        // whole request, so a large upload that is still moving is left alone.
        builder
            .user_agent(USER_AGENT)
            .http1_only()
            .connect_timeout(std::time::Duration::from_secs(15))
            .read_timeout(std::time::Duration::from_secs(120))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .pool_idle_timeout(std::time::Duration::from_secs(45))
    })
}

/// Discord's per-field complaints, flattened into one line.
///
/// "Invalid Form Body" on its own says nothing a person can act on; the thing
/// worth reading is the `errors` map underneath it, which names the field and
/// says what was wrong with it.
/// Turns a failed Discord reply into something worth showing somebody.
///
/// The raw body is JSON and reaches a toast verbatim otherwise, so a refusal
/// that Discord states perfectly clearly - "No users with that username were
/// found" - arrived as a brace-laden dump with the sentence buried in it.
///
/// The captcha case is called out because it is not a failure that trying
/// again fixes. Discord asks for one on actions it treats as abusable, and a
/// third-party client answering it is what gets an account flagged for spam,
/// so moho does not offer those actions at all. Where one turns up anyway,
/// the honest answer is where it can be done instead.
pub(super) fn discord_error_text(status: reqwest::StatusCode, body: &str, doing: &str) -> String {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if parsed.get("captcha_key").is_some() {
        return format!(
            "Discord asked for a captcha before {doing}. moho does not answer Discord's captchas - doing so is what gets an account flagged for spam - so this has to be done on discord.com."
        );
    }
    if let Some(message) = parsed.get("message").and_then(|m| m.as_str()).filter(|m| !m.is_empty()) {
        return message.to_string();
    }
    if let Some(fields) = form_errors(&parsed) {
        return fields;
    }
    let snippet: String = body.chars().take(200).collect();
    format!("Discord API error {status}: {snippet}")
}

pub(super) fn form_errors(resp: &Value) -> Option<String> {
    let errors = resp.get("errors")?.as_object()?;
    let mut parts: Vec<String> = Vec::new();
    for (field, detail) in errors {
        let messages: Vec<&str> = detail
            .get("_errors")
            .and_then(|e| e.as_array())
            .map(|list| list.iter().filter_map(|e| e["message"].as_str()).collect())
            .unwrap_or_default();
        if messages.is_empty() {
            parts.push(field.clone());
        } else {
            parts.push(format!("{field}: {}", messages.join("; ")));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

#[cfg(test)]
mod pacing_tests {
    use std::time::Duration;

    /// The wait Discord asked for, read the way it sends it: seconds, with a
    /// decimal point, in a header.
    ///
    /// Clamped at both ends on purpose - a limit with no number is not a
    /// reason to hammer, and a half-hour one is not a wait to sit through
    /// inside a request - and this is the arithmetic that decides both.
    fn pause_for(seconds: Option<&str>) -> Duration {
        let seconds = seconds.and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0);
        Duration::from_millis(((seconds.max(0.0) * 1000.0) as u64).clamp(200, 30_000))
    }

    #[test]
    fn a_wait_is_taken_as_asked() {
        assert_eq!(pause_for(Some("1.5")), Duration::from_millis(1500));
        assert_eq!(pause_for(Some("0.75")), Duration::from_millis(750));
    }

    #[test]
    fn a_missing_or_absurd_wait_is_still_a_wait() {
        assert_eq!(pause_for(None), Duration::from_millis(1000));
        assert_eq!(pause_for(Some("nonsense")), Duration::from_millis(1000));
        // Nothing is not an answer to being told to slow down.
        assert_eq!(pause_for(Some("0")), Duration::from_millis(200));
        assert_eq!(pause_for(Some("-5")), Duration::from_millis(200));
        // And nobody waits half an hour inside one request.
        assert_eq!(pause_for(Some("1800")), Duration::from_millis(30_000));
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    /// moho never answers a captcha, so one is always a refusal: said in a
    /// sentence that names the action and sends it to discord.com, rather
    /// than the body dumped.
    #[test]
    fn a_captcha_is_explained_rather_than_dumped() {
        let body = r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"abc","captcha_service":"hcaptcha"}"#;
        let text = discord_error_text(reqwest::StatusCode::BAD_REQUEST, body, "making an invite");
        assert!(text.contains("captcha"), "got {text}");
        assert!(text.contains("making an invite"), "got {text}");
        assert!(text.contains("discord.com"), "got {text}");
        // The raw body is not what somebody reads.
        assert!(!text.contains("captcha_sitekey"), "got {text}");
    }

    /// Discord often states the reason perfectly clearly; it was arriving
    /// buried in JSON.
    #[test]
    fn discord_own_words_are_used_when_it_gives_them() {
        let body = r#"{"message":"No users with that username were found.","code":80004}"#;
        assert_eq!(
            discord_error_text(reqwest::StatusCode::BAD_REQUEST, body, "adding a friend"),
            "No users with that username were found."
        );
    }

    /// Per-field complaints, and a reply that is not JSON at all, both still
    /// have to produce something rather than panicking or saying nothing.
    #[test]
    fn other_shapes_still_say_something() {
        let fields = r#"{"errors":{"username":{"_errors":[{"message":"Too short"}]}}}"#;
        assert_eq!(
            discord_error_text(reqwest::StatusCode::BAD_REQUEST, fields, "adding a friend"),
            "username: Too short"
        );

        let junk = "<html>gateway timeout</html>";
        let text = discord_error_text(reqwest::StatusCode::BAD_GATEWAY, junk, "adding a friend");
        assert!(text.contains("502"), "got {text}");
    }
}
