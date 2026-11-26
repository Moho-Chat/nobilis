//! Looking after a guild: its name, description and picture, its channels and the categories they sit under, its roles
//! and what each may do, who may do what in one channel, its webhooks, and its audit log.
//!
//! It answers the same calls as Stoat's (the window's server settings were made for that), in the same shapes: the
//! window asks for everything at once (`overview`) and then makes one change at a time, asking again after each.
//! Fluxer's model differs underneath and is translated here: a heading is a category channel; "what everybody may do"
//! is the everyone role (whose id is the guild's); a role allows and never denies, so a role's deny list is empty; and
//! roles rank by position, higher being more powerful, where the window counts rank from the top.
//!
//! Permissions cross to the window by name, never as a mask: the window is JavaScript, whose bit operations stop at 32
//! bits and Fluxer's reach 54.

use super::messages;
use super::moderation::{colour_of, guild_of};
use super::world::{self, Kind};
use super::{media, Handle};
use crate::model::Buffer;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use reqwest::Method;
use serde_json::{json, Value};

/// Every permission: its bit, its name, what the window calls it, the heading it sits under, and whether it means
/// something on a channel. From `http-api/permissions.mdx`.
pub const PERMISSIONS: &[(&str, u32, &str, &str, bool)] = &[
    ("ADMINISTRATOR", 3, "Administrator (every permission, everywhere)", "Community", false),
    ("MANAGE_GUILD", 5, "Manage the community", "Community", false),
    ("MANAGE_CHANNELS", 4, "Manage channels", "Community", true),
    ("MANAGE_ROLES", 28, "Manage roles and channel permissions", "Community", true),
    ("MANAGE_EXPRESSIONS", 30, "Manage emoji and stickers", "Community", false),
    ("CREATE_EXPRESSIONS", 43, "Add emoji and stickers", "Community", false),
    ("VIEW_AUDIT_LOG", 7, "View the audit log", "Community", false),
    ("MANAGE_WEBHOOKS", 29, "Manage webhooks", "Community", true),
    ("KICK_MEMBERS", 1, "Kick members", "Members", false),
    ("BAN_MEMBERS", 2, "Ban members", "Members", false),
    ("MODERATE_MEMBERS", 40, "Time members out", "Members", false),
    ("CHANGE_NICKNAME", 26, "Change own nickname", "Members", false),
    ("MANAGE_NICKNAMES", 27, "Change others' nicknames", "Members", false),
    ("CREATE_INSTANT_INVITE", 0, "Make invites", "Text", true),
    ("VIEW_CHANNEL", 10, "See the channel", "Text", true),
    ("READ_MESSAGE_HISTORY", 16, "Read earlier messages", "Text", true),
    ("SEND_MESSAGES", 11, "Send messages", "Text", true),
    ("MANAGE_MESSAGES", 13, "Delete others' messages", "Text", true),
    ("PIN_MESSAGES", 51, "Pin messages", "Text", true),
    ("EMBED_LINKS", 14, "Send links that unfurl", "Text", true),
    ("ATTACH_FILES", 15, "Attach files", "Text", true),
    ("ADD_REACTIONS", 6, "React", "Text", true),
    ("USE_EXTERNAL_EMOJIS", 18, "Use emoji from elsewhere", "Text", true),
    ("USE_EXTERNAL_STICKERS", 37, "Use stickers from elsewhere", "Text", true),
    ("MENTION_EVERYONE", 17, "Mention everyone", "Text", true),
    ("SEND_TTS_MESSAGES", 12, "Send spoken messages", "Text", true),
    ("BYPASS_SLOWMODE", 52, "Ignore slowmode", "Text", true),
    ("CONNECT", 20, "Join calls", "Voice", true),
    ("SPEAK", 21, "Speak", "Voice", true),
    ("STREAM", 9, "Use a camera or share a screen", "Voice", true),
    ("USE_VAD", 25, "Speak without push-to-talk", "Voice", true),
    ("PRIORITY_SPEAKER", 8, "Priority speaker", "Voice", true),
    ("MUTE_MEMBERS", 22, "Mute others in a call", "Voice", true),
    ("DEAFEN_MEMBERS", 23, "Deafen others in a call", "Voice", true),
    ("MOVE_MEMBERS", 24, "Move others between calls", "Voice", true),
    ("UPDATE_RTC_REGION", 53, "Change a call's region", "Voice", true),
];

pub fn names_of(bits: u64) -> Vec<&'static str> {
    PERMISSIONS.iter().filter(|(_, bit, ..)| bits & (1u64 << bit) != 0).map(|(name, ..)| *name).collect()
}

/// The bits the names stand for; a name nobody knows is refused, not dropped.
pub fn bits_of(names: &[String]) -> Result<u64> {
    let mut bits = 0u64;
    for name in names {
        let (_, bit, ..) = PERMISSIONS.iter().find(|(n, ..)| n == name).ok_or_else(|| anyhow!("{name} is not a permission Fluxer has"))?;
        bits |= 1u64 << bit;
    }
    Ok(bits)
}

fn names_in(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

fn fail(e: super::api::ApiError) -> anyhow::Error {
    anyhow!("{e}")
}

/// A colour typed in the window (`#rrggbb`) as Fluxer's number; empty is none.
pub fn colour_number(text: &str) -> Option<u32> {
    let hex = text.trim().trim_start_matches('#');
    if hex.is_empty() {
        return Some(0);
    }
    (hex.len() == 6).then(|| u32::from_str_radix(hex, 16).ok()).flatten()
}

/// What the account may change in the guild, for the window to offer only that.
fn powers(handle: &Handle, guild: &str) -> Value {
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let Some(g) = world.guilds.get(guild) else { return json!({}) };
    // The guild-wide mask is the permissions in a channel with no overwrites.
    let probe = world::Channel { id: String::new(), kind: Kind::Text, guild: Some(guild.to_string()), name: String::new(), topic: None, position: 0, parent: None, overwrites: Vec::new(), nsfw: false, recipients: Vec::new(), icon: None, slowmode: 0, last_message_id: None, url: None, tags: Vec::new(), joined: false };
    let bits = world.permissions(&probe);
    let has = |b: u64| bits & b != 0;
    json!({
        "owner": g.owner == world.own_id,
        "server": has(world::MANAGE_GUILD),
        "channels": has(world::MANAGE_CHANNELS),
        "roles": has(world::MANAGE_ROLES),
        "permissions": has(world::MANAGE_ROLES),
        "webhooks": has(world::MANAGE_WEBHOOKS),
        "audit": has(world::VIEW_AUDIT_LOG),
        "nickname": g.owner == world.own_id || has(world::CHANGE_NICKNAME),
        "emojis": has(world::MANAGE_EXPRESSIONS) || has(world::CREATE_EXPRESSIONS),
        "manageEmojis": has(world::MANAGE_EXPRESSIONS),
    })
}

fn overwrite_lists(c: &Value, everyone: &str) -> (Vec<Value>, Value) {
    let mut roles = Vec::new();
    let mut default = json!({ "allow": [], "deny": [] });
    for o in c["permission_overwrites"].as_array().into_iter().flatten() {
        let (allow, deny) = (names_of(world::mask(&o["allow"])), names_of(world::mask(&o["deny"])));
        match (o["id"].as_str(), o["type"].as_i64()) {
            (Some(id), Some(0)) if id == everyone => default = json!({ "allow": allow, "deny": deny }),
            (Some(id), Some(0)) => roles.push(json!({ "roleId": id, "allow": allow, "deny": deny })),
            _ => {}
        }
    }
    (roles, default)
}

/// Everything the settings window shows about the guild a conversation is in.
pub async fn overview(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let guild = guild_of(handle, buffer)?;
    let (guild_path, channels_path, roles_path) = (format!("/guilds/{guild}"), format!("/guilds/{guild}/channels"), format!("/guilds/{guild}/roles"));
    let (fresh, channels, roles) = tokio::join!(handle.api.get(&guild_path, &[]), handle.api.get(&channels_path, &[]), handle.api.get(&roles_path, &[]));
    let (fresh, channels, roles) = (fresh.map_err(fail)?, channels.map_err(fail)?, roles.map_err(fail)?);
    let mut all: Vec<Value> = channels.as_array().cloned().unwrap_or_default();
    all.sort_by_key(|c| (c["position"].as_i64().unwrap_or(0), c["id"].as_str().and_then(|i| i.parse::<u64>().ok()).unwrap_or(0)));
    let categories: Vec<Value> = all
        .iter()
        .filter(|c| c["type"].as_i64() == Some(4))
        .map(|cat| {
            let id = cat["id"].as_str().unwrap_or("");
            let inside: Vec<&str> = all.iter().filter(|c| c["parent_id"].as_str() == Some(id)).filter_map(|c| c["id"].as_str()).collect();
            json!({ "id": id, "title": cat["name"], "channels": inside })
        })
        .collect();
    let listed: Vec<Value> = all
        .iter()
        .filter(|c| matches!(c["type"].as_i64(), Some(0 | 2 | 5)))
        .map(|c| {
            let (roles, default) = overwrite_lists(c, &guild);
            json!({
                "id": c["id"], "name": c["name"], "description": c["topic"].as_str().unwrap_or(""),
                "voice": c["type"].as_i64() == Some(2),
                "nsfw": c["nsfw"].as_bool().unwrap_or(false), "slowmode": c["rate_limit_per_user"].as_u64().unwrap_or(0),
                "category": c["parent_id"],
                "roles": roles, "default": default,
            })
        })
        .collect();
    let mut role_list: Vec<&Value> = roles.as_array().into_iter().flatten().filter(|r| r["id"].as_str() != Some(guild.as_str())).collect();
    role_list.sort_by_key(|r| -r["position"].as_i64().unwrap_or(0));
    let role_list: Vec<Value> = role_list
        .iter()
        .enumerate()
        .map(|(rank, r)| json!({ "id": r["id"], "name": r["name"], "colour": colour_of(r["color"].as_u64().unwrap_or(0) as u32), "hoist": r["hoist"].as_bool().unwrap_or(false), "rank": rank, "allow": names_of(world::mask(&r["permissions"])), "deny": [] }))
        .collect();
    let everyone = roles.as_array().into_iter().flatten().find(|r| r["id"].as_str() == Some(guild.as_str())).map(|r| world::mask(&r["permissions"])).unwrap_or(0);
    let icon = fresh["icon"].as_str().and_then(|h| media::asset_url(&handle.instance, "icons", &guild, h)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp")));
    let own_nick = {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.guilds.get(&guild).and_then(|g| g.members.get(&world.own_id)).and_then(|m| m.nick.clone()).unwrap_or_default()
    };
    Ok(json!({
        "serverId": guild,
        "name": fresh["name"], "description": fresh["description"].as_str().unwrap_or(""), "iconUrl": icon,
        "ownNick": own_nick,
        "can": powers(handle, &guild),
        "categories": categories,
        "channels": listed,
        "roles": role_list,
        "defaultPermissions": names_of(everyone),
        "permissions": PERMISSIONS.iter().map(|(name, _, label, group, on_channel)| json!({ "key": name, "label": label, "group": group, "channel": on_channel })).collect::<Vec<_>>(),
        "limits": { "channels": 500, "roles": 250 },
    }))
}

/// A picture file as Fluxer takes one in JSON: a data URI.
async fn data_uri(path: &str) -> Result<String> {
    let bytes = tokio::fs::read(path).await.map_err(|e| anyhow!("reading {path}: {e}"))?;
    let name = super::send::sent_name(path, false);
    Ok(format!("data:{};base64,{}", crate::backend::stoat::send::content_type_of(&name), base64::engine::general_purpose::STANDARD.encode(bytes)))
}

pub async fn edit_guild(handle: &Handle, buffer: &Buffer, name: Option<&str>, description: Option<&str>, icon: Option<&str>) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let mut body = json!({});
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        body["name"] = json!(name);
    }
    if let Some(d) = description.map(str::trim) {
        body["description"] = if d.is_empty() { Value::Null } else { json!(d) };
    }
    match icon {
        Some("") => body["icon"] = Value::Null,
        Some(path) => body["icon"] = json!(data_uri(path).await?),
        None => {}
    }
    handle.api.patch(&format!("/guilds/{guild}"), &body).await.map(|_| ()).map_err(fail)
}

pub async fn create_channel(handle: &Handle, buffer: &Buffer, name: &str, voice: bool) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let name = name.trim();
    if name.is_empty() {
        bail!("A channel needs a name");
    }
    handle.api.post(&format!("/guilds/{guild}/channels"), &json!({ "type": if voice { 2 } else { 0 }, "name": name })).await.map(|_| ()).map_err(fail)
}

pub async fn edit_channel(handle: &Handle, channel: &str, name: Option<&str>, topic: Option<&str>, nsfw: Option<bool>, slowmode: Option<u64>) -> Result<()> {
    let mut body = json!({});
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        body["name"] = json!(name);
    }
    if let Some(t) = topic.map(str::trim) {
        body["topic"] = if t.is_empty() { Value::Null } else { json!(t) };
    }
    if let Some(nsfw) = nsfw {
        body["nsfw"] = json!(nsfw);
    }
    if let Some(s) = slowmode {
        body["rate_limit_per_user"] = json!(s.min(6 * 3600));
    }
    handle.api.patch(&format!("/channels/{channel}"), &body).await.map(|_| ()).map_err(fail)
}

pub async fn delete_channel(handle: &Handle, channel: &str) -> Result<()> {
    handle.api.delete(&format!("/channels/{channel}")).await.map(|_| ()).map_err(fail)
}

/// The headings as the window lays them out - each with a title and its channels in order - made true: a heading not
/// there yet is made as a category channel, one renamed is renamed, one dropped is deleted, and every channel is put
/// under its heading in its place.
pub async fn set_categories(handle: &Handle, buffer: &Buffer, wanted: &Value) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let list = wanted.as_array().ok_or_else(|| anyhow!("the headings must be a list"))?;
    if list.iter().any(|c| c["title"].as_str().is_none_or(|t| t.trim().is_empty())) {
        bail!("Every heading needs a name");
    }
    let existing: Vec<Value> = handle.api.get(&format!("/guilds/{guild}/channels"), &[]).await.map_err(fail)?.as_array().cloned().unwrap_or_default();
    let is_category = |id: &str| existing.iter().any(|c| c["id"].as_str() == Some(id) && c["type"].as_i64() == Some(4));
    let mut positions: Vec<Value> = Vec::new();
    let mut kept: Vec<String> = Vec::new();
    for (at, heading) in list.iter().enumerate() {
        let title = heading["title"].as_str().unwrap_or("").trim().chars().take(100).collect::<String>();
        let id = match heading["id"].as_str().filter(|id| is_category(id)) {
            Some(id) => {
                if existing.iter().any(|c| c["id"].as_str() == Some(id) && c["name"].as_str() != Some(title.as_str())) {
                    handle.api.patch(&format!("/channels/{id}"), &json!({ "name": title })).await.map_err(fail)?;
                }
                id.to_string()
            }
            None => handle.api.post(&format!("/guilds/{guild}/channels"), &json!({ "type": 4, "name": title })).await.map_err(fail)?["id"].as_str().unwrap_or("").to_string(),
        };
        positions.push(json!({ "id": id, "position": at, "parent_id": null }));
        for (place, channel) in heading["channels"].as_array().into_iter().flatten().filter_map(Value::as_str).enumerate() {
            positions.push(json!({ "id": channel, "position": place, "parent_id": id }));
        }
        kept.push(id);
    }
    // A channel under a heading that is no longer listed under any is taken out from under it: "no heading" is a place
    // too. Without this a channel moved out of every heading stayed where it was.
    let placed: Vec<&str> = list.iter().flat_map(|h| h["channels"].as_array().into_iter().flatten().filter_map(Value::as_str)).collect();
    for c in existing.iter().filter(|c| c["type"].as_i64() != Some(4) && c["parent_id"].is_string()) {
        if let Some(id) = c["id"].as_str().filter(|id| !placed.contains(id)) {
            positions.push(json!({ "id": id, "position": c["position"], "parent_id": null }));
        }
    }
    handle.api.request(Method::PATCH, &format!("/guilds/{guild}/channels"), &[], Some(&json!(positions)), None).await.map_err(fail)?;
    for gone in existing.iter().filter(|c| c["type"].as_i64() == Some(4)).filter_map(|c| c["id"].as_str()).filter(|id| !kept.iter().any(|k| k == id)) {
        handle.api.delete(&format!("/channels/{gone}")).await.map_err(fail)?;
    }
    Ok(())
}

pub async fn create_role(handle: &Handle, buffer: &Buffer, name: &str) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    handle.api.post(&format!("/guilds/{guild}/roles"), &json!({ "name": name.trim(), "permissions": "0" })).await.map(|_| ()).map_err(fail)
}

pub async fn edit_role(handle: &Handle, buffer: &Buffer, role: &str, name: Option<&str>, colour: Option<&str>, hoist: Option<bool>) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let mut body = json!({});
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        body["name"] = json!(name);
    }
    if let Some(c) = colour {
        body["color"] = json!(colour_number(c).ok_or_else(|| anyhow!("a colour is written #rrggbb"))?);
    }
    if let Some(h) = hoist {
        body["hoist"] = json!(h);
    }
    handle.api.patch(&format!("/guilds/{guild}/roles/{role}"), &body).await.map(|_| ()).map_err(fail)
}

pub async fn delete_role(handle: &Handle, buffer: &Buffer, role: &str) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    handle.api.delete(&format!("/guilds/{guild}/roles/{role}")).await.map(|_| ()).map_err(fail)
}

/// The roles in the window's order, highest first, as Fluxer's positions (highest number first, above the everyone role).
pub fn positions_for(order: &[String]) -> Value {
    let n = order.len() as i64;
    json!(order.iter().enumerate().map(|(i, id)| json!({ "id": id, "position": n - i as i64 })).collect::<Vec<_>>())
}

pub async fn reorder_roles(handle: &Handle, buffer: &Buffer, order: &[String]) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    handle.api.request(Method::PATCH, &format!("/guilds/{guild}/roles"), &[], Some(&positions_for(order)), None).await.map(|_| ()).map_err(fail)
}

/// What a role allows (Fluxer's roles have no deny); with no role, what everybody may do (the everyone role).
pub async fn set_role_permissions(handle: &Handle, buffer: &Buffer, role: Option<&str>, allow: &[String]) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let role = role.unwrap_or(&guild).to_string();
    handle.api.patch(&format!("/guilds/{guild}/roles/{role}"), &json!({ "permissions": bits_of(allow)?.to_string() })).await.map(|_| ()).map_err(fail)
}

/// Who may do what in one channel, for a role or (with none) for everybody; both lists empty takes the overwrite off.
pub async fn set_channel_permissions(handle: &Handle, buffer: &Buffer, channel: &str, role: Option<&str>, allow: &[String], deny: &[String]) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let target = role.unwrap_or(&guild).to_string();
    let path = format!("/channels/{channel}/permissions/{target}");
    if allow.is_empty() && deny.is_empty() {
        return handle.api.delete(&path).await.map(|_| ()).map_err(fail);
    }
    handle.api.put(&path, Some(&json!({ "type": 0, "allow": bits_of(allow)?.to_string(), "deny": bits_of(deny)?.to_string() }))).await.map(|_| ()).map_err(fail)
}

/// One audit log entry in words.
pub fn describe(world: &world::World, guild: &str, e: &Value) -> String {
    let who = e["user_id"].as_str().map(|u| if u == "0" { "Fluxer".to_string() } else { world.name_in(Some(guild), u) }).unwrap_or_else(|| "someone".to_string());
    let target = e["target_id"].as_str().unwrap_or("");
    let person = || world.name_in(Some(guild), target);
    let channel = |id: &str| world.channels.get(id).map(|c| format!("#{}", c.name)).unwrap_or_else(|| "a channel".to_string());
    // The name a thing had, from the entry's own record of what changed: the only place a deleted one is still named.
    let named = || {
        e["changes"].as_array().into_iter().flatten().find(|c| c["key"].as_str() == Some("name")).and_then(|c| c["new_value"].as_str().or(c["old_value"].as_str())).map(str::to_string)
    };
    let role = || e["options"]["role_name"].as_str().map(str::to_string).or_else(|| world.guilds.get(guild).and_then(|g| g.roles.get(target)).map(|r| r.name.clone())).or_else(named).unwrap_or_else(|| "a role".to_string());
    let called = |what: &str| named().map(|n| format!("the {what} {n}")).unwrap_or_else(|| format!("{} {what}", if what.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" }));
    let what = match e["action_type"].as_i64().unwrap_or(0) {
        1 => "changed the community's settings".to_string(),
        10 => format!("made {}", channel(target)),
        11 => format!("changed {}", channel(target)),
        12 => named().map(|n| format!("deleted #{n}")).unwrap_or_else(|| "deleted a channel".to_string()),
        13..=15 => format!("changed who may do what in {}", channel(e["options"]["channel_id"].as_str().unwrap_or(target))),
        20 => format!("kicked {}", person()),
        22 => format!("banned {}", person()),
        23 => format!("lifted the ban on {}", person()),
        24 => format!("changed {}", person()),
        25 => format!("changed {}'s roles ({})", person(), role()),
        26 => format!("moved {} to another call", person()),
        27 => format!("took {} out of a call", person()),
        30 => format!("made the role {}", role()),
        31 => format!("changed the role {}", role()),
        32 => format!("deleted the role {}", role()),
        40 => "made an invite".to_string(),
        41 => "changed an invite".to_string(),
        42 => "deleted an invite".to_string(),
        50 => format!("made {}", called("webhook")),
        51 => format!("changed {}", called("webhook")),
        52 => format!("deleted {}", called("webhook")),
        60 => format!("added {}", called("emoji")),
        61 => format!("changed {}", called("emoji")),
        62 => format!("removed {}", called("emoji")),
        72 => format!("deleted a message in {}", channel(e["options"]["channel_id"].as_str().unwrap_or(""))),
        73 => format!("deleted {} messages in {}", e["options"]["count"].as_str().or(e["options"]["count"].as_u64().map(|_| "")).unwrap_or("some"), channel(e["options"]["channel_id"].as_str().unwrap_or(""))),
        74 => format!("pinned a message in {}", channel(e["options"]["channel_id"].as_str().unwrap_or(""))),
        75 => format!("unpinned a message in {}", channel(e["options"]["channel_id"].as_str().unwrap_or(""))),
        90 => format!("added {}", called("sticker")),
        91 => format!("changed {}", called("sticker")),
        92 => format!("removed {}", called("sticker")),
        110 => format!("started {}", called("thread")),
        111 => format!("changed {}", called("thread")),
        112 => format!("deleted {}", called("thread")),
        n => format!("did something Fluxer calls {n}"),
    };
    let reason = e["reason"].as_str().filter(|r| !r.is_empty()).map(|r| format!(" ({r})")).unwrap_or_default();
    format!("{who} {what}{reason}")
}

pub async fn audit_log(handle: &Handle, buffer: &Buffer, before: Option<&str>) -> Result<Value> {
    let guild = guild_of(handle, buffer)?;
    let mut query = vec![("limit", "50".to_string())];
    if let Some(b) = before.filter(|b| b.parse::<u64>().is_ok()) {
        query.push(("before", b.to_string()));
    }
    let answer = handle.api.get(&format!("/guilds/{guild}/audit-logs"), &query).await.map_err(fail)?;
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        for u in answer["users"].as_array().into_iter().flatten() {
            if let Some((id, user)) = world::parse_user(u) {
                world.users.entry(id).or_insert(user);
            }
        }
    }
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let entries: Vec<Value> = answer["audit_log_entries"].as_array().into_iter().flatten().map(|e| json!({ "id": e["id"], "text": describe(&world, &guild, e), "ts": e["id"].as_str().and_then(messages::snowflake_secs).unwrap_or(0) })).collect();
    Ok(json!({ "entries": entries }))
}

/// The webhooks of a channel, each with the address it takes posts at.
pub async fn webhooks(handle: &Handle, channel: &str) -> Result<Value> {
    let answer = handle.api.get(&format!("/channels/{channel}/webhooks"), &[]).await.map_err(fail)?;
    let base = handle.api.base();
    let list: Vec<Value> = answer.as_array().into_iter().flatten().map(|w| json!({ "id": w["id"], "name": w["name"], "token": w["token"], "url": w["token"].as_str().map(|t| format!("{base}/webhooks/{}/{t}", w["id"].as_str().unwrap_or(""))) })).collect();
    Ok(json!({ "webhooks": list }))
}

/// Runs one action of the settings window; answers what it shows afterwards.
/// The community's invites, newest first: who made each, where it leads, how often it was used and when it ends.
pub async fn invites(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let guild = guild_of(handle, buffer)?;
    let list = handle.api.get(&format!("/guilds/{guild}/invites"), &[]).await.map_err(fail)?;
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let rows: Vec<Value> = list
        .as_array()
        .into_iter()
        .flatten()
        .map(|i| {
            let channel = i["channel"]["id"].as_str().and_then(|c| world.channels.get(c)).map(|c| c.name.clone()).or_else(|| i["channel"]["name"].as_str().map(str::to_string)).unwrap_or_default();
            let by = i["inviter"]["id"].as_str().map(|u| world.name_in(Some(&guild), u)).unwrap_or_default();
            json!({
                "code": i["code"], "link": format!("{}/{}", handle.instance.invite.trim_end_matches('/'), i["code"].as_str().unwrap_or("")),
                "channel": channel, "by": by, "uses": i["uses"], "maxUses": i["max_uses"], "maxAge": i["max_age"],
                "createdAt": i["created_at"], "temporary": i["temporary"],
            })
        })
        .collect();
    Ok(json!({ "invites": rows }))
}

/// Who is banned from the community, and why when a reason was given.
pub async fn bans(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let guild = guild_of(handle, buffer)?;
    let list = handle.api.get(&format!("/guilds/{guild}/bans"), &[]).await.map_err(fail)?;
    let rows: Vec<Value> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| {
            let user = &b["user"];
            let id = user["id"].as_str()?;
            let name = user["global_name"].as_str().filter(|n| !n.is_empty()).or(user["username"].as_str()).unwrap_or(id);
            Some(json!({ "userId": id, "name": name, "reason": b["reason"] }))
        })
        .collect();
    Ok(json!({ "bans": rows }))
}

/// Sets or clears this account's own nickname in the community (`PATCH /guilds/{id}/members/@me`).
pub async fn set_nickname(handle: &Handle, buffer: &Buffer, nick: &str) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let trimmed = nick.trim();
    let value = if trimmed.is_empty() { Value::Null } else { json!(trimmed.chars().take(32).collect::<String>()) };
    handle.api.patch(&format!("/guilds/{guild}/members/@me"), &json!({ "nick": value })).await.map_err(fail)?;
    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let own = world.own_id.clone();
    if let Some(m) = world.guilds.get_mut(&guild).and_then(|g| g.members.get_mut(&own)) {
        m.nick = if trimmed.is_empty() { None } else { Some(trimmed.chars().take(32).collect()) };
    }
    Ok(())
}

/// Custom emoji in this community.
pub async fn emojis(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let guild = guild_of(handle, buffer)?;
    let list = handle.api.get(&format!("/guilds/{guild}/emojis"), &[]).await.map_err(fail)?;
    let rows: Vec<Value> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let id = e["id"].as_str()?;
            let name = e["name"].as_str().unwrap_or("emoji");
            let animated = e["animated"].as_bool().unwrap_or(false);
            let url = media::emoji_url(&handle.instance, id, animated).map(|u| media::cached(&handle.account_id, media::MEDIA, &u, None, Some(if animated { "image/gif" } else { "image/webp" })));
            Some(json!({ "id": id, "name": name, "animated": animated, "url": url }))
        })
        .collect();
    Ok(json!({ "emojis": rows }))
}

/// Uploads a custom emoji to the community (`POST /guilds/{id}/emojis`).
pub async fn create_emoji(handle: &Handle, buffer: &Buffer, name: &str, path: &str) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    let name: String = name.trim().chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').take(32).collect();
    if name.len() < 2 {
        bail!("An emoji name needs at least two letters, digits or underscores");
    }
    let image = data_uri(path).await?;
    handle.api.post(&format!("/guilds/{guild}/emojis"), &json!({ "name": name, "image": image })).await.map(|_| ()).map_err(fail)
}

/// Deletes a custom emoji from the community (`DELETE /guilds/{id}/emojis/{emoji}`).
pub async fn delete_emoji(handle: &Handle, buffer: &Buffer, emoji_id: &str) -> Result<()> {
    let guild = guild_of(handle, buffer)?;
    handle.api.delete(&format!("/guilds/{guild}/emojis/{emoji_id}")).await.map(|_| ()).map_err(fail)
}

pub async fn run(handle: &Handle, buffer: &Buffer, action: &str, p: &Value) -> Result<Value> {
    let text = |k: &str| p[k].as_str();
    let id = |k: &str| p[k].as_str().filter(|s| !s.is_empty()).ok_or_else(|| anyhow!("{action} needs \"{k}\""));
    match action {
        "overview" => return overview(handle, buffer).await,
        "auditLog" => return audit_log(handle, buffer, text("before")).await,
        "webhooks" => return webhooks(handle, id("channelId")?).await,
        "editServer" => edit_guild(handle, buffer, text("name"), text("description"), text("icon")).await?,
        "createChannel" => create_channel(handle, buffer, text("name").unwrap_or(""), p["voice"].as_bool().unwrap_or(false)).await?,
        "editChannel" => edit_channel(handle, id("channelId")?, text("name"), text("description"), p["nsfw"].as_bool(), p["slowmode"].as_u64()).await?,
        "deleteChannel" => delete_channel(handle, id("channelId")?).await?,
        "setCategories" => set_categories(handle, buffer, &p["categories"]).await?,
        "createRole" => create_role(handle, buffer, text("name").unwrap_or("")).await?,
        "editRole" => edit_role(handle, buffer, id("roleId")?, text("name"), text("colour"), p["hoist"].as_bool()).await?,
        "deleteRole" => delete_role(handle, buffer, id("roleId")?).await?,
        "reorderRoles" => reorder_roles(handle, buffer, &names_in(&p["order"])).await?,
        "setRolePermissions" => set_role_permissions(handle, buffer, text("roleId").filter(|r| !r.is_empty()), &names_in(&p["allow"])).await?,
        "setChannelPermissions" => set_channel_permissions(handle, buffer, id("channelId")?, text("roleId").filter(|r| !r.is_empty()), &names_in(&p["allow"]), &names_in(&p["deny"])).await?,
        "invites" => return invites(handle, buffer).await,
        "bans" => return bans(handle, buffer).await,
        "emojis" => return emojis(handle, buffer).await,
        "setNickname" => set_nickname(handle, buffer, text("nick").unwrap_or("")).await?,
        "createEmoji" => create_emoji(handle, buffer, text("name").unwrap_or(""), id("path")?).await?,
        "deleteEmoji" => delete_emoji(handle, buffer, id("emojiId")?).await?,
        "deleteInvite" => {
            handle.api.delete(&format!("/invites/{}", id("code")?)).await.map_err(fail)?;
        }
        "createWebhook" => {
            handle.api.post(&format!("/channels/{}/webhooks", id("channelId")?), &json!({ "name": text("name").unwrap_or("").trim() })).await.map_err(fail)?;
        }
        "deleteWebhook" => {
            handle.api.delete(&format!("/webhooks/{}", id("webhookId")?)).await.map_err(fail)?;
        }
        other => bail!("no such thing to do to a community: {other}"),
    }
    Ok(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_cross_by_name_and_an_unknown_name_is_refused() {
        let bits = bits_of(&["SEND_MESSAGES".into(), "PIN_MESSAGES".into()]).unwrap();
        assert_eq!(bits, (1 << 11) | (1 << 51));
        assert_eq!(names_of(bits), vec!["SEND_MESSAGES", "PIN_MESSAGES"]);
        assert!(bits_of(&["FLY".into()]).is_err());
    }

    #[test]
    fn roles_in_the_windows_order_become_positions_highest_first() {
        assert_eq!(positions_for(&["A".into(), "B".into()]), json!([{ "id": "A", "position": 2 }, { "id": "B", "position": 1 }]));
    }

    #[test]
    fn audit_lines_name_what_was_made_or_deleted_from_its_changes() {
        let world = world::World::default();
        let entry = |action: i64, changes: Value| json!({ "user_id": "0", "target_id": "9", "action_type": action, "changes": changes });
        let named = json!([{ "key": "name", "new_value": "Test hook" }]);
        assert_eq!(describe(&world, "1", &entry(50, named.clone())), "Fluxer made the webhook Test hook");
        assert_eq!(describe(&world, "1", &entry(52, json!([{ "key": "name", "old_value": "Test hook" }]))), "Fluxer deleted the webhook Test hook");
        assert_eq!(describe(&world, "1", &entry(51, Value::Null)), "Fluxer changed a webhook");
        assert_eq!(describe(&world, "1", &entry(60, Value::Null)), "Fluxer added an emoji");
        assert_eq!(describe(&world, "1", &entry(32, json!([{ "key": "name", "old_value": "Helpers" }]))), "Fluxer deleted the role Helpers");
        assert_eq!(describe(&world, "1", &entry(12, json!([{ "key": "name", "old_value": "old-room" }]))), "Fluxer deleted #old-room");
    }

    #[test]
    fn a_colour_is_read_from_the_window_and_empty_is_none() {
        assert_eq!(colour_number("#ff8800"), Some(0xff8800));
        assert_eq!(colour_number(""), Some(0));
        assert_eq!(colour_number("red"), None);
    }
}
