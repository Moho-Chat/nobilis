//! The REST client: one per account, with the account's token and route.
//!
//! Mattermost's rate limit (where the server has it on) is one bucket per session or address, told in
//! `X-Ratelimit-Remaining` and `X-Ratelimit-Reset` (seconds until it fills again). It is kept to before a request is
//! made, so a refusal is the exception; a 429 that does come waits as long as it asks and is never taken as an answer.
//! A 401 on a request made with a token means the session is over: nothing more is sent with it.
//!
//! Every request carries `X-Requested-With: XMLHttpRequest`, which is how Mattermost's own app says it is not a
//! browser following a link: without it a refused session is answered with a redirect to the login page rather than
//! with the error.

use reqwest::Method;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The longest a rate limit is waited out in one go before the request is given up.
const LONGEST_WAIT: Duration = Duration::from_secs(30);

/// How many times a request is tried again after being told to wait.
const RETRIES: usize = 2;

/// Why a request did not give an answer.
#[derive(Debug)]
pub enum ApiError {
    /// The session has been refused: nothing more is sent with it.
    Unauthorized,
    /// Mattermost said no, with its own id for why and its own words.
    Refused { status: u16, id: String, message: String },
    /// The request never got an answer.
    Network(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Unauthorized => write!(f, "Mattermost has ended this session. Sign in again from Accounts"),
            ApiError::Refused { status, id, message } => write!(f, "{}", words(*status, id, message)),
            ApiError::Network(e) => write!(f, "could not reach the Mattermost server: {e}"),
        }
    }
}

impl std::error::Error for ApiError {}

impl ApiError {
    /// Mattermost's id for the refusal, or nothing.
    pub fn id(&self) -> &str {
        match self {
            ApiError::Refused { id, .. } => id,
            _ => "",
        }
    }
}

/// Mattermost's refusals, said so a person can act on them. Its own message is good English already and is used where
/// there is nothing better to say; a few are said in words that tell the person what to do.
pub fn words(status: u16, id: &str, message: &str) -> String {
    match id {
        "api.user.login.invalid_credentials_email_username" | "api.user.check_user_password.invalid.app_error" | "api.user.login.invalid_credentials_username" | "api.user.login.invalid_credentials_email" => {
            "That username or email and password were not accepted".to_string()
        }
        "mfa.validate_token.authenticate.app_error" | "api.user.check_user_mfa.bad_code.app_error" => "That code was not accepted".to_string(),
        "api.user.check_user_login_attempts.too_many.app_error" => "Mattermost has locked this account after too many wrong passwords. Ask the server's administrator, or reset the password".to_string(),
        "api.user.login.inactive.app_error" => "This account is deactivated".to_string(),
        "api.user.login.not_verified.app_error" => "This account's email has not been verified. Do that from the link Mattermost emailed first".to_string(),
        "api.context.permissions.app_error" => "You do not have permission to do that here".to_string(),
        "api.post.create_post.town_square_read_only" | "api.post.create_post.channel_read_only" => "This channel is read-only".to_string(),
        "api.file.upload_file.too_large_detailed.app_error" | "api.file.upload_file.too_large.app_error" => "That file is larger than this server allows".to_string(),
        "api.context.invalid_url_param.app_error" | "store.sql_channel.get.existing.app_error" | "app.channel.get.existing.app_error" => "Mattermost says that is not there any more".to_string(),
        _ if status == 429 => "The Mattermost server says too many requests were made. Wait a moment".to_string(),
        _ if !message.is_empty() => message.trim_end_matches('.').to_string(),
        _ if !id.is_empty() => format!("Mattermost refused that ({id})"),
        _ => format!("The Mattermost server answered {status}"),
    }
}

/// Calls left before the server's window fills again.
struct Bucket {
    remaining: u32,
    until: Instant,
}

pub struct Api {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
    bucket: Mutex<Option<Bucket>>,
    halted: AtomicBool,
}

/// A response's headers and body, for a caller that needs both (the session token comes back in a header).
pub struct Answer {
    pub headers: reqwest::header::HeaderMap,
    pub body: Value,
}

impl Api {
    /// A client for a server. `key` names the route (the account's id, or a sign-in's pending key).
    pub fn new(key: &str, server: &str, token: Option<String>) -> Self {
        let router = crate::net::route::router();
        let http = router.client_if("mattermost", router.routed(key), |b| b.user_agent(concat!("moho/", env!("CARGO_PKG_VERSION"))).timeout(Duration::from_secs(60)));
        Self { http, base: server.trim_end_matches('/').to_string(), token, bucket: Mutex::new(None), halted: AtomicBool::new(false) }
    }

    /// The server's address.
    pub fn base(&self) -> &str {
        &self.base
    }

    /// Whether the session was refused.
    pub fn halted(&self) -> bool {
        self.halted.load(Ordering::Relaxed)
    }

    pub async fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Value, ApiError> {
        self.request(Method::GET, path, query, None).await.map(|a| a.body)
    }

    pub async fn post(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.request(Method::POST, path, &[], Some(body)).await.map(|a| a.body)
    }

    pub async fn put(&self, path: &str, body: &Value) -> Result<Value, ApiError> {
        self.request(Method::PUT, path, &[], Some(body)).await.map(|a| a.body)
    }

    pub async fn delete(&self, path: &str) -> Result<Value, ApiError> {
        self.request(Method::DELETE, path, &[], None).await.map(|a| a.body)
    }

    /// Waits for the server's window if it has run dry.
    async fn pace(&self) {
        let wait = {
            let bucket = self.bucket.lock().unwrap_or_else(|e| e.into_inner());
            bucket.as_ref().filter(|b| b.remaining == 0 && b.until > Instant::now()).map(|b| b.until - Instant::now())
        };
        if let Some(wait) = wait {
            tokio::time::sleep(wait.min(LONGEST_WAIT)).await;
        }
    }

    fn learn(&self, headers: &reqwest::header::HeaderMap) {
        let number = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok());
        if let (Some(remaining), Some(reset)) = (number("x-ratelimit-remaining"), number("x-ratelimit-reset")) {
            *self.bucket.lock().unwrap_or_else(|e| e.into_inner()) = Some(Bucket { remaining: remaining as u32, until: Instant::now() + Duration::from_secs(reset) });
        }
    }

    fn authorised(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.header("X-Requested-With", "XMLHttpRequest");
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// What a refused answer says, and whether it ends the session.
    fn refused(&self, status: u16, json: &Value) -> ApiError {
        // The session is over only where one was sent: a wrong password on the login is a refusal, not that.
        if status == 401 && self.token.is_some() {
            self.halted.store(true, Ordering::Relaxed);
            return ApiError::Unauthorized;
        }
        ApiError::Refused { status, id: json["id"].as_str().unwrap_or("").to_string(), message: json["message"].as_str().unwrap_or("").to_string() }
    }

    pub async fn request(&self, method: Method, path: &str, query: &[(&str, String)], body: Option<&Value>) -> Result<Answer, ApiError> {
        for attempt in 0..=RETRIES {
            if self.halted() {
                return Err(ApiError::Unauthorized);
            }
            self.pace().await;
            let mut request = self.authorised(self.http.request(method.clone(), format!("{}{path}", self.base)).query(query));
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request.send().await.map_err(|e| ApiError::Network(e.to_string()))?;
            self.learn(response.headers());
            let status = response.status();
            let headers = response.headers().clone();
            let text = response.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
            let json: Value = if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text).unwrap_or(Value::Null) };
            if status.as_u16() == 429 {
                let wait = headers.get("x-ratelimit-reset").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<u64>().ok()).map(Duration::from_secs).unwrap_or(Duration::from_secs(1)).min(LONGEST_WAIT);
                tracing::info!("mattermost: told to wait {} s on {method} {}", wait.as_secs(), route_of(path));
                if attempt == RETRIES {
                    return Err(self.refused(429, &json));
                }
                tokio::time::sleep(wait + Duration::from_millis(100)).await;
                continue;
            }
            if !status.is_success() {
                return Err(self.refused(status.as_u16(), &json));
            }
            return Ok(Answer { headers, body: json });
        }
        Err(ApiError::Network("gave up waiting".into()))
    }

    /// Sends files to a channel's upload area, and answers what the server made of each (`file_infos`).
    pub async fn upload(&self, channel_id: &str, files: Vec<(String, String, Vec<u8>)>) -> Result<Value, ApiError> {
        if self.halted() {
            return Err(ApiError::Unauthorized);
        }
        self.pace().await;
        let mut form = reqwest::multipart::Form::new().text("channel_id", channel_id.to_string());
        for (name, content_type, bytes) in files {
            let part = reqwest::multipart::Part::bytes(bytes).file_name(name).mime_str(&content_type).map_err(|e| ApiError::Network(e.to_string()))?;
            form = form.part("files", part);
        }
        let response = self.authorised(self.http.post(format!("{}/api/v4/files", self.base))).multipart(form).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        self.learn(response.headers());
        let status = response.status();
        let text = response.text().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            return Err(self.refused(status.as_u16(), &json));
        }
        Ok(json)
    }

    /// Sends a picture as multipart field `image` (a profile picture, a team's icon).
    pub async fn upload_image(&self, path: &str, name: &str, content_type: &str, bytes: Vec<u8>) -> Result<(), ApiError> {
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name.to_string()).mime_str(content_type).map_err(|e| ApiError::Network(e.to_string()))?;
        let form = reqwest::multipart::Form::new().part("image", part);
        let response = self.authorised(self.http.post(format!("{}{path}", self.base))).multipart(form).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let json: Value = response.json().await.unwrap_or(Value::Null);
            return Err(self.refused(status.as_u16(), &json));
        }
        Ok(())
    }

    /// Reads a file the server keeps (an avatar, an attachment, an emoji), with the session: Mattermost serves none of
    /// them to a stranger.
    pub async fn bytes(&self, path: &str) -> Result<(Vec<u8>, Option<String>), ApiError> {
        if self.halted() {
            return Err(ApiError::Unauthorized);
        }
        let response = self.authorised(self.http.get(format!("{}{path}", self.base))).send().await.map_err(|e| ApiError::Network(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let json: Value = response.json().await.unwrap_or(Value::Null);
            return Err(self.refused(status.as_u16(), &json));
        }
        let content_type = response.headers().get(reqwest::header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(str::to_string);
        let bytes = response.bytes().await.map_err(|e| ApiError::Network(e.to_string()))?;
        Ok((bytes.to_vec(), content_type))
    }
}

/// A path with its ids taken out, for a log line that should not hold them.
pub fn route_of(path: &str) -> String {
    path.split('/').map(|p| if super::is_id(p) { ":id" } else { p }).collect::<Vec<_>>().join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_route_is_named_without_its_ids() {
        assert_eq!(route_of("/api/v4/channels/o3tso8bkdjb6dgr3stk8ip7fta/posts"), "/api/v4/channels/:id/posts");
        assert_eq!(route_of("/api/v4/users/me"), "/api/v4/users/me");
    }

    #[test]
    fn mattermosts_refusals_are_said_in_words() {
        assert!(words(401, "api.user.login.invalid_credentials_email_username", "x").contains("password were not accepted"));
        assert!(words(401, "mfa.validate_token.authenticate.app_error", "").contains("code"));
        assert_eq!(words(403, "something.new", "You cannot do this."), "You cannot do this");
        assert_eq!(words(418, "", ""), "The Mattermost server answered 418");
        assert!(words(429, "", "").contains("too many requests"));
    }

    /// A made-up server that answers every request with this status, headers and body.
    async fn serving(status: u16, extra: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let mut seen = [0u8; 4096];
                    let _ = socket.read(&mut seen).await;
                    let head = format!("HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n", body.len());
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        address
    }

    #[tokio::test]
    async fn a_refused_session_halts_the_client_and_a_refused_password_does_not() {
        let server = serving(401, "", r#"{"id":"api.context.session_expired.app_error","message":"Invalid or expired session","status_code":401}"#).await;
        let session = Api::new("mattermost:test-401", &server, Some("t".into()));
        assert!(matches!(session.get("/api/v4/users/me", &[]).await, Err(ApiError::Unauthorized)));
        assert!(session.halted());
        let anonymous = Api::new("mattermost:test-401-anon", &server, None);
        match anonymous.post("/api/v4/users/login", &serde_json::json!({})).await {
            Err(ApiError::Refused { status: 401, id, .. }) => assert_eq!(id, "api.context.session_expired.app_error"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!anonymous.halted());
    }

    #[tokio::test]
    async fn the_rate_limit_the_server_states_is_learned() {
        let server = serving(200, "X-Ratelimit-Remaining: 7\r\nX-Ratelimit-Reset: 1\r\n", "{}").await;
        let api = Api::new("mattermost:test-rate", &server, Some("t".into()));
        api.get("/api/v4/users/me", &[]).await.unwrap();
        assert_eq!(api.bucket.lock().unwrap().as_ref().map(|b| b.remaining), Some(7));
    }
}
