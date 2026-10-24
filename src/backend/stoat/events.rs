//! What the events websocket says: the first big answer (`Ready`), which makes the rail and the conversations, and
//! everything after - messages, edits, deletions, reactions, channels and servers changing, somebody typing, a
//! conversation read on another device.

use super::media;
use super::messages;
use super::world::{self, Channel, Kind, World};
use super::Handle;
use crate::model::BufferGroup;
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::HashSet;

/// How long somebody's "typing" lasts without another: Stoat sends one when it starts and one when it stops, and a
/// client that never hears the stop must not show it for ever.
const TYPING_FOR_MS: u64 = 8000;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The rail entry for a server.
pub fn rail_id(account_id: &str, server_id: &str) -> String {
    format!("{account_id}|server:{server_id}")
}

/// A server on the rail, with its name and picture.
fn register_server(state: &AppState, handle: &Handle, server: &world::Server, position: i64) {
    let icon_url = server.icon.as_deref().and_then(|f| media::file_url(&handle.instance, "icons", f)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, None));
    state.runtime.upsert_buffer_group(
        state,
        BufferGroup { id: rail_id(&handle.account_id, &server.id), account_id: handle.account_id.clone(), service: "stoat".to_string(), kind: "guild".to_string(), name: server.name.clone(), icon_url, position, pending: false },
    );
}

/// What a name is in the list: the title, and where it is filed. A name another channel already has is told apart by
/// the end of this one's id.
fn unique_name(state: &AppState, account_id: &str, wanted: &str, channel_id: &str) -> String {
    let taken = state.runtime.list_buffers().into_iter().any(|b| b.account_id == account_id && b.name == wanted && b.remote_id.as_deref() != Some(channel_id));
    if taken {
        format!("{wanted} ({})", &channel_id[channel_id.len().saturating_sub(4)..])
    } else {
        wanted.to_string()
    }
}

/// A channel's conversation: made if it is not there, renamed if it was called something else, filed under its server
/// and its category, and told whether the account may write in it.
pub fn register_channel(state: &AppState, handle: &Handle, channel: &Channel, order: i64, heading: Option<(&str, i64)>) {
    let account_id = &handle.account_id;
    let (title, read_only, avatar) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if world.permissions(channel, now()) & world::VIEW_CHANNEL == 0 {
            return;
        }
        let avatar = match channel.kind {
            Kind::Direct => channel.recipients.iter().find(|r| **r != world.own_id).and_then(|r| world.users.get(r)).and_then(|u| u.avatar.clone()).and_then(|f| media::file_url(&handle.instance, "avatars", &f)),
            Kind::Group => channel.icon.as_deref().and_then(|f| media::file_url(&handle.instance, "icons", f)),
            _ => None,
        };
        (world.title_of(channel), world.read_only(channel, now()), avatar)
    };
    // The older kind of voice channel has no text: it is joined, which is a later round.
    if channel.kind == Kind::Voice {
        return;
    }
    let kind = if matches!(channel.kind, Kind::Direct | Kind::Saved) { "dm" } else { "channel" };
    let existing = state.runtime.list_buffers().into_iter().find(|b| b.account_id == *account_id && b.remote_id.as_deref() == Some(channel.id.as_str()));
    let name = unique_name(state, account_id, &title, &channel.id);
    let buffer = match existing {
        Some(found) if found.name == name => found,
        Some(found) => {
            state.runtime.rename_buffer(state, account_id, &found.name, &name, kind);
            state.runtime.get_buffer(&crate::model::buffer_id(account_id, &name)).unwrap_or(found)
        }
        None => state.runtime.ensure_buffer(state, account_id, &name, kind),
    };
    state.runtime.set_buffer_remote_id(state, &buffer.id, &channel.id);
    match (&channel.server, heading) {
        (Some(server), heading) => {
            state.runtime.set_buffer_group(state, &buffer.id, &rail_id(account_id, server));
            // Under the server's own heading, in the server's own order: the heading's place first, then the channel's.
            match heading {
                Some((title, place)) => state.runtime.set_buffer_category(state, &buffer.id, Some(title), place * 1000 + order),
                None => state.runtime.set_buffer_category(state, &buffer.id, None, order),
            }
        }
        (None, _) => {
            let heading = if channel.kind == Kind::Group { "Groups" } else { "Direct Messages" };
            state.runtime.set_buffer_category(state, &buffer.id, Some(heading), 0);
        }
    }
    state.runtime.set_buffer_read_only(state, &buffer.id, read_only);
    if let Some(topic) = &channel.description {
        state.runtime.set_buffer_topic(state, &buffer.id, topic);
    }
    // A channel that can also be called into is marked, as a horn marks a Telegram channel.
    state.runtime.set_buffer_name_icon(state, &buffer.id, channel.voice.then_some("volume_up"));
    if let Some(url) = avatar {
        state.runtime.set_buffer_avatar(state, &buffer.id, &media::cached(account_id, media::AVATARS, &url, None, None));
    }
}

/// A server and its channels, made at once: one just joined.
pub fn register_server_entry(state: &AppState, handle: &Handle, server: &world::Server) {
    register_server(state, handle, server, 0);
    register_server_channels(state, handle, server);
}

/// A server's channels under its headings.
fn register_server_channels(state: &AppState, handle: &Handle, server: &world::Server) {
    let channels: Vec<Channel> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        server.channels.iter().filter_map(|id| world.channels.get(id).cloned()).collect()
    };
    for (order, channel) in channels.iter().enumerate() {
        let heading = server.categories.iter().enumerate().find(|(_, c)| c.channels.contains(&channel.id)).map(|(place, c)| (c.title.as_str(), place as i64 + 1));
        register_channel(state, handle, channel, order as i64, heading);
    }
}

/// Keeps who is in each voice channel as an event says, and tells the window of the channels it touched.
fn voice_changed(state: &AppState, handle: &Handle, event: &Value) {
    let mut touched: Vec<String> = Vec::new();
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        match event["type"].as_str().unwrap_or("") {
            "VoiceChannelJoin" => {
                if let (Some(channel), Some(member)) = (event["id"].as_str(), world::parse_voice_member(&event["state"])) {
                    let list = world.voice.entry(channel.to_string()).or_default();
                    list.retain(|m| m.user != member.user);
                    list.push(member);
                    touched.push(channel.to_string());
                }
            }
            "VoiceChannelLeave" => {
                if let (Some(channel), Some(user)) = (event["id"].as_str(), event["user"].as_str()) {
                    if let Some(list) = world.voice.get_mut(channel) {
                        list.retain(|m| m.user != user);
                    }
                    touched.push(channel.to_string());
                }
            }
            "VoiceChannelMove" => {
                if let (Some(from), Some(to), Some(member)) = (event["from"].as_str(), event["to"].as_str(), world::parse_voice_member(&event["state"])) {
                    if let Some(list) = world.voice.get_mut(from) {
                        list.retain(|m| m.user != member.user);
                    }
                    let list = world.voice.entry(to.to_string()).or_default();
                    list.retain(|m| m.user != member.user);
                    list.push(member);
                    touched.extend([from.to_string(), to.to_string()]);
                }
            }
            _ => {
                if let (Some(channel), Some(user)) = (event["channel_id"].as_str(), event["id"].as_str()) {
                    if let Some(member) = world.voice.get_mut(channel).and_then(|l| l.iter_mut().find(|m| m.user == user)) {
                        let data = &event["data"];
                        if let Some(v) = data["camera"].as_bool() {
                            member.camera = v;
                        }
                        if let Some(v) = data["screensharing"].as_bool() {
                            member.screen = v;
                        }
                        if let Some(v) = data["is_publishing"].as_bool() {
                            member.publishing = v;
                        }
                        touched.push(channel.to_string());
                    }
                }
            }
        }
        world.voice.retain(|_, l| !l.is_empty());
    }
    for channel in touched {
        tell_voice(state, handle, &channel);
    }
}

/// Says who is in a channel's call, in the words the window draws.
pub fn tell_voice(state: &AppState, handle: &Handle, channel: &str) {
    let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) else { return };
    state.events.emit("stoatVoice", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "members": voice_members(handle, channel) }));
}

/// The people in a channel's call.
pub fn voice_members(handle: &Handle, channel: &str) -> Vec<Value> {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    world
        .voice
        .get(channel)
        .into_iter()
        .flatten()
        .map(|m| {
            let avatar = world.users.get(&m.user).and_then(|u| u.avatar.clone()).and_then(|f| media::file_url(&handle.instance, "avatars", &f)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, None));
            json!({ "userId": m.user, "name": world.name_of(&m.user), "avatarUrl": avatar, "camera": m.camera, "screen": m.screen, "muted": !m.publishing })
        })
        .collect()
}

/// The custom emoji the account can use, as the picker's sections: one for each server that has some, and one for the
/// rest. Said again whenever an emoji is made or deleted.
pub fn register_emoji(state: &AppState, handle: &Handle) {
    /// Each server's emoji (the ones with no server together), by server, and what each server is called and shown as.
    type ByServer = std::collections::BTreeMap<Option<String>, Vec<(String, world::EmojiInfo)>>;
    type Names = std::collections::HashMap<String, (String, Option<String>)>;
    let account_id = &handle.account_id;
    let (groups, names): (ByServer, Names) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let mut groups: ByServer = Default::default();
        for (id, info) in &world.emojis {
            groups.entry(info.server.clone()).or_default().push((id.clone(), info.clone()));
        }
        let names = world.servers.iter().map(|(id, s)| (id.clone(), (s.name.clone(), s.icon.clone()))).collect();
        (groups, names)
    };
    state.runtime.clear_emoji_sources(account_id, "stoat");
    for (server, mut list) in groups {
        list.sort_by_key(|(_, e)| e.name.to_lowercase());
        let (name, icon) = server.as_ref().and_then(|s| names.get(s).cloned()).unwrap_or_else(|| ("Your emoji".to_string(), None));
        let buffers: Vec<String> = match &server {
            Some(s) => state.runtime.list_buffers().into_iter().filter(|b| b.account_id == *account_id && b.group_id.as_deref() == Some(rail_id(account_id, s).as_str())).map(|b| b.id).collect(),
            None => Vec::new(),
        };
        let entries = list
            .into_iter()
            .map(|(id, info)| crate::model::EmojiEntry {
                url: media::file_url(&handle.instance, "emojis", &id).map(|u| media::cached(account_id, media::MEDIA, &u, None, Some(if info.animated { "image/gif" } else { "image/png" }))),
                id,
                name: info.name,
                animated: info.animated,
                locked: false,
            })
            .collect();
        state.runtime.set_emoji_source(
            account_id,
            crate::model::EmojiSource {
                id: format!("stoat:{}", server.as_deref().unwrap_or("detached")),
                name,
                service: "stoat".to_string(),
                kind: "text".to_string(),
                icon_url: icon.as_deref().and_then(|f| media::file_url(&handle.instance, "icons", f)).map(|u| media::cached(account_id, media::AVATARS, &u, None, None)),
                buffers,
                // Stoat lets an account use an emoji from another of its servers.
                sendable_anywhere: true,
                emoji: entries,
            },
        );
    }
}

/// Everything `Ready` carries, made into the rail and the conversations.
pub fn register_all(state: &AppState, handle: &Handle, ready: &Value) {
    let (servers, others, name) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let servers: Vec<world::Server> = {
            let mut list: Vec<world::Server> = world.servers.values().cloned().collect();
            list.sort_by_key(|s| s.name.to_lowercase());
            list
        };
        let others: Vec<Channel> = world.channels.values().filter(|c| c.server.is_none()).cloned().collect();
        (servers, others, world.name_of(&world.own_id))
    };
    state.runtime.set_own_identity(&handle.account_id, &name);
    for (position, server) in servers.iter().enumerate() {
        register_server(state, handle, server, position as i64);
        register_server_channels(state, handle, server);
    }
    for channel in &others {
        register_channel(state, handle, channel, 0, None);
    }
    register_emoji(state, handle);
    let in_calls: Vec<String> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).voice.keys().cloned().collect();
    for channel in in_calls {
        tell_voice(state, handle, &channel);
    }
    // Gone since the last time: a server left, a channel closed.
    let present: HashSet<String> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.keys().cloned().collect();
    for buffer in state.runtime.list_buffers() {
        if buffer.account_id == handle.account_id && buffer.remote_id.as_ref().is_some_and(|r| !present.contains(r)) {
            state.runtime.remove_buffer(state, &buffer.id);
        }
    }
    let rails: HashSet<String> = servers.iter().map(|s| rail_id(&handle.account_id, &s.id)).collect();
    for group in state.runtime.list_buffer_groups() {
        if group.account_id == handle.account_id && group.service == "stoat" && group.kind == "guild" && !rails.contains(&group.id) {
            state.runtime.remove_buffer_group(state, &group.id);
        }
    }
    // What was unread, as far as Stoat says it: where this account last read to, and how many mentions since.
    for unread in ready["channel_unreads"].as_array().into_iter().flatten() {
        let (Some(channel), Some(last)) = (unread["_id"]["channel"].as_str(), unread["last_id"].as_str()) else { continue };
        let newest = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(channel).and_then(|c| c.last_message_id.clone());
        let mentions = unread["mentions"].as_array().map(|m| m.len() as u32).unwrap_or(0);
        if let (Some(buffer), Some(newest)) = (messages::buffer_of(state, &handle.account_id, channel), newest) {
            // Stoat says where reading stopped and where the channel ends, not how many lie between.
            let unread_here = u32::from(newest.as_str() > last);
            state.runtime.set_buffer_unread(state, &buffer.id, unread_here.max(mentions), mentions);
        }
    }
    tracing::info!("stoat[{}]: {} servers, {} conversations", handle.account_id, servers.len(), handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.len());
}

fn typing(state: &AppState, handle: &Handle, channel: &str, user: &str) {
    let (own, name) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        (world.own_id.clone(), world.name_of(user))
    };
    if user == own {
        return;
    }
    if let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) {
        state.events.emit("typing", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "nicks": [name], "expiresInMs": TYPING_FOR_MS }));
    }
}

/// The message an edit or an append touched, brought up to date.
fn edited(state: &AppState, handle: &Handle, event: &Value) {
    let (Some(id), Some(channel)) = (event["id"].as_str(), event["channel"].as_str()) else { return };
    let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) else { return };
    let data = &event["data"];
    if let Some(content) = data["content"].as_str() {
        let body = {
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            let server = world.channels.get(channel).and_then(|c| c.server.clone());
            messages::words(&world, Some((&handle.instance, &handle.account_id)), server.as_deref(), content)
        };
        state.runtime.update_message_body_only(state, &buffer.id, id, &body);
    }
}

fn reaction(state: &AppState, handle: &Handle, event: &Value, add: bool) {
    let (Some(id), Some(channel), Some(user), Some(emoji)) = (event["id"].as_str(), event["channel_id"].as_str(), event["user_id"].as_str(), event["emoji_id"].as_str()) else { return };
    let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) else { return };
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let me = user == world.own_id;
    let shown = if emoji.len() == 26 && emoji.chars().all(|c| c.is_ascii_alphanumeric()) { world.emojis.get(emoji).map(|e| format!(":{}:", e.name)).unwrap_or_else(|| ":emoji:".to_string()) } else { emoji.to_string() };
    drop(world);
    state.runtime.update_reaction(state, &buffer.id, id, &shown, me, add);
}

/// One event.
pub fn handle(state: &AppState, handle: &Handle, event: &Value) {
    let Some(kind) = event["type"].as_str() else { return };
    let account_id = &handle.account_id;
    match kind {
        "Bulk" => {
            for inner in event["v"].as_array().into_iter().flatten() {
                self::handle(state, handle, inner);
            }
        }
        "Message" => {
            messages::record(state, handle, event);
        }
        "MessageUpdate" | "MessageAppend" => edited(state, handle, event),
        "MessageDelete" => {
            if let (Some(id), Some(channel)) = (event["id"].as_str(), event["channel"].as_str()) {
                if let Some(buffer) = messages::buffer_of(state, account_id, channel) {
                    state.runtime.delete_message(state, &buffer.id, id);
                }
            }
        }
        "BulkMessageDelete" => {
            if let (Some(ids), Some(channel)) = (event["ids"].as_array(), event["channel"].as_str()) {
                if let Some(buffer) = messages::buffer_of(state, account_id, channel) {
                    for id in ids.iter().filter_map(Value::as_str) {
                        state.runtime.delete_message(state, &buffer.id, id);
                    }
                }
            }
        }
        "MessageReact" => reaction(state, handle, event, true),
        "MessageUnreact" => reaction(state, handle, event, false),
        "MessageRemoveReaction" => {
            if let (Some(id), Some(channel)) = (event["id"].as_str(), event["channel_id"].as_str()) {
                if let Some(buffer) = messages::buffer_of(state, account_id, channel) {
                    let emoji = event["emoji_id"].as_str().unwrap_or("");
                    state.runtime.remove_reaction_entirely(state, &buffer.id, id, emoji);
                }
            }
        }
        "ChannelCreate" => {
            if let Some(channel) = world::parse_channel(event) {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.insert(channel.id.clone(), channel.clone());
                if let Some(server) = channel.server.as_ref().and_then(|s| handle.world.lock().unwrap_or_else(|e| e.into_inner()).servers.get(s).cloned()) {
                    register_server_channels(state, handle, &server);
                } else {
                    register_channel(state, handle, &channel, 0, None);
                }
            }
        }
        "ChannelUpdate" => {
            let Some(id) = event["id"].as_str() else { return };
            let updated = {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(channel) = world.channels.get_mut(id) {
                    let data = &event["data"];
                    if let Some(name) = data["name"].as_str() {
                        channel.name = name.to_string();
                    }
                    if data.get("description").is_some() {
                        channel.description = data["description"].as_str().filter(|d| !d.is_empty()).map(str::to_string);
                    }
                    if data.get("default_permissions").is_some() {
                        channel.default = (data["default_permissions"]["a"].as_u64().unwrap_or(0), data["default_permissions"]["d"].as_u64().unwrap_or(0));
                    }
                    if let Some(roles) = data["role_permissions"].as_object() {
                        channel.roles = roles.iter().map(|(k, p)| (k.clone(), (p["a"].as_u64().unwrap_or(0), p["d"].as_u64().unwrap_or(0)))).collect();
                    }
                    if let Some(p) = data["permissions"].as_u64() {
                        channel.group_permissions = Some(p);
                    }
                }
                world.channels.get(id).cloned()
            };
            if let Some(channel) = updated {
                match channel.server.as_ref().and_then(|s| handle.world.lock().unwrap_or_else(|e| e.into_inner()).servers.get(s).cloned()) {
                    Some(server) => register_server_channels(state, handle, &server),
                    None => register_channel(state, handle, &channel, 0, None),
                }
            }
        }
        "ChannelDelete" => {
            if let Some(id) = event["id"].as_str() {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.remove(id);
                if let Some(buffer) = messages::buffer_of(state, account_id, id) {
                    state.runtime.remove_buffer(state, &buffer.id);
                }
            }
        }
        "ChannelStartTyping" => {
            if let (Some(channel), Some(user)) = (event["id"].as_str(), event["user"].as_str()) {
                typing(state, handle, channel, user);
            }
        }
        "ChannelAck" => {
            let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
            if event["user"].as_str() == Some(own.as_str()) {
                if let Some(buffer) = event["id"].as_str().and_then(|c| messages::buffer_of(state, account_id, c)) {
                    state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
                    state.events.emit("bufferRead", json!({ "accountId": account_id, "bufferId": buffer.id }));
                }
            }
        }
        "ServerCreate" => {
            let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            let server = world::parse_server(&event["server"]);
            let channels: Vec<Channel> = event["channels"].as_array().into_iter().flatten().filter_map(world::parse_channel).collect();
            let member = world::parse_member(&json!({ "_id": { "server": event["server"]["_id"], "user": world.own_id }, "roles": [] }));
            for channel in channels {
                world.channels.insert(channel.id.clone(), channel);
            }
            if let Some(server) = &server {
                if let Some((s, _, m)) = member {
                    world.members.entry(s).or_insert(m);
                }
                world.servers.insert(server.id.clone(), server.clone());
            }
            drop(world);
            if let Some(server) = server {
                register_server(state, handle, &server, 0);
                register_server_channels(state, handle, &server);
            }
        }
        "ServerUpdate" => {
            let Some(id) = event["id"].as_str() else { return };
            let updated = {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(server) = world.servers.get_mut(id) {
                    let data = &event["data"];
                    if let Some(name) = data["name"].as_str() {
                        server.name = name.to_string();
                    }
                    if let Some(p) = data["default_permissions"].as_u64() {
                        server.default_permissions = p;
                    }
                    if let Some(parsed) = world::parse_server(&json!({ "_id": id, "categories": data["categories"], "channels": data["channels"] })) {
                        if data.get("categories").is_some() {
                            server.categories = parsed.categories;
                        }
                        if data.get("channels").is_some() {
                            server.channels = parsed.channels;
                        }
                    }
                    if data.get("icon").is_some() {
                        server.icon = data["icon"]["_id"].as_str().map(str::to_string);
                    }
                }
                world.servers.get(id).cloned()
            };
            if let Some(server) = updated {
                register_server(state, handle, &server, 0);
                register_server_channels(state, handle, &server);
            }
        }
        "ServerDelete" => {
            if let Some(id) = event["id"].as_str() {
                let channels: Vec<String> = {
                    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                    world.members.remove(id);
                    let gone = world.servers.remove(id).map(|s| s.channels).unwrap_or_default();
                    for c in &gone {
                        world.channels.remove(c);
                    }
                    gone
                };
                for channel in channels {
                    if let Some(buffer) = messages::buffer_of(state, account_id, &channel) {
                        state.runtime.remove_buffer(state, &buffer.id);
                    }
                }
                state.runtime.remove_buffer_group(state, &rail_id(account_id, id));
            }
        }
        "ServerMemberUpdate" => {
            let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
            if event["id"]["user"].as_str() != Some(own.as_str()) {
                return;
            }
            let Some(server_id) = event["id"]["server"].as_str() else { return };
            {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                let member = world.members.entry(server_id.to_string()).or_default();
                let data = &event["data"];
                if let Some(roles) = data["roles"].as_array() {
                    member.roles = roles.iter().filter_map(|r| r.as_str().map(str::to_string)).collect();
                }
                if data.get("timeout").is_some() {
                    member.timeout = data["timeout"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp());
                }
                if let Some(nick) = data["nickname"].as_str() {
                    member.nickname = Some(nick.to_string());
                }
                if let Some(c) = event["clear"].as_array() {
                    if c.iter().any(|x| x.as_str() == Some("Timeout")) {
                        member.timeout = None;
                    }
                }
            }
            // Looked up first, and the lock let go: making the channels takes it again.
            let server = handle.world.lock().unwrap_or_else(|e| e.into_inner()).servers.get(server_id).cloned();
            if let Some(server) = server {
                register_server_channels(state, handle, &server);
            }
        }
        "ServerRoleUpdate" => {
            let (Some(server_id), Some(role_id)) = (event["id"].as_str(), event["role_id"].as_str()) else { return };
            {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(server) = world.servers.get_mut(server_id) {
                    let role = server.roles.entry(role_id.to_string()).or_default();
                    let data = &event["data"];
                    if let Some(name) = data["name"].as_str() {
                        role.name = name.to_string();
                    }
                    if let Some(rank) = data["rank"].as_i64() {
                        role.rank = rank;
                    }
                    if data.get("permissions").is_some() {
                        role.allow = data["permissions"]["a"].as_u64().unwrap_or(0);
                        role.deny = data["permissions"]["d"].as_u64().unwrap_or(0);
                    }
                    if let Some(c) = data["colour"].as_str() {
                        role.colour = Some(c.to_string());
                    }
                }
            }
            // Looked up first, and the lock let go: making the channels takes it again.
            let server = handle.world.lock().unwrap_or_else(|e| e.into_inner()).servers.get(server_id).cloned();
            if let Some(server) = server {
                register_server_channels(state, handle, &server);
            }
        }
        "ServerRoleDelete" => {
            if let (Some(server_id), Some(role_id)) = (event["id"].as_str(), event["role_id"].as_str()) {
                if let Some(server) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).servers.get_mut(server_id) {
                    server.roles.remove(role_id);
                }
            }
        }
        "UserUpdate" => {
            let Some(id) = event["id"].as_str() else { return };
            let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(user) = world.users.get_mut(id) {
                let data = &event["data"];
                if let Some(name) = data["display_name"].as_str() {
                    user.name = name.to_string();
                }
                if let Some(online) = data["online"].as_bool() {
                    user.online = online;
                }
                if data.get("avatar").is_some() {
                    user.avatar = data["avatar"]["_id"].as_str().map(str::to_string);
                }
                if let Some(presence) = data["status"]["presence"].as_str() {
                    user.presence = presence.to_string();
                }
                if data["status"].get("text").is_some() {
                    user.status_text = data["status"]["text"].as_str().filter(|t| !t.is_empty()).map(str::to_string);
                }
                if let Some(clear) = event["clear"].as_array() {
                    if clear.iter().any(|c| c.as_str() == Some("StatusText")) {
                        user.status_text = None;
                    }
                }
            }
            drop(world);
            super::members::update_status(state, handle, id);
        }
        "ServerMemberJoin" | "ServerMemberLeave" => {
            // Whoever's list is open for the server is read again.
            let Some(server) = event["id"].as_str() else { return };
            let channels: Vec<String> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).servers.get(server).map(|s| s.channels.clone()).unwrap_or_default();
            for channel in channels {
                if let Some(buffer) = messages::buffer_of(state, account_id, &channel) {
                    if state.runtime.get_presence(&buffer.id).is_some() {
                        let (state, handle) = (state.clone(), handle.clone());
                        tokio::spawn(async move {
                            let _ = super::members::refresh(&state, &handle, &buffer).await;
                        });
                    }
                }
            }
        }
        "UserRelationship" => {
            // The other person, as Stoat sees them (with what they are to this account in `relationship`); an older
            // instance says it beside, in `status`, and names the person by id.
            {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                match world::parse_user(&event["user"]) {
                    Some((id, parsed)) => {
                        let status = event["status"].as_str().map(str::to_string).unwrap_or_else(|| parsed.relationship.clone());
                        let entry = world.users.entry(id).or_insert_with(|| parsed.clone());
                        entry.name = parsed.name;
                        entry.avatar = parsed.avatar;
                        entry.relationship = status;
                    }
                    None => {
                        if let (Some(id), Some(status)) = (event["user"].as_str(), event["status"].as_str()) {
                            if let Some(user) = world.users.get_mut(id) {
                                user.relationship = status.to_string();
                            }
                        }
                    }
                }
            }
            // A conversation with them may now be one that can be written in, or no longer.
            let other = event["user"]["_id"].as_str().or(event["user"].as_str()).unwrap_or("").to_string();
            let direct: Vec<Channel> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.values().filter(|c| c.kind == Kind::Direct && c.recipients.contains(&other)).cloned().collect();
            for channel in direct {
                register_channel(state, handle, &channel, 0, None);
            }
            super::people::tell_requests(state, handle);
        }
        // Who is in a voice channel: told as they come, go, move, and mute or turn a camera on.
        "VoiceChannelJoin" | "VoiceChannelLeave" | "VoiceChannelMove" | "UserVoiceStateUpdate" => voice_changed(state, handle, event),
        "EmojiCreate" => {
            if let Some((id, info)) = world::parse_emoji(event) {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).emojis.insert(id, info);
                register_emoji(state, handle);
            }
        }
        "EmojiDelete" => {
            if let Some(id) = event["id"].as_str() {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).emojis.remove(id);
                register_emoji(state, handle);
            }
        }
        _ => {}
    }
}

/// Makes the world from `Ready`, and the conversations from the world.
pub fn ready(state: &AppState, handle: &Handle, own_id: &str, event: &Value) {
    {
        let mut world: std::sync::MutexGuard<'_, World> = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.absorb_ready(own_id, event);
    }
    register_all(state, handle, event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_relationship_event_names_the_other_person_and_what_they_are_to_this_account() {
        let state = crate::backend::kick::testkit::simulated_daemon("stoat-relationship");
        let h = crate::backend::stoat::tests::bare_handle("stoat:ME");
        h.world.lock().unwrap().own_id = "ME".into();
        handle(&state, &h, &json!({ "type": "UserRelationship", "id": "ME", "user": { "_id": "ERIN", "username": "erin", "relationship": "Incoming" } }));
        assert_eq!(super::super::people::incoming(&h).iter().map(|r| r["name"].as_str().unwrap().to_string()).collect::<Vec<_>>(), vec!["erin"]);
        handle(&state, &h, &json!({ "type": "UserRelationship", "id": "ME", "user": { "_id": "ERIN", "username": "erin", "relationship": "Friend" } }));
        assert!(super::super::people::incoming(&h).is_empty(), "answered: no longer waiting");
    }

    /// A role changing or an own membership changing makes the server's channels again, which takes the world's lock: an
    /// event handled while still holding it would never come back (found against a real instance, where it stopped the
    /// whole connection).
    #[test]
    fn a_role_or_membership_change_does_not_wait_on_itself() {
        let state = crate::backend::kick::testkit::simulated_daemon("stoat-deadlock");
        let h = crate::backend::stoat::tests::bare_handle("stoat:ME");
        {
            let mut w = h.world.lock().unwrap();
            w.own_id = "ME".into();
            w.servers.insert("S".into(), world::parse_server(&json!({ "_id": "S", "owner": "ME", "name": "Hall", "channels": ["C"] })).unwrap());
            w.channels.insert("C".into(), world::parse_channel(&json!({ "_id": "C", "channel_type": "TextChannel", "server": "S", "name": "general" })).unwrap());
        }
        let (done, heard) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            handle(&state, &h, &json!({ "type": "ServerRoleUpdate", "id": "S", "role_id": "R", "data": { "name": "Mods", "rank": 1 } }));
            handle(&state, &h, &json!({ "type": "ServerMemberUpdate", "id": { "server": "S", "user": "ME" }, "data": { "roles": ["R"] } }));
            handle(&state, &h, &json!({ "type": "ServerUpdate", "id": "S", "data": { "name": "Hall 2" } }));
            handle(&state, &h, &json!({ "type": "ChannelUpdate", "id": "C", "data": { "name": "lobby" } }));
            let _ = done.send(());
        });
        assert!(heard.recv_timeout(std::time::Duration::from_secs(10)).is_ok(), "an event took the world's lock twice");
    }

    #[test]
    fn who_is_in_a_call_follows_the_events_and_leaves_with_the_last_one() {
        let state = crate::backend::kick::testkit::simulated_daemon("stoat-voice");
        let h = crate::backend::stoat::tests::bare_handle("stoat:ME");
        h.world.lock().unwrap().absorb_ready("ME", &json!({ "voice_states": [{ "id": "C", "participants": [{ "id": "U1", "is_publishing": true, "camera": false, "screensharing": false }] }] }));
        assert_eq!(h.world.lock().unwrap().voice["C"].len(), 1, "who was already in it when the account connected");
        handle(&state, &h, &json!({ "type": "VoiceChannelJoin", "id": "C", "state": { "id": "U2", "is_publishing": true } }));
        handle(&state, &h, &json!({ "type": "UserVoiceStateUpdate", "id": "U2", "channel_id": "C", "data": { "camera": true, "is_publishing": false } }));
        {
            let w = h.world.lock().unwrap();
            let two = w.voice["C"].iter().find(|m| m.user == "U2").unwrap();
            assert!(two.camera && !two.publishing, "a camera on and a microphone off");
        }
        handle(&state, &h, &json!({ "type": "VoiceChannelMove", "user": "U2", "from": "C", "to": "D", "state": { "id": "U2" } }));
        assert!(h.world.lock().unwrap().voice["D"].iter().any(|m| m.user == "U2") && h.world.lock().unwrap().voice["C"].iter().all(|m| m.user != "U2"));
        handle(&state, &h, &json!({ "type": "VoiceChannelLeave", "id": "C", "user": "U1" }));
        assert!(!h.world.lock().unwrap().voice.contains_key("C"), "an empty call is not kept");
        assert_eq!(voice_members(&h, "D").len(), 1);
    }
}
