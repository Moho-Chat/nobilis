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

pub(super) fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(USER_AGENT)
        // Pinned to HTTP/1.1. Enabling reqwest's http2 feature (which the
        // file-upload path needs - see upload::http_client) would otherwise
        // let every client here negotiate h2 as a side effect, changing the
        // transport under a backend that works and is tested as it stands.
        // Nothing here wants h2; if it ever does, that is its own change.
            .http1_only()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new())
    })
}

/// What Discord wants answered before it will do something.
///
/// Not an error, though it arrives as one. Discord refuses certain actions -
/// adding a friend, joining a server - from anything it scores as automated,
/// and says so by naming a captcha rather than by saying no. Answered, the
/// same request goes through.
///
/// `rqdata` is hCaptcha's enterprise binding: it ties the challenge to this
/// account and this action, so a token solved for something else is not a
/// token for this. It is passed to the widget verbatim and comes back with
/// `rqtoken` beside it.
#[derive(Clone, Debug)]
pub struct CaptchaAsked {
    pub sitekey: String,
    pub service: String,
    pub rqdata: Option<String>,
    pub rqtoken: Option<String>,
}

impl CaptchaAsked {
    /// Reads a captcha demand out of a refusal, if that is what it is.
    pub fn read(body: &Value) -> Option<Self> {
        // The demand is `captcha_key`, an array of reasons. Everything else
        // is optional: a site with no enterprise binding sends no rqdata, and
        // a very old response names no service.
        body.get("captcha_key")?;
        Some(Self {
            sitekey: body["captcha_sitekey"].as_str().unwrap_or_default().to_string(),
            service: body["captcha_service"].as_str().unwrap_or("hcaptcha").to_string(),
            rqdata: body["captcha_rqdata"].as_str().map(str::to_string),
            rqtoken: body["captcha_rqtoken"].as_str().map(str::to_string),
        })
    }

    /// What a client needs to put the widget on screen and come back.
    pub fn to_question(&self) -> Value {
        json!({
            "captcha": {
                "sitekey": self.sitekey,
                "service": self.service,
                "rqdata": self.rqdata,
                "rqtoken": self.rqtoken,
            }
        })
    }
}

/// A solved captcha, on its way back to the request that asked for one.
#[derive(Clone, Debug, Default)]
pub struct CaptchaAnswer {
    pub key: String,
    pub rqtoken: Option<String>,
}

impl CaptchaAnswer {
    /// Reads the answer off an RPC's parameters, where one was sent.
    pub fn from_params(params: &Value) -> Option<Self> {
        let key = params.get("captchaKey")?.as_str()?.to_string();
        if key.is_empty() {
            return None;
        }
        Some(Self { key, rqtoken: params.get("captchaRqtoken").and_then(|v| v.as_str()).map(str::to_string) })
    }
}

/// Puts a solved captcha on a request, where there is one.
///
/// Discord reads it from headers rather than the body, which is what lets the
/// retry be the same call with two more lines on it rather than a second code
/// path per action.
pub(super) fn with_captcha(request: reqwest::RequestBuilder, answer: Option<&CaptchaAnswer>) -> reqwest::RequestBuilder {
    let Some(answer) = answer else { return request };
    let request = request.header("X-Captcha-Key", &answer.key);
    match &answer.rqtoken {
        Some(rqtoken) => request.header("X-Captcha-Rqtoken", rqtoken),
        None => request,
    }
}

/// Runs a write that Discord may want a captcha for.
///
/// Three outcomes rather than two: it worked, Discord asked a question, or it
/// failed. The question is not an error - it is the client's turn - so it
/// comes back as a value for the caller to hand on.
pub(super) async fn send_answerable(request: reqwest::RequestBuilder, doing: &str) -> Result<Value> {
    let resp = send_write(request).await?;
    if resp.status().is_success() {
        return Ok(json!({ "ok": true }));
    }
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if let Some(asked) = CaptchaAsked::read(&parsed) {
        return Ok(asked.to_question());
    }
    bail!("{}", discord_error_text(status, &text, doing));
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
/// again fixes. Discord asks for one on actions it treats as abusable, adding
/// a friend among them, and answering it here is precisely the automated
/// circumvention it exists to stop. Saying so, and where it can be done
/// instead, is the only honest response.
pub(super) fn discord_error_text(status: reqwest::StatusCode, body: &str, doing: &str) -> String {
    let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    // Only reached where the caller had no way to offer the challenge - a
    // write that does not go through `send_answerable`. Where one does, the
    // captcha is a question the client answers rather than a refusal.
    if parsed.get("captcha_key").is_some() {
        return format!("Discord asked for a captcha before {doing}, and this action has no way to show one.");
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
mod captcha_tests {
    use super::*;

    /// The enterprise shape, which is what Discord actually sends: a sitekey
    /// to render and an `rqdata` binding the challenge to this account and
    /// this action. Losing the binding would mean solving the right puzzle
    /// and being told no.
    #[test]
    fn a_refusal_carrying_a_challenge_is_a_question() {
        let body: Value = serde_json::from_str(
            r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"4c672d35","captcha_service":"hcaptcha",
                "captcha_rqdata":"bound-to-this","captcha_rqtoken":"rq-1"}"#,
        )
        .unwrap();
        let asked = CaptchaAsked::read(&body).expect("a challenge");
        assert_eq!(asked.sitekey, "4c672d35");
        assert_eq!(asked.rqdata.as_deref(), Some("bound-to-this"));
        assert_eq!(asked.rqtoken.as_deref(), Some("rq-1"));

        let question = asked.to_question();
        assert_eq!(question["captcha"]["sitekey"], "4c672d35");
        assert_eq!(question["captcha"]["rqdata"], "bound-to-this");
    }

    /// Without the enterprise binding there is still a challenge to show, and
    /// the two optional halves are absent rather than empty strings.
    #[test]
    fn a_plain_challenge_is_still_a_challenge() {
        let body: Value = serde_json::from_str(r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"abc"}"#).unwrap();
        let asked = CaptchaAsked::read(&body).expect("a challenge");
        // Named even when Discord does not name it: hcaptcha is what it is.
        assert_eq!(asked.service, "hcaptcha");
        assert!(asked.rqdata.is_none());
        assert!(asked.rqtoken.is_none());
    }

    /// An ordinary refusal must not be read as a question, or every failure
    /// would put a captcha on screen.
    #[test]
    fn an_ordinary_refusal_is_not_a_question() {
        let body: Value = serde_json::from_str(r#"{"message":"You are being rate limited.","code":20016}"#).unwrap();
        assert!(CaptchaAsked::read(&body).is_none());
        assert!(CaptchaAsked::read(&Value::Null).is_none());
    }

    /// The answer only counts when there is one. An absent or empty
    /// `captchaKey` is no answer, not an empty answer - sending a blank
    /// header would fail the request in a way nothing could explain.
    #[test]
    fn an_answer_is_read_only_when_there_is_one() {
        let with = serde_json::json!({ "captchaKey": "P1_eyJ0", "captchaRqtoken": "rq-1" });
        let answer = CaptchaAnswer::from_params(&with).expect("an answer");
        assert_eq!(answer.key, "P1_eyJ0");
        assert_eq!(answer.rqtoken.as_deref(), Some("rq-1"));

        assert!(CaptchaAnswer::from_params(&serde_json::json!({})).is_none());
        assert!(CaptchaAnswer::from_params(&serde_json::json!({ "captchaKey": "" })).is_none());
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    /// A captcha reaching this function at all means the action had no way to
    /// show one - `send_answerable` turns it into a question everywhere that
    /// can. What is left to do is say so in a sentence rather than dump the
    /// body, which still names the action so it is clear what was refused.
    #[test]
    fn a_captcha_with_nowhere_to_go_is_explained_rather_than_dumped() {
        let body = r#"{"captcha_key":["captcha-required"],"captcha_sitekey":"abc","captcha_service":"hcaptcha"}"#;
        let text = discord_error_text(reqwest::StatusCode::BAD_REQUEST, body, "adding a friend");
        assert!(text.contains("captcha"), "got {text}");
        assert!(text.contains("adding a friend"), "got {text}");
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
