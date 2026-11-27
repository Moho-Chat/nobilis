//! Meteor DDP (Distributed Data Protocol) v1 framing for Rocket.Chat's realtime WebSocket.
//!
//! Reference:
//! - Handshake: `{"msg":"connect","version":"1","support":["1"]}` -> `{"msg":"connected","session":"..."}`
//! - Keepalive: `{"msg":"ping","id":"..."}` <-> `{"msg":"pong","id":"..."}`
//! - Auth: `{"msg":"method","method":"login","id":"...","params":[{"resume":"<authToken>"}]}`
//! - Subscriptions: `{"msg":"sub","id":"...","name":"stream-room-messages","params":["__my_messages__",false]}`
//! - Events arrive as `{"msg":"changed","collection":"<stream>","fields":{"eventName":"...","args":[...]}}`

use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn next_id(prefix: &str) -> String {
    let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{n}")
}

/// Client -> Server DDP connect handshake.
pub fn connect_frame() -> String {
    json!({
        "msg": "connect",
        "version": "1",
        "support": ["1", "pre2", "pre1"]
    })
    .to_string()
}

/// Client -> Server DDP ping.
pub fn ping_frame(id: Option<&str>) -> String {
    match id {
        Some(id) => json!({ "msg": "ping", "id": id }).to_string(),
        None => json!({ "msg": "ping" }).to_string(),
    }
}

/// Client -> Server DDP pong.
pub fn pong_frame(id: Option<&str>) -> String {
    match id {
        Some(id) => json!({ "msg": "pong", "id": id }).to_string(),
        None => json!({ "msg": "pong" }).to_string(),
    }
}

/// Client -> Server DDP method call.
pub fn method_frame(id: &str, method: &str, params: Value) -> String {
    json!({
        "msg": "method",
        "method": method,
        "id": id,
        "params": params
    })
    .to_string()
}

/// Client -> Server DDP `login` with resume auth token.
pub fn login_resume_frame(id: &str, auth_token: &str) -> String {
    method_frame(id, "login", json!([{ "resume": auth_token }]))
}

/// Client -> Server DDP subscription.
pub fn sub_frame(id: &str, name: &str, params: Value) -> String {
    json!({
        "msg": "sub",
        "id": id,
        "name": name,
        "params": params
    })
    .to_string()
}

/// Client -> Server DDP unsubscribe.
pub fn unsub_frame(id: &str) -> String {
    json!({ "msg": "unsub", "id": id }).to_string()
}

/// Client -> Server typing indicator via DDP `stream-notify-room` (`{rid}/user-activity`).
pub fn typing_frame(id: &str, room_id: &str, username: &str, is_typing: bool) -> String {
    let activities = if is_typing { json!(["user-typing"]) } else { json!([]) };
    method_frame(
        id,
        "stream-notify-room",
        json!([format!("{room_id}/user-activity"), username, activities]),
    )
}

/// Parsed Server -> Client DDP frame.
#[derive(Debug, Clone, PartialEq)]
pub enum DdpMessage {
    Connected { session: String },
    Failed { version: String },
    Ping { id: Option<String> },
    Pong { id: Option<String> },
    Result { id: String, result: Value, error: Option<DdpError> },
    Ready { subs: Vec<String> },
    NoSub { id: String, error: Option<DdpError> },
    Changed { collection: String, id: String, event_name: String, args: Vec<Value>, fields: Value },
    Added { collection: String, id: String, fields: Value },
    Removed { collection: String, id: String },
    Error { reason: String },
    Other(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DdpError {
    pub error: String,
    pub reason: String,
    pub error_type: String,
}

impl DdpError {
    pub fn from_value(v: &Value) -> Option<Self> {
        if v.is_null() {
            return None;
        }
        let error = v["error"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| v["error"].as_i64().map(|n| n.to_string()))
            .unwrap_or_default();
        let reason = v["reason"]
            .as_str()
            .or_else(|| v["message"].as_str())
            .unwrap_or("")
            .to_string();
        let error_type = v["errorType"].as_str().unwrap_or("").to_string();
        Some(Self { error, reason, error_type })
    }

    pub fn is_auth_failure(&self) -> bool {
        self.error == "403"
            || self.error == "401"
            || self.reason.to_ascii_lowercase().contains("expired")
            || self.reason.to_ascii_lowercase().contains("invalid login token")
            || self.reason.to_ascii_lowercase().contains("user not found")
    }
}

pub fn parse_frame(raw: &str) -> Option<DdpMessage> {
    let v: Value = serde_json::from_str(raw).ok()?;
    let msg = v["msg"].as_str().unwrap_or("");
    Some(match msg {
        "connected" => DdpMessage::Connected {
            session: v["session"].as_str().unwrap_or("").to_string(),
        },
        "failed" => DdpMessage::Failed {
            version: v["version"].as_str().unwrap_or("").to_string(),
        },
        "ping" => DdpMessage::Ping {
            id: v["id"].as_str().map(|s| s.to_string()),
        },
        "pong" => DdpMessage::Pong {
            id: v["id"].as_str().map(|s| s.to_string()),
        },
        "result" => DdpMessage::Result {
            id: v["id"].as_str().unwrap_or("").to_string(),
            result: v["result"].clone(),
            error: DdpError::from_value(&v["error"]),
        },
        "ready" => DdpMessage::Ready {
            subs: v["subs"]
                .as_array()
                .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                .unwrap_or_default(),
        },
        "nosub" => DdpMessage::NoSub {
            id: v["id"].as_str().unwrap_or("").to_string(),
            error: DdpError::from_value(&v["error"]),
        },
        "changed" => {
            let fields = v["fields"].clone();
            let event_name = fields["eventName"].as_str().unwrap_or("").to_string();
            let args = fields["args"].as_array().cloned().unwrap_or_default();
            DdpMessage::Changed {
                collection: v["collection"].as_str().unwrap_or("").to_string(),
                id: v["id"].as_str().unwrap_or("").to_string(),
                event_name,
                args,
                fields,
            }
        }
        "added" => DdpMessage::Added {
            collection: v["collection"].as_str().unwrap_or("").to_string(),
            id: v["id"].as_str().unwrap_or("").to_string(),
            fields: v["fields"].clone(),
        },
        "removed" => DdpMessage::Removed {
            collection: v["collection"].as_str().unwrap_or("").to_string(),
            id: v["id"].as_str().unwrap_or("").to_string(),
        },
        "error" => DdpMessage::Error {
            reason: v["reason"].as_str().unwrap_or("DDP error").to_string(),
        },
        _ => DdpMessage::Other(v),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_and_parses_ddp_lifecycle() {
        let c: Value = serde_json::from_str(&connect_frame()).unwrap();
        assert_eq!(c["msg"], "connect");
        assert_eq!(c["version"], "1");

        let login: Value = serde_json::from_str(&login_resume_frame("login-1", "tok123")).unwrap();
        assert_eq!(login["msg"], "method");
        assert_eq!(login["method"], "login");
        assert_eq!(login["params"][0]["resume"], "tok123");

        assert_eq!(
            parse_frame(r#"{"msg":"connected","session":"abc"}"#),
            Some(DdpMessage::Connected { session: "abc".into() })
        );
        assert_eq!(
            parse_frame(r#"{"msg":"ping","id":"p1"}"#),
            Some(DdpMessage::Ping { id: Some("p1".into()) })
        );
        let ev = parse_frame(
            r#"{"msg":"changed","collection":"stream-room-messages","id":"id","fields":{"eventName":"__my_messages__","args":[{"_id":"m1","rid":"r1","msg":"hi"}]}}"#,
        )
        .unwrap();
        match ev {
            DdpMessage::Changed { collection, event_name, args, .. } => {
                assert_eq!(collection, "stream-room-messages");
                assert_eq!(event_name, "__my_messages__");
                assert_eq!(args[0]["msg"], "hi");
            }
            _ => panic!("expected Changed"),
        }
    }
}
