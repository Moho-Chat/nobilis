//! Rocket.Chat DDP realtime event dispatch and server rail / buffer mapping.

use super::ddp::DdpMessage;
use super::messages;
use super::world::{Room, RoomKind};
use super::Handle;
use crate::model::BufferGroup;
use crate::state::AppState;
use serde_json::json;

const TYPING_FOR_MS: u64 = 8_000;

/// Rail group ID for a Rocket.Chat server.
pub fn server_rail_id(account_id: &str) -> String {
    format!("{account_id}|server")
}

/// Register the Rocket.Chat server on moho's server rail.
pub fn register_server_rail(state: &AppState, handle: &Handle) {
    state.runtime.upsert_buffer_group(
        state,
        BufferGroup {
            id: server_rail_id(&handle.account_id),
            account_id: handle.account_id.clone(),
            service: "rocketchat".to_string(),
            kind: "guild".to_string(),
            name: handle.server.site_name.clone(),
            icon_url: None,
            position: 0,
            pending: false,
        },
    );
}

/// Register or update a single Rocket.Chat room as a moho buffer.
pub fn register_room(state: &AppState, handle: &Handle, room: &Room) {
    if !room.open && room.kind != RoomKind::Direct {
        return;
    }
    let account_id = &handle.account_id;
    let (title, is_muted) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let muted = room.muted_usernames.iter().any(|u| u == &world.own_username);
        (world.room_title(room), muted)
    };
    let kind = if room.kind == RoomKind::Direct { "dm" } else { "channel" };

    let existing = state
        .runtime
        .list_buffers()
        .into_iter()
        .find(|b| b.account_id == *account_id && b.remote_id.as_deref() == Some(room.id.as_str()));
    let fresh = existing.is_none();

    let buffer = match existing {
        Some(found) if found.name == title => state.runtime.ensure_buffer(state, account_id, &title, kind),
        Some(found) => {
            state.runtime.rename_buffer(state, account_id, &found.name, &title, kind);
            state
                .runtime
                .get_buffer(&crate::model::buffer_id(account_id, &title))
                .unwrap_or(found)
        }
        None => state.runtime.ensure_buffer(state, account_id, &title, kind),
    };

    state.runtime.set_buffer_remote_id(state, &buffer.id, &room.id);

    if room.kind == RoomKind::Direct {
        state
            .runtime
            .set_buffer_category(state, &buffer.id, Some("Direct Messages"), 0);
    } else {
        state
            .runtime
            .set_buffer_group(state, &buffer.id, &server_rail_id(account_id));
        let (category, rank) = if room.team_main {
            ("Teams", 100)
        } else if room.prid.is_some() {
            ("Discussions", 300)
        } else if room.kind == RoomKind::Livechat {
            ("Omnichannel", 500)
        } else if room.kind == RoomKind::Group {
            ("Private Groups", 200)
        } else {
            ("Channels", 150)
        };
        state
            .runtime
            .set_buffer_category(state, &buffer.id, Some(category), rank);
    }

    // Topic + announcement banner
    let topic_banner = match (
        room.announcement.as_deref().filter(|s| !s.is_empty()),
        room.topic.as_deref().filter(|s| !s.is_empty()),
        room.description.as_deref().filter(|s| !s.is_empty()),
    ) {
        (Some(ann), Some(top), _) => Some(format!("[Announcement] {ann} — {top}")),
        (Some(ann), None, _) => Some(format!("[Announcement] {ann}")),
        (None, Some(top), _) => Some(top.to_string()),
        (None, None, Some(desc)) => Some(desc.to_string()),
        _ => None,
    };
    if let Some(t) = topic_banner {
        state.runtime.set_buffer_topic(state, &buffer.id, &t);
    }

    // Read-only or archived or muted reason
    let ro_reason = if room.archived {
        Some("This room is archived.".to_string())
    } else if is_muted {
        Some("You are muted in this room.".to_string())
    } else if room.read_only && !room.roles.iter().any(|r| r == "owner" || r == "moderator") {
        Some("This room is read-only.".to_string())
    } else {
        None
    };
    state.runtime.set_buffer_read_only(state, &buffer.id, ro_reason.as_deref());

    // Encryption badge
    if room.encrypted {
        state.runtime.set_buffer_encrypted(state, &buffer.id, true);
    }

    // Icon for Teams / Discussions / Private Groups / Federated
    let icon = if room.team_main {
        Some("groups")
    } else if room.prid.is_some() {
        Some("forum")
    } else if room.federated {
        Some("public")
    } else if room.kind == RoomKind::Group {
        Some("lock")
    } else {
        None
    };
    state.runtime.set_buffer_name_icon(state, &buffer.id, icon);

    // Unread & mention badges
    if fresh && (room.unread > 0 || room.alert) {
        let mentions = room.user_mentions.saturating_add(room.group_mentions);
        state
            .runtime
            .set_buffer_unread(state, &buffer.id, room.unread.max(1), mentions);
    }
}

/// Dispatch an incoming DDP `changed` event from Rocket.Chat streams.
pub fn handle_ddp(state: &AppState, handle: &Handle, msg: DdpMessage) {
    let DdpMessage::Changed {
        collection,
        event_name,
        args,
        ..
    } = msg
    else {
        return;
    };

    match collection.as_str() {
        "stream-room-messages" => {
            for arg in &args {
                messages::record(state, handle, arg);
            }
        }
        "stream-notify-user" => {
            let suffix = event_name.split('/').nth(1).unwrap_or("");
            match suffix {
                "rooms-changed" => {
                    if let (Some(action), Some(room_val)) = (args.first().and_then(|v| v.as_str()), args.get(1)) {
                        if action == "removed" {
                            if let Some(rid) = room_val["_id"].as_str() {
                                handle.world.lock().unwrap_or_else(|e| e.into_inner()).rooms.remove(rid);
                            }
                        } else {
                            let room = {
                                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                                world.merge_room_json(room_val).and_then(|rid| world.rooms.get(&rid).cloned())
                            };
                            if let Some(room) = room {
                                register_room(state, handle, &room);
                            }
                        }
                    }
                }
                "subscriptions-changed" => {
                    if let (Some(action), Some(sub_val)) = (args.first().and_then(|v| v.as_str()), args.get(1)) {
                        if action == "removed" {
                            if let Some(rid) = sub_val["rid"].as_str() {
                                handle.world.lock().unwrap_or_else(|e| e.into_inner()).rooms.remove(rid);
                            }
                        } else {
                            let room = {
                                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                                world.merge_subscription_json(sub_val).and_then(|rid| world.rooms.get(&rid).cloned())
                            };
                            if let Some(room) = room {
                                register_room(state, handle, &room);
                                if room.unread == 0 && !room.alert {
                                    if let Some(buf) = messages::buffer_of(state, &handle.account_id, &room.id) {
                                        state.runtime.set_buffer_unread(state, &buf.id, 0, 0);
                                    }
                                }
                            }
                        }
                    }
                }
                "e2ekeyRequest" => {
                    // args: [rid, keyId] — distribute Ks to members without key in background
                    if let Some(rid) = args.first().and_then(|v| v.as_str()).map(str::to_string) {
                        let h = handle.clone();
                        tokio::spawn(async move {
                            let _ = h.distribute_room_keys(&rid).await;
                        });
                    }
                }
                "webrtc" => {
                    // Native 1:1 WebRTC signaling (SDP offer/answer, ICE candidate)
                    state.events.emit(
                        "rocketchatWebrtcSignal",
                        json!({
                            "accountId": handle.account_id,
                            "payload": args,
                        }),
                    );
                }
                "video-conference" => {
                    // Video conference ring / join / cancel events
                    state.events.emit(
                        "rocketchatVideoConference",
                        json!({
                            "accountId": handle.account_id,
                            "payload": args,
                        }),
                    );
                }
                "otr" => {
                    // Legacy OTR handshake / notice
                    state.events.emit(
                        "rocketchatOtrNotice",
                        json!({
                            "accountId": handle.account_id,
                            "payload": args,
                        }),
                    );
                }
                "message" => {
                    // Ephemeral / system message directed to the user
                    for arg in &args {
                        messages::record(state, handle, arg);
                    }
                }
                _ => {}
            }
        }
        "stream-notify-room" => {
            let mut parts = event_name.split('/');
            let rid = parts.next().unwrap_or("");
            let kind = parts.next().unwrap_or("");
            match kind {
                "typing" | "user-activity" => {
                    let username = args.first().and_then(|v| v.as_str()).unwrap_or("");
                    let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_username.clone();
                    if username.is_empty() || username == own {
                        return;
                    }
                    let is_typing = args
                        .get(1)
                        .and_then(|v| {
                            v.as_bool().or_else(|| {
                                v.as_array()
                                    .map(|a| a.iter().any(|x| x.as_str() == Some("user-typing")))
                            })
                        })
                        .unwrap_or(false);
                    if let Some(buf) = messages::buffer_of(state, &handle.account_id, rid) {
                        if is_typing {
                            state.events.emit(
                                "typing",
                                json!({ "accountId": handle.account_id, "bufferId": buf.id, "nicks": [username], "expiresInMs": TYPING_FOR_MS }),
                            );
                        } else {
                            state.events.emit(
                                "typing",
                                json!({ "accountId": handle.account_id, "bufferId": buf.id, "nicks": [], "expiresInMs": 0 }),
                            );
                        }
                    }
                }
                "deleteMessage" => {
                    if let (Some(mid), Some(buf)) = (
                        args.first().and_then(|v| v["_id"].as_str()),
                        messages::buffer_of(state, &handle.account_id, rid),
                    ) {
                        let _ = state.store.delete_message(&buf.id, mid);
                        state.events.emit(
                            "messageDeleted",
                            json!({ "bufferId": buf.id, "id": mid }),
                        );
                    }
                }
                "deleteMessageBulk" => {
                    if let Some(buf) = messages::buffer_of(state, &handle.account_id, rid) {
                        state.events.emit(
                            "rocketchatBulkDelete",
                            json!({ "bufferId": buf.id, "detail": args }),
                        );
                    }
                }
                _ => {}
            }
        }
        "stream-notify-logged" => match event_name.as_str() {
            "user-status" => {
                // args: [[uid, username, status_num, statusText, ...]]
                if let Some(arr) = args.first().and_then(|v| v.as_array()) {
                    let uid = arr.first().and_then(|v| v.as_str()).unwrap_or("");
                    let username = arr.get(1).and_then(|v| v.as_str()).unwrap_or("");
                    let status_num = arr.get(2).and_then(|v| v.as_i64()).unwrap_or(0);
                    let status_text = arr.get(3).and_then(|v| v.as_str()).unwrap_or("");
                    let status = match status_num {
                        1 => "online",
                        2 => "away",
                        3 => "busy",
                        _ => "offline",
                    };
                    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                    if !uid.is_empty() {
                        let entry = world.users.entry(uid.to_string()).or_default();
                        entry.id = uid.to_string();
                        if !username.is_empty() {
                            entry.username = username.to_string();
                        }
                        entry.status = status.to_string();
                        entry.status_text = status_text.to_string();
                    }
                }
            }
            "Users:NameChanged" => {
                if let Some(u) = args.first() {
                    if let (Some(uid), Some(name)) = (u["_id"].as_str(), u["name"].as_str()) {
                        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                        if let Some(entry) = world.users.get_mut(uid) {
                            entry.name = Some(name.to_string());
                        }
                    }
                }
            }
            "updateEmojiCustom" => {
                // Refresh custom emoji source in background
                let s = state.clone();
                let h = handle.clone();
                tokio::spawn(async move {
                    let _ = super::media::sync_custom_emoji(&s, &h).await;
                });
            }
            "roles-change" => {
                if let Some(ev) = args.first() {
                    let _ = ev; // Roles updated on next member fetch or room-change
                }
            }
            _ => {}
        },
        _ => {}
    }
}
