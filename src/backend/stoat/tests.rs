//! The whole of the connection against a made-up Stoat: an HTTP side that answers the root, `/users/@me` and the
//! message routes, and an events side that authenticates a token, says `Ready` and then speaks. What is asserted is
//! what lands in a daemon: the conversations, how they are filed, what may be written in them, a message, a send.

#![allow(clippy::result_large_err)]

use super::*;
use crate::backend::kick::testkit;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as Ws;

fn id(name: &str) -> String {
    format!("01H{name:0<23}")
}

/// What the made-up Stoat was told.
#[derive(Default)]
struct Heard {
    /// Every request that was made: method, path and body.
    requests: Vec<(String, String, String)>,
    /// What the events side was sent.
    socket: Vec<String>,
    /// The address the events side was asked at.
    socket_path: String,
}

async fn fake_http(heard: Arc<Mutex<Heard>>, ws_url: String, me: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut tcp, _)) = listener.accept().await else { return };
            let (heard, ws_url, me) = (heard.clone(), ws_url.clone(), me.clone());
            tokio::spawn(async move {
                let mut raw = Vec::new();
                let mut buf = [0u8; 4096];
                // Until the headers and as much body as they promise have come.
                loop {
                    let n = tcp.read(&mut buf).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&raw).to_string();
                    if let Some(split) = text.find("\r\n\r\n") {
                        let length = text[..split].lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                        if raw.len() >= split + 4 + length {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                let head = text.lines().next().unwrap_or("").to_string();
                let mut parts = head.split(' ');
                let (method, path) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("").to_string());
                let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                heard.lock().unwrap().requests.push((method.clone(), path.clone(), body.clone()));
                let answer = match (method.as_str(), path.split('?').next().unwrap_or("")) {
                    ("GET", "/") => json!({ "stoat": "0.16.2", "revolt": "0.16.2", "ws": ws_url, "features": { "autumn": { "enabled": true, "url": "https://cdn.example" }, "january": { "enabled": true, "url": "https://proxy.example" }, "limits": { "global": { "new_user_hours": 72 }, "default": { "message_length": 2000 }, "new_user": { "message_length": 2000 } } } }),
                    ("GET", "/users/@me") => json!({ "_id": me, "username": "me", "discriminator": "0001", "display_name": "Me" }),
                    ("POST", p) if p.ends_with("/messages") => {
                        let sent: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                        json!({ "_id": id("SENT1"), "channel": id("GEN"), "author": me, "content": sent["content"], "nonce": sent["nonce"] })
                    }
                    ("PUT", _) | ("DELETE", _) | ("PATCH", _) => Value::Null,
                    _ => json!({ "type": "NotFound" }),
                };
                let status = if answer["type"] == "NotFound" { "404 Not Found" } else { "200 OK" };
                let payload = if answer.is_null() { String::new() } else { answer.to_string() };
                let response = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}", payload.len());
                let _ = tcp.write_all(response.as_bytes()).await;
                let _ = tcp.shutdown().await;
            });
        }
    });
    base
}

/// The events side: wants the token in an `Authenticate`, then says `Authenticated`, `Ready` and a message.
async fn fake_events(heard: Arc<Mutex<Heard>>, token: &'static str, ready: Value, then: Vec<Value>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let seen = heard.clone();
        let mut ws = tokio_tungstenite::accept_hdr_async(tcp, move |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
            seen.lock().unwrap().socket_path = request.uri().to_string();
            Ok(response)
        })
        .await
        .unwrap();
        while let Some(Ok(frame)) = ws.next().await {
            let Ws::Text(text) = frame else { continue };
            heard.lock().unwrap().socket.push(text.to_string());
            let v: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            if v["type"] == "Authenticate" {
                if v["token"] != token {
                    let _ = ws.send(Ws::text(json!({ "type": "Error", "error": "InvalidSession" }).to_string())).await;
                    continue;
                }
                let _ = ws.send(Ws::text(json!({ "type": "Authenticated" }).to_string())).await;
                let _ = ws.send(Ws::text(ready.to_string())).await;
                for event in &then {
                    let _ = ws.send(Ws::text(event.to_string())).await;
                }
            }
        }
    });
    url
}

use futures::{SinkExt, StreamExt};

fn ready_event(me: &str) -> Value {
    let (pal, srv, cat) = (id("PAL"), id("SRV"), id("CAT"));
    json!({
        "type": "Ready",
        "users": [
            { "_id": me, "username": "me", "display_name": "Me", "relationship": "User" },
            { "_id": pal, "username": "pal", "display_name": "Pal", "relationship": "Friend" }
        ],
        "servers": [{
            "_id": srv, "owner": pal, "name": "Chasm", "default_permissions": world::VIEW_ONLY | world::SEND_MESSAGE,
            "channels": [id("GEN"), id("RULES"), id("VOICE")],
            "categories": [{ "id": cat, "title": "Lobby", "channels": [id("GEN"), id("RULES")] }],
            "roles": {}
        }],
        "channels": [
            { "_id": id("GEN"), "channel_type": "TextChannel", "server": srv, "name": "general" },
            { "_id": id("RULES"), "channel_type": "TextChannel", "server": srv, "name": "rules", "default_permissions": { "a": 0, "d": world::SEND_MESSAGE } },
            { "_id": id("VOICE"), "channel_type": "VoiceChannel", "server": srv, "name": "hangout" },
            { "_id": id("DM"), "channel_type": "DirectMessage", "recipients": [me, pal], "active": true },
            { "_id": id("NOTES"), "channel_type": "SavedMessages", "user": me }
        ],
        "members": [{ "_id": { "server": srv, "user": me }, "roles": [] }],
        "emojis": [],
        "channel_unreads": [{ "_id": { "channel": id("GEN"), "user": me }, "last_id": id("OLD"), "mentions": [] }]
    })
}

async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    for _ in 0..200 {
        if ready() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("never saw {what}");
}

#[tokio::test]
async fn an_account_connects_makes_its_conversations_hears_a_message_and_sends_one() {
    let me = id("ME");
    let heard = Arc::new(Mutex::new(Heard::default()));
    let hello = json!({ "type": "Message", "_id": id("MSG1"), "channel": id("GEN"), "author": id("PAL"), "content": format!("hello <@{}> !!twist!!", me), "mentions": [me.clone()] });
    let ws_url = fake_events(heard.clone(), "secret-token", ready_event(&me), vec![hello]).await;
    let base = fake_http(heard.clone(), ws_url, me.clone()).await;

    let state = testkit::simulated_daemon("stoat-connect");
    let account = format!("stoat:{me}");
    let config = crate::accounts::StoatAccountConfig { instance: base, user_id: me.clone(), token: "secret-token".into(), ..Default::default() };
    state.accounts.add_stoat(config.clone()).unwrap();
    spawn(state.clone(), config);

    until("the account connected", || state.runtime.list_accounts(&state).iter().any(|a| a.id == account && a.state == "connected")).await;
    until("a message in the general channel", || state.store.get_backlog(&crate::model::buffer_id(&account, "Chasm/#general"), 0, 10).map(|l| !l.is_empty()).unwrap_or(false)).await;

    // The conversations, and where each is filed.
    let buffers = state.runtime.list_buffers();
    let named = |name: &str| buffers.iter().find(|b| b.account_id == account && b.name == name).cloned();
    let general = named("Chasm/#general").expect("the general channel");
    assert_eq!((general.kind.as_str(), general.category.as_deref(), general.remote_id.as_deref()), ("channel", Some("Lobby"), Some(id("GEN").as_str())));
    assert_eq!(general.group_id.as_deref(), Some(events::rail_id(&account, &id("SRV")).as_str()), "under its server's entry on the rail");
    assert_eq!(general.read_only, None);
    assert_eq!(named("Chasm/#rules").unwrap().read_only.as_deref(), Some(world::NO_SEND_PERMISSION), "a channel it may not write in says so");
    assert!(named("Chasm/#hangout").is_none(), "a voice channel is not a conversation");
    let dm = named("Pal").expect("the direct message");
    assert_eq!((dm.kind.as_str(), dm.category.as_deref()), ("dm", Some("Direct Messages")));
    assert!(named("Saved Notes").is_some());
    assert!(state.runtime.list_buffer_groups().iter().any(|g| g.name == "Chasm" && g.service == "stoat"));

    // The message that was heard: its mention said by name, its spoiler in the window's marks, from who sent it.
    let log = state.store.get_backlog(&general.id, 0, 10).unwrap();
    assert_eq!((log[0].from.as_str(), log[0].body.as_str()), ("Pal", "hello @Me ||twist||"));
    assert!(log[0].is_highlight, "it mentions the account");

    // Said: through the connection that is up, answered by Stoat, and kept once.
    send::send(&state, &general, "hi there", None).await.unwrap();
    let log = state.store.get_backlog(&general.id, 0, 10).unwrap();
    assert_eq!(log.iter().filter(|m| m.body == "hi there").count(), 1);
    {
        let heard = heard.lock().unwrap();
        let post = heard.requests.iter().find(|(m, p, _)| m == "POST" && p.ends_with("/messages")).expect("a message was posted");
        assert!(post.1.contains(&id("GEN")));
        let body: Value = serde_json::from_str(&post.2).unwrap();
        assert_eq!(body["content"], "hi there");
        assert!(body["nonce"].as_str().is_some_and(|n| n.starts_with("moho-")));
        // The token went in a message, and is in no address.
        assert!(heard.socket[0].contains("secret-token") && heard.socket[0].contains("Authenticate"));
        assert!(heard.requests.iter().all(|(_, p, _)| !p.contains("secret-token")));
        assert!(heard.socket_path.starts_with("/?version=1&format=json") && !heard.socket_path.contains("secret-token"), "{}", heard.socket_path);
    }
    // And refused before it is sent where it may not be.
    let rules = named("Chasm/#rules").unwrap();
    assert!(send::send(&state, &rules, "no", None).await.unwrap_err().contains("permission"));
}
