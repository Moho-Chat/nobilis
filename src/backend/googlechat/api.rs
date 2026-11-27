//! The RPCs: binary protobuf POSTed to `/u/0/api/<name>`, answered in binary protobuf (or that in base64, which the
//! server sometimes chooses).
//!
//! A 401 is first taken to mean the XSRF token went stale: the page is asked for a fresh one and the request made once
//! more. Only a second refusal, or the page itself saying nobody is signed in, ends the session.
//!
//! Requests that make something - a message, a conversation - are not repeated after a server error, since the first
//! one may well have done it. A 429 is repeated after the wait it asks for, as it was refused rather than done.

use super::pb;
use super::session::{GcError, Session, BASE};
use base64::Engine;
use reqwest::header::{HeaderValue, CONTENT_TYPE};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The web client's API key: without it every RPC is refused.
const API_KEY: &str = "AIzaSyD7InnYR3VKdb4j2rMUEbTCIr2VyEazl6k";

/// The web client version the requests say they come from.
const CLIENT_VERSION: i64 = 2_440_378_181_258;

const ATTEMPTS: usize = 3;

pub struct Api {
    pub session: Session,
    counter: AtomicU64,
}

pub fn request_header() -> pb::RequestHeader {
    pb::RequestHeader {
        client_type: Some(pb::request_header::ClientType::Web as i32),
        client_version: Some(CLIENT_VERSION),
        client_feature_capabilities: Some(pb::ClientFeatureCapabilities {
            spam_room_invites_level: Some(pb::client_feature_capabilities::CapabilityLevel::FullySupported as i32),
            ..Default::default()
        }),
        ..Default::default()
    }
}

impl Api {
    pub fn new(session: Session) -> Self {
        Api { session, counter: AtomicU64::new(0) }
    }

    /// One RPC. `idempotent` says whether a server error may be answered by asking again.
    pub async fn call<Req: prost::Message, Resp: prost::Message + Default>(&self, endpoint: &str, request: &Req, idempotent: bool) -> Result<Resp, GcError> {
        let token = self.session.ensure_xsrf().await?;
        match self.call_once(endpoint, request, &token, idempotent).await {
            Err(GcError::Status { status: 401, .. }) => {
                let fresh = self.session.refresh_xsrf(Some(&token)).await?;
                self.call_once(endpoint, request, &fresh, idempotent).await
            }
            other => other,
        }
    }

    async fn call_once<Req: prost::Message, Resp: prost::Message + Default>(&self, endpoint: &str, request: &Req, xsrf: &str, idempotent: bool) -> Result<Resp, GcError> {
        let body = request.encode_to_vec();
        let mut last = GcError::Network("no attempt made".into());
        for attempt in 0..ATTEMPTS {
            let count = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
            let url = format!("{BASE}/api/{endpoint}?c={count}&rt=b&alt=proto&key={API_KEY}");
            let mut headers = self.session.headers(&url);
            headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/x-protobuf"));
            headers.insert("x-goog-encode-response-if-executable", HeaderValue::from_static("base64"));
            if let Ok(v) = HeaderValue::from_str(xsrf) {
                headers.insert("x-framework-xsrf-token", v);
            }
            let response = match self.session.client().post(&url).headers(headers).body(body.clone()).send().await {
                Ok(r) => r,
                Err(e) => {
                    last = GcError::Network(crate::backend::mattermost::api::network_words(&e));
                    // Not sent at all, or no answer: a send may have happened, so only an idempotent call goes again.
                    if !idempotent && !e.is_connect() {
                        return Err(last);
                    }
                    tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
                    continue;
                }
            };
            self.session.absorb(&response);
            let status = response.status().as_u16();
            let retry_after = response.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
            let bytes = response.bytes().await.map_err(|e| GcError::Network(e.to_string()))?;
            if status == 200 {
                return decode_response(&bytes).map_err(|e| GcError::Network(format!("could not read the answer to {endpoint}: {e}")));
            }
            last = GcError::Status { status, body: String::from_utf8_lossy(&bytes[..bytes.len().min(512)]).into_owned() };
            if status == 429 {
                tokio::time::sleep(Duration::from_secs(retry_after.unwrap_or(2).min(30))).await;
                continue;
            }
            if (500..600).contains(&status) && idempotent {
                tokio::time::sleep(Duration::from_millis(250 << attempt)).await;
                continue;
            }
            return Err(last);
        }
        Err(last)
    }

    pub async fn self_user_status(&self) -> Result<pb::GetSelfUserStatusResponse, GcError> {
        self.call("get_self_user_status", &pb::GetSelfUserStatusRequest { request_header: Some(request_header()) }, true).await
    }

    /// Profiles for a list of people.
    pub async fn members(&self, gaia_ids: &[String]) -> Result<pb::GetMembersResponse, GcError> {
        let request = pb::GetMembersRequest { request_header: Some(request_header()), member_ids: gaia_ids.iter().map(|id| super::member_id(id)).collect(), ..Default::default() };
        self.call("get_members", &request, true).await
    }

    /// Every conversation the account is in. Without a section request carrying a page size the server answers
    /// with an empty list.
    pub async fn paginated_world(&self) -> Result<pb::PaginatedWorldResponse, GcError> {
        let request = pb::PaginatedWorldRequest {
            request_header: Some(request_header()),
            fetch_from_user_spaces: Some(true),
            fetch_options: vec![pb::paginated_world_request::FetchOptions::ExcludeGroupLite as i32],
            world_section_requests: vec![pb::WorldSectionRequest { page_size: Some(999), ..Default::default() }],
            ..Default::default()
        };
        self.call("paginated_world", &request, true).await
    }

    /// The newest topics of a conversation, oldest last, with their replies.
    pub async fn list_topics(&self, group: &super::Group, topics: i32) -> Result<pb::ListTopicsResponse, GcError> {
        let request = pb::ListTopicsRequest {
            request_header: Some(request_header()),
            group_id: Some(group.to_pb()),
            page_size_for_topics: Some(topics),
            page_size_for_replies: Some(30),
            ..Default::default()
        };
        self.call("list_topics", &request, true).await
    }

    /// Starts a topic: a message of its own, which in a direct message or a flat space is every message.
    pub async fn create_topic(&self, request: pb::CreateTopicRequest) -> Result<pb::CreateTopicResponse, GcError> {
        let request = pb::CreateTopicRequest { request_header: Some(request_header()), ..request };
        self.call("create_topic", &request, false).await
    }

    /// A reply inside a topic (a thread).
    pub async fn create_message(&self, request: pb::CreateMessageRequest) -> Result<pb::CreateMessageResponse, GcError> {
        let request = pb::CreateMessageRequest { request_header: Some(request_header()), ..request };
        self.call("create_message", &request, false).await
    }

    /// Marks a conversation read up to a time, in microseconds.
    pub async fn mark_read(&self, group: &super::Group, at_micros: i64) -> Result<pb::MarkGroupReadstateResponse, GcError> {
        let request = pb::MarkGroupReadstateRequest { request_header: Some(request_header()), id: Some(group.to_pb()), last_read_time: Some(at_micros) };
        self.call("mark_group_readstate", &request, true).await
    }
}

/// The answer as protobuf, or as base64 of it.
fn decode_response<M: prost::Message + Default>(bytes: &[u8]) -> anyhow::Result<M> {
    if let Ok(message) = M::decode(bytes) {
        return Ok(message);
    }
    let text = std::str::from_utf8(bytes)?.trim();
    let decoded = base64::engine::general_purpose::STANDARD.decode(text)?;
    Ok(M::decode(decoded.as_slice())?)
}
