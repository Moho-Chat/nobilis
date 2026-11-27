//! Rocket.Chat message conversion, Markdown/KaTeX formatting, reactions, attachments,
//! threads, E2EE decryption hook, and room history pagination (`im.history`, `channels.history`, `groups.history`).

use super::world::{parse_rc_timestamp, RoomKind};
use super::Handle;
use crate::model::{Attachment, Embed, Reaction, ReplyPreview, SenderStyle};
use crate::state::AppState;
use anyhow::Result;
use regex::Regex;
use serde_json::{json, Value};
use std::sync::OnceLock;

pub const HISTORY_PAGE: usize = 50;

/// Find the moho buffer for a Rocket.Chat room ID.
pub fn buffer_of(state: &AppState, account_id: &str, room_id: &str) -> Option<crate::model::Buffer> {
    state
        .runtime
        .list_buffers()
        .into_iter()
        .find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(room_id))
}

/// Parse quoted permalink syntax `[ ](https://<host>/channel/foo?msg=<id>)` used by Rocket.Chat.
pub fn extract_quote_permalink(text: &str) -> (Option<String>, String) {
    static QUOTE_RE: OnceLock<Regex> = OnceLock::new();
    let re = QUOTE_RE.get_or_init(|| {
        Regex::new(r"^\[\s*\]\(https?://[^\s)]+[?&]msg=([A-Za-z0-9_-]+)[^\s)]*\)\s*")
            .expect("quote regex compiles")
    });
    if let Some(caps) = re.captures(text) {
        let msg_id = caps.get(1).map(|m| m.as_str().to_string());
        let stripped = re.replace(text, "").to_string();
        return (msg_id, stripped);
    }
    (None, text.to_string())
}

/// Format Rocket.Chat message body: handles KaTeX delimiters (`$$...$$` / `\[...\]`),
/// preserves standard Markdown (`*bold*`, `_italic_`, `~strike~`, `` `code` ``, quotes),
/// and renders custom server emoji tokens when known.
pub fn format_body(raw: &str, katex_enabled: bool) -> String {
    let mut out = raw.to_string();
    if katex_enabled {
        // Normalize Rocket.Chat's \[...\] display math and \(...\) inline math to readable markers
        out = out.replace("\\[", "$$").replace("\\]", "$$");
    }
    out
}

/// Extract Rocket.Chat reactions (`reactions: { ":thumbsup:": { "usernames": ["alice", "bob"] } }`).
pub fn extract_reactions(msg: &Value, own_username: &str) -> Vec<Reaction> {
    let Some(obj) = msg["reactions"].as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (emoji, val) in obj {
        let users: Vec<&str> = val["usernames"]
            .as_array()
            .map(|a| a.iter().filter_map(|u| u.as_str()).collect())
            .unwrap_or_default();
        if users.is_empty() {
            continue;
        }
        let me = users.contains(&own_username);
        out.push(Reaction {
            emoji: emoji.clone(),
            count: users.len() as i64,
            me,
            animated: false,
        });
    }
    out
}

/// Extract attachments and embeds from a Rocket.Chat message (`attachments[]` and `file`).
pub fn extract_media(handle: &Handle, msg: &Value) -> (Vec<Attachment>, Vec<Embed>) {
    let mut attachments = Vec::new();
    let mut embeds = Vec::new();
    let base = handle.api.base();

    if let Some(arr) = msg["attachments"].as_array() {
        for att in arr {
            // Image attachment
            if let Some(img_url) = att["image_url"].as_str().filter(|s| !s.is_empty()) {
                let full = absolutize(base, img_url);
                attachments.push(Attachment {
                    kind: "image".to_string(),
                    filename: att["title"].as_str().map(str::to_string),
                    url: Some(full),
                    mimetype: att["image_type"].as_str().map(str::to_string),
                    size: att["image_size"].as_u64(),
                    width: att["image_dimensions"]["width"].as_u64().map(|n| n as u32),
                    height: att["image_dimensions"]["height"].as_u64().map(|n| n as u32),
                    ..Default::default()
                });
                continue;
            }
            // Audio / voice note attachment
            if let Some(aud_url) = att["audio_url"].as_str().filter(|s| !s.is_empty()) {
                let full = absolutize(base, aud_url);
                attachments.push(Attachment {
                    kind: "audio".to_string(),
                    filename: att["title"].as_str().map(str::to_string),
                    url: Some(full),
                    mimetype: att["audio_type"].as_str().map(str::to_string),
                    size: att["audio_size"].as_u64(),
                    ..Default::default()
                });
                continue;
            }
            // Video attachment
            if let Some(vid_url) = att["video_url"].as_str().filter(|s| !s.is_empty()) {
                let full = absolutize(base, vid_url);
                attachments.push(Attachment {
                    kind: "video".to_string(),
                    filename: att["title"].as_str().map(str::to_string),
                    url: Some(full),
                    mimetype: att["video_type"].as_str().map(str::to_string),
                    size: att["video_size"].as_u64(),
                    ..Default::default()
                });
                continue;
            }
            // Generic file link attachment
            if let Some(link) = att["title_link"].as_str().filter(|s| s.starts_with("/file-upload/")) {
                let full = absolutize(base, link);
                attachments.push(Attachment {
                    kind: "file".to_string(),
                    filename: att["title"].as_str().map(str::to_string),
                    url: Some(full),
                    mimetype: att["type"].as_str().map(str::to_string),
                    ..Default::default()
                });
                continue;
            }
            // Rich embed / quote / bot card
            let title = att["title"].as_str().map(str::to_string);
            let description = att["text"]
                .as_str()
                .or_else(|| att["description"].as_str())
                .map(str::to_string);
            let author = att["author_name"].as_str().map(str::to_string);
            let url = att["title_link"]
                .as_str()
                .or_else(|| att["message_link"].as_str())
                .map(|u| absolutize(base, u));
            let color = att["color"].as_str().and_then(parse_hex_color);
            if title.is_some() || description.is_some() || author.is_some() {
                embeds.push(Embed {
                    title,
                    description,
                    color,
                    url,
                    author,
                    ..Default::default()
                });
            }
        }
    }

    // Link previews from `urls[]`
    if let Some(urls) = msg["urls"].as_array() {
        for u in urls {
            let meta = &u["meta"];
            if !meta.is_object() {
                continue;
            }
            let title = meta["ogTitle"]
                .as_str()
                .or_else(|| meta["pageTitle"].as_str())
                .map(str::to_string);
            let description = meta["ogDescription"]
                .as_str()
                .or_else(|| meta["description"].as_str())
                .map(str::to_string);
            let provider = meta["ogSiteName"].as_str().map(str::to_string);
            let url = u["url"].as_str().map(str::to_string);
            if title.is_some() || description.is_some() {
                embeds.push(Embed {
                    title,
                    description,
                    provider,
                    url,
                    ..Default::default()
                });
            }
        }
    }

    (attachments, embeds)
}

fn absolutize(base: &str, url: &str) -> String {
    if url.starts_with("http://") || url.starts_with("https://") {
        url.to_string()
    } else if url.starts_with('/') {
        format!("{base}{url}")
    } else {
        format!("{base}/{url}")
    }
}

fn parse_hex_color(s: &str) -> Option<i64> {
    let hex = s.trim().trim_start_matches('#');
    if hex.len() == 6 {
        i64::from_str_radix(hex, 16).ok()
    } else {
        None
    }
}

/// System message text for Rocket.Chat's `t` field (`uj`, `ul`, `ru`, `au`, `rm`, `message_pinned`, `room_changed_topic`, etc.).
pub fn system_message_text(t: &str, who: &str, msg_text: &str) -> Option<String> {
    Some(match t {
        "uj" => format!("{who} joined the channel"),
        "ul" => format!("{who} left the channel"),
        "ut" => format!("{who} joined the conversation"),
        "au" => format!("{who} added {msg_text}"),
        "ru" => format!("{who} removed {msg_text}"),
        "r" => format!("{who} renamed the room to {msg_text}"),
        "room_changed_topic" => format!("{who} changed the topic to: {msg_text}"),
        "room_changed_announcement" => format!("{who} changed the announcement to: {msg_text}"),
        "room_changed_description" => format!("{who} changed the description to: {msg_text}"),
        "room_changed_privacy" => format!("{who} changed room privacy to {msg_text}"),
        "room_e2e_enabled" => format!("{who} enabled end-to-end encryption for this room"),
        "room_e2e_disabled" => format!("{who} disabled end-to-end encryption for this room"),
        "message_pinned" => format!("{who} pinned a message"),
        "rm" => format!("{who} removed a message"),
        "user-muted" => format!("{who} muted {msg_text}"),
        "user-unmuted" => format!("{who} unmuted {msg_text}"),
        "subscription-role-added" => format!("{who} assigned a role to {msg_text}"),
        "subscription-role-removed" => format!("{who} removed a role from {msg_text}"),
        "discussion-created" => format!("{who} started a discussion: {msg_text}"),
        "otr" => format!("OTR session notice from {who}: {msg_text} (moho uses Rocket.Chat E2EE; legacy OTR is view-only)"),
        "videoconf" => format!("{who} started a video conference"),
        "livechat-close" => format!("Livechat conversation closed by {who}"),
        _ => return None,
    })
}

/// Record a Rocket.Chat message JSON into moho's runtime and SQLite scrollback.
pub fn record(state: &AppState, handle: &Handle, msg: &Value) -> bool {
    let (Some(id), Some(rid)) = (msg["_id"].as_str(), msg["rid"].as_str()) else {
        return false;
    };
    let account_id = &handle.account_id;
    let Some(buffer) = buffer_of(state, account_id, rid) else {
        return false;
    };

    let ts = parse_rc_timestamp(&msg["ts"]).unwrap_or_else(|| chrono::Utc::now().timestamp());
    let author_id = msg["u"]["_id"].as_str().unwrap_or("").to_string();
    let author_username = msg["u"]["username"].as_str().unwrap_or("");
    let alias = msg["alias"].as_str().filter(|s| !s.is_empty());

    let (own_id, own_username, from_display, is_ignored, is_federated) = {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if !author_id.is_empty() && !author_username.is_empty() {
            let entry = world.users.entry(author_id.clone()).or_default();
            entry.id = author_id.clone();
            entry.username = author_username.to_string();
            if let Some(n) = msg["u"]["name"].as_str().filter(|s| !s.is_empty()) {
                entry.name = Some(n.to_string());
            }
            entry.federated = super::world::World::is_federated_username(author_username);
            world
                .users_by_name
                .insert(author_username.to_ascii_lowercase(), author_id.clone());
        }
        let from = alias
            .map(str::to_string)
            .unwrap_or_else(|| world.display_name_for_user(&author_id, author_username));
        let ignored = world.ignored_users.contains(&author_id);
        let fed = super::world::World::is_federated_username(author_username);
        (world.own_id.clone(), world.own_username.clone(), from, ignored, fed)
    };

    if is_ignored && author_id != own_id {
        return false;
    }

    let msg_type = msg["t"].as_str().unwrap_or("");
    let raw_text = msg["msg"].as_str().unwrap_or("");

    // Handle system messages
    if !msg_type.is_empty() && msg_type != "e2e" {
        if let Some(sys_text) = system_message_text(msg_type, &from_display, raw_text) {
            return state.runtime.record_message_at(
                state,
                account_id,
                &buffer.name,
                &buffer.kind,
                "Rocket.Chat",
                &sys_text,
                false,
                "system",
                None,
                Some(id.to_string()),
                false,
                None,
                Vec::new(),
                Vec::new(),
                None,
                Some(ts),
                None,
                None,
            );
        }
    }

    // Handle E2EE messages (`t == "e2e"`) — decrypted via `e2ee::decrypt_message` once Round 5 is wired
    let plaintext = if msg_type == "e2e" {
        handle.decrypt_message_text(rid, msg).unwrap_or_else(|| {
            "[Encrypted Rocket.Chat message — unlock E2EE passphrase in Accounts → Rocket.Chat]".to_string()
        })
    } else {
        raw_text.to_string()
    };

    let (quoted_id, body_clean) = extract_quote_permalink(&plaintext);
    let body = format_body(&body_clean, handle.server.katex_enabled);

    // Thread reply (`tmid`) or quoted reply
    let reply = if let Some(tmid) = msg["tmid"].as_str().filter(|s| !s.is_empty()) {
        Some(ReplyPreview {
            id: tmid.to_string(),
            from: String::new(),
            body: String::new(),
            thread: true,
            forwarded: false,
            thread_root: Some(tmid.to_string()),
            chat: None,
        })
    } else {
        quoted_id.map(|qid| ReplyPreview {
            id: qid,
            from: String::new(),
            body: String::new(),
            thread: false,
            forwarded: false,
            thread_root: None,
            chat: None,
        })
    };

    let highlight = author_id != own_id
        && msg["mentions"].as_array().is_some_and(|arr| {
            arr.iter().any(|m| {
                let u = m["username"].as_str().unwrap_or("");
                u == own_username || u == "all" || u == "here" || m["_id"].as_str() == Some(own_id.as_str())
            })
        });

    let avatar = msg["avatar"]
        .as_str()
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .map(str::to_string)
        .or_else(|| {
            if !author_username.is_empty() {
                Some(format!("{}/avatar/{}", handle.api.base(), author_username))
            } else {
                None
            }
        });

    let (attachments, embeds) = extract_media(handle, msg);
    let reactions = extract_reactions(msg, &own_username);

    let mut badges = Vec::new();
    if msg["bot"].is_object() || alias.is_some() {
        badges.push(crate::backend::kick::api::Badge {
            kind: "app".into(),
            text: "Bot".into(),
            ..Default::default()
        });
    }
    if is_federated {
        badges.push(crate::backend::kick::api::Badge {
            kind: "federated".into(),
            text: "Matrix".into(),
            ..Default::default()
        });
    }
    let style = (!badges.is_empty()).then_some(SenderStyle { color: None, badges });

    let recorded = state.runtime.record_message_at(
        state,
        account_id,
        &buffer.name,
        &buffer.kind,
        &from_display,
        &body,
        false,
        "chat",
        reply,
        Some(id.to_string()),
        highlight,
        avatar,
        embeds,
        attachments,
        Some(author_id),
        Some(ts),
        None,
        style,
    );

    if recorded && !reactions.is_empty() && state.store.set_reactions(&buffer.id, id, &reactions).unwrap_or(false) {
        state.events.emit(
            "reactionsChanged",
            json!({ "bufferId": buffer.id, "id": id, "reactions": reactions }),
        );
    }

    recorded
}

/// Fetch a page of history for a room (`im.history`, `channels.history`, or `groups.history`),
/// persisting messages oldest-first into SQLite scrollback.
pub async fn history(
    state: &AppState,
    handle: &Handle,
    buffer: &crate::model::Buffer,
    before_ts: Option<i64>,
) -> Result<usize> {
    let Some(rid) = buffer.remote_id.as_deref() else {
        return Ok(0);
    };
    let kind = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world
            .rooms
            .get(rid)
            .map(|r| r.kind)
            .unwrap_or(if buffer.kind == "dm" { RoomKind::Direct } else { RoomKind::Channel })
    };
    let path = format!("/api/v1/{}.history", kind.endpoint_prefix());
    let mut query = vec![
        ("roomId", rid.to_string()),
        ("count", HISTORY_PAGE.to_string()),
        ("inclusive", "false".to_string()),
    ];
    if let Some(ts) = before_ts.filter(|t| *t > 0) {
        if let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) {
            query.push(("latest", dt.to_rfc3339()));
        }
    }

    let resp = handle.api.get(&path, &query).await?;
    let msgs = resp["messages"].as_array().cloned().unwrap_or_default();
    let count = msgs.len();
    // Rocket.Chat returns newest-first; record oldest-first so scrollback order is chronological.
    for m in msgs.iter().rev() {
        record(state, handle, m);
    }
    Ok(count)
}
