//! The forum session an account signs in with, and keeping it alive.
//!
//! An account is signed in in a browser - moho's own sign-in window, where a
//! person answers the forum's verification themselves - and what the daemon
//! keeps is the session that sign-in left: its cookies. Signing in with a
//! username, password and TOTP code from here is gone: the forum's login form
//! carries a CAPTCHA and its gate admits only a browser, so a password typed
//! into moho can no longer get anybody in. When the session runs out, the
//! account says to sign in with the browser again.

use anyhow::{bail, Context, Result};

use super::http::{CookieJar, HttpClient, Response};
use super::pow;
#[cfg(test)]
use crate::net::tor::Transport;

/// Session cookie the forum issues. Clearing it forces a fresh one.
const SESSION_COOKIE: &str = "xf_session";
/// Identity cookie, which embeds our user ID.
const USER_COOKIE: &str = "xf_user";
/// Upper bound on chained gate challenges for a client of our own (tests).
const MAX_GATE_STEPS: usize = 8;

/// What an account says when its forum session is gone and nothing here can
/// bring it back.
pub const SIGN_IN_AGAIN: &str =
    "the forum session has run out - sign in again with \"Sign in with a browser\" on the account's page";

/// Cloneable so one authenticated session can be shared by everything the
/// account does - cheap, since `HttpClient`'s cookie jar is itself
/// `Arc`-shared, so every clone sees the same live cookies.
#[derive(Clone)]
pub struct Session {
    pub http: HttpClient,
    base: String,
}

impl Session {
    #[cfg(test)]
    pub fn new(transport: Transport, base: String, user_agent: String) -> Self {
        Self { http: HttpClient::new(transport, CookieJar::new(), user_agent), base }
    }

    /// A session whose requests the web helper makes - the only kind the
    /// site's gate now admits (see net/webhelper.rs).
    pub fn via_browser(route: super::http::BrowserRoute, base: String) -> Self {
        Self { http: HttpClient::via_browser(route, CookieJar::new(), String::new()), base }
    }

    /// Whether the current session is authenticated, costing one request.
    pub async fn is_authenticated(&self) -> Result<bool> {
        let resp = self.fetch(&self.base).await?;
        Ok(logged_in_marker(&resp.body))
    }

    /// Succeeds while the forum still knows this session; otherwise says to
    /// sign in with the browser again.
    pub async fn ensure_signed_in(&self) -> Result<()> {
        if self.is_authenticated().await? {
            return Ok(());
        }
        bail!("{SIGN_IN_AGAIN}")
    }

    /// Fetch a page, getting past the site's gate first if challenged.
    pub async fn fetch(&self, url: &str) -> Result<Response> {
        let resp = self.http.get(url).await?;
        if resp.status != pow::GATE_STATUS {
            return Ok(resp);
        }
        if self.http.browser_route().is_some() {
            // The gate's own script, run in a page of the helper's: the
            // clearance it leaves is good only on that network stack, which is
            // the one this session's requests already go out on.
            self.http.clear_gate_in_browser(url).await.context("getting past the site's gate")?;
        } else {
            pow::clear(&self.http, url, MAX_GATE_STEPS).await.context("solving the gate")?;
        }
        let resp = self.http.get(url).await?;
        if resp.status == pow::GATE_STATUS {
            bail!("the site's gate is still in the way after clearing it");
        }
        Ok(resp)
    }

    /// Drop the session cookie and obtain a new one from the remember-me
    /// cookie - what repairs a chat connection that was downgraded to a
    /// guest while the forum still knows the account.
    ///
    /// The old cookie is put back when no better one comes: dropping it with
    /// nothing to replace it is how a working account becomes a locked-out
    /// one. And if the forum no longer knows the account at all, that is a
    /// browser sign-in, which is said.
    pub async fn refresh(&self) -> Result<()> {
        let previous = self.http.jar.get(SESSION_COOKIE);
        let put_back = || {
            if let Some(value) = &previous {
                self.http.jar.set(SESSION_COOKIE, value);
            }
        };

        self.http.jar.remove(SESSION_COOKIE);
        let resp = match self.fetch(&self.base).await {
            Ok(resp) => resp,
            Err(e) => {
                put_back();
                return Err(e);
            }
        };
        if !(200..400).contains(&resp.status) {
            put_back();
            bail!("session refresh got HTTP {}", resp.status);
        }
        if logged_in_marker(&resp.body) {
            return Ok(());
        }
        put_back();
        bail!("{SIGN_IN_AGAIN}")
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
}

/// XenForo marks the `<html>` element of every page it renders, which is a
/// more reliable signal than a cookie's mere presence: a stale `xf_user`
/// looks identical to a live one from the client side.
fn logged_in_marker(html: &str) -> bool {
    html.contains("data-logged-in=\"true\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_the_logged_in_marker() {
        assert!(logged_in_marker(r#"<html id="XF" data-logged-in="true" data-xf="2.3">"#));
        assert!(!logged_in_marker(r#"<html id="XF" data-logged-in="false" data-xf="2.3">"#));
        assert!(!logged_in_marker("<html>"));
    }

    #[test]
    fn a_lapsed_session_points_at_the_browser_sign_in() {
        assert!(SIGN_IN_AGAIN.contains("Sign in with a browser"));
    }
}
