//! The signed-in session: the cookies a browser sign-in left, sent the way the web app sends them, and the XSRF token
//! the web app's own page carries.
//!
//! The cookies are kept by name alone, with no domains and no paths, and go only to Google's own hosts. Google rejects
//! quoted values, which is one reason this is not a cookie store of the usual kind; the other is that what matters is
//! the set the web app sends to chat.google.com, which is what the sign-in window took. Google rotates some of them as
//! the session goes on (`SIDCC`, and the BrowserChannel's own `COMPASS`), so whatever a response sets is taken up and
//! the account's copy saved again now and then - a session whose rotations are thrown away dies early.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use regex::Regex;
use reqwest::header::{HeaderMap, HeaderValue, COOKIE, SET_COOKIE, USER_AGENT};
use std::sync::OnceLock;

/// What a browser of the day says it is, for an account signed in before the window's own was kept.
pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36";

/// Where the web app lives. Always the first signed-in account in the browser (`/u/0`): the sign-in window holds one.
pub const BASE: &str = "https://chat.google.com/u/0";

/// The shell state Gmail's Chat roster sends for `/mole/world`, copied as the web client sends it.
const MOLE_WORLD_HS: &str = r#"["h_hs",null,null,[1,0],null,null,"gmail.pinto-server_20230730.06_p0",1,null,[15,38,36,35,26,30,41,18,24,11,21,14,6],null,null,"3Mu86PSulM4.en..es5",0,null,null,[0]]"#;

/// How long an XSRF token is used before the page is asked for a fresh one.
const XSRF_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);

/// Why a request did not give an answer.
#[derive(Debug)]
pub enum GcError {
    /// The cookies no longer sign anybody in: the person has to sign in again.
    SignedOut,
    /// Google answered, with something other than success.
    Status { status: u16, body: String },
    /// Google could not be reached, or the answer could not be read.
    Network(String),
}

impl std::fmt::Display for GcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GcError::SignedOut => write!(f, "Google has signed this session out. Sign in to Google Chat again from Accounts"),
            GcError::Status { status, body } => {
                let body = body.trim();
                if body.is_empty() || body.starts_with('<') {
                    write!(f, "Google Chat answered {status}")
                } else {
                    write!(f, "Google Chat answered {status}: {}", body.chars().take(200).collect::<String>())
                }
            }
            GcError::Network(e) => write!(f, "could not reach Google Chat: {e}"),
        }
    }
}

impl std::error::Error for GcError {}

impl GcError {
    pub fn is_signed_out(&self) -> bool {
        matches!(self, GcError::SignedOut) || matches!(self, GcError::Status { status: 401, .. })
    }
}

pub struct Session {
    /// The route key: the account's id, or a sign-in's.
    key: String,
    user_agent: String,
    cookies: Mutex<BTreeMap<String, String>>,
    /// Whether the cookies changed since the account's copy was last saved.
    rotated: Mutex<bool>,
    xsrf: Mutex<Option<(String, Instant)>>,
    /// One refresh at a time: a burst of 401s should not each fetch the page.
    refreshing: tokio::sync::Mutex<()>,
}

impl Session {
    pub fn new(key: &str, cookies: BTreeMap<String, String>, user_agent: &str) -> Self {
        let user_agent = if user_agent.trim().is_empty() { DEFAULT_USER_AGENT.to_string() } else { user_agent.trim().to_string() };
        Session {
            key: key.to_string(),
            user_agent,
            cookies: Mutex::new(cookies),
            rotated: Mutex::new(false),
            xsrf: Mutex::new(None),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// The jar as it stands, for saving.
    pub fn cookies(&self) -> BTreeMap<String, String> {
        self.cookies.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The jar, if it changed since this was last asked.
    pub fn take_rotated(&self) -> Option<BTreeMap<String, String>> {
        let mut rotated = self.rotated.lock().unwrap_or_else(|e| e.into_inner());
        if !*rotated {
            return None;
        }
        *rotated = false;
        Some(self.cookies())
    }

    pub fn cookie(&self, name: &str) -> Option<String> {
        self.cookies.lock().unwrap_or_else(|e| e.into_inner()).get(name).cloned()
    }

    fn cookie_header(&self) -> String {
        let jar = self.cookies.lock().unwrap_or_else(|e| e.into_inner());
        jar.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }

    /// A client for ordinary requests: they each finish within a minute.
    pub fn client(&self) -> reqwest::Client {
        let router = crate::net::route::router();
        router.client_if("googlechat", router.routed(&self.key), |b| b.timeout(Duration::from_secs(60)).connect_timeout(Duration::from_secs(30)))
    }

    /// A client for the long poll, which is held open for up to an hour: only connecting has a limit. Reading is
    /// watched by the channel itself.
    pub fn poll_client(&self) -> reqwest::Client {
        let router = crate::net::route::router();
        router.client_if("googlechat-poll", router.routed(&self.key), |b| b.connect_timeout(Duration::from_secs(30)))
    }

    /// The browser's headers, and the cookies for a Google host.
    pub fn headers(&self, url: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&self.user_agent) {
            headers.insert(USER_AGENT, v);
        }
        if is_google(url) {
            if let Ok(v) = HeaderValue::from_str(&self.cookie_header()) {
                headers.insert(COOKIE, v);
            }
        }
        headers
    }

    /// Takes up whatever cookies a response from Google set. Only where the answer finally came from was Google: a
    /// redirect somewhere else must not be able to write into the jar.
    pub fn absorb(&self, response: &reqwest::Response) {
        if !is_google(response.url().as_str()) {
            return;
        }
        let mut changed = false;
        {
            let mut jar = self.cookies.lock().unwrap_or_else(|e| e.into_inner());
            for value in response.headers().get_all(SET_COOKIE) {
                let Ok(text) = value.to_str() else { continue };
                let Some((name, value)) = text.split(';').next().and_then(|pair| pair.split_once('=')) else { continue };
                let (name, value) = (name.trim(), value.trim());
                if name.is_empty() || value.is_empty() {
                    continue;
                }
                // A cookie being expired is set to a past date; keeping the old value is the lesser harm than
                // misreading the date, and the next sign-in replaces the lot.
                let lower = text.to_ascii_lowercase();
                if lower.contains("max-age=0") || lower.contains("expires=thu, 01-jan-1970") || lower.contains("expires=thu, 01 jan 1970") {
                    continue;
                }
                if jar.get(name).map(String::as_str) != Some(value) {
                    jar.insert(name.to_string(), value.to_string());
                    changed = true;
                }
            }
        }
        if changed {
            *self.rotated.lock().unwrap_or_else(|e| e.into_inner()) = true;
        }
    }

    pub fn xsrf(&self) -> Option<String> {
        self.xsrf.lock().unwrap_or_else(|e| e.into_inner()).as_ref().filter(|(_, at)| at.elapsed() < XSRF_LIFETIME).map(|(t, _)| t.clone())
    }

    /// The XSRF token, fetched when there is none or it is old.
    pub async fn ensure_xsrf(&self) -> Result<String, GcError> {
        if let Some(token) = self.xsrf() {
            return Ok(token);
        }
        self.refresh_xsrf(None).await
    }

    /// Asks the web app's page for a fresh XSRF token. `stale` is the token a refused request used: if another
    /// request has already replaced it, that one is taken rather than asking again.
    pub async fn refresh_xsrf(&self, stale: Option<&str>) -> Result<String, GcError> {
        let _one = self.refreshing.lock().await;
        if let (Some(current), Some(stale)) = (self.xsrf(), stale) {
            if current != stale {
                return Ok(current);
            }
        }
        let token = self.fetch_xsrf().await?;
        *self.xsrf.lock().unwrap_or_else(|e| e.into_inner()) = Some((token.clone(), Instant::now()));
        Ok(token)
    }

    async fn fetch_xsrf(&self) -> Result<String, GcError> {
        let url = reqwest::Url::parse_with_params(
            &format!("{BASE}/mole/world"),
            &[("origin", "https://mail.google.com"), ("shell", "9"), ("hl", "en"), ("wfi", "gtn-roster-iframe-id"), ("hs", MOLE_WORLD_HS)],
        )
        .map_err(|e| GcError::Network(e.to_string()))?;
        let mut headers = self.headers(url.as_str());
        // Misspelled as the web client sends it.
        headers.insert("refer", HeaderValue::from_static("https://mail.google.com/"));
        let response = self.client().get(url).headers(headers).send().await.map_err(|e| GcError::Network(crate::backend::mattermost::api::network_words(&e)))?;
        self.absorb(&response);
        let status = response.status().as_u16();
        let final_host = response.url().host_str().unwrap_or("").to_string();
        let body = response.text().await.map_err(|e| GcError::Network(e.to_string()))?;
        // Signed out, a signed-out page sends the browser to the sign-in page rather than answering.
        if final_host == "accounts.google.com" || status == 401 {
            return Err(GcError::SignedOut);
        }
        if status != 200 {
            return Err(GcError::Status { status, body: String::new() });
        }
        parse_wiz(&body)
    }
}

/// Reads the XSRF token out of the page's `WIZ_global_data`.
pub fn parse_wiz(body: &str) -> Result<String, GcError> {
    static WIZ: OnceLock<Regex> = OnceLock::new();
    let wiz = WIZ.get_or_init(|| Regex::new(r">window\.WIZ_global_data = (\{.+?\});</script>").expect("a fixed pattern"));
    let data = wiz
        .captures(body)
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c[1]).ok())
        .ok_or_else(|| GcError::Network("the Google Chat page did not carry its settings".into()))?;
    if data["qwAQke"].as_str() == Some("AccountsSignInUi") {
        return Err(GcError::SignedOut);
    }
    data["SMqcke"].as_str().filter(|s| !s.is_empty()).map(str::to_string).ok_or_else(|| GcError::Network("the Google Chat page carried no XSRF token".into()))
}

/// Whether a URL is one of Google's own, which alone are sent the cookies. Not googleusercontent.com: what is fetched
/// from there (avatars, attachments) goes without them.
pub fn is_google(url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(url) else { return false };
    url.scheme() == "https" && url.host_str().is_some_and(|h| h == "google.com" || h.ends_with(".google.com"))
}

/// The cookies a sign-in window captured, as a `Cookie:` header (`a=b; c=d`) or a JSON object of name to value.
pub fn parse_cookies(text: &str) -> BTreeMap<String, String> {
    let text = text.trim();
    if text.starts_with('{') {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(text) {
            return map.into_iter().filter_map(|(k, v)| v.as_str().map(|v| (k, v.to_string()))).filter(|(k, v)| !k.is_empty() && !v.is_empty()).collect();
        }
    }
    text.split(';')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, v)| !k.is_empty() && !v.is_empty())
        .collect()
}

/// The cookies without which there is no session, as the bridge found them.
pub const REQUIRED_COOKIES: [&str; 4] = ["SID", "HSID", "SSID", "COMPASS"];

pub fn missing_cookies(jar: &BTreeMap<String, String>) -> Vec<&'static str> {
    REQUIRED_COOKIES.iter().copied().filter(|name| !jar.contains_key(*name)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_token_and_the_signed_out_page() {
        let page = r#"<script>window.WIZ_global_data = {"SMqcke":"AKx:123","qwAQke":"DynamiteWebUi"};</script>"#;
        assert_eq!(parse_wiz(page).unwrap(), "AKx:123");
        let out = r#"<script>window.WIZ_global_data = {"qwAQke":"AccountsSignInUi"};</script>"#;
        assert!(matches!(parse_wiz(out), Err(GcError::SignedOut)));
        assert!(parse_wiz("<html></html>").is_err());
    }

    #[test]
    fn cookies_go_only_to_google() {
        assert!(is_google("https://chat.google.com/u/0/api/x"));
        assert!(!is_google("http://chat.google.com/"));
        assert!(!is_google("https://lh3.googleusercontent.com/a"));
        assert!(!is_google("https://evilgoogle.com/"));
    }

    #[test]
    fn parses_a_header_or_an_object() {
        let jar = parse_cookies("SID=a; HSID=b ;SSID=c; COMPASS=dynamite=x=y");
        assert_eq!(jar.get("COMPASS").map(String::as_str), Some("dynamite=x=y"));
        assert!(missing_cookies(&jar).is_empty());
        let jar = parse_cookies(r#"{"SID":"a","OSID":"b"}"#);
        assert_eq!(missing_cookies(&jar), vec!["HSID", "SSID", "COMPASS"]);
    }
}
