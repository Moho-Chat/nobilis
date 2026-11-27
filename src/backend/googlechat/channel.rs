//! The live events: Google's BrowserChannel ("webchannel") at `/u/0/webchannel/`, a long poll the server holds open
//! for up to an hour and writes events into as they happen.
//!
//! As the web client does it, and as mautrix-googlechat found it has to be done:
//!
//! 1. `GET register?ignore_compass_cookie=1` sets a `COMPASS` cookie of the channel's own (`dynamite-ui=...`), which
//!    the rest of the channel's requests carry.
//! 2. The first `GET events` (`SID=null`) answers with the session id in its `X-HTTP-Initial-Response` header and
//!    starts streaming.
//! 3. An acknowledging `GET` with that id - required, for reasons nobody has found - and then a `PingEvent` saying the
//!    client is active and in the foreground, POSTed on the forward channel. Without the ping nothing is streamed.
//! 4. Chunks arrive framed by their length in UTF-16 units ([`super::chunk`]); each holds `[array id, data]` pairs, and
//!    the last array id is acknowledged (`AID`) on the next poll. `["noop"]` is a keep-alive, sent every 15-30 s - so
//!    a minute of silence means the connection is gone even if the socket does not say so.
//! 5. When the server ends a poll cleanly, the next one is opened with the same session id.
//!
//! A session id is given up after an hour and a half (a long-lived one runs into a server bug), and when the server
//! says it does not know it ("Unknown SID"): the caller registers again.

use super::pb;
use super::session::{GcError, Session, BASE};
use futures::StreamExt;
use rand::Rng;
use reqwest::header::{HeaderValue, CONTENT_TYPE, REFERER};
use serde_json::Value;
use std::time::{Duration, Instant};

/// How long a session id is used before registering again.
const LIFETIME: Duration = Duration::from_secs(90 * 60);

/// Silence longer than this is a dead connection: the server sends a keep-alive every 15-30 s.
const IDLE: Duration = Duration::from_secs(60);

/// Polls that fail in a row before the session id is given up.
const RETRIES: u32 = 3;

/// How a channel ended.
#[derive(Debug)]
pub enum End {
    /// It lived its full time; register again at once.
    Expired,
    /// The server no longer knows the session id; register again.
    UnknownSid,
    /// Signed out: nothing more is to be done until the person signs in again.
    SignedOut,
    /// Anything else: wait, then register again.
    Failed(String),
}

pub struct Channel<'a> {
    session: &'a Session,
    sid: Option<String>,
    aid: i64,
    ofs: u64,
    rid: u64,
}

fn channel_url(path: &str) -> String {
    format!("{BASE}/webchannel/{path}")
}

/// The `zx` cache-buster: a random base-36 word.
fn zx() -> String {
    const DIGITS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut n: u64 = rand::thread_rng().gen();
    let mut out = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    if out.is_empty() {
        out.push(b'a');
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_else(|_| "a".into())
}

/// The session id in `X-HTTP-Initial-Response`: `[[0,["c","<sid>","",8,12]]]`.
pub fn parse_sid(header: &str) -> Option<String> {
    let value: Value = serde_json::from_str(header).ok()?;
    value.get(0)?.get(1)?.get(1)?.as_str().map(str::to_string)
}

impl<'a> Channel<'a> {
    pub fn new(session: &'a Session) -> Self {
        Channel { session, sid: None, aid: 0, ofs: 0, rid: rand::thread_rng().gen_range(10_000..100_000) }
    }

    /// Registers and polls until the channel ends. Each event is handed to `on_event` as it comes; `on_live` is told
    /// once the first chunk has arrived, which is when the channel is known to work.
    pub async fn run(mut self, on_event: &mut (dyn FnMut(pb::Event) + Send), on_live: &mut (dyn FnMut() + Send)) -> End {
        if let Err(e) = self.register().await {
            return failed(e);
        }
        let started = Instant::now();
        let mut failures = 0u32;
        let mut told_live = false;
        loop {
            if started.elapsed() > LIFETIME {
                return End::Expired;
            }
            match self.poll(on_event, &mut || {
                if !told_live {
                    told_live = true;
                    on_live();
                }
            })
            .await
            {
                Ok(()) => failures = 0,
                Err(PollError::End(end)) => return end,
                Err(PollError::Retry(why)) => {
                    failures += 1;
                    if failures > RETRIES {
                        return End::Failed(why);
                    }
                    tokio::time::sleep(Duration::from_secs(2 << (failures - 1))).await;
                }
            }
        }
    }

    async fn register(&mut self) -> Result<(), GcError> {
        self.sid = None;
        self.aid = 0;
        self.ofs = 0;
        let url = channel_url("register?ignore_compass_cookie=1");
        let mut headers = self.session.headers(&url);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/x-protobuf"));
        let response = self.session.client().get(&url).headers(headers).send().await.map_err(|e| GcError::Network(crate::backend::mattermost::api::network_words(&e)))?;
        self.session.absorb(&response);
        let status = response.status().as_u16();
        if status != 200 {
            let body = response.text().await.unwrap_or_default();
            return Err(GcError::Status { status, body });
        }
        Ok(())
    }

    async fn poll(&mut self, on_event: &mut (dyn FnMut(pb::Event) + Send), on_chunk: &mut (dyn FnMut() + Send)) -> Result<(), PollError> {
        let mut params: Vec<(&str, String)> = vec![("VER", "8".into()), ("t", "1".into()), ("zx", zx())];
        match &self.sid {
            None => {
                params.push(("RID", self.rid.to_string()));
                params.push(("CVER", "22".into()));
                params.push(("$req", "count=1&ofs=0&req0_data=%5B%5D".into()));
                params.push(("SID", "null".into()));
                self.rid += 1;
            }
            Some(sid) => {
                params.push(("RID", "rpc".into()));
                params.push(("CI", "0".into()));
                params.push(("TYPE", "xmlhttp".into()));
                params.push(("AID", self.aid.to_string()));
                params.push(("SID", sid.clone()));
            }
        }
        let url = reqwest::Url::parse_with_params(&channel_url("events"), &params).map_err(|e| PollError::Retry(e.to_string()))?;
        let mut headers = self.session.headers(url.as_str());
        headers.insert(REFERER, HeaderValue::from_static("https://chat.google.com/"));
        let response = match tokio::time::timeout(IDLE, self.session.poll_client().get(url).headers(headers).send()).await {
            Err(_) => return Err(PollError::Retry("Google Chat did not answer the event stream".into())),
            Ok(Err(e)) => return Err(PollError::Retry(crate::backend::mattermost::api::network_words(&e))),
            Ok(Ok(r)) => r,
        };
        self.session.absorb(&response);
        let status = response.status();
        if !status.is_success() {
            let reason = status.canonical_reason().unwrap_or("").to_string();
            let body = response.text().await.unwrap_or_default();
            return Err(PollError::End(match status.as_u16() {
                400 if body.contains("Unknown SID") || reason.contains("Unknown SID") => End::UnknownSid,
                401 => End::SignedOut,
                code => End::Failed(GcError::Status { status: code, body }.to_string()),
            }));
        }
        if let Some(initial) = response.headers().get("x-http-initial-response").and_then(|v| v.to_str().ok()) {
            let sid = parse_sid(initial).ok_or_else(|| PollError::Retry("the event stream gave no session id".into()))?;
            if self.sid.as_deref() != Some(sid.as_str()) {
                self.sid = Some(sid);
                self.aid = 0;
                self.ofs = 0;
                self.acknowledge().await.map_err(|e| PollError::Retry(e.to_string()))?;
                self.send_ping().await.map_err(|e| match e {
                    GcError::SignedOut | GcError::Status { status: 401, .. } => PollError::End(End::SignedOut),
                    other => PollError::Retry(other.to_string()),
                })?;
            }
        }

        let mut parser = super::chunk::ChunkParser::default();
        let mut body = response.bytes_stream();
        loop {
            let next = match tokio::time::timeout(IDLE, body.next()).await {
                Err(_) => return Err(PollError::Retry("the event stream went quiet".into())),
                Ok(None) => return Ok(()),
                Ok(Some(Err(e))) => {
                    // The body cut off: the server is done with this session id.
                    tracing::debug!("googlechat: event stream ended: {e}");
                    return Err(PollError::End(End::UnknownSid));
                }
                Ok(Some(Ok(bytes))) => bytes,
            };
            for chunk in parser.feed(&next) {
                on_chunk();
                let Ok(Value::Array(arrays)) = serde_json::from_str::<Value>(&chunk) else {
                    tracing::debug!("googlechat: an event chunk was not an array");
                    continue;
                };
                for pair in arrays {
                    let (Some(id), Some(data)) = (pair.get(0).and_then(Value::as_i64), pair.get(1)) else { continue };
                    for event in read_array(data) {
                        on_event(event);
                    }
                    self.aid = id;
                }
            }
        }
    }

    async fn acknowledge(&self) -> Result<(), GcError> {
        let sid = self.sid.clone().unwrap_or_default();
        let params = [("VER", "8".to_string()), ("RID", "rpc".into()), ("SID", sid), ("AID", self.aid.to_string()), ("CI", "0".into()), ("TYPE", "xmlhttp".into()), ("zx", zx()), ("t", "1".into())];
        let url = reqwest::Url::parse_with_params(&channel_url("events"), &params).map_err(|e| GcError::Network(e.to_string()))?;
        // Read to its end as the web client does, but not waited on for long: only that it was taken matters.
        let headers = self.session.headers(url.as_str());
        let request = self.session.poll_client().get(url).headers(headers).send();
        match tokio::time::timeout(Duration::from_secs(30), request).await {
            Ok(Ok(response)) => {
                self.session.absorb(&response);
                let _ = tokio::time::timeout(Duration::from_secs(10), response.bytes()).await;
                Ok(())
            }
            Ok(Err(e)) => Err(GcError::Network(crate::backend::mattermost::api::network_words(&e))),
            Err(_) => Err(GcError::Network("the event stream's acknowledgement got no answer".into())),
        }
    }

    /// Says the client is here and looking, without which the server streams nothing.
    async fn send_ping(&mut self) -> Result<(), GcError> {
        let request = pb::StreamEventsRequest {
            ping_event: Some(pb::PingEvent {
                state: Some(pb::ping_event::State::Active as i32),
                application_focus_state: Some(pb::ping_event::ApplicationFocusState::FocusStateForeground as i32),
                client_interactive_state: Some(pb::ping_event::ClientInteractiveState::Interactive as i32),
                client_notifications_enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        self.forward(&request).await
    }

    /// Sends a request on the forward channel.
    async fn forward(&mut self, request: &pb::StreamEventsRequest) -> Result<(), GcError> {
        let Some(sid) = self.sid.clone() else {
            return Err(GcError::Network("the event stream is not open".into()));
        };
        let data = super::pblite::encode(request, "StreamEventsRequest").map_err(|e| GcError::Network(e.to_string()))?;
        let params = [("VER", "8".to_string()), ("RID", self.rid.to_string()), ("t", "1".into()), ("SID", sid), ("AID", self.aid.to_string())];
        self.rid += 1;
        let form = [("count", "1".to_string()), ("ofs", self.ofs.to_string()), ("req0_data", data.to_string())];
        self.ofs += 1;
        let url = reqwest::Url::parse_with_params(&channel_url("events"), &params).map_err(|e| GcError::Network(e.to_string()))?;
        let headers = self.session.headers(url.as_str());
        let response = self.session.client().post(url).headers(headers).form(&form).send().await.map_err(|e| GcError::Network(crate::backend::mattermost::api::network_words(&e)))?;
        self.session.absorb(&response);
        let status = response.status().as_u16();
        if status != 200 {
            let body = response.text().await.unwrap_or_default();
            return Err(GcError::Status { status, body });
        }
        Ok(())
    }
}

enum PollError {
    End(End),
    Retry(String),
}

fn failed(e: GcError) -> End {
    if e.is_signed_out() {
        End::SignedOut
    } else {
        End::Failed(e.to_string())
    }
}

/// One data array: `["noop"]`, or a `StreamEventsResponse` in pblite, whose event may carry several bodies. Each body
/// becomes an event of its own, with the type the body names.
pub fn read_array(data: &Value) -> Vec<pb::Event> {
    let Some(first) = data.get(0) else { return Vec::new() };
    if first.is_string() {
        return Vec::new();
    }
    let response: pb::StreamEventsResponse = match super::pblite::decode(first, "StreamEventsResponse") {
        Ok(r) => r,
        Err(e) => {
            tracing::debug!("googlechat: skipping an event that could not be read: {e}");
            return Vec::new();
        }
    };
    let Some(mut event) = response.event else { return Vec::new() };
    split_bodies(&mut event)
}

pub fn split_bodies(event: &mut pb::Event) -> Vec<pb::Event> {
    let extra = std::mem::take(&mut event.bodies);
    let mut out = Vec::new();
    if event.body.is_some() {
        out.push(event.clone());
    }
    for body in extra {
        let mut one = event.clone();
        one.r#type = body.event_type;
        one.body = Some(body);
        out.push(one);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_the_session_id() {
        assert_eq!(parse_sid(r#"[[0,["c","ABC123","",8,12]]]"#).as_deref(), Some("ABC123"));
        assert_eq!(parse_sid("[]"), None);
    }

    #[test]
    fn noop_is_nothing_and_extra_bodies_are_events_of_their_own() {
        assert!(read_array(&json!(["noop"])).is_empty());
        // An event with one body (typing, 26) and a second in `bodies` (message posted, 6).
        let typing_body = json!([null, null, null, null, null, null, null, null, null, null, null, 29]);
        let posted_body = json!([null, null, null, null, null, [[["", "M"]]], null, null, null, null, null, 6]);
        let event = json!([null, null, 29, typing_body, null, null, null, [posted_body]]);
        let events = read_array(&json!([[event]]));
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].r#type(), pb::event::EventType::MessagePosted);
        assert!(events[1].bodies.is_empty());
    }

    #[test]
    fn zx_is_base36() {
        let z = zx();
        assert!(!z.is_empty() && z.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()));
    }
}
