//! Logging in to the forum and keeping the resulting session alive.
//!
//! Ported from sockchat-rs's `auth/mod.rs` + `chat/session.rs`
//! (<https://gitgud.io/jcmoon/sockchat-rs>), combined here since nobilis
//! doesn't need the reference's separate on-disk session-cache file - the
//! account's own username/password (persisted in accounts.toml, same as
//! Discord's token) is always available to log back in with, so there's
//! nothing to fall back to a cached cookie file for.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use hyper::Method;

use super::form;
use super::http::{CookieJar, HttpClient, Response};
use super::pow;
use super::totp;
use crate::net::tor::Transport;

/// Session cookie the forum issues. Clearing it forces a fresh one.
const SESSION_COOKIE: &str = "xf_session";
/// Identity cookie, which embeds our user ID.
const USER_COOKIE: &str = "xf_user";
/// Upper bound on chained gate challenges while authenticating.
const MAX_GATE_STEPS: usize = 8;
/// XenForo records how long the form was on screen and treats an instant
/// submission as automation. Pausing also keeps us far from any rate limit.
const FORM_DWELL: Duration = Duration::from_millis(1500);

pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for Credentials {
    /// Hand-written so the password can't reach a log through a stray `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials").field("username", &self.username).field("password", &"<redacted>").finish()
    }
}

/// How to answer a two-factor challenge. Unlike the reference CLI, there's
/// no interactive terminal to fall back to - a challenge this can't answer
/// is reported as an error rather than prompting (see the plan's "Open
/// Decisions": an interactive round-trip is a real feature to add later,
/// not something to fake here).
pub enum TwoFactor {
    /// Derive the code from a stored TOTP secret - no interaction needed.
    Totp(Vec<u8>),
    /// No secret configured; only usable if the account has no 2FA at all.
    None,
}

impl TwoFactor {
    fn code(&self, provider: &str) -> Result<String> {
        match self {
            TwoFactor::Totp(secret) if provider == "totp" => {
                let left = totp::seconds_remaining();
                if left <= 2 {
                    tracing::info!("TOTP code expires in {left}s; waiting for the next window");
                    std::thread::sleep(Duration::from_secs(left + 1));
                }
                totp::generate_now(secret)
            }
            TwoFactor::Totp(_) => bail!(
                "the account asked for a {provider} code, but only a TOTP secret is configured - \
                 email/backup-code 2FA isn't supported yet"
            ),
            TwoFactor::None => bail!(
                "the account requires two-factor authentication but no TOTP secret is configured; \
                 add one when creating the account"
            ),
        }
    }
}

/// Cloneable so one authenticated session can be shared across a multi-room
/// account's per-room connection tasks (see backend/sockchat/mod.rs) -
/// cheap, since `HttpClient`'s cookie jar is itself `Arc`-shared, so every
/// clone sees the same live cookies (a refresh from any one room's retry
/// loop benefits every other room's next attempt too).
#[derive(Clone)]
pub struct Session {
    pub http: HttpClient,
    base: String,
}

impl Session {
    pub fn new(transport: Transport, base: String, user_agent: String) -> Self {
        Self { http: HttpClient::new(transport, CookieJar::new(), user_agent), base }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Whether the current session is authenticated, costing one request.
    pub async fn is_authenticated(&self) -> Result<bool> {
        let resp = self.fetch(&self.base).await?;
        Ok(logged_in_marker(&resp.body))
    }

    /// Log in if the session isn't already authenticated.
    pub async fn ensure_authenticated(&self, creds: &Credentials, two_factor: &TwoFactor) -> Result<()> {
        if self.is_authenticated().await? {
            return Ok(());
        }
        self.log_in(creds, two_factor).await
    }

    /// Fetch a page, solving the proof-of-work gate first if challenged.
    pub async fn fetch(&self, url: &str) -> Result<Response> {
        let resp = self.http.get(url).await?;
        if resp.status != pow::GATE_STATUS {
            return Ok(resp);
        }
        pow::clear(&self.http, url, MAX_GATE_STEPS).await.context("solving the gate")?;
        self.http.get(url).await
    }

    /// Drop the session cookie and obtain a new one, logging in again if the
    /// refreshed session comes back unauthenticated (the login itself
    /// expired, not just the session cookie).
    pub async fn refresh(&self, creds: &Credentials, two_factor: &TwoFactor) -> Result<()> {
        self.http.jar.remove(SESSION_COOKIE);
        let resp = self.fetch(&self.base).await?;
        if !(200..400).contains(&resp.status) {
            bail!("session refresh got HTTP {}", resp.status);
        }
        if !logged_in_marker(&resp.body) {
            tracing::info!("refreshed session is not authenticated; logging in again");
            self.log_in(creds, two_factor).await?;
        }
        Ok(())
    }

    /// `Cookie` header for the websocket handshake.
    pub fn cookie_header(&self) -> Option<String> {
        self.http.jar.header()
    }

    /// Our forum user ID, read from the `xf_user` cookie. XenForo formats
    /// it as `<user id>,<token>`, but the separator arrives percent-encoded
    /// (`1544%2CFzO…`) - read the leading digits directly rather than
    /// guessing at the delimiter.
    pub fn user_id(&self) -> Option<u32> {
        let raw = self.http.jar.get(USER_COOKIE)?;
        let digits: String = raw.trim().chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    }

    async fn log_in(&self, creds: &Credentials, two_factor: &TwoFactor) -> Result<()> {
        let login_url = format!("{}/login/", self.base);
        let page = self.fetch(&login_url).await?;
        if logged_in_marker(&page.body) {
            return Ok(());
        }

        let section = form::form_section(&page.body, "/login/login").context("could not find the login form; the site layout may have changed")?;
        let mut fields = form::inputs(section);

        form::set(&mut fields, "login", &creds.username);
        form::set(&mut fields, "password", &creds.password);
        // "Stay logged in" - without it the session is short and nobilis
        // would be logging in again constantly.
        form::set(&mut fields, "remember", "1");

        tokio::time::sleep(FORM_DWELL).await;

        let resp = self.post(&format!("{}/login/login", self.base), &fields).await?;
        match outcome(&resp)? {
            Outcome::TwoFactor(location) => {
                let url = absolute(&self.base, &location)?;
                self.two_step(&url, two_factor).await
            }
            Outcome::Success => self.confirm().await,
            Outcome::Rejected(why) => bail!("login failed: {why}"),
        }
    }

    async fn two_step(&self, url: &str, two_factor: &TwoFactor) -> Result<()> {
        let page = self.fetch(url).await?;
        let provider = two_step_provider(&page.body)?;
        tracing::info!("two-factor challenge, provider {provider}");

        let code = two_factor.code(&provider).context("obtaining the two-factor code")?;
        let fields = two_step_fields(&page.body, &code)?;

        let resp = self.post(&format!("{}/login/two-step", self.base), &fields).await?;
        match outcome(&resp)? {
            Outcome::Success => self.confirm().await,
            Outcome::TwoFactor(_) => bail!("two-factor step failed: the code was not accepted"),
            Outcome::Rejected(why) => bail!("two-factor step failed: {why}"),
        }
    }

    /// Verify the session really is authenticated rather than trusting a
    /// redirect.
    async fn confirm(&self) -> Result<()> {
        if self.is_authenticated().await? {
            Ok(())
        } else {
            bail!("login appeared to succeed but the session is not authenticated")
        }
    }

    /// POST form fields, following only the redirects that preserve the
    /// method. 307/308 mean "resend this exact request elsewhere" - the
    /// proxy uses a 308 to move `http` requests to `https`, and not
    /// following it means the credentials never get delivered at all.
    /// Everything else is left unfollowed, since a 303 *is* the answer
    /// being read.
    async fn post(&self, url: &str, fields: &[(String, String)]) -> Result<Response> {
        let pairs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let body = super::http::encode_form(&pairs);

        let mut url = url.to_string();
        for _ in 0..3 {
            let resp = self.http.send_no_redirect(Method::POST, &url, Some(body.clone())).await?;
            if !matches!(resp.status, 307 | 308) {
                return Ok(resp);
            }
            let Some(location) = redirect_target(&resp) else { return Ok(resp) };
            let next = absolute(&url, &location)?;
            tracing::info!("re-posting to {next} after {} redirect", resp.status);
            url = next;
        }
        bail!("too many method-preserving redirects while posting to {url}")
    }
}

/// XenForo marks the `<html>` element of every page it renders, which is a
/// more reliable signal than a cookie's mere presence: a stale `xf_user`
/// looks identical to a live one from the client side.
fn logged_in_marker(html: &str) -> bool {
    html.contains("data-logged-in=\"true\"")
}

enum Outcome {
    Success,
    TwoFactor(String),
    Rejected(String),
}

/// Interpret a login or two-step response. Being strict matters here: an
/// overly permissive version might treat any 3xx as success, which would
/// silently accept the `http`-to-`https` upgrade redirect as a completed
/// login even though the credentials never reached XenForo.
fn outcome(resp: &Response) -> Result<Outcome> {
    let status = resp.status;
    let location = redirect_target(resp);

    let Some(location) = location else {
        return Ok(Outcome::Rejected(form_error(&resp.body).unwrap_or_else(|| format!("the server returned {status} without redirecting"))));
    };

    if location.contains("two-step") {
        return Ok(Outcome::TwoFactor(location));
    }
    if location.contains("/login") {
        return Ok(Outcome::Rejected(form_error(&resp.body).unwrap_or_else(|| format!("redirected back to {location}"))));
    }
    if (300..400).contains(&status) {
        return Ok(Outcome::Success);
    }
    Ok(Outcome::Rejected(format!("unexpected status {status}")))
}

/// Which second factor the server is asking for. Defaults to TOTP, which is
/// what the form uses when it doesn't say.
fn two_step_provider(html: &str) -> Result<String> {
    let section = form::form_section(html, "two-step").context("could not find the two-step form")?;
    Ok(form::inputs(section).into_iter().find(|(n, _)| n == "provider").map(|(_, v)| v).filter(|v| !v.is_empty()).unwrap_or_else(|| "totp".to_string()))
}

fn two_step_fields(html: &str, code: &str) -> Result<Vec<(String, String)>> {
    let section = form::form_section(html, "two-step").context("could not find the two-step form")?;
    let mut fields = form::inputs(section);
    form::set(&mut fields, "code", code.trim());
    // Trusting this client makes the server issue xf_tfa_trust and stop
    // asking on subsequent logins, so 2FA costs one prompt per machine
    // rather than one per login.
    form::set(&mut fields, "trust", "1");
    form::set(&mut fields, "remember", "1");
    Ok(fields)
}

fn redirect_target(resp: &Response) -> Option<String> {
    resp.headers.get(hyper::header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string)
}

fn absolute(base: &str, location: &str) -> Result<String> {
    Ok(url::Url::parse(base)?.join(location)?.to_string())
}

/// Pull the human-readable error out of a re-rendered form, so a rejection
/// reports "incorrect password" rather than just "HTTP 200".
fn form_error(html: &str) -> Option<String> {
    for marker in ["blockMessage--error", "js-errorMessage", "formRow-explain--error"] {
        let Some(i) = html.find(marker) else { continue };
        let after = &html[i..];
        let Some(start) = after.find('>') else { continue };
        let Some(end) = after[start..].find('<') else { continue };
        let text = strip_tags(&after[start + 1..start + end]);
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

fn strip_tags(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = (depth as i32 - 1).max(0) as u32,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_never_debug_print_the_password() {
        let c = Credentials { username: "Null Test Account".into(), password: "hunter2".into() };
        let rendered = format!("{c:?}");
        assert!(rendered.contains("Null Test Account"));
        assert!(!rendered.contains("hunter2"), "password leaked: {rendered}");
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn detects_the_logged_in_marker() {
        assert!(logged_in_marker(r#"<html id="XF" data-logged-in="true" data-xf="2.3">"#));
        assert!(!logged_in_marker(r#"<html id="XF" data-logged-in="false" data-xf="2.3">"#));
        assert!(!logged_in_marker("<html>"));
    }

    #[test]
    fn extracts_the_error_text_from_a_rejected_login() {
        let html = r#"<div class="blockMessage blockMessage--error">
            The requested user could not be found.</div>"#;
        assert_eq!(form_error(html).unwrap(), "The requested user could not be found.");
    }

    #[test]
    fn missing_error_block_yields_none() {
        assert!(form_error("<html><body>fine</body></html>").is_none());
    }

    #[test]
    fn strip_tags_flattens_markup_and_whitespace() {
        assert_eq!(strip_tags("a <b>bold</b>  word"), "a bold word");
        assert_eq!(strip_tags("  spaced   out  "), "spaced out");
    }

    const TWO_STEP_PAGE: &str = r#"
<form action="/login/two-step" method="post" class="block">
  <input type="hidden" name="_xfToken" value="1785255424,abcdef" />
  <input type="hidden" name="provider" value="totp" />
  <input type="hidden" name="remember" value="1" />
  <input type="hidden" name="_xfRedirect" value="https://example.onion/" />
  <input type="text" name="code" autocomplete="one-time-code" />
  <label><input type="checkbox" name="trust" value="1" /></label>
  <input type="submit" name="go" value="Confirm" />
</form>"#;

    #[test]
    fn two_step_submission_echoes_the_form_and_sets_our_fields() {
        let fields = two_step_fields(TWO_STEP_PAGE, "123456").unwrap();
        let get = |n: &str| fields.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str()).unwrap_or_default();

        assert_eq!(get("_xfToken"), "1785255424,abcdef");
        assert_eq!(get("_xfRedirect"), "https://example.onion/");
        assert_eq!(get("code"), "123456");
        assert_eq!(get("trust"), "1");
        assert_eq!(get("remember"), "1");
        assert!(!fields.iter().any(|(k, _)| k == "go"));
    }

    #[test]
    fn two_step_reads_the_provider_and_defaults_to_totp() {
        assert_eq!(two_step_provider(TWO_STEP_PAGE).unwrap(), "totp");
        let email = TWO_STEP_PAGE.replace(r#"name="provider" value="totp""#, r#"name="provider" value="email""#);
        assert_eq!(two_step_provider(&email).unwrap(), "email");
        let none = TWO_STEP_PAGE.replace(r#"<input type="hidden" name="provider" value="totp" />"#, "");
        assert_eq!(two_step_provider(&none).unwrap(), "totp");
        assert!(two_step_provider("<html><body>nope</body></html>").is_err());
    }

    #[test]
    fn a_checkbox_trust_field_is_set_even_though_it_starts_unchecked() {
        let section = form::form_section(TWO_STEP_PAGE, "two-step").unwrap();
        assert!(!form::inputs(section).iter().any(|(k, _)| k == "trust"));
        let fields = two_step_fields(TWO_STEP_PAGE, "000000").unwrap();
        assert!(fields.iter().any(|(k, v)| k == "trust" && v == "1"));
    }

    #[test]
    fn relative_redirects_resolve_against_the_base() {
        assert_eq!(absolute("https://h.onion", "/login/two-step?remember=1").unwrap(), "https://h.onion/login/two-step?remember=1");
    }

    #[test]
    fn a_stored_totp_secret_still_errors_for_other_providers() {
        // An email or backup-code challenge cannot be answered from a TOTP
        // seed - the code path must error rather than produce a wrong code.
        let secret = totp::decode_secret("GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ").unwrap();
        let tfa = TwoFactor::Totp(secret);
        assert!(tfa.code("email").is_err());
        let code = tfa.code("totp").unwrap();
        assert_eq!(code.len(), 6);
    }
}
