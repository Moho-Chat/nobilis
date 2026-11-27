//! Rocket.Chat REST API v1 client (`/api/v1/*`).
//!
//! Handles `X-Auth-Token` + `X-User-Id` headers, per-route rate limits
//! (`X-RateLimit-Limit`, `X-RateLimit-Remaining`, `X-RateLimit-Reset`, HTTP 429),
//! 401/403 session halt, multipart uploads (`rooms.upload/{rid}`), authenticated
//! file downloads, and SOCKS5/Tor routing via `crate::net::route::router()`.

use reqwest::Method;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const LONGEST_WAIT: Duration = Duration::from_secs(30);
const RETRIES: usize = 2;
const SPACING: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum ApiError {
    /// Auth token revoked or invalid (HTTP 401/403 on auth).
    Unauthorized,
    /// Two-factor authentication code required or invalid.
    TotpRequired { method: String },
    /// Rocket.Chat returned a structured error (`success: false` or 4xx/5xx).
    Refused { status: u16, error_type: String, message: String },
    /// Transport / network error.
    Network(String),
}

impl ApiError {
    pub fn error_type(&self) -> &str {
        match self {
            ApiError::Refused { error_type, .. } => error_type,
            _ => "",
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => {
                write!(f, "Rocket.Chat session expired or was revoked. Sign in again from Accounts.")
            }
            ApiError::TotpRequired { method } => {
                write!(f, "Two-factor authentication code required ({method}).")
            }
            ApiError::Refused { status, error_type, message } => {
                if !message.is_empty() {
                    write!(f, "{message}")
                } else if !error_type.is_empty() {
                    write!(f, "Rocket.Chat refused request ({error_type})")
                } else {
                    write!(f, "Rocket.Chat returned HTTP {status}")
                }
            }
            ApiError::Network(e) => write!(f, "could not reach Rocket.Chat: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

struct Bucket {
    remaining: u32,
    until: Instant,
}

pub struct Api {
    http: reqwest::Client,
    base: String,
    user_id: Mutex<Option<String>>,
    auth_token: Mutex<Option<String>>,
    buckets: Mutex<HashMap<String, Bucket>>,
    last: tokio::sync::Mutex<Instant>,
    halted: AtomicBool,
}

pub fn route_key(method: &Method, path: &str) -> String {
    let p = path.split('?').next().unwrap_or(path);
    format!("{method} {p}")
}

impl Api {
    pub fn new(route_id: &str, server_url: &str, user_id: Option<String>, auth_token: Option<String>) -> Self {
        let router = crate::net::route::router();
        let http = router.client_if("rocketchat", router.routed(route_id), |b| {
            b.user_agent(super::user_agent()).timeout(Duration::from_secs(60))
        });
        Self {
            http,
            base: server_url.trim_end_matches('/').to_string(),
            user_id: Mutex::new(user_id),
            auth_token: Mutex::new(auth_token),
            buckets: Mutex::new(HashMap::new()),
            last: tokio::sync::Mutex::new(Instant::now() - SPACING),
            halted: AtomicBool::new(false),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    pub fn credentials(&self) -> (Option<String>, Option<String>) {
        let uid = self.user_id.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let tok = self.auth_token.lock().unwrap_or_else(|e| e.into_inner()).clone();
        (uid, tok)
    }

    pub fn set_credentials(&self, user_id: String, auth_token: String) {
        *self.user_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(user_id);
        *self.auth_token.lock().unwrap_or_else(|e| e.into_inner()) = Some(auth_token);
        self.halted.store(false, Ordering::Relaxed);
    }

    fn authorize(&self, mut req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let (uid, tok) = self.credentials();
        if let Some(t) = tok.filter(|s| !s.is_empty()) {
            req = req.header("X-Auth-Token", t);
        }
        if let Some(u) = uid.filter(|s| !s.is_empty()) {
            req = req.header("X-User-Id", u);
        }
        req
    }

    async fn pace(&self, key: &str) {
        let wait = {
            let buckets = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
            buckets
                .get(key)
                .filter(|b| b.remaining == 0 && b.until > Instant::now())
                .map(|b| b.until - Instant::now())
        };
        if let Some(w) = wait {
            tokio::time::sleep(w.min(LONGEST_WAIT)).await;
        }
        let mut last = self.last.lock().await;
        let elapsed = last.elapsed();
        if elapsed < SPACING {
            tokio::time::sleep(SPACING - elapsed).await;
        }
        *last = Instant::now();
    }

    fn learn_rate_limit(&self, key: &str, headers: &reqwest::header::HeaderMap) {
        let remaining = headers
            .get("x-ratelimit-remaining")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u32>().ok());
        let reset_ms = headers
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        if let (Some(rem), Some(reset)) = (remaining, reset_ms) {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let delta = reset.saturating_sub(now_ms).min(30_000);
            self.buckets.lock().unwrap_or_else(|e| e.into_inner()).insert(
                key.to_string(),
                Bucket {
                    remaining: rem,
                    until: Instant::now() + Duration::from_millis(delta),
                },
            );
        }
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
        self.request(Method::GET, path, query, None).await
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.request(Method::POST, path, &[], Some(body)).await
    }

    pub async fn delete(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
        self.request(Method::DELETE, path, query, None).await
    }

    pub async fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value, ApiError> {
        if self.halted() && path != "/api/v1/login" {
            return Err(ApiError::Unauthorized);
        }
        let key = route_key(&method, path);
        let url = format!("{}{path}", self.base);

        for attempt in 0..=RETRIES {
            self.pace(&key).await;
            let mut req = self.http.request(method.clone(), &url);
            if !query.is_empty() {
                req = req.query(query);
            }
            if let Some(b) = body {
                req = req.json(b);
            }
            req = self.authorize(req);

            let resp = req.send().await.map_err(|e| ApiError::Network(e.to_string()))?;
            let status = resp.status().as_u16();
            self.learn_rate_limit(&key, resp.headers());

            if status == 429 && attempt < RETRIES {
                let retry_after = resp
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(2)
                    .min(30);
                tokio::time::sleep(Duration::from_secs(retry_after)).await;
                continue;
            }

            let text = resp.text().await.unwrap_or_default();
            let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let error_type = json["errorType"]
                .as_str()
                .or_else(|| json["error"].as_str())
                .unwrap_or("")
                .to_string();
            let message = json["message"]
                .as_str()
                .or_else(|| json["error"].as_str())
                .unwrap_or("")
                .to_string();

            if error_type == "totp-required" || error_type == "totp-invalid" {
                let method_name = json["details"]["method"]
                    .as_str()
                    .unwrap_or("totp")
                    .to_string();
                return Err(ApiError::TotpRequired { method: method_name });
            }

            if status == 401 && path != "/api/v1/login" {
                self.halted.store(true, Ordering::Relaxed);
                return Err(ApiError::Unauthorized);
            }

            if !(200..300).contains(&status) || json["success"].as_bool() == Some(false) {
                return Err(ApiError::Refused { status, error_type, message });
            }

            return Ok(json);
        }

        Err(ApiError::Refused {
            status: 429,
            error_type: "too-many-requests".to_string(),
            message: "Rocket.Chat rate limit exceeded".to_string(),
        })
    }

    /// Multipart file upload to `POST /api/v1/rooms.upload/{rid}`.
    pub async fn upload_multipart(
        &self,
        path: &str,
        form: reqwest::multipart::Form,
    ) -> Result<Value, ApiError> {
        if self.halted() {
            return Err(ApiError::Unauthorized);
        }
        let url = format!("{}{path}", self.base);
        let req = self.authorize(self.http.post(&url).multipart(form));
        let resp = req.send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 401 {
            self.halted.store(true, Ordering::Relaxed);
            return Err(ApiError::Unauthorized);
        }
        let text = resp.text().await.unwrap_or_default();
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !(200..300).contains(&status) || json["success"].as_bool() == Some(false) {
            return Err(ApiError::Refused {
                status,
                error_type: json["errorType"].as_str().unwrap_or("").to_string(),
                message: json["error"].as_str().or_else(|| json["message"].as_str()).unwrap_or("").to_string(),
            });
        }
        Ok(json)
    }

    /// Download an authenticated file or avatar from the Rocket.Chat server.
    /// Sends `X-Auth-Token`/`X-User-Id` headers as well as `rc_uid`/`rc_token` cookies
    /// (required by Rocket.Chat's `/file-upload/{id}/{name}` endpoint).
    pub async fn get_bytes(&self, path_or_url: &str) -> Result<Option<(String, Vec<u8>)>, ApiError> {
        if self.halted() {
            return Err(ApiError::Unauthorized);
        }
        let url = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            path_or_url.to_string()
        } else {
            format!("{}{}", self.base, path_or_url)
        };
        let mut req = self.authorize(self.http.get(&url));
        let (uid, tok) = self.credentials();
        if let (Some(u), Some(t)) = (uid.filter(|s| !s.is_empty()), tok.filter(|s| !s.is_empty())) {
            req = req.header("Cookie", format!("rc_uid={u}; rc_token={t}"));
        }
        let resp = req.send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        if status == 401 {
            return Err(ApiError::Unauthorized);
        }
        if !(200..300).contains(&status) {
            return Err(ApiError::Refused {
                status,
                error_type: String::new(),
                message: format!("HTTP {status}"),
            });
        }
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = resp.bytes().await.map_err(|e| ApiError::Network(e.to_string()))?;
        Ok(Some((ct, bytes.to_vec())))
    }
}
