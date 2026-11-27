//! Rocket.Chat: an open-source, self-hostable team chat platform speaking REST API v1 (`/api/v1/*`)
//! and Meteor DDP over WebSocket (`wss://<host>/websocket`), with RSA-OAEP + AES-CBC/AES-GCM E2EE.
//!
//! Plan and verification checklist: issue #31.
//! Account ID format: `rocketchat:<server_host>:<user_id>`.

#![allow(dead_code)]

pub mod api;
pub mod calls;
pub mod connect;
pub mod ddp;
pub mod discovery;
pub mod e2ee;
pub mod events;
pub mod extras;
pub mod login;
pub mod media;
pub mod members;
pub mod messages;
pub mod moderation;
pub mod profile;
pub mod rooms;
pub mod rpc;
pub mod search;
pub mod send;
pub mod threads;
pub mod world;

#[cfg(test)]
#[path = "tests.rs"]
mod tests_extra;

pub use connect::spawn;

use serde_json::Value;
use std::sync::{Arc, Mutex};

/// Live handle for a connected Rocket.Chat account.
#[derive(Clone)]
pub struct Handle {
    pub account_id: String,
    pub api: Arc<api::Api>,
    pub server: Arc<discovery::ServerInfo>,
    pub world: Arc<Mutex<world::World>>,
    /// Outgoing DDP frames written by `connect.rs`.
    pub ddp: tokio::sync::mpsc::UnboundedSender<String>,
    /// Unlocked E2EE keypair & per-room symmetric session keys.
    pub e2ee: Arc<Mutex<e2ee::E2eeState>>,
}

impl Handle {
    pub fn decrypt_message_text(&self, room_id: &str, msg: &Value) -> Option<String> {
        e2ee::decrypt_incoming(self, room_id, msg)
    }

    pub async fn distribute_room_keys(&self, room_id: &str) -> anyhow::Result<usize> {
        e2ee::distribute_room_keys(self, room_id).await
    }
}

/// What moho calls itself to Rocket.Chat servers.
pub fn user_agent() -> String {
    concat!("moho/", env!("CARGO_PKG_VERSION")).to_string()
}

/// Normalize a user-entered Rocket.Chat server URL into a clean origin without trailing slash.
/// Accepts bare hosts (`chat.example.com`), `http://localhost:3000`, or `https://open.rocket.chat/`.
pub fn normalize_server_url(raw: &str) -> anyhow::Result<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        anyhow::bail!("server URL is required");
    }
    let with_scheme = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else if trimmed.starts_with("localhost")
        || trimmed.starts_with("127.0.0.1")
        || trimmed.starts_with("[::1]")
    {
        format!("http://{trimmed}")
    } else {
        format!("https://{trimmed}")
    };
    let mut parsed = url::Url::parse(&with_scheme)
        .map_err(|e| anyhow::anyhow!("invalid Rocket.Chat server URL: {e}"))?;
    if parsed.host_str().is_none() {
        anyhow::bail!("Rocket.Chat server URL has no host");
    }
    parsed.set_query(None);
    parsed.set_fragment(None);
    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

/// Extract a stable `<server_host>` token (host[:port], lowercase) for account IDs.
pub fn server_host(server_url: &str) -> anyhow::Result<String> {
    let norm = normalize_server_url(server_url)?;
    let parsed = url::Url::parse(&norm)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("server URL has no host"))?
        .to_ascii_lowercase();
    Ok(match parsed.port() {
        Some(port) => format!("{host}:{port}"),
        None => host,
    })
}

/// Build the WebSocket DDP endpoint (`wss://<host>/websocket` or `ws://<host>/websocket`).
pub fn websocket_url(server_url: &str) -> anyhow::Result<String> {
    let norm = normalize_server_url(server_url)?;
    let mut parsed = url::Url::parse(&norm)?;
    let ws_scheme = match parsed.scheme() {
        "http" => "ws",
        _ => "wss",
    };
    let _ = parsed.set_scheme(ws_scheme);
    let base_path = parsed.path().trim_end_matches('/');
    parsed.set_path(&format!("{base_path}/websocket"));
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_server_urls_and_websocket_endpoints() {
        assert_eq!(normalize_server_url("open.rocket.chat").unwrap(), "https://open.rocket.chat");
        assert_eq!(normalize_server_url("https://chat.example.com/").unwrap(), "https://chat.example.com");
        assert_eq!(normalize_server_url("localhost:3000").unwrap(), "http://localhost:3000");
        assert_eq!(server_host("https://Chat.Example.com:8443/").unwrap(), "chat.example.com:8443");
        assert_eq!(websocket_url("https://open.rocket.chat").unwrap(), "wss://open.rocket.chat/websocket");
        assert_eq!(websocket_url("http://localhost:3000").unwrap(), "ws://localhost:3000/websocket");
        assert_eq!(websocket_url("https://example.com/rocketchat/").unwrap(), "wss://example.com/rocketchat/websocket");
    }
}
