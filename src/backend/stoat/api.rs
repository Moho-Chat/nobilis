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
        "InSlowmode" => format!("This channel has slowmode on: wait {} more seconds before sending again", detail["retry_after"].as_u64().unwrap_or(1)),
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

    /// The instance's address, as requests are made to it.
    pub fn base(&self) -> &str {
        &self.base
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

    /// Puts a file on the instance's file server, and answers the id to name it by in a message.
    pub async fn upload(&self, server: &str, tag: &str, name: &str, content_type: &str, bytes: Vec<u8>) -> Result<String, ApiError> {
        if self.halted() {
            return Err(ApiError::Unauthorized);
        }
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name.to_string()).mime_str(content_type).map_err(|e| ApiError::Network(e.to_string()))?;
        let form = reqwest::multipart::Form::new().part("file", part);
        let mut request = self.http.post(format!("{}/{tag}", server.trim_end_matches('/'))).multipart(form);
        if let Some(token) = &self.token {
            request = request.header("X-Session-Token", token);
        }
        let response = request.send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.as_u16() == 401 {
            self.halted.store(true, Ordering::Relaxed);
            return Err(ApiError::Unauthorized);
        }
        if !status.is_success() {
            return Err(ApiError::Refused { status: status.as_u16(), kind: json["type"].as_str().unwrap_or("").to_string(), detail: json });
        }
        json["id"].as_str().map(str::to_string).ok_or_else(|| ApiError::Refused { status: status.as_u16(), kind: "NoFileId".to_string(), detail: json })
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

    /// A read that gives up on an answer larger than `most` bytes, which is said as `TooLarge` rather than held in memory:
    /// a server's whole list of members may be a hundred thousand long.
    pub async fn get_capped(&self, path: &str, query: &[(&str, String)], most: usize) -> Result<Value, ApiError> {
        self.request_capped(Method::GET, path, query, None, Some(most)).await
    }

    pub async fn request(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>) -> Result<Value, ApiError> {
        self.request_capped(method, path, query, body, None).await
    }

    async fn request_capped(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>, most: Option<usize>) -> Result<Value, ApiError> {
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
            let mut response = request.send().await.map_err(|e| ApiError::Network(e.to_string()))?;
            self.learn(&key, response.headers());
            let status = response.status();
            let text = match most {
                None => response.text().await.map_err(|e| ApiError::Network(e.to_string()))?,
                Some(most) => {
                    // Read as it comes, and stopped the moment it is more than was wanted.
                    let mut held: Vec<u8> = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|e| ApiError::Network(e.to_string()))? {
                        held.extend_from_slice(&chunk);
                        if held.len() > most {
                            return Err(ApiError::Refused { status: status.as_u16(), kind: "TooLarge".into(), detail: Value::Null });
                        }
                    }
                    String::from_utf8_lossy(&held).into_owned()
                }
            };
            let json: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) };
            // A channel's slowmode is told in the same status, in seconds, and is not a rate limit to wait out and try again:
            // it is the answer, and the account is not to ask again until it is over.
            if status.as_u16() == 429 && json["type"].as_str() == Some("InSlowmode") {
                return Err(ApiError::Refused { status: 429, kind: "InSlowmode".into(), detail: json });
            }
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

    /// A made-up server that answers every request with this body.
    async fn serving(body: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                let body = body.clone();
                tokio::spawn(async move {
                    let mut seen = [0u8; 2048];
                    let _ = socket.read(&mut seen).await;
                    let head = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        address
    }

    #[tokio::test]
    async fn an_answer_larger_than_was_wanted_is_given_up_on_and_a_smaller_one_is_read() {
        let big = format!("{{\"members\":[{}]}}", vec!["\"0123456789abcdef\""; 5000].join(","));
        let api = Api::new("stoat:test-capped", &serving(big).await, Some("t".into()));
        match api.get_capped("/servers/X/members", &[], 10_000).await {
            Err(ApiError::Refused { kind, .. }) => assert_eq!(kind, "TooLarge"),
            other => panic!("expected the answer to be given up on, got {other:?}"),
        }
        let small = Api::new("stoat:test-capped-small", &serving("{\"members\":[1,2,3]}".to_string()).await, Some("t".into()));
        assert_eq!(small.get_capped("/servers/X/members", &[], 10_000).await.unwrap()["members"].as_array().unwrap().len(), 3);
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
