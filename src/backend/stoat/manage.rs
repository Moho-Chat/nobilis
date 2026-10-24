//! Looking after a server: its name and picture, its channels and the headings they sit under, its roles and what
//! each may do, who may do what in one channel, its webhooks, and the record of what its moderators did.
//!
//! The window asks for all of it at once when the settings are opened (`overview`) and then makes one change at a time;
//! after each it asks again, so what it shows is what Stoat holds and not what it hoped. What the account may do is
//! worked out from its own permissions, to offer only that; Stoat checks again and says why in words if it refuses.
//!
//! Permissions cross the boundary by name (`ViewChannel`, `SendMessage`), not as the bit sets Stoat keeps: the window
//! is JavaScript, whose whole numbers stop being exact at 2^53 and whose bit operations stop at 32 bits.

use super::messages;
use super::world::{self, Server};
use super::{media, Handle};
use crate::model::Buffer;
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};

/// Every permission: its bit, its name, what the window calls it, the heading it sits under, and whether it means
/// something on a channel (a server's roles and defaults carry all of them; a channel carries only these).
pub const PERMISSIONS: &[(&str, u32, &str, &str, bool)] = &[
    ("ManageChannel", 0, "Manage channels", "Server", true),
    ("ManageServer", 1, "Manage the server", "Server", false),
    ("ManagePermissions", 2, "Manage permissions", "Server", true),
    ("ManageRole", 3, "Manage roles", "Server", false),
    ("ManageCustomisation", 4, "Manage emoji", "Server", false),
    ("ViewAuditLogs", 40, "View the audit log", "Server", false),
    ("KickMembers", 6, "Kick members", "Members", false),
    ("BanMembers", 7, "Ban members", "Members", false),
    ("TimeoutMembers", 8, "Time members out", "Members", false),
    ("AssignRoles", 9, "Give roles", "Members", false),
    ("ChangeNickname", 10, "Change own nickname", "Members", false),
    ("ManageNicknames", 11, "Change others' nicknames", "Members", false),
    ("ChangeAvatar", 12, "Change own server picture", "Members", false),
    ("RemoveAvatars", 13, "Remove others' server pictures", "Members", false),
    ("ViewChannel", 20, "See the channel", "Text", true),
    ("ReadMessageHistory", 21, "Read earlier messages", "Text", true),
    ("SendMessage", 22, "Send messages", "Text", true),
    ("ManageMessages", 23, "Delete others' messages and pin", "Text", true),
    ("ManageWebhooks", 24, "Manage webhooks", "Text", true),
    ("InviteOthers", 25, "Make invites", "Text", true),
    ("SendEmbeds", 26, "Send links that unfurl", "Text", true),
    ("UploadFiles", 27, "Attach files", "Text", true),
    ("Masquerade", 28, "Send under another name and picture", "Text", true),
    ("React", 29, "React", "Text", true),
    ("BypassSlowmode", 39, "Ignore slowmode", "Text", true),
    ("MentionEveryone", 37, "Mention everyone", "Text", true),
    ("MentionRoles", 38, "Mention roles", "Text", true),
    ("Connect", 30, "Join calls", "Voice", true),
    ("Speak", 31, "Speak", "Voice", true),
    ("Video", 32, "Use a camera or share a screen", "Voice", true),
    ("Listen", 36, "Hear a call", "Voice", true),
    ("MuteMembers", 33, "Mute others in a call", "Voice", true),
    ("DeafenMembers", 34, "Deafen others in a call", "Voice", true),
    ("MoveMembers", 35, "Move others between calls", "Voice", true),
];

/// The names of the permissions a set of bits holds.
pub fn names_of(bits: u64) -> Vec<&'static str> {
    PERMISSIONS.iter().filter(|(_, bit, ..)| bits & (1u64 << bit) != 0).map(|(name, ..)| *name).collect()
}

/// The bits the names stand for; a name nobody knows is refused, not dropped.
pub fn bits_of(names: &[String]) -> Result<u64> {
    let mut bits = 0u64;
    for name in names {
        let (_, bit, ..) = PERMISSIONS.iter().find(|(n, ..)| n == name).ok_or_else(|| anyhow!("{name} is not a permission Stoat has"))?;
        bits |= 1u64 << bit;
    }
    Ok(bits)
}

fn names_in(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(str::to_string)).collect()
}

fn server_of(handle: &Handle, buffer: &Buffer) -> Result<Server> {
    let channel = buffer.remote_id.as_deref().ok_or_else(|| anyhow!("this is not a Stoat conversation"))?;
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    world.channels.get(channel).and_then(|c| c.server.as_ref()).and_then(|s| world.servers.get(s)).cloned().ok_or_else(|| anyhow!("this conversation is not in a server"))
}

fn fail(e: super::api::ApiError) -> anyhow::Error {
    anyhow!("{e}")
}

/// What the account may do to the server, for the window to offer only that.
fn powers(handle: &Handle, server: &Server) -> Value {
    let bits = handle.world.lock().unwrap_or_else(|e| e.into_inner()).server_permissions(server);
    let has = |name: &str| bits & PERMISSIONS.iter().find(|(n, ..)| *n == name).map(|(_, b, ..)| 1u64 << b).unwrap_or(0) != 0;
    json!({
        "owner": server.owner == handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id,
        "server": has("ManageServer"),
        "channels": has("ManageChannel"),
        "roles": has("ManageRole"),
        "permissions": has("ManagePermissions"),
        "webhooks": has("ManageWebhooks"),
        "audit": has("ViewAuditLogs"),
    })
}

/// Everything the settings window shows about the server a conversation is in.
pub async fn overview(handle: &Handle, buffer: &Buffer) -> Result<Value> {
    let server = server_of(handle, buffer)?;
    let fresh = handle.api.get(&format!("/servers/{}", server.id), &[("include_channels", "true".to_string())]).await.map_err(fail)?;
    let categories = if fresh["categories"].is_array() { fresh["categories"].clone() } else { json!([]) };
    let category_of = |channel: &str| categories.as_array().into_iter().flatten().find(|c| c["channels"].as_array().is_some_and(|l| l.iter().any(|x| x.as_str() == Some(channel)))).and_then(|c| c["id"].as_str().map(str::to_string));
    let channels: Vec<Value> = fresh["channels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c.is_object())
        .map(|c| {
            json!({
                "id": c["_id"], "name": c["name"], "description": c["description"].as_str().unwrap_or(""),
                "voice": c.get("voice").is_some_and(|v| v.is_object()) || c["channel_type"] == "VoiceChannel",
                "nsfw": c["nsfw"].as_bool().unwrap_or(false), "slowmode": c["slowmode"].as_u64().unwrap_or(0),
                "category": category_of(c["_id"].as_str().unwrap_or("")),
                "roles": c["role_permissions"].as_object().map(|o| o.iter().map(|(role, p)| json!({ "roleId": role, "allow": names_of(p["a"].as_u64().unwrap_or(0)), "deny": names_of(p["d"].as_u64().unwrap_or(0)) })).collect::<Vec<_>>()).unwrap_or_default(),
                "default": { "allow": names_of(c["default_permissions"]["a"].as_u64().unwrap_or(0)), "deny": names_of(c["default_permissions"]["d"].as_u64().unwrap_or(0)) },
            })
        })
        .collect();
    let mut roles: Vec<Value> = fresh["roles"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(id, r)| json!({ "id": id, "name": r["name"], "colour": r["colour"], "hoist": r["hoist"].as_bool().unwrap_or(false), "rank": r["rank"].as_i64().unwrap_or(0), "allow": names_of(r["permissions"]["a"].as_u64().unwrap_or(0)), "deny": names_of(r["permissions"]["d"].as_u64().unwrap_or(0)) }))
        .collect();
    roles.sort_by_key(|r| r["rank"].as_i64().unwrap_or(0));
    let icon = fresh["icon"]["_id"].as_str().and_then(|f| media::file_url(&handle.instance, "icons", f)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, None));
    Ok(json!({
        "serverId": server.id,
        "name": fresh["name"], "description": fresh["description"].as_str().unwrap_or(""), "iconUrl": icon,
        "can": powers(handle, &server),
        "categories": categories,
        "channels": channels,
        "roles": roles,
        "defaultPermissions": names_of(fresh["default_permissions"].as_u64().unwrap_or(0)),
        "permissions": PERMISSIONS.iter().map(|(name, _, label, group, on_channel)| json!({ "key": name, "label": label, "group": group, "channel": on_channel })).collect::<Vec<_>>(),
        "limits": { "channels": handle.instance.server_channels, "roles": handle.instance.server_roles },
    }))
}

/// Changes the server's name or description, or its picture (`icon` a file; empty takes it off).
pub async fn edit_server(handle: &Handle, buffer: &Buffer, name: Option<&str>, description: Option<&str>, icon: Option<&str>) -> Result<()> {
    let server = server_of(handle, buffer)?;
    let mut body = json!({});
    let mut remove: Vec<&str> = Vec::new();
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        body["name"] = json!(name);
    }
    match description.map(str::trim) {
        Some("") => remove.push("Description"),
        Some(d) => body["description"] = json!(d),
        None => {}
    }
    match icon {
        Some("") => remove.push("Icon"),
        Some(path) => {
            let size = std::fs::metadata(path).map_err(|e| anyhow!("{path}: {e}"))?.len();
            super::limits::may_use_picture("icon", size, handle.limits()).map_err(|e| anyhow!(e))?;
            let host = handle.instance.autumn.clone().ok_or_else(|| anyhow!("this instance has no file server"))?;
            let file = super::send::sent_name(path, false);
            let bytes = tokio::fs::read(path).await.map_err(|e| anyhow!("reading {path}: {e}"))?;
            body["icon"] = json!(handle.api.upload(&host, "icons", &file, super::send::content_type_of(&file), bytes).await.map_err(fail)?);
        }
        None => {}
    }
    if !remove.is_empty() {
        body["remove"] = json!(remove);
    }
    handle.api.patch(&format!("/servers/{}", server.id), &body).await.map(|_| ()).map_err(fail)
}

/// Makes a channel, text or voice, at the end of the server's list.
pub async fn create_channel(handle: &Handle, buffer: &Buffer, name: &str, voice: bool) -> Result<()> {
    let server = server_of(handle, buffer)?;
    if server.channels.len() >= handle.instance.server_channels {
        bail!("A server may have at most {} channels on this instance, and this one has {}", handle.instance.server_channels, server.channels.len());
    }
    let name = name.trim();
    if name.is_empty() {
        bail!("A channel needs a name");
    }
    handle.api.post(&format!("/servers/{}/channels", server.id), &json!({ "type": if voice { "Voice" } else { "Text" }, "name": name })).await.map(|_| ()).map_err(fail)
}

/// Changes a channel's name, description, age-restriction or slowmode (seconds; none takes it off).
pub async fn edit_channel(handle: &Handle, channel: &str, name: Option<&str>, description: Option<&str>, nsfw: Option<bool>, slowmode: Option<u64>) -> Result<()> {
    let mut body = json!({});
    let mut remove: Vec<&str> = Vec::new();
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        body["name"] = json!(name);
    }
    match description.map(str::trim) {
        Some("") => remove.push("Description"),
        Some(d) => body["description"] = json!(d),
        None => {}
    }
    if let Some(nsfw) = nsfw {
        body["nsfw"] = json!(nsfw);
    }
    match slowmode {
        Some(0) => remove.push("Slowmode"),
        Some(s) => body["slowmode"] = json!(s.min(6 * 3600)),
        None => {}
    }
    if !remove.is_empty() {
        body["remove"] = json!(remove);
    }
    handle.api.patch(&format!("/channels/{channel}"), &body).await.map(|_| ()).map_err(fail)
}

pub async fn delete_channel(handle: &Handle, channel: &str) -> Result<()> {
    handle.api.delete(&format!("/channels/{channel}")).await.map(|_| ()).map_err(fail)
}

/// Sets the headings and which channels sit under each, whole.
pub async fn set_categories(handle: &Handle, buffer: &Buffer, categories: &Value) -> Result<()> {
    let server = server_of(handle, buffer)?;
    let list: Vec<Value> = categories
        .as_array()
        .ok_or_else(|| anyhow!("the headings must be a list"))?
        .iter()
        .map(|c| json!({ "id": c["id"], "title": c["title"].as_str().unwrap_or("").chars().take(32).collect::<String>(), "channels": c["channels"] }))
        .collect();
    if list.iter().any(|c| c["id"].as_str().is_none_or(str::is_empty) || c["title"].as_str().is_none_or(|t| t.trim().is_empty())) {
        bail!("Every heading needs a name");
    }
    handle.api.patch(&format!("/servers/{}", server.id), &json!({ "categories": list })).await.map(|_| ()).map_err(fail)
}

pub async fn create_role(handle: &Handle, buffer: &Buffer, name: &str) -> Result<()> {
    let server = server_of(handle, buffer)?;
    if server.roles.len() >= handle.instance.server_roles {
        bail!("A server may have at most {} roles on this instance, and this one has {}", handle.instance.server_roles, server.roles.len());
    }
    handle.api.post(&format!("/servers/{}/roles", server.id), &json!({ "name": name.trim() })).await.map(|_| ()).map_err(fail)
}

/// Changes a role's name, colour (empty takes it off) or whether it is listed apart.
pub async fn edit_role(handle: &Handle, buffer: &Buffer, role: &str, name: Option<&str>, colour: Option<&str>, hoist: Option<bool>) -> Result<()> {
    let server = server_of(handle, buffer)?;
    let mut body = json!({});
    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        body["name"] = json!(name);
    }
    match colour.map(str::trim) {
        Some("") => body["remove"] = json!(["Colour"]),
        Some(c) => body["colour"] = json!(c),
        None => {}
    }
    if let Some(h) = hoist {
        body["hoist"] = json!(h);
    }
    handle.api.patch(&format!("/servers/{}/roles/{role}", server.id), &body).await.map(|_| ()).map_err(fail)
}

pub async fn delete_role(handle: &Handle, buffer: &Buffer, role: &str) -> Result<()> {
    let server = server_of(handle, buffer)?;
    handle.api.delete(&format!("/servers/{}/roles/{role}", server.id)).await.map(|_| ()).map_err(fail)
}

/// Puts the roles in this order, the most powerful first.
pub async fn reorder_roles(handle: &Handle, buffer: &Buffer, order: &[String]) -> Result<()> {
    let server = server_of(handle, buffer)?;
    handle.api.patch(&format!("/servers/{}/roles/ranks", server.id), &json!({ "ranks": order })).await.map(|_| ()).map_err(fail)
}

/// What a role allows and denies server-wide; with no role, what everybody is allowed.
pub async fn set_role_permissions(handle: &Handle, buffer: &Buffer, role: Option<&str>, allow: &[String], deny: &[String]) -> Result<()> {
    let server = server_of(handle, buffer)?;
    match role {
        Some(role) => handle.api.put(&format!("/servers/{}/permissions/{role}", server.id), Some(&json!({ "permissions": { "allow": bits_of(allow)?, "deny": bits_of(deny)? } }))).await,
        None => handle.api.put(&format!("/servers/{}/permissions/default", server.id), Some(&json!({ "permissions": bits_of(allow)? }))).await,
    }
    .map(|_| ())
    .map_err(fail)
}

/// What a role (or everybody) is allowed and denied in one channel.
pub async fn set_channel_permissions(handle: &Handle, channel: &str, role: Option<&str>, allow: &[String], deny: &[String]) -> Result<()> {
    let permissions = json!({ "allow": bits_of(allow)?, "deny": bits_of(deny)? });
    match role {
        Some(role) => handle.api.put(&format!("/channels/{channel}/permissions/{role}"), Some(&json!({ "permissions": permissions }))).await,
        None => handle.api.put(&format!("/channels/{channel}/permissions/default"), Some(&json!({ "permissions": permissions }))).await,
    }
    .map(|_| ())
    .map_err(fail)
}

/// What a recorded action says, in words.
pub fn describe(world: &world::World, entry: &Value) -> String {
    let who = world.name_of(entry["user"].as_str().unwrap_or(""));
    let target = entry["target"].as_str().map(|t| world.name_of(t));
    let action = &entry["action"];
    let channel = |id: &Value| id.as_str().and_then(|c| world.channels.get(c)).map(|c| format!("#{}", c.name)).unwrap_or_else(|| "a channel".to_string());
    let name_of = |id: &Value| world.name_of(id.as_str().unwrap_or(""));
    let text = match action["type"].as_str().unwrap_or("") {
        "MessageDelete" => format!("deleted a message by {} in {}", name_of(&action["author"]), channel(&action["channel"])),
        "MessageBulkDelete" => format!("deleted {} messages in {}", action["count"].as_u64().unwrap_or(0), channel(&action["channel"])),
        "MessagePin" => format!("pinned a message by {} in {}", name_of(&action["author"]), channel(&action["channel"])),
        "MessageUnpin" => format!("unpinned a message by {} in {}", name_of(&action["author"]), channel(&action["channel"])),
        "BanCreate" => format!("banned {}", name_of(&action["user"])),
        "BanDelete" => format!("let {} back in", name_of(&action["user"])),
        "ChannelCreate" => format!("made the channel {}", action["name"].as_str().unwrap_or("")),
        "ChannelEdit" => format!("changed {}", channel(&action["channel"])),
        "ChannelRolePermissionsEdit" => format!("changed who may do what in {}", channel(&action["channel"])),
        "ChannelDelete" => format!("deleted the channel {}", action["name"].as_str().unwrap_or("")),
        "MemberEdit" => format!("changed {}", name_of(&action["user"])),
        "MemberKick" => format!("kicked {}", name_of(&action["user"])),
        "ServerEdit" => "changed the server".to_string(),
        "RoleEdit" => "changed a role".to_string(),
        "RoleCreate" => format!("made the role {}", action["name"].as_str().unwrap_or("")),
        "RoleDelete" => format!("deleted the role {}", action["name"].as_str().unwrap_or("")),
        "RolesReorder" => "put the roles in a new order".to_string(),
        "InviteCreate" => format!("made an invite to {}", channel(&action["channel"])),
        "InviteDelete" => format!("deleted an invite to {}", channel(&action["channel"])),
        "WebhookCreate" => format!("made the webhook {} in {}", action["name"].as_str().unwrap_or(""), channel(&action["channel"])),
        "WebhookDelete" => format!("deleted the webhook {} in {}", action["name"].as_str().unwrap_or(""), channel(&action["channel"])),
        "EmojiCreate" => format!("added the emoji {}", action["name"].as_str().unwrap_or("")),
        "EmojiUpdate" => "changed an emoji".to_string(),
        "EmojiDelete" => format!("removed the emoji {}", action["name"].as_str().unwrap_or("")),
        other => other.to_string(),
    };
    let reason = entry["reason"].as_str().filter(|r| !r.is_empty()).map(|r| format!(" ({r})")).unwrap_or_default();
    match (&target, action["type"].as_str()) {
        (Some(t), Some("MemberEdit" | "MemberKick" | "BanCreate" | "BanDelete")) if !text.contains(t.as_str()) => format!("{who} {text} - {t}{reason}"),
        _ => format!("{who} {text}{reason}"),
    }
}

/// What the server's moderators did, newest first: each as a line and when.
pub async fn audit_log(handle: &Handle, buffer: &Buffer, before: Option<&str>) -> Result<Value> {
    let server = server_of(handle, buffer)?;
    let mut query = vec![("limit", "50".to_string())];
    if let Some(b) = before.filter(|b| b.len() == 26) {
        query.push(("before", b.to_string()));
    }
    let answer = handle.api.get(&format!("/servers/{}/audit_logs", server.id), &query).await.map_err(fail)?;
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        for u in answer["users"].as_array().into_iter().flatten() {
            if let Some((id, user)) = world::parse_user(u) {
                world.users.entry(id).or_insert(user);
            }
        }
    }
    let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    let entries: Vec<Value> = answer["audit_logs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|e| json!({ "id": e["_id"], "text": describe(&world, e), "ts": e["_id"].as_str().and_then(messages::ulid_ms).map(|ms| ms / 1000).unwrap_or(0) }))
        .collect();
    Ok(json!({ "entries": entries }))
}

/// The webhooks of a channel, each with the address it takes posts at.
pub async fn webhooks(handle: &Handle, channel: &str) -> Result<Value> {
    let answer = handle.api.get(&format!("/channels/{channel}/webhooks"), &[]).await.map_err(fail)?;
    let base = handle.api.base();
    let list: Vec<Value> = answer
        .as_array()
        .into_iter()
        .flatten()
        .map(|w| json!({ "id": w["id"], "name": w["name"], "token": w["token"], "url": w["token"].as_str().map(|t| format!("{base}/webhooks/{}/{t}", w["id"].as_str().unwrap_or(""))) }))
        .collect();
    Ok(json!({ "webhooks": list }))
}

pub async fn create_webhook(handle: &Handle, channel: &str, name: &str) -> Result<()> {
    handle.api.post(&format!("/channels/{channel}/webhooks"), &json!({ "name": name.trim() })).await.map(|_| ()).map_err(fail)
}

pub async fn delete_webhook(handle: &Handle, id: &str, token: Option<&str>) -> Result<()> {
    let path = match token.filter(|t| !t.is_empty()) {
        Some(t) => format!("/webhooks/{id}/{t}"),
        None => format!("/webhooks/{id}"),
    };
    handle.api.delete(&path).await.map(|_| ()).map_err(fail)
}

/// Runs one action of the settings window; answers what it shows afterwards.
pub async fn run(handle: &Handle, buffer: &Buffer, action: &str, p: &Value) -> Result<Value> {
    let text = |k: &str| p[k].as_str();
    let id = |k: &str| text(k).filter(|v| !v.is_empty()).ok_or_else(|| anyhow!("{action} needs \"{k}\""));
    match action {
        "overview" => return overview(handle, buffer).await,
        "auditLog" => return audit_log(handle, buffer, text("before")).await,
        "webhooks" => return webhooks(handle, id("channelId")?).await,
        "editServer" => edit_server(handle, buffer, text("name"), text("description"), text("icon")).await?,
        "createChannel" => create_channel(handle, buffer, text("name").unwrap_or(""), p["voice"].as_bool().unwrap_or(false)).await?,
        "editChannel" => edit_channel(handle, id("channelId")?, text("name"), text("description"), p["nsfw"].as_bool(), p["slowmode"].as_u64()).await?,
        "deleteChannel" => delete_channel(handle, id("channelId")?).await?,
        "setCategories" => set_categories(handle, buffer, &p["categories"]).await?,
        "createRole" => create_role(handle, buffer, text("name").unwrap_or("")).await?,
        "editRole" => edit_role(handle, buffer, id("roleId")?, text("name"), text("colour"), p["hoist"].as_bool()).await?,
        "deleteRole" => delete_role(handle, buffer, id("roleId")?).await?,
        "reorderRoles" => reorder_roles(handle, buffer, &names_in(&p["order"])).await?,
        "setRolePermissions" => set_role_permissions(handle, buffer, text("roleId").filter(|r| !r.is_empty()), &names_in(&p["allow"]), &names_in(&p["deny"])).await?,
        "setChannelPermissions" => set_channel_permissions(handle, id("channelId")?, text("roleId").filter(|r| !r.is_empty()), &names_in(&p["allow"]), &names_in(&p["deny"])).await?,
        "createWebhook" => create_webhook(handle, id("channelId")?, text("name").unwrap_or("")).await?,
        "deleteWebhook" => delete_webhook(handle, id("webhookId")?, text("token")).await?,
        other => bail!("Stoat has no way to {other}"),
    }
    Ok(json!({ "ok": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_cross_as_names_and_back_without_loss_at_the_top_bits() {
        let bits = (1u64 << 40) | (1u64 << 22) | 1;
        assert_eq!(names_of(bits), vec!["ManageChannel", "ViewAuditLogs", "SendMessage"]);
        assert_eq!(bits_of(&["SendMessage".into(), "ViewAuditLogs".into(), "ManageChannel".into()]).unwrap(), bits);
        assert!(bits_of(&["Fly".into()]).unwrap_err().to_string().contains("Fly"));
        assert_eq!(bits_of(&[]).unwrap(), 0);
    }

    #[test]
    fn no_two_permissions_share_a_bit_or_a_name() {
        let mut bits: Vec<u32> = PERMISSIONS.iter().map(|p| p.1).collect();
        bits.sort();
        bits.dedup();
        assert_eq!(bits.len(), PERMISSIONS.len());
        let mut names: Vec<&str> = PERMISSIONS.iter().map(|p| p.0).collect();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), PERMISSIONS.len());
    }

    #[test]
    fn what_a_moderator_did_is_said_by_name() {
        let mut w = world::World::default();
        w.users.insert("U1".into(), world::User { name: "Mod".into(), ..Default::default() });
        w.users.insert("U2".into(), world::User { name: "Troll".into(), ..Default::default() });
        let said = |v: Value| describe(&w, &v);
        assert_eq!(said(json!({ "user": "U1", "action": { "type": "BanCreate", "user": "U2" }, "reason": "spam" })), "Mod banned Troll (spam)");
        assert_eq!(said(json!({ "user": "U1", "target": "U2", "action": { "type": "MemberKick", "user": "U2" } })), "Mod kicked Troll");
        assert_eq!(said(json!({ "user": "U1", "action": { "type": "RoleCreate", "role": "R", "name": "Helpers" } })), "Mod made the role Helpers");
        assert_eq!(said(json!({ "user": "U1", "action": { "type": "SomethingNew" } })), "Mod SomethingNew");
    }
}
