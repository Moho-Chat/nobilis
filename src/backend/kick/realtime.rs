//! Kick's realtime service, which carries its chat.
//!
//! Kick ran its chat on Pusher and turned that application off: its key is
//! refused on every cluster. What kick.com's own pages use now is Centrifugo,
//! behind two small HTTP calls on `web.kick.com`:
//!
//! 1. Ask where the service is. The answer is a websocket address, and it is
//!    given to anybody - the client names itself with an id of its own making.
//! 2. Ask for a token for that id, which the connection is opened with. A guest
//!    gets one without signing in; it lasts forty-five minutes and the
//!    connection has to be given a fresh one before then or it is closed.
//!
//! Then it is subscribe and listen. A public channel is subscribed with no
//! token at all; what arrives is a *publication*, whose payload is the same
//! `{event, data}` pair Pusher carried, with `data` still a JSON string.
//!
//! Frames are JSON objects, one to a line, and several may share one websocket
//! message. The keepalive is an empty object, sent by the server and answered
//! in kind.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

const WEB_ROOT: &str = "https://web.kick.com";
const PLATFORM: &str = "web";

/// A name for this client, in the shape of a version 4 UUID, which is what
/// kick.com's own pages make one of.
pub fn client_id() -> String {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    let mut b: [u8; 16] = rng.gen();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

/// Where the service is. Asked, not written down: the address names a region,
/// and a client that pinned one would break the day Kick moved it.
pub async fn address(http: &reqwest::Client, client: &str) -> Result<String> {
    let body = json!({
        "client": { "id": client, "type": PLATFORM },
        "capabilities": { "accepted_providers": [ { "provider": "centrifugo" } ] }
    });
    let answer = post(http, "/api/v1/realtime/connection", None, body).await.context("asking Kick where its realtime service is")?;
    answer["data"]["connections"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["provider"].as_str() == Some("centrifugo"))
        .and_then(|c| c["credentials"]["url"].as_str())
        .map(str::to_string)
        .context("Kick named no realtime service")
}

/// The token a connection is opened (or renewed) with. `bearer` is the signed-in
/// account's own token where the client is one; a guest needs none.
pub async fn token(http: &reqwest::Client, client: &str, bearer: Option<&str>) -> Result<String> {
    let answer = post(http, "/api/v1/realtime/auth/connection", bearer, json!({ "client_id": client })).await.context("asking Kick for a realtime token")?;
    answer["data"]["token"].as_str().map(str::to_string).context("Kick sent no realtime token")
}

async fn post(http: &reqwest::Client, path: &str, bearer: Option<&str>, body: Value) -> Result<Value> {
    let mut request = http.post(format!("{WEB_ROOT}{path}")).header("Accept", "application/json").header("x-app-platform", PLATFORM).json(&body);
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    let response = request.send().await.context("request failed")?;
    let status = response.status();
    if !status.is_success() {
        bail!("Kick answered {status}");
    }
    response.json().await.context("unreadable answer")
}

/// Numbers each request, which is how an answer is matched to what asked. One
/// counter for everything that writes to any connection: the numbers only have
/// to differ within a connection, and a shared one cannot repeat.
static NEXT: AtomicU64 = AtomicU64::new(1);

fn id() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub fn connect(token: &str) -> Value {
    json!({ "id": id(), "connect": { "token": token, "name": "js" } })
}

/// A public channel needs no token.
pub fn subscribe(channel: &str) -> Value {
    json!({ "id": id(), "subscribe": { "channel": channel } })
}

pub fn unsubscribe(channel: &str) -> Value {
    json!({ "id": id(), "unsubscribe": { "channel": channel } })
}

pub fn refresh(token: &str) -> Value {
    json!({ "id": id(), "refresh": { "token": token } })
}

pub fn pong() -> Value {
    json!({})
}

/// One thing the service said.
#[derive(Debug, PartialEq)]
pub enum Said {
    /// The keepalive, to be answered.
    Ping,
    /// The connection was accepted. `ttl` is how many seconds the token it was
    /// opened with is good for, if it expires.
    Connected { ttl: Option<u64> },
    /// Something was published to a channel: the event it names, and its data
    /// read out of the string it arrives in.
    Published { channel: String, event: String, data: Value },
    /// A request was refused.
    Refused(String),
    /// The service is ending the connection.
    Disconnect(String),
    /// Anything else - an answer to a subscription, mostly - which says nothing.
    Other,
}

/// Reads one websocket message, which may hold several frames.
pub fn read(text: &str) -> Vec<Said> {
    text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str::<Value>(l).ok()).map(|v| read_frame(&v)).collect()
}

fn read_frame(v: &Value) -> Said {
    if v.as_object().is_some_and(|o| o.is_empty()) {
        return Said::Ping;
    }
    if let Some(error) = v.get("error") {
        let message = error["message"].as_str().unwrap_or("refused");
        return Said::Refused(match error["code"].as_u64() {
            Some(code) => format!("{message} ({code})"),
            None => message.to_string(),
        });
    }
    if let Some(connect) = v.get("connect") {
        let ttl = connect.get("expires").and_then(|e| e.as_bool()).filter(|e| *e).and_then(|_| connect["ttl"].as_u64());
        return Said::Connected { ttl };
    }
    if let Some(d) = v.get("disconnect") {
        let reason = d["reason"].as_str().unwrap_or("no reason given");
        return Said::Disconnect(match d["code"].as_u64() {
            Some(code) => format!("{reason} ({code})"),
            None => reason.to_string(),
        });
    }
    if let Some(push) = v.get("push") {
        let channel = push["channel"].as_str().unwrap_or_default();
        if let Some(inner) = push.get("pub").and_then(|p| p.get("data")) {
            if let Some(event) = inner["event"].as_str() {
                // The payload is a JSON string inside the JSON.
                let data = match &inner["data"] {
                    Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                    other => other.clone(),
                };
                return Said::Published { channel: channel.to_string(), event: event.to_string(), data };
            }
        }
        if let Some(d) = push.get("disconnect") {
            return Said::Disconnect(d["reason"].as_str().unwrap_or("unsubscribed").to_string());
        }
    }
    Said::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_is_named_like_a_uuid() {
        let a = client_id();
        assert_eq!(a.len(), 36);
        assert_eq!(a.as_bytes()[14], b'4', "version 4");
        assert_ne!(a, client_id());
    }

    /// Frames as the service sends them, from a live channel.
    #[test]
    fn what_a_chat_channel_sends_is_read() {
        let hello = r#"{"id":1,"connect":{"client":"c","version":"6.9.7 PRO","expires":true,"ttl":2700,"ping":25,"pong":true}}"#;
        assert_eq!(read(hello), vec![Said::Connected { ttl: Some(2700) }]);
        assert_eq!(read(r#"{"id":1,"connect":{"client":"c","ping":25}}"#), vec![Said::Connected { ttl: None }]);
        assert_eq!(read("{}"), vec![Said::Ping]);
        assert_eq!(read(r#"{"id":2,"subscribe":{}}"#), vec![Said::Other]);

        let publication = r#"{"push":{"channel":"chatrooms.668.v2","pub":{"data":{"event":"App\\Events\\ChatMessageEvent","data":"{\"id\":\"a\",\"chatroom_id\":668,\"content\":\"hi\"}"}}}}"#;
        let said = read(publication);
        assert_eq!(said.len(), 1);
        match &said[0] {
            Said::Published { channel, event, data } => {
                assert_eq!((channel.as_str(), event.as_str()), ("chatrooms.668.v2", "App\\Events\\ChatMessageEvent"));
                assert_eq!((data["chatroom_id"].as_u64(), data["content"].as_str()), (Some(668), Some("hi")));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn several_frames_in_one_message_are_all_read() {
        let both = "{}\n{\"id\":3,\"subscribe\":{}}\n\n{\"id\":4,\"error\":{\"code\":103,\"message\":\"permission denied\"}}";
        assert_eq!(read(both), vec![Said::Ping, Said::Other, Said::Refused("permission denied (103)".into())]);
    }

    #[test]
    fn an_ending_is_read_with_its_reason() {
        assert_eq!(read(r#"{"disconnect":{"code":3005,"reason":"expired"}}"#), vec![Said::Disconnect("expired (3005)".into())]);
    }

    #[test]
    fn the_requests_are_what_the_service_takes() {
        let c = connect("jwt");
        assert_eq!((c["connect"]["token"].as_str(), c["connect"]["name"].as_str()), (Some("jwt"), Some("js")));
        assert_eq!(subscribe("chatrooms.1.v2")["subscribe"]["channel"], json!("chatrooms.1.v2"));
        assert_eq!(unsubscribe("x")["unsubscribe"]["channel"], json!("x"));
        assert_eq!(refresh("t")["refresh"]["token"], json!("t"));
        assert_ne!(connect("a")["id"], connect("a")["id"], "each request has a number of its own");
        assert_eq!(pong(), json!({}));
    }
}
