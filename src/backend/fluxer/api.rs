//! The REST client: one per account, with the account's token and route.
//!
//! Fluxer keeps a bucket per route and, above them, a global allowance of 50 requests a second per caller - and **a
//! global refusal on a user's session revokes the session** (`topics/rate-limits.md`). So requests here are spaced to
//! a fifth of that at most, whatever the routes say, and a route that has run dry (`X-RateLimit-Remaining` 0) is waited
//! out before it is asked again. A 429 that does come waits `retry_after` (seconds, with a fraction) and is never taken
//! as an answer; a 503 waits its `Retry-After`. A 401 means the session is over, and nothing more is sent with it.

use reqwest::Method;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The longest a refusal is waited out in one go before the request is given up.
const LONGEST_WAIT: Duration = Duration::from_secs(30);
/// How many times a request is tried again after being told to wait.
const RETRIES: usize = 2;
/// The least time between two requests on one account: ten a second, a fifth of Fluxer's global allowance.
const SPACING: Duration = Duration::from_millis(100);

/// Why a request did not give an answer.
#[derive(Debug)]
pub enum ApiError {
    /// The session has been refused: nothing more is sent with it.
    Unauthorized,
    /// Fluxer said no, with its own code for why.
    Refused { status: u16, code: String, detail: Value },
    /// The request never got an answer.
    Network(String),
}

impl ApiError {
    /// Fluxer's code, or nothing.
    pub fn code(&self) -> &str {
        match self {
            ApiError::Refused { code, .. } => code,
            _ => "",
        }
    }

    /// The first validation code of a 400 `INVALID_FORM_BODY`, which is where Fluxer says what was wrong with a field.
    pub fn field_code(&self) -> Option<&str> {
        match self {
            ApiError::Refused { detail, .. } => detail["errors"].as_array().and_then(|e| e.first()).and_then(|e| e["code"].as_str()),
            _ => None,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "Fluxer has ended this session. Sign in again from Accounts"),
            ApiError::Refused { status, code, detail } => write!(f, "{}", words(*status, code, detail)),
            ApiError::Network(e) => write!(f, "could not reach Fluxer: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Fluxer's error codes, said so a person can act on them. Anything not listed falls back to Fluxer's own message,
/// which is already a sentence in the request's language.
pub fn words(status: u16, code: &str, detail: &Value) -> String {
    let field = detail["errors"].as_array().and_then(|e| e.first()).and_then(|e| e["code"].as_str()).unwrap_or("");
    let own = detail["message"].as_str().filter(|m| !m.is_empty());
    match (code, field) {
        (_, "INVALID_EMAIL_OR_PASSWORD") => "That email and password were not accepted".to_string(),
        (_, "INVALID_LOGIN_OR_PASSWORD") => "That username and password were not accepted".to_string(),
        (_, "INVALID_CODE") | (_, "INVALID_MFA_CODE") => "That code is not right. Check it and try again".to_string(),
        (_, "SESSION_TIMEOUT") => "The sign-in waited too long. Start it again".to_string(),
        ("CAPTCHA_REQUIRED" | "INVALID_CAPTCHA", _) => "Fluxer's proof of work was not accepted. Try again in a moment".to_string(),
        ("IP_AUTHORIZATION_REQUIRED", _) => "Fluxer has emailed a link to approve signing in from here".to_string(),
        ("ACCOUNT_SUSPENDED_TEMPORARILY" | "ACCOUNT_SUSPENDED_PERMANENTLY", _) => {
            let summary = detail["notice"]["summary"].as_str().unwrap_or("This account is suspended.");
            format!("{summary} See Fluxer's own app for the notice")
        }
        ("GLOBAL_IP_BANNED" | "GLOBAL_IP_TEMPORARILY_BANNED", _) => "Fluxer is refusing this network. Fluxer's own app says why".to_string(),
        ("DATE_OF_BIRTH_REQUIRED", _) => "Fluxer wants this account's date of birth first. Enter it in Fluxer's own app".to_string(),
        ("SUDO_MODE_REQUIRED", _) => "Fluxer wants the password or a second factor again for this. Do it from Fluxer's own app".to_string(),
        ("ACCOUNT_LIMITED", _) => "Fluxer has limited this account, and will not allow that. Fluxer's own app says why".to_string(),
        ("SSO_REQUIRED", _) => "This instance signs people in through its own single sign-on, which moho does not do. Paste a token instead".to_string(),
        ("MISSING_PERMISSIONS" | "ACCESS_DENIED", _) => "You do not have permission to do that here".to_string(),
        ("COMMUNICATION_DISABLED", _) => "You are timed out in this community".to_string(),
        ("SLOWMODE_RATE_LIMITED", _) => format!("This channel has slowmode on: wait {} more seconds before sending again", detail["retry_after"].as_f64().map(|s| s.ceil() as u64).unwrap_or(1)),
        ("RATE_LIMITED", _) => "Fluxer says too many requests were made. Wait a moment".to_string(),
        ("FILE_SIZE_TOO_LARGE", _) => "That file is larger than Fluxer allows".to_string(),
        ("TWO_FACTOR_REQUIRED", _) => "This community wants two-factor sign-in on the account for that".to_string(),
        (c, _) if c.starts_with("UNKNOWN_") || c == "NOT_FOUND" => "Fluxer says that is not there any more".to_string(),
        _ => match own {
            Some(message) => format!("Fluxer refused that: {message}"),
            None if !code.is_empty() => format!("Fluxer refused that ({code})"),
            None => format!("Fluxer answered {status}"),
        },
    }
}

/// Calls left in a route's bucket, and when it is full again.
struct Bucket {
    remaining: u32,
    until: Instant,
}

pub struct Api {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    buckets: Mutex<HashMap<String, Bucket>>,
    /// When the last request went, for the spacing every request keeps.
    last: tokio::sync::Mutex<Instant>,
    /// Set by a refusal of the global bucket: nothing goes before it.
    global_until: Mutex<Option<Instant>>,
    halted: AtomicBool,
}

/// The route a request belongs to, for pacing: the method and the path with its snowflakes taken out.
pub fn route_key(method: &Method, path: &str) -> String {
    let path = path.split('?').next().unwrap_or("");
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).map(|p| if p.len() >= 15 && p.chars().all(|c| c.is_ascii_digit()) { ":id" } else { p }).collect();
    format!("{method} /{}", parts.join("/"))
}

impl Api {
    /// A client for an instance's API (`base` ends in `/v1`). `key` names the route (the account's id, or a sign-in's
    /// pending key).
    pub fn new(key: &str, base: &str, token: Option<String>) -> Self {
        let router = crate::net::route::router();
        let http = router.client_if("fluxer", router.routed(key), |b| b.user_agent(super::user_agent()).timeout(Duration::from_secs(60)));
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            token,
            buckets: Mutex::new(HashMap::new()),
            last: tokio::sync::Mutex::new(Instant::now() - SPACING),
            global_until: Mutex::new(None),
            halted: AtomicBool::new(false),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Whether the session was refused.
    pub fn halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
        self.request(Method::GET, path, query, None, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.request(Method::POST, path, &[], Some(body), None).await
    }

    pub async fn put(&self, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        self.request(Method::PUT, path, &[], body, None).await
    }

    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.request(Method::PATCH, path, &[], Some(body), None).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value, ApiError> {
        self.request(Method::DELETE, path, &[], None, None).await
    }

    /// A request that may be gated by ALTCHA: asked once, and if Fluxer wants its proof of work, asked again with the
    /// challenge solved - twice at most, since a rejected answer comes with a new challenge.
    pub async fn gated(&self, method: Method, path: &str, body: &Value) -> Result<Value, ApiError> {
        let mut captcha: Option<String> = None;
        for _ in 0..3 {
            match self.request(method.clone(), path, &[], Some(body), captcha.as_deref()).await {
                Err(ApiError::Refused { code, detail, .. }) if matches!(code.as_str(), "CAPTCHA_REQUIRED" | "INVALID_CAPTCHA") && detail["altcha_challenge"].is_object() => {
                    captcha = Some(super::altcha::solve(&detail["altcha_challenge"]).await.map_err(|e| ApiError::Network(e.to_string()))?);
                }
                other => return other,
            }
        }
        Err(ApiError::Refused { status: 400, code: "INVALID_CAPTCHA".into(), detail: Value::Null })
    }

    /// Keeps the spacing, the global wait and the route's bucket before a request goes.
    async fn pace(&self, key: &str) {
        let global = *self.global_until.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(until) = global.filter(|u| *u > Instant::now()) {
            tokio::time::sleep((until - Instant::now()).min(LONGEST_WAIT)).await;
        }
        let route = {
            let buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
            buckets.get(key).filter(|b| b.remaining == 0 && b.until > Instant::now()).map(|b| b.until - Instant::now())
        };
        if let Some(wait) = route {
            tokio::time::sleep(wait.min(LONGEST_WAIT)).await;
        }
        let mut last = self.last.lock().await;
        let since = last.elapsed();
        if since < SPACING {
            tokio::time::sleep(SPACING - since).await;
        }
        *last = Instant::now();
    }

    fn learn(&self, key: &str, headers: &reqwest::header::HeaderMap) {
        let number = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
        if let (Some(remaining), Some(reset)) = (number("x-ratelimit-remaining"), number("x-ratelimit-reset-after")) {
            self.buckets.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), Bucket { remaining: remaining as u32, until: Instant::now() + Duration::from_secs_f64(reset.clamp(0.0, 3600.0)) });
        }
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        // The session goes in `Authorization`, bare, as a user's does (`authentication.md`).
        match &self.token {
            Some(token) => request.header("Authorization", token),
            None => request,
        }
    }

    pub async fn request(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>, captcha: Option<&str>) -> Result<Value, ApiError> {
        self.send(method, path, query, captcha, |r| match body {
            Some(body) => r.json(body),
            None => r,
        })
        .await
    }

    /// A message with files, as `multipart/form-data`: `payload_json` and `files[n]` (`http-api/index.md`).
    pub async fn multipart(&self, method: Method, path: &str, payload: &Value, files: &[(String, String, Vec<u8>)]) -> Result<Value, ApiError> {
        self.send(method, path, &[], None, |r| {
            let mut form = reqwest::multipart::Form::new().text("payload_json", payload.to_string());
            for (n, (name, content_type, bytes)) in files.iter().enumerate() {
                let part = reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.clone());
                let part = part.mime_str(content_type).unwrap_or_else(|_| reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.clone()));
                form = form.part(format!("files[{n}]"), part);
            }
            r.multipart(form)
        })
        .await
    }

    async fn send(&self, method: Method, path: &str, query: &[(&str, String)], captcha: Option<&str>, body: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder) -> Result<Value, ApiError> {
        let key = route_key(&method, path);
        for attempt in 0..=RETRIES {
            if self.halted() {
                return Err(ApiError::Unauthorized);
            }
            self.pace(&key).await;
            let mut request = self.authorize(self.http.request(method.clone(), format!("{}{path}", self.base)).query(query));
            if let Some(token) = captcha {
                request = request.header("X-Captcha-Token", token);
            }
            let response = body(request).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
            self.learn(&key, response.headers());
            let status = response.status().as_u16();
            let retry_header = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
            let text = response.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
            let json: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) };
            let code = json["code"].as_str().unwrap_or("").to_string();
            if status == 429 && code == "RATE_LIMITED" || status == 429 && json["retry_after"].is_number() && code.is_empty() {
                let wait = Duration::from_secs_f64(json["retry_after"].as_f64().or(retry_header).unwrap_or(1.0).clamp(0.001, 3600.0));
                if json["global"].as_bool() == Some(true) {
                    *self.global_until.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now() + wait);
                }
                tracing::info!("fluxer: told to wait {} ms on {key}", wait.as_millis());
                if attempt == RETRIES || wait > LONGEST_WAIT {
                    return Err(ApiError::Refused { status, code: "RATE_LIMITED".into(), detail: json });
                }
                tokio::time::sleep(wait + Duration::from_millis(50)).await;
                continue;
            }
            if status == 503 && attempt < RETRIES {
                tokio::time::sleep(Duration::from_secs_f64(retry_header.unwrap_or(1.0).clamp(0.5, 30.0))).await;
                continue;
            }
            // The session is over only where one was sent: a wrong password on the login is a refusal, not that.
            if status == 401 && self.token.is_some() {
                self.halted.store(true, Ordering::Relaxed);
                return Err(ApiError::Unauthorized);
            }
            if !(200..300).contains(&status) {
                return Err(ApiError::Refused { status, code, detail: json });
            }
            return Ok(json);
        }
        Err(ApiError::Network("gave up waiting".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_route_is_named_without_its_snowflakes() {
        assert_eq!(route_key(&Method::POST, "/channels/1501314428688998182/messages"), "POST /channels/:id/messages");
        assert_eq!(route_key(&Method::GET, "/users/@me"), "GET /users/@me");
        assert_eq!(route_key(&Method::PUT, "/channels/1501314428688998182/messages/1501320000000000000/reactions/%F0%9F%91%8D/@me"), "PUT /channels/:id/messages/:id/reactions/%F0%9F%91%8D/@me");
        assert_eq!(route_key(&Method::GET, "/channels/1501314428688998182/messages?limit=50"), "GET /channels/:id/messages");
    }

    #[test]
    fn fluxers_refusals_are_said_in_words() {
        let form = |field: &str| json!({ "code": "INVALID_FORM_BODY", "message": "Invalid form body.", "errors": [{ "path": "login", "code": field, "message": "x" }] });
        assert!(words(400, "INVALID_FORM_BODY", &form("INVALID_LOGIN_OR_PASSWORD")).contains("username and password"));
        assert!(words(400, "INVALID_FORM_BODY", &form("INVALID_CODE")).contains("code"));
        assert!(words(400, "SLOWMODE_RATE_LIMITED", &json!({ "retry_after": 4.2 })).contains("5 more seconds"));
        assert!(words(403, "MISSING_PERMISSIONS", &Value::Null).contains("permission"));
        assert!(words(404, "UNKNOWN_CHANNEL", &Value::Null).contains("not there"));
        assert!(words(403, "ACCOUNT_SUSPENDED_TEMPORARILY", &json!({ "notice": { "summary": "Suspended for a week." } })).starts_with("Suspended for a week."));
        assert_eq!(words(400, "SOMETHING_NEW", &json!({ "message": "Nope." })), "Fluxer refused that: Nope.");
        assert_eq!(words(418, "", &Value::Null), "Fluxer answered 418");
    }

    #[test]
    fn a_form_error_says_which_field_and_why() {
        let e = ApiError::Refused { status: 400, code: "INVALID_FORM_BODY".into(), detail: json!({ "errors": [{ "path": "code", "code": "INVALID_CODE" }] }) };
        assert_eq!(e.field_code(), Some("INVALID_CODE"));
        assert_eq!(e.code(), "INVALID_FORM_BODY");
        assert_eq!(ApiError::Unauthorized.field_code(), None);
    }
}
