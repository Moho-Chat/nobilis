//! The REST client: one per account, with the account's token and route.
//!
//! Stoat limits each route's bucket to a number of calls in a fixed ten-second window, and says so in
//! `X-RateLimit-Remaining` and `X-RateLimit-Reset-After` (milliseconds). Those are kept to before a request is made,
//! so a refusal is the exception; a 429 that does come waits as long as it asks and is never taken as an answer. A 401
//! means the session is over: nothing more is sent with it.

use anyhow::{anyhow, Result};
use reqwest::Method;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The official instance's API address.
pub const DEFAULT_INSTANCE: &str = "https://api.stoat.chat";

/// The longest a rate limit is waited out in one go before the request is given up.
const LONGEST_WAIT: Duration = Duration::from_secs(30);

/// How many times a request is tried again after being told to wait.
const RETRIES: usize = 2;

/// Why a request did not give an answer.
#[derive(Debug)]
pub enum ApiError {
    /// The session has been refused: nothing more is sent with it.
    Unauthorized,
    /// Stoat said no, with its own name for why.
    Refused { status: u16, kind: String, detail: Value },
    /// The request never got an answer.
    Network(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "Stoat has ended this session. Sign in again from Accounts"),
            ApiError::Refused { status, kind, detail } => write!(f, "{}", words(*status, kind, detail)),
            ApiError::Network(e) => write!(f, "could not reach Stoat: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

/// Stoat's error names, said so a person can act on them.
pub fn words(status: u16, kind: &str, detail: &Value) -> String {
    let permission = detail["permission"].as_str().unwrap_or("that");
    match kind {
        "InvalidCredentials" => "That email and password were not accepted".to_string(),
        "InvalidSession" => "Stoat has ended this session. Sign in again from Accounts".to_string(),
        "LockedOut" => "Stoat has locked this account for a while after too many wrong passwords. Try again later".to_string(),
        "UnverifiedAccount" => "This account's email has not been verified. Do that from Stoat's own app first".to_string(),
        "DisabledAccount" => "This account is disabled".to_string(),
        "InvalidToken" | "InvalidOneTimeCode" => "That code was not accepted".to_string(),
        "CaptchaFailed" => "Stoat wants a captcha answered for this, which moho does not do. Do it from Stoat's own app".to_string(),
        "MissingPermission" => format!("You do not have permission here ({permission})"),
        "NotFound" | "UnknownChannel" | "UnknownMessage" | "UnknownServer" | "UnknownUser" => "Stoat says that is not there any more".to_string(),
        "PayloadTooLarge" => "That is too large for Stoat".to_string(),
        "TooManyAttachments" | "TooManyEmbeds" | "TooManyReplies" => "That has more than Stoat allows in one message".to_string(),
        "Blocked" | "BlockedByOther" => "Stoat will not allow that between you and this person".to_string(),
        "TooManyRequests" | "RateLimited" => "Stoat says too many requests were made. Wait a moment".to_string(),
        "FailedValidation" => format!("Stoat refused that: {}", detail["error"].as_str().unwrap_or("it is not valid")),
        other if !other.is_empty() => format!("Stoat refused that ({other})"),
        _ => format!("Stoat answered {status}"),
    }
}

/// Calls left in a route's window, and when it is replenished.
struct Bucket {
    remaining: u32,
    until: Instant,
}

pub struct Api {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    buckets: Mutex<HashMap<String, Bucket>>,
    halted: AtomicBool,
}

/// The route a request belongs to, for pacing: the method and the path with its ids taken out.
pub fn route_key(method: &Method, path: &str) -> String {
    let parts: Vec<&str> = path
        .split('/')
        .filter(|p| !p.is_empty())
        .map(|p| if p.len() == 26 && p.chars().all(|c| c.is_ascii_alphanumeric()) { ":id" } else { p })
        .collect();
    format!("{method} /{}", parts.join("/"))
}

impl Api {
    /// A client for an instance. `key` names the route (the account's id, or a sign-in's pending key).
    pub fn new(key: &str, instance: &str, token: Option<String>) -> Self {
        let router = crate::net::route::router();
        let http = router.client_if("stoat", router.routed(key), |b| b.user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"))).timeout(Duration::from_secs(30)));
        Self { http, base: instance.trim_end_matches('/').to_string(), token, buckets: Mutex::new(HashMap::new()), halted: AtomicBool::new(false) }
    }

    /// Whether the session was refused.
    pub fn halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
        self.request(Method::GET, path, query, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.request(Method::POST, path, &[], Some(body)).await
    }

    pub async fn put(&self, path: &str, body: Option<&Value>) -> Result<Value, ApiError> {
        self.request(Method::PUT, path, &[], body).await
    }

    pub async fn patch(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.request(Method::PATCH, path, &[], Some(body)).await
    }

    pub async fn delete(&self, path: &str) -> Result<Value, ApiError> {
        self.request(Method::DELETE, path, &[], None).await
    }

    /// Waits for the route's window if it has run dry.
    async fn pace(&self, key: &str) {
        let wait = {
            let buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
            buckets.get(key).filter(|b| b.remaining == 0 && b.until > Instant::now()).map(|b| b.until - Instant::now())
        };
        if let Some(wait) = wait {
            tokio::time::sleep(wait.min(LONGEST_WAIT)).await;
        }
    }

    fn learn(&self, key: &str, headers: &reqwest::header::HeaderMap) {
        let number = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        if let (Some(remaining), Some(reset)) = (number("x-ratelimit-remaining"), number("x-ratelimit-reset-after")) {
            self.buckets.lock().unwrap_or_else(|e| e.into_inner()).insert(key.to_string(), Bucket { remaining: remaining as u32, until: Instant::now() + Duration::from_millis(reset) });
        }
    }

    pub async fn request(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>) -> Result<Value, ApiError> {
        let key = route_key(&method, path);
        for attempt in 0..=RETRIES {
            if self.halted() {
                return Err(ApiError::Unauthorized);
            }
            self.pace(&key).await;
            let mut request = self.http.request(method.clone(), format!("{}{path}", self.base)).query(query);
            if let Some(token) = &self.token {
                request = request.header("X-Session-Token", token);
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await.map_err(|e| ApiError::Network(e.to_string()))?;
            self.learn(&key, response.headers());
            let status = response.status();
            let text = response.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
            let json: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) };
            if status.as_u16() == 429 {
                let wait = Duration::from_millis(json["retry_after"].as_u64().unwrap_or(1000)).min(LONGEST_WAIT);
                tracing::info!("stoat: told to wait {} ms on {key}", wait.as_millis());
                if attempt == RETRIES {
                    return Err(ApiError::Refused { status: 429, kind: "TooManyRequests".into(), detail: json });
                }
                tokio::time::sleep(wait + Duration::from_millis(50)).await;
                continue;
            }
            // The session is over only where one was sent: a wrong password on the login is a refusal, not that.
            if status.as_u16() == 401 && self.token.is_some() {
                self.halted.store(true, Ordering::Relaxed);
                return Err(ApiError::Unauthorized);
            }
            if !status.is_success() {
                let kind = json["type"].as_str().unwrap_or("").to_string();
                return Err(ApiError::Refused { status: status.as_u16(), kind, detail: json });
            }
            return Ok(json);
        }
        Err(ApiError::Network("gave up waiting".into()))
    }
}

/// What the instance says of itself at its root, and `Err` if what answered is not Stoat (or Revolt).
pub async fn instance_info(api: &Api) -> Result<Value> {
    let root = api.get("/", &[]).await.map_err(|e| anyhow!("{e}"))?;
    if root.get("stoat").is_none() && root.get("revolt").is_none() {
        return Err(anyhow!("that address did not answer like a Stoat instance"));
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_is_named_without_its_ids() {
        let id = "01HZZZZZZZZZZZZZZZZZZZZZZZ";
        assert_eq!(route_key(&Method::POST, &format!("/channels/{id}/messages")), "POST /channels/:id/messages");
        assert_eq!(route_key(&Method::GET, "/users/@me"), "GET /users/@me");
        assert_eq!(route_key(&Method::GET, "/"), "GET /");
        assert_eq!(route_key(&Method::DELETE, &format!("/channels/{id}/messages/{id}")), "DELETE /channels/:id/messages/:id");
    }

    #[test]
    fn stoats_refusals_are_said_in_words() {
        assert!(words(401, "InvalidCredentials", &Value::Null).contains("email and password"));
        assert!(words(403, "MissingPermission", &serde_json::json!({ "permission": "SendMessage" })).contains("SendMessage"));
        assert!(words(400, "CaptchaFailed", &Value::Null).contains("captcha"));
        assert_eq!(words(418, "", &Value::Null), "Stoat answered 418");
        assert!(words(400, "SomethingNew", &Value::Null).contains("SomethingNew"));
    }
}
