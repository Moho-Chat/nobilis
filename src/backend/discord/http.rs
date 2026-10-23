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

/// Where every read waits its turn: when Discord has said how long to hold off - a 429, or a bucket that ran out - no
/// request of any kind goes out before then.
static HOLD_UNTIL: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

/// What is added to a bucket's reset time, as in arikawa's limiter: Discord's own clock and ours do not agree to the
/// millisecond, and arriving on the boundary is arriving early.
const BUCKET_SLACK: Duration = Duration::from_millis(250);

/// How many times a read that was rate limited is made again, waiting as long as it was told to.
const READ_RETRIES: usize = 2;

fn hold_for(wait: Duration) {
    let until = std::time::Instant::now() + wait;
    let mut held = HOLD_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
    if held.is_none_or(|at| at < until) {
        *held = Some(until);
    }
}

async fn wait_out_hold() {
    loop {
        let wait = {
            let held = HOLD_UNTIL.lock().unwrap_or_else(|e| e.into_inner());
            held.map(|at| at.saturating_duration_since(std::time::Instant::now())).unwrap_or_default()
        };
        if wait.is_zero() {
            return;
        }
        tokio::time::sleep(wait).await;
    }
}

/// Notes what a response says about the bucket it counted against: with none left, nothing goes out until it resets.
fn note_bucket(resp: &reqwest::Response) {
    let header = |name: &str| resp.headers().get(name).and_then(|v| v.to_str().ok());
    if header("x-ratelimit-remaining") == Some("0") {
        if let Some(reset) = header("x-ratelimit-reset-after").and_then(|v| v.parse::<f64>().ok()) {
            hold_for(Duration::from_secs_f64(reset.clamp(0.0, 30.0)) + BUCKET_SLACK);
        }
    }
}

/// What this client has learned about where it is no longer welcome, and so does not ask again: a token Discord has
/// refused (every request stops until the person signs in again - a revoked login that keeps asking is a client
/// hammering a dead door), and a route that keeps answering 403 (three times in five minutes closes it for five, so a
/// channel that cannot be read is not asked about at every opening).
#[derive(Default)]
pub struct Safety {
    halted: std::collections::HashSet<String>,
    circuits: HashMap<(String, String, String), Circuit>,
}

struct Circuit {
    refusals: u8,
    last: std::time::Instant,
    open_until: Option<std::time::Instant>,
}

/// Why a request was not made.
#[derive(Debug, PartialEq, Eq)]
pub enum Blocked {
    /// The token was refused (401): nothing more is sent with it.
    Refused,
    /// The route was refused (403) repeatedly: not for this long.
    Closed(Duration),
}

impl std::fmt::Display for Blocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Blocked::Refused => write!(f, "Discord has refused this account's login, so nothing more is sent for it - sign in again from Accounts"),
            Blocked::Closed(wait) => write!(f, "Discord has refused this three times; not asking again for {} minutes", wait.as_secs().div_ceil(60).max(1)),
        }
    }
}

impl std::error::Error for Blocked {}

const CIRCUIT_REFUSALS: u8 = 3;
const CIRCUIT_WINDOW: Duration = Duration::from_secs(5 * 60);
const CIRCUIT_OPEN_FOR: Duration = Duration::from_secs(5 * 60);
/// Routes remembered: they include ids, so a few refusals on many messages must not be kept all run.
const MAX_CIRCUITS: usize = 512;

impl Safety {
    pub fn preflight_at(&mut self, token: &str, method: &str, path: &str, now: std::time::Instant) -> Result<(), Blocked> {
        if self.halted.contains(token) {
            return Err(Blocked::Refused);
        }
        self.circuits.retain(|_, c| match c.open_until {
            Some(until) => until > now,
            None => now.saturating_duration_since(c.last) < CIRCUIT_WINDOW,
        });
        let key = (token.to_string(), method.to_string(), path.to_string());
        match self.circuits.get(&key).and_then(|c| c.open_until) {
            Some(until) => Err(Blocked::Closed(until.saturating_duration_since(now))),
            None => Ok(()),
        }
    }

    pub fn record_at(&mut self, token: &str, method: &str, path: &str, status: u16, now: std::time::Instant) {
        let key = (token.to_string(), method.to_string(), path.to_string());
        if status == 401 {
            self.halted.insert(token.to_string());
            return;
        }
        if status != 403 {
            self.circuits.remove(&key);
            return;
        }
        if !self.circuits.contains_key(&key) && self.circuits.len() >= MAX_CIRCUITS {
            if let Some(oldest) = self.circuits.iter().min_by_key(|(_, c)| c.last).map(|(k, _)| k.clone()) {
                self.circuits.remove(&oldest);
            }
        }
        let circuit = self.circuits.entry(key).or_insert(Circuit { refusals: 0, last: now, open_until: None });
        if now.saturating_duration_since(circuit.last) >= CIRCUIT_WINDOW {
            circuit.refusals = 0;
            circuit.open_until = None;
        }
        circuit.refusals = circuit.refusals.saturating_add(1);
        circuit.last = now;
        if circuit.refusals >= CIRCUIT_REFUSALS {
            circuit.open_until = Some(now + CIRCUIT_OPEN_FOR);
        }
    }

    pub fn halt(&mut self, token: &str) {
        self.halted.insert(token.to_string());
    }

    pub fn is_halted(&self, token: &str) -> bool {
        self.halted.contains(token)
    }
}

static SAFETY: std::sync::Mutex<Option<Safety>> = std::sync::Mutex::new(None);

fn safety<R>(f: impl FnOnce(&mut Safety) -> R) -> R {
    let mut slot = SAFETY.lock().unwrap_or_else(|e| e.into_inner());
    f(slot.get_or_insert_with(Safety::default))
}

/// Nothing more is sent with this token: Discord has said it is no good.
pub(super) fn halt_token(token: &str) {
    safety(|s| s.halt(token));
}

pub(super) fn token_is_halted(token: &str) -> bool {
    safety(|s| s.is_halted(token))
}

/// Which token, method and path a request is for, where it can be told without sending it.
fn inspect(request: &reqwest::RequestBuilder) -> Option<(String, String, String)> {
    let built = request.try_clone()?.build().ok()?;
    let token = built.headers().get("authorization")?.to_str().ok()?.to_string();
    Some((token, built.method().as_str().to_string(), built.url().path().to_string()))
}

/// Asked before a request is sent, told after: see `Safety`.
fn preflight(who: &Option<(String, String, String)>) -> Result<()> {
    if let Some((token, method, path)) = who {
        safety(|s| s.preflight_at(token, method, path, std::time::Instant::now())).map_err(anyhow::Error::from)?;
    }
    Ok(())
}

fn note_status(who: &Option<(String, String, String)>, status: reqwest::StatusCode) {
    if let Some((token, method, path)) = who {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            tracing::warn!("discord: a request was refused as unauthorised: nothing more is sent with this login");
        }
        safety(|s| s.record_at(token, method, path, status.as_u16(), std::time::Instant::now()));
    }
}

/// Whether a 429's body is a channel's slowmode (code 20016): the wait is the person's, and sending again when it is
/// over is not for this client to do on their behalf.
fn is_slowmode(body: &[u8]) -> bool {
    serde_json::from_slice::<Value>(body).ok().and_then(|v| v["code"].as_u64()) == Some(20016)
}

/// A response put back together once its body has been read, so a caller that reads it again finds it whole.
fn rebuilt(status: reqwest::StatusCode, headers: reqwest::header::HeaderMap, body: bytes::Bytes) -> reqwest::Response {
    let mut response = tokio_tungstenite::tungstenite::http::Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    reqwest::Response::from(response)
}

/// Sending that behaves as a client of an API with rate limits should: waits out any hold Discord has put on this
/// client, notes how much of a bucket is left and waits for it to refill rather than running into the limit, and when
/// it is told to slow down anyway waits as long as it was told and asks again - instead of reading a refusal as an
/// answer. Reads were sent plain, so a 429 was a silent "nothing found" and the next request followed straight on.
pub(super) trait SendPaced {
    fn send_paced(self) -> impl std::future::Future<Output = Result<reqwest::Response>> + Send;
}

impl SendPaced for reqwest::RequestBuilder {
    async fn send_paced(self) -> Result<reqwest::Response> {
        let who = inspect(&self);
        preflight(&who)?;
        wait_out_hold().await;
        let mut pending = Some(self);
        for attempt in 0..=READ_RETRIES {
            let Some(current) = pending.take() else { break };
            // A body that cannot be copied is sent once.
            let again = current.try_clone();
            let resp = current.send().await.context("talking to Discord")?;
            note_bucket(&resp);
            note_status(&who, resp.status());
            if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS || attempt == READ_RETRIES {
                return Ok(resp);
            }
            let Some(again) = again else { return Ok(resp) };
            let pause = retry_after(&resp);
            tracing::debug!("discord: rate limited on a read, waiting {}ms", pause.as_millis());
            hold_for(pause);
            wait_out_hold().await;
            pending = Some(again);
        }
        unreachable!("the last attempt returns")
    }
}

/// Like `send_write`, for a request whose body cannot be copied - a file
/// streamed from disk - and so is built again for every try.
pub(super) async fn send_write_rebuilt<F>(mut build: F) -> Result<reqwest::Response>
where
    F: FnMut() -> Result<reqwest::RequestBuilder>,
{
    wait_out_hold().await;
    pace().await;
    for attempt in 0..=RATE_LIMIT_RETRIES {
        let resp = build()?.send().await.context("talking to Discord")?;
        note_bucket(&resp);
        if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS || attempt == RATE_LIMIT_RETRIES {
            return Ok(resp);
        }
        let pause = retry_after(&resp);
        tracing::debug!("discord: rate limited, waiting {}ms", pause.as_millis());
        hold_for(pause);
        wait_out_hold().await;
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
    let who = inspect(&request);
    preflight(&who)?;
    wait_out_hold().await;
    pace().await;

    let mut pending = Some(request);
    for attempt in 0..=RATE_LIMIT_RETRIES {
        let Some(current) = pending.take() else { break };
        // A retry needs its own copy, taken before the body is consumed. One
        // that cannot be cloned is one this cannot retry, and is sent once.
        let again = current.try_clone();
        let resp = current.send().await.context("talking to Discord")?;
        note_bucket(&resp);
        note_status(&who, resp.status());
        if resp.status() != reqwest::StatusCode::TOO_MANY_REQUESTS || attempt == RATE_LIMIT_RETRIES {
            return Ok(resp);
        }
        // A channel's slowmode is answered with a 429 too. It is the person's wait, and a message they have not seen
        // refused is not one to send again for them when it ends.
        let (status, headers) = (resp.status(), resp.headers().clone());
        let body = resp.bytes().await.context("reading Discord's answer")?;
        if is_slowmode(&body) {
            return Ok(rebuilt(status, headers, body));
        }
        let pause = pause_for(headers.get("retry-after").and_then(|v| v.to_str().ok()));
        let Some(again) = again else { return Ok(rebuilt(status, headers, body)) };
        tracing::debug!("discord: rate limited, waiting {}ms", pause.as_millis());
        hold_for(pause);
        wait_out_hold().await;
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
    pause_for(resp.headers().get("retry-after").and_then(|v| v.to_str().ok()))
}

/// The wait a `Retry-After` value asks for; none given, or none that reads as
/// a number, is a second.
fn pause_for(seconds: Option<&str>) -> Duration {
    let seconds = seconds.and_then(|v| v.parse::<f64>().ok()).unwrap_or(1.0);
    Duration::from_millis(((seconds.max(0.0) * 1000.0) as u64).clamp(200, 30_000))
}

/// The client for requests made under `key` - the account's token, or its
/// id - routed through Tor or the configured proxy when that account is.
pub(super) fn http_client_for(key: &str) -> reqwest::Client {
    discord_client(crate::net::route::router().routed(key), true)
}

/// The client for a request that is not for Discord's API but is made on an account's behalf - an upload to the storage
/// address Discord hands out - routed as the account is, and carrying none of the headers that belong to the API.
pub(super) fn bare_client_for(key: &str) -> reqwest::Client {
    discord_client(crate::net::route::router().routed(key), false)
}

/// The client for requests that belong to no account yet - the sign-in - or
/// that cannot say whose they are, such as a thumbnail off the CDN. Routed
/// when the add form's switch says so, or when any Discord account is.
pub(super) fn anonymous_client() -> reqwest::Client {
    let router = crate::net::route::router();
    discord_client(router.routed(&crate::net::route::pending_key("discord")) || router.wanted(SHARED_KEY), false)
}

/// Marked while any Discord account is routed.
pub(super) const SHARED_KEY: &str = "discord:any-routed";

/// `api` clients say which install, language and timezone they are (see `install_headers`); the others - the CDN,
/// the sign-in before there is an account - do not, since nothing there asks.
fn discord_client(routed: bool, api: bool) -> reqwest::Client {
    crate::net::route::router().client_if(if api { "discord-api" } else { "discord" }, routed, |builder| {
        let builder = if api { builder.default_headers(super::install_headers::headers().clone()) } else { builder };
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
    use super::pause_for;
    use std::time::Duration;

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

    /// What a client of an API with rate limits does: told to slow down, it waits as long as it was told and asks
    /// again; told a bucket is empty, it lets it refill before anything else goes out. Asked of a local listener.
    #[tokio::test]
    async fn a_read_waits_out_a_rate_limit_and_a_bucket_that_has_run_dry() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        // 1: refused with a one-second wait. 2: answered, saying the bucket is empty for one second. 3: answered.
        let server = tokio::spawn(async move {
            let mut seen = Vec::new();
            for reply in [
                "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nX-RateLimit-Remaining: 0\r\nX-RateLimit-Reset-After: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let _ = socket.read(&mut buf).await.unwrap();
                seen.push(std::time::Instant::now());
                socket.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        let client = reqwest::Client::new();
        let started = std::time::Instant::now();
        let first = client.get(&url).send_paced().await.unwrap();
        assert_eq!(first.status(), 200, "the refusal was waited out and the read made again");
        assert!(started.elapsed() >= Duration::from_millis(950), "it waited the second it was told to: {:?}", started.elapsed());
        let second_started = std::time::Instant::now();
        let second = client.get(&url).send_paced().await.unwrap();
        assert_eq!(second.status(), 200);
        assert!(second_started.elapsed() >= Duration::from_millis(950), "the empty bucket was let refill first: {:?}", second_started.elapsed());
        let seen = server.await.unwrap();
        assert_eq!(seen.len(), 3);
    }

    #[test]
    fn a_refused_login_stops_everything_and_repeated_403s_close_their_route() {
        use std::time::Instant;
        let mut s = Safety::default();
        let t = Instant::now();
        let at = |secs: u64| t + Duration::from_secs(secs);
        assert!(s.preflight_at("tok", "GET", "/channels/1/messages", t).is_ok());
        // Two refusals are not three.
        s.record_at("tok", "GET", "/channels/1/messages", 403, at(1));
        s.record_at("tok", "GET", "/channels/1/messages", 403, at(2));
        assert!(s.preflight_at("tok", "GET", "/channels/1/messages", at(3)).is_ok());
        s.record_at("tok", "GET", "/channels/1/messages", 403, at(4));
        let closed = s.preflight_at("tok", "GET", "/channels/1/messages", at(5)).unwrap_err();
        assert!(matches!(closed, Blocked::Closed(w) if w > Duration::from_secs(290)));
        // Other routes, other methods and other accounts are unaffected.
        assert!(s.preflight_at("tok", "GET", "/channels/2/messages", at(5)).is_ok());
        assert!(s.preflight_at("tok", "POST", "/channels/1/messages", at(5)).is_ok());
        assert!(s.preflight_at("other", "GET", "/channels/1/messages", at(5)).is_ok());
        // It opens again after the five minutes, and a success clears the count.
        assert!(s.preflight_at("tok", "GET", "/channels/1/messages", at(4 + 301)).is_ok());
        s.record_at("tok", "GET", "/channels/9/messages", 403, at(10));
        s.record_at("tok", "GET", "/channels/9/messages", 200, at(11));
        s.record_at("tok", "GET", "/channels/9/messages", 403, at(12));
        s.record_at("tok", "GET", "/channels/9/messages", 403, at(13));
        assert!(s.preflight_at("tok", "GET", "/channels/9/messages", at(14)).is_ok(), "403, 200, 403, 403 is not three in a row");
        // A 401 ends all of it for that token, for good.
        s.record_at("tok", "GET", "/users/@me", 401, at(20));
        assert_eq!(s.preflight_at("tok", "POST", "/anything", at(21)), Err(Blocked::Refused));
        assert!(s.preflight_at("fresh", "POST", "/anything", at(21)).is_ok(), "a new login is a new token");
    }

    #[test]
    fn a_slowmode_answer_is_told_from_any_other_429() {
        assert!(is_slowmode(br#"{"message":"The resource is being rate limited.","retry_after":3.5,"code":20016}"#));
        assert!(!is_slowmode(br#"{"message":"You are being rate limited.","retry_after":1.2,"global":false}"#));
        assert!(!is_slowmode(b"not json"));
    }
}
