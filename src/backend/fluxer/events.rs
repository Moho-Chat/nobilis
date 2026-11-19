//! What the gateway says: `READY`, which makes the rail and the conversations, and the Dispatches after it - messages,
//! edits, deletions, reactions, typing, channels and guilds changing, a conversation read on another device.

use super::media;
use super::messages;
use super::world::{self, Channel, Kind};
use super::Handle;
use crate::model::BufferGroup;
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::HashSet;

/// How long somebody's "typing" lasts without another (Fluxer sends one every few seconds while it goes on).
const TYPING_FOR_MS: u64 = 10_000;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The rail entry for a guild.
pub fn rail_id(account_id: &str, guild_id: &str) -> String {
    format!("{account_id}|guild:{guild_id}")
}

fn register_guild(state: &AppState, handle: &Handle, guild: &world::Guild, position: i64) {
    let icon_url = guild.icon.as_deref().and_then(|h| media::asset_url(&handle.instance, "icons", &guild.id, h)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp")));
    state.runtime.upsert_buffer_group(
        state,
        BufferGroup { id: rail_id(&handle.account_id, &guild.id), account_id: handle.account_id.clone(), service: "fluxer".to_string(), kind: "guild".to_string(), name: guild.name.clone(), icon_url, position, pending: false },
    );
}

/// What a channel is called in the list: its title, told apart from another of the same name by the end of its id.
fn unique_name(state: &AppState, account_id: &str, wanted: &str, channel_id: &str) -> String {
    let taken = state.runtime.list_buffers().into_iter().any(|b| b.account_id == account_id && b.name == wanted && b.remote_id.as_deref() != Some(channel_id));
    if taken {
        format!("{wanted} ({})", &channel_id[channel_id.len().saturating_sub(4)..])
    } else {
        wanted.to_string()
    }
}

/// A channel's conversation: made if it is not there, renamed if it was called something else, filed under its guild
/// and its category, and told whether the account may write in it.
pub fn register_channel(state: &AppState, handle: &Handle, channel: &Channel, place: i64, heading: Option<(&str, i64)>) {
    let account_id = &handle.account_id;
    // Categories are headings and a link channel is only a link. A voice channel is a conversation too: it has a call,
    // and messages beside it. A forum is a list of posts; a thread is a conversation once this account has joined it.
    if matches!(channel.kind, Kind::Category | Kind::Link | Kind::Other(_)) || (channel.kind == Kind::Thread && !channel.joined) {
        return;
    }
    let (title, read_only, avatar, unread) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if channel.guild.is_some() && world.permissions(channel) & world::VIEW_CHANNEL == 0 {
            return;
        }
        let avatar = match channel.kind {
            Kind::Direct => channel.recipients.iter().find(|r| **r != world.own_id).and_then(|r| world.users.get(r).map(|u| (r.clone(), u.avatar.clone()))).and_then(|(id, h)| media::asset_url(&handle.instance, "avatars", &id, &h?)),
            Kind::Group => channel.icon.as_deref().and_then(|h| media::asset_url(&handle.instance, "channel-icons", &channel.id, h)),
            _ => None,
        };
        let read = world.read.get(&channel.id).cloned();
        let unread = match (&channel.last_message_id, read) {
            (Some(last), Some((Some(seen), mentions))) => (last.parse::<u64>().unwrap_or(0) > seen.parse::<u64>().unwrap_or(0), mentions),
            (Some(_), Some((None, mentions))) => (true, mentions),
            _ => (false, 0),
        };
        (world.title_of(channel), world.read_only(channel, now()), avatar, unread)
    };
    let kind = if channel.kind.private() && channel.kind != Kind::Group { "dm" } else { "channel" };
    let existing = state.runtime.list_buffers().into_iter().find(|b| b.account_id == *account_id && b.remote_id.as_deref() == Some(channel.id.as_str()));
    let fresh = existing.is_none();
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
    match &channel.guild {
        Some(guild) => {
            state.runtime.set_buffer_group(state, &buffer.id, &rail_id(account_id, guild));
            match heading {
                Some((title, at)) => state.runtime.set_buffer_category(state, &buffer.id, Some(title), at * 1000 + place),
                None => state.runtime.set_buffer_category(state, &buffer.id, None, place),
            }
        }
        None => {
            let heading = match channel.kind {
                Kind::Group => "Groups",
                Kind::Notes => "Notes",
                _ => "Direct Messages",
            };
            state.runtime.set_buffer_category(state, &buffer.id, Some(heading), 0);
        }
    }
    state.runtime.set_buffer_read_only(state, &buffer.id, read_only);
    if let Some(topic) = &channel.topic {
        state.runtime.set_buffer_topic(state, &buffer.id, topic);
    }
    let icon = match channel.kind {
        Kind::Announcement => Some("campaign"),
        Kind::Voice => Some("volume_up"),
        Kind::Thread => Some("forum"),
        _ => None,
    };
    state.runtime.set_buffer_name_icon(state, &buffer.id, icon);
    state.runtime.set_buffer_forum(state, &buffer.id, channel.kind == Kind::Forum);
    if let Some(url) = avatar {
        state.runtime.set_buffer_avatar(state, &buffer.id, &media::cached(account_id, media::AVATARS, &url, None, Some("image/webp")));
    }
    if fresh && unread.0 {
        state.runtime.set_buffer_unread(state, &buffer.id, 1, unread.1 as u32);
    }
}

/// A guild's channels, under its categories, in its own order.
pub fn register_guild_channels(state: &AppState, handle: &Handle, guild_id: &str) {
    let channels: Vec<Channel> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let Some(guild) = world.guilds.get(guild_id) else { return };
        guild.channels.iter().filter_map(|id| world.channels.get(id).cloned()).collect()
    };
    let mut categories: Vec<&Channel> = channels.iter().filter(|c| c.kind == Kind::Category).collect();
    categories.sort_by_key(|c| (c.position, c.id.parse::<u64>().unwrap_or(0)));
    let mut rest: Vec<&Channel> = channels.iter().filter(|c| c.kind != Kind::Category && c.kind != Kind::Thread).collect();
    rest.sort_by_key(|c| (c.position, c.id.parse::<u64>().unwrap_or(0)));
    // A thread goes just after the channel it is in, under the same heading.
    let mut ordered: Vec<&Channel> = Vec::new();
    for channel in rest {
        ordered.push(channel);
        let mut threads: Vec<&Channel> = channels.iter().filter(|t| t.kind == Kind::Thread && t.parent.as_deref() == Some(channel.id.as_str())).collect();
        threads.sort_by_key(|t| t.id.parse::<u64>().unwrap_or(0));
        ordered.extend(threads);
    }
    let parent_of = |c: &Channel| -> Option<String> {
        if c.kind == Kind::Thread {
            c.parent.as_ref().and_then(|p| channels.iter().find(|x| &x.id == p)).and_then(|p| p.parent.clone())
        } else {
            c.parent.clone()
        }
    };
    for (place, channel) in ordered.iter().enumerate() {
        let heading = parent_of(channel).and_then(|p| categories.iter().position(|c| c.id == p).map(|at| (categories[at].name.as_str(), at as i64 + 1)));
        register_channel(state, handle, channel, place as i64, heading);
    }
}

/// The custom emoji of each guild, as the picker's sections.
pub fn register_emoji(state: &AppState, handle: &Handle) {
    let account_id = &handle.account_id;
    /// A guild's id, name and icon, and its emoji by id.
    type Section = (String, String, Option<String>, Vec<(String, world::Emoji)>);
    let guilds: Vec<Section> = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world
            .guilds
            .values()
            .map(|g| {
                let mut list: Vec<(String, world::Emoji)> = world.emojis.iter().filter(|(_, e)| e.guild == g.id).map(|(id, e)| (id.clone(), e.clone())).collect();
                list.sort_by_key(|(_, e)| e.name.to_lowercase());
                (g.id.clone(), g.name.clone(), g.icon.clone(), list)
            })
            .filter(|(_, _, _, list)| !list.is_empty())
            .collect()
    };
    state.runtime.clear_emoji_sources(account_id, "fluxer");
    for (guild_id, name, icon, list) in guilds {
        let buffers: Vec<String> = state.runtime.list_buffers().into_iter().filter(|b| b.account_id == *account_id && b.group_id.as_deref() == Some(rail_id(account_id, &guild_id).as_str())).map(|b| b.id).collect();
        let emoji = list
            .into_iter()
            .map(|(id, e)| crate::model::EmojiEntry {
                url: media::emoji_url(&handle.instance, &id, e.animated).map(|u| media::cached(account_id, media::MEDIA, &u, None, Some(if e.animated { "image/gif" } else { "image/webp" }))),
                id,
                name: e.name,
                animated: e.animated,
                locked: false,
            })
            .collect();
        state.runtime.set_emoji_source(
            account_id,
            crate::model::EmojiSource {
                id: format!("fluxer:{guild_id}"),
                name,
                service: "fluxer".to_string(),
                kind: "text".to_string(),
                icon_url: icon.as_deref().and_then(|h| media::asset_url(&handle.instance, "icons", &guild_id, h)).map(|u| media::cached(account_id, media::AVATARS, &u, None, Some("image/webp"))),
                buffers,
                // Using another guild's emoji needs USE_EXTERNAL_EMOJIS, and Fluxer answers for that when it is sent.
                sendable_anywhere: true,
                emoji,
            },
        );
    }
}

/// `READY`: everything the account can see, made into the rail and the conversations.
pub fn ready(state: &AppState, handle: &Handle, data: &Value) {
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((id, user)) = world::parse_user(&data["user"]) {
            world.own_id = id.clone();
            world.users.insert(id, user);
        }
        for u in data["users"].as_array().into_iter().flatten() {
            if let Some((id, user)) = world::parse_user(u) {
                world.users.insert(id, user);
            }
        }
        for g in data["guilds"].as_array().into_iter().flatten() {
            world.put_guild(g);
            for s in g["voice_states"].as_array().into_iter().flatten() {
                super::voice::keep_state(handle, s);
            }
        }
        for c in data["private_channels"].as_array().into_iter().flatten() {
            world.put_channel(c);
        }
        // The account's personal notes are a channel whose id is the account's own (type 999), which `READY` does not
        // list among the private channels (seen on fluxer.com, 2026-11-18): it is there all the same.
        let own = world.own_id.clone();
        if !own.is_empty() && !world.channels.contains_key(&own) {
            world.put_channel(&json!({ "id": own, "type": 999 }));
        }
        for r in data["relationships"].as_array().into_iter().flatten() {
            let Some(id) = r["id"].as_str().or_else(|| r["user"]["id"].as_str()) else { continue };
            world.set_relationship(id, r["type"].as_i64());
        }
        for s in data["user_guild_settings"].as_array().into_iter().flatten() {
            super::settings::keep(handle, s);
        }
        for p in data["presences"].as_array().into_iter().flatten() {
            if let Some(id) = p["user"]["id"].as_str() {
                world.presence.insert(id.to_string(), (p["status"].as_str().unwrap_or("offline").to_string(), p["custom_status"]["text"].as_str().map(str::to_string)));
            }
        }
        for r in data["read_states"].as_array().into_iter().flatten() {
            if let Some(id) = r["id"].as_str() {
                world.read.insert(id.to_string(), (r["last_message_id"].as_str().map(str::to_string), r["mention_count"].as_u64().unwrap_or(0)));
            }
        }
    }
    register_all(state, handle);
    super::own::settings_changed(state, handle, &data["user_settings"]);
}

/// After a resume, which has no `READY`: what the account can see is asked for again over REST.
pub async fn refetch(state: &AppState, handle: &Handle) {
    let guilds = handle.api.get("/users/@me/guilds", &[]).await.unwrap_or(Value::Null);
    for g in guilds.as_array().into_iter().flatten() {
        let Some(id) = g["id"].as_str() else { continue };
        let channels = handle.api.get(&format!("/guilds/{id}/channels"), &[]).await.unwrap_or(Value::Null);
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if !world.guilds.contains_key(id) {
            let mut whole = g.clone();
            whole["channels"] = channels;
            world.put_guild(&whole);
        } else {
            for c in channels.as_array().into_iter().flatten() {
                world.put_channel(c);
            }
        }
    }
    let private = handle.api.get("/users/@me/channels", &[]).await.unwrap_or(Value::Null);
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        for c in private.as_array().into_iter().flatten() {
            world.put_channel(c);
        }
    }
    register_all(state, handle);
}

fn register_all(state: &AppState, handle: &Handle) {
    let account_id = &handle.account_id;
    let (guilds, private, name) = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let mut guilds: Vec<world::Guild> = world.guilds.values().cloned().collect();
        guilds.sort_by_key(|g| g.name.to_lowercase());
        let private: Vec<Channel> = world.channels.values().filter(|c| c.guild.is_none()).cloned().collect();
        (guilds, private, world.name_in(None, &world.own_id))
    };
    state.runtime.set_own_identity(account_id, &name);
    for (position, guild) in guilds.iter().enumerate() {
        register_guild(state, handle, guild, position as i64);
        register_guild_channels(state, handle, &guild.id);
    }
    for channel in &private {
        register_channel(state, handle, channel, 0, None);
    }
    register_emoji(state, handle);
    super::settings::apply(state, handle);
    super::people::tell_requests(state, handle);
    let in_calls: Vec<String> = handle.voice.lock().unwrap_or_else(|e| e.into_inner()).keys().cloned().collect();
    for channel in in_calls {
        super::voice::tell(state, handle, &channel);
    }
    // Gone since the last time: a guild left, a channel deleted or hidden.
    let present: HashSet<String> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.keys().cloned().collect();
    for buffer in state.runtime.list_buffers() {
        if buffer.account_id == *account_id && buffer.remote_id.as_ref().is_some_and(|r| !present.contains(r)) {
            state.runtime.remove_buffer(state, &buffer.id);
        }
    }
    let rails: HashSet<String> = guilds.iter().map(|g| rail_id(account_id, &g.id)).collect();
    for group in state.runtime.list_buffer_groups() {
        if group.account_id == *account_id && group.service == "fluxer" && group.kind == "guild" && !rails.contains(&group.id) {
            state.runtime.remove_buffer_group(state, &group.id);
        }
    }
    tracing::info!("fluxer[{account_id}]: {} guilds, {} conversations", guilds.len(), present.len());
}

fn typing(state: &AppState, handle: &Handle, data: &Value) {
    let (Some(channel), Some(user)) = (data["channel_id"].as_str(), data["user_id"].as_str()) else { return };
    let name = {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if user == world.own_id {
            return;
        }
        if let (Some(g), true) = (data["guild_id"].as_str(), data["member"].is_object()) {
            world.put_member(g, &data["member"]);
        }
        world.name_in(data["guild_id"].as_str(), user)
    };
    if let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) {
        state.events.emit("typing", json!({ "accountId": handle.account_id, "bufferId": buffer.id, "nicks": [name], "expiresInMs": TYPING_FOR_MS }));
    }
}

/// An edited message brought up to date: its words, and its embeds when Fluxer has unfurled a link since.
fn edited(state: &AppState, handle: &Handle, data: &Value) {
    let (Some(id), Some(channel)) = (data["id"].as_str(), data["channel_id"].as_str()) else { return };
    let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) else { return };
    if data["embeds"].is_array() {
        state.runtime.set_message_embeds(state, &buffer.id, id, &crate::backend::discord::messages::extract_embeds(data));
    }
    if let Some(content) = data["content"].as_str() {
        let body = {
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            let guild = world.channels.get(channel).and_then(|c| c.guild.clone());
            messages::words(&world, Some(handle), guild.as_deref(), content)
        };
        state.runtime.update_message_body_only(state, &buffer.id, id, &body);
    }
}

fn reaction(state: &AppState, handle: &Handle, data: &Value, add: bool) {
    let (Some(id), Some(channel), Some(user)) = (data["message_id"].as_str(), data["channel_id"].as_str(), data["user_id"].as_str()) else { return };
    let Some(buffer) = messages::buffer_of(state, &handle.account_id, channel) else { return };
    let me = user == handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id;
    let shown = messages::reaction_key(handle, &data["emoji"]);
    state.runtime.update_reaction(state, &buffer.id, id, &shown, me, add);
}

/// A guild's channels said again after it changed.
fn redraw_guild(state: &AppState, handle: &Handle, guild: Option<&str>, channel: Option<&Channel>) {
    match guild {
        Some(g) => register_guild_channels(state, handle, g),
        None => {
            if let Some(c) = channel {
                register_channel(state, handle, c, 0, None);
            }
        }
    }
}

/// A refused voice join, said in words (op 12).
pub fn gateway_error(state: &AppState, handle: &Handle, data: &Value) {
    if super::voice::refused(handle, data) {
        return;
    }
    let words = match data["code"].as_str().unwrap_or("") {
        "VOICE_CHANNEL_FULL" => "That voice channel is full".to_string(),
        "VOICE_P2P_CONSENT_REQUIRED" => "That call is peer-to-peer, which moho does not join".to_string(),
        "VOICE_P2P_UNAVAILABLE" => "Fluxer could not make that call peer-to-peer".to_string(),
        other => data["message"].as_str().map(str::to_string).unwrap_or_else(|| format!("Fluxer refused that ({other})")),
    };
    // Said to the call panel by the voice round; until then it is only logged.
    let _ = state;
    tracing::info!("fluxer[{}]: {words}", handle.account_id);
}

/// One Dispatch.
pub fn handle(state: &AppState, handle: &Handle, kind: &str, data: &Value) {
    let account_id = &handle.account_id;
    match kind {
        "MESSAGE_CREATE" => {
            messages::record(state, handle, data);
        }
        "MESSAGE_UPDATE" => edited(state, handle, data),
        "MESSAGE_DELETE" => {
            if let (Some(id), Some(channel)) = (data["id"].as_str(), data["channel_id"].as_str()) {
                if let Some(buffer) = messages::buffer_of(state, account_id, channel) {
                    state.runtime.delete_message(state, &buffer.id, id);
                }
            }
        }
        "MESSAGE_DELETE_BULK" => {
            if let (Some(ids), Some(channel)) = (data["ids"].as_array(), data["channel_id"].as_str()) {
                if let Some(buffer) = messages::buffer_of(state, account_id, channel) {
                    for id in ids.iter().filter_map(Value::as_str) {
                        state.runtime.delete_message(state, &buffer.id, id);
                    }
                }
            }
        }
        "MESSAGE_REACTION_ADD" => reaction(state, handle, data, true),
        "MESSAGE_REACTION_REMOVE" => reaction(state, handle, data, false),
        "MESSAGE_REACTION_REMOVE_EMOJI" => {
            if let (Some(id), Some(channel)) = (data["message_id"].as_str(), data["channel_id"].as_str()) {
                if let Some(buffer) = messages::buffer_of(state, account_id, channel) {
                    state.runtime.remove_reaction_entirely(state, &buffer.id, id, &messages::reaction_key(handle, &data["emoji"]));
                }
            }
        }
        "MESSAGE_REACTION_REMOVE_ALL" => {
            if let (Some(id), Some(channel)) = (data["message_id"].as_str(), data["channel_id"].as_str()) {
                if let Some(buffer) = messages::buffer_of(state, account_id, channel) {
                    if state.store.set_reactions(&buffer.id, id, &[]).unwrap_or(false) {
                        state.events.emit("reactionsChanged", json!({ "bufferId": buffer.id, "id": id, "reactions": [] }));
                    }
                }
            }
        }
        "TYPING_START" => typing(state, handle, data),
        "MESSAGE_ACK" => {
            if let Some(channel) = data["channel_id"].as_str() {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).read.insert(channel.to_string(), (data["message_id"].as_str().map(str::to_string), data["mention_count"].as_u64().unwrap_or(0)));
                if let Some(buffer) = messages::buffer_of(state, account_id, channel) {
                    state.runtime.set_buffer_unread(state, &buffer.id, 0, 0);
                }
            }
        }
        "CHANNEL_CREATE" | "CHANNEL_UPDATE" => {
            let channel = {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                world.put_channel(data).and_then(|id| world.channels.get(&id).cloned())
            };
            if let Some(c) = channel {
                redraw_guild(state, handle, c.guild.clone().as_deref(), Some(&c));
            }
        }
        "CHANNEL_UPDATE_BULK" => {
            let mut guilds = HashSet::new();
            {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                for c in data["channels"].as_array().into_iter().flatten() {
                    if let Some(g) = world.put_channel(c).and_then(|id| world.channels.get(&id).and_then(|c| c.guild.clone())) {
                        guilds.insert(g);
                    }
                }
            }
            for g in guilds {
                register_guild_channels(state, handle, &g);
            }
        }
        "THREAD_CREATE" | "THREAD_UPDATE" => {
            let channel = {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                let was_joined = data["id"].as_str().and_then(|id| world.channels.get(id)).is_some_and(|c| c.joined);
                let id = world.put_channel(data);
                if let Some(c) = id.as_ref().and_then(|id| world.channels.get_mut(id)) {
                    c.joined |= was_joined;
                }
                id.and_then(|id| world.channels.get(&id).cloned())
            };
            if let Some(c) = channel.filter(|c| c.joined) {
                redraw_guild(state, handle, c.guild.clone().as_deref(), Some(&c));
            }
        }
        "THREAD_DELETE" | "CHANNEL_DELETE" => {
            if let Some(id) = data["id"].as_str() {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).remove_channel(id);
                if let Some(buffer) = messages::buffer_of(state, account_id, id) {
                    state.runtime.remove_buffer(state, &buffer.id);
                }
            }
        }
        "GUILD_CREATE" | "GUILD_SYNC" | "GUILD_UPDATE" => {
            let id = {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                if kind == "GUILD_UPDATE" {
                    // Only the guild's own record: its roles, channels and members stay as they are.
                    let id = data["id"].as_str().map(str::to_string);
                    if let Some(g) = id.as_ref().and_then(|id| world.guilds.get_mut(id)) {
                        g.name = data["name"].as_str().unwrap_or(&g.name).to_string();
                        g.icon = data["icon"].as_str().map(str::to_string);
                        g.owner = data["owner_id"].as_str().unwrap_or(&g.owner).to_string();
                    }
                    id
                } else {
                    world.put_guild(data)
                }
            };
            if let Some(id) = id {
                let guild = handle.world.lock().unwrap_or_else(|e| e.into_inner()).guilds.get(&id).cloned();
                if let Some(guild) = guild {
                    register_guild(state, handle, &guild, 0);
                    register_guild_channels(state, handle, &id);
                    register_emoji(state, handle);
                }
            }
        }
        "GUILD_DELETE" => {
            if let Some(id) = data["id"].as_str() {
                let channels: Vec<String> = {
                    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                    let list = world.guilds.remove(id).map(|g| g.channels).unwrap_or_default();
                    for c in &list {
                        world.channels.remove(c);
                    }
                    list
                };
                for c in channels {
                    if let Some(buffer) = messages::buffer_of(state, account_id, &c) {
                        state.runtime.remove_buffer(state, &buffer.id);
                    }
                }
                state.runtime.remove_buffer_group(state, &rail_id(account_id, id));
            }
        }
        "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" | "GUILD_ROLE_DELETE" | "GUILD_ROLE_UPDATE_BULK" => {
            let Some(guild) = data["guild_id"].as_str() else { return };
            {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                let Some(g) = world.guilds.get_mut(guild) else { return };
                match kind {
                    "GUILD_ROLE_DELETE" => {
                        if let Some(r) = data["role_id"].as_str() {
                            g.roles.remove(r);
                        }
                    }
                    _ => {
                        let list: Vec<&Value> = if data["roles"].is_array() { data["roles"].as_array().into_iter().flatten().collect() } else { vec![&data["role"]] };
                        for r in list {
                            if let Some(rid) = r["id"].as_str() {
                                g.roles.insert(rid.to_string(), world::parse_role(r));
                            }
                        }
                    }
                }
            }
            register_guild_channels(state, handle, guild);
        }
        "GUILD_EMOJIS_UPDATE" => {
            let Some(guild) = data["guild_id"].as_str() else { return };
            {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                world.emojis.retain(|_, e| e.guild != guild);
                for e in data["emojis"].as_array().into_iter().flatten() {
                    if let Some(id) = e["id"].as_str() {
                        world.emojis.insert(id.to_string(), world::Emoji { name: e["name"].as_str().unwrap_or("emoji").to_string(), animated: e["animated"].as_bool().unwrap_or(false), guild: guild.to_string() });
                    }
                }
            }
            register_emoji(state, handle);
        }
        "GUILD_MEMBER_ADD" | "GUILD_MEMBER_UPDATE" => {
            let Some(guild) = data["guild_id"].as_str() else { return };
            let own = {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                world.put_member(guild, data);
                data["user"]["id"].as_str() == Some(world.own_id.as_str())
            };
            // The account's own roles or timeout changed what it may do.
            if own {
                register_guild_channels(state, handle, guild);
            }
        }
        "GUILD_MEMBER_REMOVE" => {
            if let (Some(guild), Some(user)) = (data["guild_id"].as_str(), data["user"]["id"].as_str()) {
                if let Some(g) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).guilds.get_mut(guild) {
                    g.members.remove(user);
                }
            }
        }
        "USER_UPDATE" => {
            if let Some((id, user)) = world::parse_user(data) {
                handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.insert(id, user);
            }
        }
        "RELATIONSHIP_ADD" | "RELATIONSHIP_UPDATE" | "RELATIONSHIP_REMOVE" => {
            let Some(id) = data["id"].as_str() else { return };
            {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                world.set_relationship(id, if kind == "RELATIONSHIP_REMOVE" { None } else { data["type"].as_i64() });
                if let Some((uid, user)) = world::parse_user(&data["user"]) {
                    world.users.insert(uid, user);
                }
            }
            super::people::tell_requests(state, handle);
            super::people::redraw_direct(state, handle, id);
        }
        "PASSIVE_UPDATES" => {
            // A passive guild's channels that have new messages, as a map of channel to newest message: marked unread,
            // the messages fetched when the conversation is opened.
            for s in data["voice_states"].as_array().into_iter().flatten() {
                super::voice::state_changed(state, handle, s);
            }
            for (id, last) in data["channels"].as_object().into_iter().flatten() {
                let Some(last) = last.as_str() else { continue };
                let seen = {
                    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(ch) = world.channels.get_mut(id) {
                        ch.last_message_id = Some(last.to_string());
                    }
                    world.read.get(id).and_then(|r| r.0.clone())
                };
                let newer = seen.is_none_or(|s| last.parse::<u64>().unwrap_or(0) > s.parse::<u64>().unwrap_or(0));
                if newer {
                    if let Some(buffer) = messages::buffer_of(state, account_id, id) {
                        if buffer.unread_seed == 0 {
                            state.runtime.set_buffer_unread(state, &buffer.id, 1, 0);
                        }
                    }
                }
            }
        }
        "CHANNEL_PINS_UPDATE" => {
            if let Some(buffer) = data["channel_id"].as_str().and_then(|c| messages::buffer_of(state, account_id, c)) {
                super::search::announce(state, handle, &buffer);
            }
        }
        "USER_SETTINGS_UPDATE" => super::own::settings_changed(state, handle, data),
        "USER_GUILD_SETTINGS_UPDATE" => {
            super::settings::keep(handle, data);
            super::settings::apply(state, handle);
        }
        "GUILD_MEMBER_LIST_UPDATE" => super::members::list_update(state, handle, data),
        "PRESENCE_UPDATE" => super::members::presence(state, handle, data),
        "PRESENCE_UPDATE_BULK" => {
            for p in data["presences"].as_array().into_iter().flatten() {
                super::members::presence(state, handle, p);
            }
        }
        "VOICE_STATE_UPDATE" => super::voice::state_changed(state, handle, data),
        "VOICE_SERVER_UPDATE" => super::voice::grant(handle, data),
        "CALL_CREATE" | "CALL_UPDATE" | "CALL_DELETE" => super::voice::call_changed(state, handle, kind, data),
        "SESSIONS_REPLACE" | "CHANNEL_PINS_ACK" | "GUILD_MEMBERS_CHUNK" => {}
        other => tracing::debug!("fluxer[{account_id}]: {other} not handled"),
    }
}
