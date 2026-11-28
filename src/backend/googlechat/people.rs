//! People, presence, member lists, profile cards, creating DMs, and managing spaces (rename, description, invite,
//! kick, accept/decline space invites).

use super::{pb, world, Group, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use serde_json::{json, Value};

/// Maps Google Chat's `UserPresence` to moho's status words (`"online"`, `"dnd"`, `"offline"`).
pub fn presence_word(presence: &pb::UserPresence) -> &'static str {
    if presence.dnd_state() == pb::DndStateState::Dnd {
        return "dnd";
    }
    match presence.presence() {
        pb::Presence::Active => "online",
        _ => "offline",
    }
}

/// Extracts custom status text from `UserPresence.user_status`, if any.
fn custom_status_text(presence: &pb::UserPresence) -> Option<String> {
    let cs = presence.user_status.as_ref()?.custom_status.as_ref()?;
    let emoji = cs
        .emoji
        .as_ref()
        .and_then(|e| match &e.content {
            Some(pb::emoji::Content::Unicode(u)) => Some(u.as_str()),
            _ => None,
        })
        .unwrap_or(cs.status_emoji());
    let text = cs.status_text();
    let combined = format!("{emoji} {text}").trim().to_string();
    (!combined.is_empty()).then_some(combined)
}

/// Fetches presence for a set of gaia IDs and updates `world.people`.
pub async fn refresh_presence(handle: &Handle, ids: &[String]) {
    for batch in ids.chunks(50) {
        match handle.api.get_user_presence(batch).await {
            Ok(resp) => {
                let mut world = handle.world();
                for up in &resp.user_presences {
                    let Some(id) = up.user_id.as_ref().map(|u| u.id().to_string()).filter(|s| !s.is_empty()) else { continue };
                    let status = presence_word(up).to_string();
                    let custom = custom_status_text(up);
                    let person = world.people.entry(id).or_default();
                    person.status = status;
                    person.custom_status = custom;
                }
            }
            Err(e) => tracing::debug!("googlechat[{}]: get_user_presence: {e}", handle.account_id),
        }
    }
}

fn member_entry(world: &world::World, gaia: &str, owner: bool) -> Value {
    let person = world.people.get(gaia);
    let status = person.map(|p| p.status.as_str()).filter(|s| !s.is_empty()).unwrap_or("offline");
    let activity = person
        .and_then(|p| p.custom_status.as_ref())
        .map(|text| json!({ "kind": "custom", "text": text }))
        .unwrap_or(Value::Null);
    json!({
        "nick": world.name_of(gaia),
        "userId": gaia,
        "prefix": if owner { "@" } else { "" },
        "away": status == "offline",
        "status": status,
        "activity": activity,
        "bot": person.is_some_and(|p| p.bot),
        "avatarUrl": person.and_then(|p| p.avatar.clone()),
        "roles": if owner { vec!["Space Manager"] } else { Vec::<&str>::new() },
    })
}

/// Refreshes a conversation's member list, roles, topic, and presence via `get_group` + `get_members` +
/// `get_user_presence`, then emits `presenceChange`.
pub async fn refresh_members(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<usize, String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("this is not a Google Chat conversation")?;
    let resp = handle.api.get_group(&group).await.map_err(|e| e.to_string())?;

    let mut member_ids: Vec<String> = Vec::new();
    let mut owners: Vec<String> = Vec::new();
    for m in &resp.memberships {
        let Some(id) = m
            .id
            .as_ref()
            .and_then(|id| id.member_id.as_ref())
            .and_then(|mid| mid.id.as_ref())
            .map(|pb::member_id::Id::UserId(u)| u.id().to_string())
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        if !member_ids.contains(&id) {
            member_ids.push(id.clone());
        }
        if m.membership_role() == pb::MembershipRole::RoleOwner {
            owners.push(id);
        }
    }

    let space_name = resp
        .group
        .as_ref()
        .and_then(|g| g.name.clone())
        .filter(|n| !n.trim().is_empty());
    let description = resp
        .group
        .as_ref()
        .and_then(|g| g.group_details.as_ref())
        .and_then(|d| d.description.clone());
    if let Some(desc) = &description {
        state.runtime.set_buffer_topic(state, &buffer.id, desc);
    }

    let mut renamed = false;
    let unknown: Vec<String> = {
        let mut world = handle.world();
        let conv = world.conversations.entry(group.clone()).or_default();
        if !member_ids.is_empty() {
            conv.members = member_ids.clone();
        }
        conv.owners = owners.clone();
        if let Some(name) = space_name {
            if conv.name.as_deref() != Some(name.as_str()) {
                conv.name = Some(name);
                renamed = true;
            }
        }
        if let Some(desc) = description {
            conv.description = Some(desc);
        }
        if let Some(g) = resp.group.as_ref() {
            conv.history_off = super::retention::is_ephemeral(g.retention_settings.as_ref());
        }
        member_ids.iter().filter(|id| !world.people.contains_key(*id)).cloned().collect()
    };
    if renamed {
        world::register(state, handle, &group, false);
    }

    if !unknown.is_empty() {
        world::learn_people(handle, unknown).await;
    }
    refresh_presence(handle, &member_ids).await;

    let list = {
        let world = handle.world();
        Value::Array(member_ids.iter().map(|id| member_entry(&world, id, owners.contains(id))).collect())
    };
    let total = member_ids.len();
    state.runtime.set_presence(&buffer.id, list.clone());
    state.runtime.set_presence_total(&buffer.id, total);
    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": list, "total": total }));
    Ok(total)
}

/// Builds a profile card for `requestProfile`.
pub async fn profile(state: &AppState, account_id: &str, _buffer_id: &str, who: &str, user_id: &str) -> Value {
    let mut card = crate::profile::pending("googlechat", account_id, who);
    card["pending"] = json!(false);
    let Some(handle) = state.runtime.googlechat_handle(account_id) else {
        return card;
    };

    let gaia = if user_id.chars().all(|c| c.is_ascii_digit()) && !user_id.is_empty() {
        user_id.to_string()
    } else {
        let world = handle.world();
        world
            .people
            .iter()
            .find(|(_, p)| p.name.eq_ignore_ascii_case(who) || p.email.eq_ignore_ascii_case(who))
            .map(|(id, _)| id.clone())
            .unwrap_or_default()
    };

    if gaia.is_empty() {
        return card;
    }

    let missing = !handle.world().people.contains_key(&gaia);
    if missing {
        world::learn_people(&handle, vec![gaia.clone()]).await;
    }
    refresh_presence(&handle, std::slice::from_ref(&gaia)).await;

    let world = handle.world();
    if let Some(person) = world.people.get(&gaia) {
        if !person.name.is_empty() {
            card["displayName"] = json!(person.name);
        }
        if !person.email.is_empty() {
            card["email"] = json!(person.email);
            card["subtitle"] = json!(person.email);
        }
        if let Some(avatar) = &person.avatar {
            card["avatarUrl"] = json!(avatar);
        }
        card["id"] = json!(gaia);
        card["bot"] = json!(person.bot);
        if !person.status.is_empty() {
            card["status"] = json!(person.status);
        }
        if let Some(cs) = &person.custom_status {
            card["statusText"] = json!(cs);
        }
    }
    let shared: Vec<String> = world
        .conversations
        .iter()
        .filter(|(_, c)| c.members.contains(&gaia))
        .map(|(g, _)| world.title_of(g))
        .collect();
    if !shared.is_empty() {
        card["channels"] = json!(shared);
    }
    card
}

/// Starts or opens a direct message by email address or numeric gaia user ID.
pub async fn create_dm(state: &AppState, handle: &Handle, identifier: &str) -> Result<Buffer, String> {
    let target = identifier.trim().trim_start_matches("mailto:");
    if target.is_empty() {
        return Err("enter an email address or Google Chat user ID".into());
    }
    let own_id = handle.world().own_id.clone();
    let request = if target.chars().all(|c| c.is_ascii_digit()) {
        if target == own_id {
            return Err("Google Chat cannot open a direct message with your own account".into());
        }
        pb::CreateDmRequest {
            members: vec![super::user_id(target)],
            ..Default::default()
        }
    } else if target.contains('@') {
        // By email: as the web app does it (create_dm_extended). create_dm with an invitee email is refused (500).
        let resp = handle.api.create_dm_extended(Some(target), None).await.map_err(|e| format!("could not open DM: {e}"))?;
        let group = Group::from_pb(resp.dm.as_ref().and_then(|d| d.group_id.as_ref()))
            .ok_or_else(|| "Google Chat did not return a conversation ID".to_string())?;
        let members: Vec<String> = resp
            .memberships
            .iter()
            .filter_map(|m| m.id.as_ref()?.member_id.as_ref()?.id.as_ref().map(|pb::member_id::Id::UserId(u)| u.id().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
        {
            let mut world = handle.world();
            for member in &resp.members {
                if let Some(pb::member::Profile::User(user)) = &member.profile {
                    world.learn(user);
                }
            }
            let conversation = world.conversations.entry(group.clone()).or_default();
            for id in members {
                if !conversation.members.contains(&id) {
                    conversation.members.push(id);
                }
            }
        }
        return open_dm(state, handle, &group).await;
    } else {
        return Err(format!("{target:?} is neither an email address nor a numeric Google Chat user ID"));
    };

    let resp = handle.api.create_dm(request).await.map_err(|e| format!("could not open DM: {e}"))?;
    let group = Group::from_pb(resp.dm.as_ref().and_then(|d| d.group_id.as_ref()))
        .ok_or_else(|| "Google Chat did not return a conversation ID".to_string())?;

    let mut member_ids: Vec<String> = Vec::new();
    for m in &resp.memberships {
        if let Some(id) = m
            .id
            .as_ref()
            .and_then(|id| id.member_id.as_ref())
            .and_then(|mid| mid.id.as_ref())
            .map(|pb::member_id::Id::UserId(u)| u.id().to_string())
            .filter(|s| !s.is_empty())
        {
            if !member_ids.contains(&id) {
                member_ids.push(id);
            }
        }
    }
    let unknown: Vec<String> = {
        let mut world = handle.world();
        let conv = world.conversations.entry(group.clone()).or_default();
        for id in &member_ids {
            if !conv.members.contains(id) {
                conv.members.push(id.clone());
            }
        }
        world.unknown_people()
    };
    if !unknown.is_empty() {
        world::learn_people(handle, unknown).await;
    }
    Ok(world::register(state, handle, &group, false))
}

/// The buffer of a direct message just made, its people read with get_group.
async fn open_dm(state: &AppState, handle: &Handle, group: &Group) -> Result<Buffer, String> {
    handle.world().conversations.entry(group.clone()).or_default();
    let buffer = world::register(state, handle, group, false);
    // Members, names and roles, then the buffer again under the other person's name.
    let _ = refresh_members(state, handle, &buffer).await;
    // Named again now its people are known (a direct message is called after them).
    Ok(world::register(state, handle, group, false))
}

/// Renames a space (spaces only).
pub async fn rename_space(state: &AppState, handle: &Handle, buffer: &Buffer, new_name: &str) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let Group::Space(ref space_id) = group else {
        return Err("cannot rename a direct message".into());
    };
    let req = pb::UpdateGroupRequest {
        space_id: Some(pb::SpaceId { space_id: Some(space_id.clone()) }),
        name: Some(new_name.to_string()),
        update_masks: vec![pb::update_group_request::UpdateMask::Name as i32],
        ..Default::default()
    };
    handle.api.update_group(req).await.map_err(|e| e.to_string())?;
    if let Some(conv) = handle.world().conversations.get_mut(&group) {
        conv.name = Some(new_name.to_string());
    }
    world::register(state, handle, &group, false);
    Ok(())
}

/// Updates a space's description while preserving its existing guidelines (read back via `get_group` first, because
/// `SPACE_DETAILS` replaces both fields together).
pub async fn set_space_description(state: &AppState, handle: &Handle, buffer: &Buffer, description: &str) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let Group::Space(ref space_id) = group else {
        return Err("a direct message has no description".into());
    };
    let current = handle.api.get_group(&group).await.map_err(|e| format!("reading current space details failed: {e}"))?;
    let guidelines = current
        .group
        .and_then(|g| g.group_details)
        .and_then(|d| d.guidelines);
    let req = pb::UpdateGroupRequest {
        space_id: Some(pb::SpaceId { space_id: Some(space_id.clone()) }),
        space_details: Some(pb::GroupDetails {
            description: Some(description.to_string()),
            guidelines,
        }),
        update_masks: vec![pb::update_group_request::UpdateMask::SpaceDetails as i32],
        ..Default::default()
    };
    handle.api.update_group(req).await.map_err(|e| e.to_string())?;
    if let Some(conv) = handle.world().conversations.get_mut(&group) {
        conv.description = Some(description.to_string());
    }
    state.runtime.set_buffer_topic(state, &buffer.id, description);
    Ok(())
}

/// Invites a user (by gaia ID or email) into a space.
pub async fn invite_member(handle: &Handle, buffer: &Buffer, identifier: &str) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    if group.is_dm() {
        return Err("membership changes are only supported in spaces".into());
    }
    let target = identifier.trim();
    let invitee_info = if target.chars().all(|c| c.is_ascii_digit()) && !target.is_empty() {
        pb::InviteeInfo { user_id: Some(super::user_id(target)), ..Default::default() }
    } else if target.contains('@') {
        pb::InviteeInfo { email: Some(target.to_string()), ..Default::default() }
    } else {
        return Err("enter an email address or numeric Google Chat user ID".into());
    };
    let req = pb::CreateMembershipRequest {
        group_id: Some(group.to_pb()),
        invitee_member_infos: vec![pb::InviteeMemberInfo {
            id: Some(pb::invitee_member_info::Id::InviteeInfo(invitee_info)),
        }],
        ..Default::default()
    };
    handle.api.create_membership(req).await.map(|_| ()).map_err(|e| e.to_string())
}

/// Removes (kicks) a member from a space by gaia ID.
pub async fn kick_member(handle: &Handle, buffer: &Buffer, gaia_id: &str) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    if group.is_dm() {
        return Err("membership changes are only supported in spaces".into());
    }
    handle.api.remove_memberships(&group, gaia_id).await.map(|_| ()).map_err(|e| e.to_string())
}

/// Accepts or declines a pending space invite.
pub async fn answer_invite(state: &AppState, handle: &Handle, space_id: &str, accept: bool) -> Result<Option<Buffer>, String> {
    let group = Group::Space(space_id.to_string());
    let own_id = handle.world().own_id.clone();
    if accept {
        let req = pb::CreateMembershipRequest {
            group_id: Some(group.to_pb()),
            member_ids: vec![super::member_id(&own_id)],
            membership_state: Some(pb::MembershipState::MemberJoined as i32),
            ..Default::default()
        };
        handle.api.create_membership(req).await.map_err(|e| e.to_string())?;
    } else {
        handle.api.remove_memberships(&group, &own_id).await.map_err(|e| e.to_string())?;
    }
    handle.world().pending_invites.retain(|id| id != space_id);
    if accept {
        let buffer = world::register(state, handle, &group, false);
        let _ = refresh_members(state, handle, &buffer).await;
        Ok(Some(buffer))
    } else {
        Ok(None)
    }
}

/// Applies moho's unified status (`"online"`, `"dnd"`, `"invisible"`) and optional custom status text to Google Chat.
pub async fn apply_status(handle: &Handle, status: &str, text: Option<&str>) -> Result<(), String> {
    match status {
        "dnd" => {
            handle.api.set_presence_shared(true).await.map_err(|e| e.to_string())?;
            handle.api.set_dnd_duration(true, None).await.map_err(|e| e.to_string())?;
        }
        "invisible" => {
            handle.api.set_presence_shared(false).await.map_err(|e| e.to_string())?;
        }
        _ => {
            handle.api.set_presence_shared(true).await.map_err(|e| e.to_string())?;
            handle.api.set_dnd_duration(false, None).await.map_err(|e| e.to_string())?;
        }
    }
    if let Some(t) = text {
        handle.api.set_custom_status(t, None, None).await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Sets custom status text + optional emoji + optional duration in seconds (empty clears it).
pub async fn set_custom_status(handle: &Handle, text: &str, emoji: Option<&str>, duration_secs: Option<i64>) -> Result<(), String> {
    handle
        .api
        .set_custom_status(text, emoji, duration_secs)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Toggles Do Not Disturb with an optional duration in seconds.
pub async fn set_dnd(handle: &Handle, dnd: bool, duration_secs: Option<i64>) -> Result<(), String> {
    handle
        .api
        .set_dnd_duration(dnd, duration_secs)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Blocks or unblocks a user by gaia ID, optionally reporting abuse.
pub async fn block_user(handle: &Handle, gaia_id: &str, blocked: bool, reported: bool) -> Result<(), String> {
    handle
        .api
        .block_entity(pb::block_entity_request::Entity::UserId(super::user_id(gaia_id)), blocked, reported)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Blocks or unblocks a conversation (DM or Space), optionally reporting abuse.
pub async fn block_group(handle: &Handle, buffer: &Buffer, blocked: bool, reported: bool) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    handle
        .api
        .block_entity(pb::block_entity_request::Entity::GroupId(group.to_pb()), blocked, reported)
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Creates a new Space and registers its buffer.
pub async fn create_space(state: &AppState, handle: &Handle, name: &str, description: Option<&str>) -> Result<Buffer, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("enter a name for the Space".into());
    }
    let local_id = format!("moho-{:016x}", rand::random::<u64>());
    let resp = handle.api.create_group(name, &local_id).await.map_err(|e| e.to_string())?;
    let group = Group::from_pb(resp.group.as_ref().and_then(|g| g.group_id.as_ref()))
        .ok_or_else(|| "Google Chat did not return a Space ID".to_string())?;
    {
        let mut world = handle.world();
        let conv = world.conversations.entry(group.clone()).or_default();
        conv.name = Some(name.to_string());
    }
    let buffer = world::register(state, handle, &group, false);
    if let Some(desc) = description.map(str::trim).filter(|s| !s.is_empty()) {
        let _ = set_space_description(state, handle, &buffer, desc).await;
    }
    Ok(buffer)
}

/// Pending space invites shaped like `MatrixInvite` so moho's Invitations tile can display them.
pub async fn pending_invites_json(handle: &Handle) -> Vec<Value> {
    let ids = handle.world().pending_invites.clone();
    let mut out = Vec::with_capacity(ids.len());
    for space_id in ids {
        let name = match handle.api.get_group(&Group::Space(space_id.clone())).await {
            Ok(resp) => resp
                .group
                .and_then(|g| g.name)
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(|| format!("Space {space_id}")),
            Err(_) => format!("Space {space_id}"),
        };
        out.push(json!({
            "roomId": space_id,
            "name": name,
            "avatarUrl": Value::Null,
            "isDirect": false,
            "inviter": Value::Null,
        }));
    }
    out
}

/// Joins a Space by ID (`space:<id>` or `AAQA...`) or opens a DM by email / numeric gaia ID.
pub async fn join_or_dm(state: &AppState, handle: &Handle, target: &str) -> Result<Buffer, String> {
    let trimmed = target.trim();
    if let Some(space_id) = trimmed
        .strip_prefix("space:")
        .or_else(|| (trimmed.starts_with("AAQA") && trimmed.len() >= 8 && !trimmed.contains('@')).then_some(trimmed))
    {
        return answer_invite(state, handle, space_id, true)
            .await?
            .ok_or_else(|| "could not join that Space".to_string());
    }
    create_dm(state, handle, trimmed).await
}

/// Updates both description and guidelines of a Space in one `update_group` call (`SPACE_DETAILS`).
pub async fn set_space_details(
    state: &AppState,
    handle: &Handle,
    buffer: &Buffer,
    description: Option<&str>,
    guidelines: Option<&str>,
) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let Group::Space(ref space_id) = group else {
        return Err("only Spaces have description and guidelines".into());
    };
    let current = handle.api.get_group(&group).await.map_err(|e| format!("reading current space details failed: {e}"))?;
    let existing = current.group.and_then(|g| g.group_details);
    let final_desc = description
        .map(str::to_string)
        .or_else(|| existing.as_ref().and_then(|d| d.description.clone()));
    let final_guide = guidelines
        .map(str::to_string)
        .or_else(|| existing.as_ref().and_then(|d| d.guidelines.clone()));
    let req = pb::UpdateGroupRequest {
        space_id: Some(pb::SpaceId { space_id: Some(space_id.clone()) }),
        space_details: Some(pb::GroupDetails {
            description: final_desc.clone(),
            guidelines: final_guide.clone(),
        }),
        update_masks: vec![pb::update_group_request::UpdateMask::SpaceDetails as i32],
        ..Default::default()
    };
    handle.api.update_group(req).await.map_err(|e| e.to_string())?;
    if let Some(conv) = handle.world().conversations.get_mut(&group) {
        conv.description = final_desc.clone();
        conv.guidelines = final_guide;
    }
    if let Some(desc) = final_desc {
        state.runtime.set_buffer_topic(state, &buffer.id, &desc);
    }
    Ok(())
}

/// Lists uploaded files and Google Drive embeds shared in a conversation (from scrollback).
pub fn list_shared_files(state: &AppState, buffer: &Buffer) -> Vec<Value> {
    let rows = state.store.get_backlog(&buffer.id, 0, 500).unwrap_or_default();
    let mut files = Vec::new();
    for m in rows.into_iter().rev() {
        for att in m.attachments {
            files.push(json!({
                "kind": att.kind,
                "name": att.filename.unwrap_or_else(|| "Attachment".into()),
                "mimetype": att.mimetype,
                "url": att.url,
                "path": att.path,
                "from": m.from,
                "ts": m.ts,
                "messageId": m.id,
            }));
        }
        for emb in m.embeds {
            if emb.provider.as_deref() == Some("Google Drive") {
                files.push(json!({
                    "kind": "drive",
                    "name": emb.title.unwrap_or_else(|| "Google Drive file".into()),
                    "url": emb.url,
                    "from": m.from,
                    "ts": m.ts,
                    "messageId": m.id,
                }));
            }
        }
    }
    files
}

/// Returns known reactors for `(message_id, emoji)` as `{ users: [{ name }] }`.
pub fn reactors(handle: &Handle, message_id: &str, emoji: &str) -> Value {
    let bare = emoji.replace('\u{fe0f}', "");
    let world = handle.world();
    let users: Vec<Value> = world
        .reactors
        .get(&(message_id.to_string(), bare))
        .into_iter()
        .flatten()
        .map(|uid| json!({ "name": world.name_of(uid) }))
        .collect();
    json!({ "users": users })
}

/// Creates a Google Meet video call link via `create_video_call` and posts the Meet card into the conversation.
pub async fn create_meet_link(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<Option<String>, String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let resp = handle.api.create_video_call(&group).await.map_err(|e| e.to_string())?;
    let ann = resp.annotation.ok_or_else(|| "Google Chat returned no Meet annotation".to_string())?;
    let url = match &ann.metadata {
        Some(pb::annotation::Metadata::VideoCallMetadata(vc)) => vc
            .meeting_space
            .as_ref()
            .map(|m| m.meeting_url().to_string())
            .filter(|s| !s.is_empty()),
        _ => None,
    };
    let local_id = format!("{:016x}", rand::random::<u64>());
    let topic_req = pb::CreateTopicRequest {
        group_id: Some(group.to_pb()),
        text_body: Some(String::new()),
        annotations: vec![ann],
        local_id: Some(local_id),
        history_v2: Some(true),
        ..Default::default()
    };
    let created = handle.api.create_topic(topic_req).await.map_err(|e| e.to_string())?;
    if let Some(msg) = created.topic.and_then(|t| t.replies.into_iter().next()) {
        super::messages::record(state, handle, &group, &msg).await;
    }
    Ok(url)
}

/// Lists users blocked by this account (`list_blocked_users`), resolving display names when needed.
pub async fn list_blocked_users(handle: &Handle) -> Result<Vec<Value>, String> {
    let resp = handle.api.list_blocked_users().await.map_err(|e| e.to_string())?;
    let ids: Vec<String> = resp.blocked_user_id.into_iter().filter_map(|u| u.id).filter(|s| !s.is_empty()).collect();
    let unknown: Vec<String> = {
        let world = handle.world();
        ids.iter().filter(|id| !world.people.contains_key(*id)).cloned().collect()
    };
    if !unknown.is_empty() {
        world::learn_people(handle, unknown).await;
    }
    let world = handle.world();
    Ok(ids
        .into_iter()
        .map(|id| {
            let p = world.people.get(&id);
            json!({
                "userId": id,
                "name": world.name_of(&id),
                "email": p.map(|x| x.email.as_str()).unwrap_or(""),
                "avatarUrl": p.and_then(|x| x.avatar.clone()),
            })
        })
        .collect())
}

/// Lists slash commands offered by bots installed in this conversation (`autocomplete_slash_commands`).
pub async fn slash_commands(handle: &Handle, buffer: &Buffer, query: &str) -> Result<Vec<Value>, String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let resp = handle.api.autocomplete_slash_commands(&group, query).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for bot_group in resp.bots_in_group {
        let source = bot_group
            .bot
            .as_ref()
            .map(|u| u.name().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Google Chat App".to_string());
        for cmd in bot_group.slash_commands {
            let name = cmd.name().trim_start_matches('/').to_string();
            if name.is_empty() {
                continue;
            }
            let hint = cmd.arguments_hint();
            let usage = if hint.is_empty() { format!("/{name}") } else { format!("/{name} {hint}") };
            out.push(json!({
                "name": name,
                "usage": usage,
                "description": cmd.description(),
                "source": source,
                "kind": "application",
            }));
        }
    }
    Ok(out)
}

/// Sets a Space's emoji avatar (`update_group` with `AVATAR_INFO = 3`).
pub async fn set_space_emoji(handle: &Handle, buffer: &Buffer, emoji: &str) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let Group::Space(ref space_id) = group else {
        return Err("only Spaces have an emoji avatar".into());
    };
    let emoji = emoji.trim();
    let req = pb::UpdateGroupRequest {
        space_id: Some(pb::SpaceId { space_id: Some(space_id.clone()) }),
        avatar_info: Some(pb::AvatarInfo {
            emoji: (!emoji.is_empty()).then(|| pb::Emoji {
                content: Some(pb::emoji::Content::Unicode(emoji.to_string())),
            }),
        }),
        update_masks: vec![pb::update_group_request::UpdateMask::AvatarInfo as i32],
        ..Default::default()
    };
    handle.api.update_group(req).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Toggles History On (`PERMANENT`) or History Off (`EPHEMERAL_ONE_DAY`, 24-hour messages) in a DM or Space.
pub async fn set_history_state(state: &AppState, handle: &Handle, buffer: &Buffer, history_on: bool) -> Result<bool, String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let resp = handle.api.update_group_retention_settings(&group, history_on).await.map_err(|e| e.to_string())?;
    let is_on = resp
        .retention_settings
        .and_then(|r| r.state)
        .map(|s| s == pb::retention_settings::RetentionState::Permanent as i32)
        .unwrap_or(history_on);
    super::retention::set_history_off(state, handle, &group, !is_on);
    Ok(is_on)
}

/// Promotes or demotes a Space member (`role`: `"member"` -> 3, `"owner"` -> 4, `"manager"` -> 6).
pub async fn set_member_role(handle: &Handle, buffer: &Buffer, user_id: &str, role: &str) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let role_code = match role.trim().to_ascii_lowercase().as_str() {
        "member" => 3,
        "owner" => 4,
        "manager" | "space_manager" | "spacemanager" => 6,
        other => return Err(format!("unknown role {other:?} (expected \"member\", \"manager\", or \"owner\")")),
    };
    handle.api.update_membership_role(&group, user_id, role_code).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Pins or unpins a DM or Space (`batchexecute` `XZhsdc`) and updates its sidebar category in moho.
pub async fn pin_conversation(state: &AppState, handle: &Handle, buffer: &Buffer, pinned: bool) -> Result<bool, String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let result = handle.api.set_pinned(&group, pinned).await.map_err(|e| e.to_string())?;
    if let Some(conv) = handle.world().conversations.get_mut(&group) {
        conv.starred = result;
    }
    let category = if result {
        Some("Starred")
    } else if matches!(group, Group::Space(_)) {
        Some("Spaces")
    } else {
        None
    };
    state.runtime.set_buffer_category(state, &buffer.id, category, 0);
    Ok(result)
}

/// Creates a custom sidebar section (`create_roster_section`), returning its server section ID.
pub async fn create_section(handle: &Handle, name: &str, sort_order: i32) -> Result<Value, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("section name cannot be empty".into());
    }
    let resp = handle.api.create_roster_section(name, sort_order).await.map_err(|e| e.to_string())?;
    let sec_id = resp
        .section
        .as_ref()
        .and_then(|s| s.section_id.as_ref())
        .and_then(|id| id.id.clone().or_else(|| id.label_id.as_ref().and_then(|l| l.id.clone())))
        .unwrap_or_default();
    Ok(json!({ "ok": true, "sectionId": sec_id, "name": name }))
}

/// Deletes a custom sidebar section (`delete_roster_section`).
pub async fn delete_section(handle: &Handle, section_id: &str) -> Result<(), String> {
    handle.api.delete_roster_section(section_id).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Moves a DM or Space into a custom sidebar section (`apply_group_label`).
pub async fn move_to_section(handle: &Handle, buffer: &Buffer, section_id: &str) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    handle.api.apply_group_label(&group, section_id).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Browses / discovers Spaces (`batchexecute` `jfcZG`).
pub async fn browse_spaces(handle: &Handle, query: &str, unjoined_only: bool) -> Result<Vec<Value>, String> {
    let val = handle.api.browse_spaces(query, unjoined_only).await.map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let joined_ids: std::collections::HashSet<String> = handle
        .world()
        .conversations
        .keys()
        .filter_map(|g| match g {
            Group::Space(id) => Some(id.clone()),
            _ => None,
        })
        .collect();
    if let Some(spaces) = val.get(1).and_then(|v| v.as_array()) {
        for s in spaces {
            let space_id = s
                .get(0)
                .and_then(|v| v.get(0))
                .and_then(|v| v.get(0))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if space_id.is_empty() {
                continue;
            }
            let name = s.get(1).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let member_count = s.get(5).and_then(|v| v.as_i64()).unwrap_or(0);
            let avatar_url = s.get(6).and_then(|v| v.as_str()).map(str::to_string);
            let joined = joined_ids.contains(&space_id);
            out.push(json!({
                "spaceId": space_id,
                "name": name,
                "memberCount": member_count,
                "avatarUrl": avatar_url,
                "joined": joined,
            }));
        }
    }
    Ok(out)
}

/// Server-side message search (`batchexecute` `SBNmJb`) with filter operator parsing
/// (`in:`, `from:`, `has:link`, `has:file`, `has:image`, `before:YYYY-MM-DD`, `after:YYYY-MM-DD`).
pub async fn search_messages(handle: &Handle, raw_query: &str, scope_buffer: Option<&Buffer>) -> Result<Vec<Value>, String> {
    let mut terms = Vec::new();
    let mut filter_in: Option<String> = scope_buffer
        .and_then(|b| b.remote_id.as_deref())
        .and_then(Group::from_remote_id)
        .map(|g| match g {
            Group::Space(id) | Group::Dm(id) => id,
        });
    let mut filter_from: Option<String> = None;
    let mut has_link = false;
    let mut has_file = false;
    let mut has_image = false;
    let mut before_ms: Option<i64> = None;
    let mut after_ms: Option<i64> = None;

    for token in raw_query.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("in:") {
            if !rest.is_empty() {
                filter_in = Some(token[3..].to_string());
            }
        } else if let Some(rest) = lower.strip_prefix("from:") {
            if !rest.is_empty() {
                filter_from = Some(rest.to_string());
            }
        } else if lower == "has:link" {
            has_link = true;
        } else if lower == "has:file" {
            has_file = true;
        } else if lower == "has:image" {
            has_image = true;
        } else if let Some(date) = lower.strip_prefix("before:") {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
                before_ms = d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc().timestamp_millis());
            }
        } else if let Some(date) = lower.strip_prefix("after:") {
            if let Ok(d) = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
                after_ms = d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc().timestamp_millis());
            }
        } else {
            terms.push(token);
        }
    }
    let query_text = terms.join(" ");
    let val = handle.api.search_messages(&query_text, has_link).await.map_err(|e| e.to_string())?;

    // Map group_id -> group_name from response[4]
    let mut group_names: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(groups) = val.get(4).and_then(|v| v.as_array()) {
        for g in groups {
            let gid = g.get(0).and_then(|v| v.get(1)).and_then(|v| v.as_str()).unwrap_or("");
            let gname = g.get(2).and_then(|v| v.as_str()).unwrap_or("");
            if !gid.is_empty() {
                group_names.insert(gid.to_string(), gname.to_string());
            }
        }
    }

    let world = handle.world();
    let mut out = Vec::new();
    if let Some(hits) = val.get(9).and_then(|v| v.as_array()) {
        for hit in hits {
            let Some(msg) = hit.get(0) else { continue };
            let raw_gid = msg.get(2).and_then(|v| v.get(1)).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let group_kind = msg.get(2).and_then(|v| v.get(2)).and_then(|v| v.as_i64()).unwrap_or(2);
            let gname = group_names.get(&raw_gid).cloned().unwrap_or_else(|| raw_gid.clone());
            let sender_id = msg.get(4).and_then(|v| v.get(0)).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let sender_name = world.name_of(&sender_id);
            let text = msg.get(5).and_then(|v| v.as_str()).unwrap_or("").to_string();
            let ts_ms = msg
                .get(16)
                .and_then(|v| v.as_i64())
                .or_else(|| msg.get(11).and_then(|v| v.as_str()).and_then(|s| s.parse::<i64>().ok()).map(|u| u / 1000))
                .unwrap_or(0);
            let message_id = msg.get(17).and_then(|v| v.get(0)).and_then(|v| v.as_str()).unwrap_or("").to_string();

            // Apply client-side filter operators that aren't encoded in the base SBNmJb flags
            if let Some(ref want_in) = filter_in {
                let w = want_in.to_ascii_lowercase();
                if raw_gid.to_ascii_lowercase() != w && !gname.to_ascii_lowercase().contains(&w) {
                    continue;
                }
            }
            if let Some(ref want_from) = filter_from {
                let email = world.people.get(&sender_id).map(|p| p.email.to_ascii_lowercase()).unwrap_or_default();
                if !sender_name.to_ascii_lowercase().contains(want_from) && !email.contains(want_from) && sender_id != *want_from {
                    continue;
                }
            }
            if let Some(b) = before_ms {
                if ts_ms >= b {
                    continue;
                }
            }
            if let Some(a) = after_ms {
                if ts_ms <= a {
                    continue;
                }
            }
            if has_file || has_image {
                let json_str = msg.to_string().to_ascii_lowercase();
                if has_image && !json_str.contains("image/") {
                    continue;
                }
                if has_file && !json_str.contains("upload") && !json_str.contains("drive.google.com") && !json_str.contains("attachment") {
                    continue;
                }
            }

            let remote_id = if group_kind == 1 { format!("dm:{raw_gid}") } else { format!("space:{raw_gid}") };
            out.push(json!({
                "id": message_id,
                "remoteId": remote_id,
                "groupName": gname,
                "from": sender_name,
                "senderId": sender_id,
                "body": text,
                "ts": ts_ms,
            }));
        }
    }
    Ok(out)
}

/// Lists all active Huddles / Meet calls detected on this Google Chat account.
pub fn list_huddles(handle: &Handle) -> Vec<world::ActiveHuddle> {
    handle.world().huddles.values().filter(|h| h.active).cloned().collect()
}

/// Starts a Huddle / Meet call in a DM or Space, posts the call annotation, marks it active, and returns it.
pub async fn start_huddle(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<world::ActiveHuddle, String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let url = create_meet_link(state, handle, buffer).await?;
    if let Some(huddle) = handle.world().huddles.get(&group).cloned() {
        return Ok(huddle);
    }
    let meeting_url = url.ok_or_else(|| "Google Chat did not return a Meet/Huddle URL".to_string())?;
    let meeting_code = meeting_url
        .split('/')
        .last()
        .unwrap_or("")
        .split('?')
        .next()
        .unwrap_or("")
        .to_string();
    let huddle = {
        let mut world = handle.world();
        let own_id = world.own_id.clone();
        let own_name = if !world.own_name.is_empty() { world.own_name.clone() } else { world.name_of(&own_id) };
        let own_avatar = world.people.get(&own_id).and_then(|p| p.avatar.clone());
        let h = world::ActiveHuddle {
            buffer_id: buffer.id.clone(),
            message_id: String::new(),
            meeting_url,
            meeting_code,
            starter_id: own_id.clone(),
            starter_name: own_name.clone(),
            participants: vec![world::HuddleParticipant {
                user_id: own_id,
                name: own_name,
                avatar_url: own_avatar,
            }],
            started_at: chrono::Utc::now().timestamp(),
            active: true,
        };
        world.huddles.insert(group, h.clone());
        h
    };
    state.events.emit("googlechatHuddle", json!({ "bufferId": buffer.id, "huddle": huddle }));
    Ok(huddle)
}

/// Records the signed-in user as joined in the conversation's active Huddle and returns it.
pub fn join_huddle(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<world::ActiveHuddle, String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    let huddle = {
        let mut world = handle.world();
        let own_id = world.own_id.clone();
        let own_name = if !world.own_name.is_empty() { world.own_name.clone() } else { world.name_of(&own_id) };
        let own_avatar = world.people.get(&own_id).and_then(|p| p.avatar.clone());
        let h = world.huddles.get_mut(&group).ok_or("no active Huddle in this conversation")?;
        if !own_id.is_empty() && !h.participants.iter().any(|p| p.user_id == own_id) {
            h.participants.push(world::HuddleParticipant {
                user_id: own_id,
                name: own_name,
                avatar_url: own_avatar,
            });
        }
        h.clone()
    };
    state.events.emit("googlechatHuddle", json!({ "bufferId": buffer.id, "huddle": huddle }));
    Ok(huddle)
}

/// Dismisses / clears an active Huddle banner for a conversation.
pub fn dismiss_huddle(state: &AppState, handle: &Handle, buffer: &Buffer) -> Result<(), String> {
    let group = buffer.remote_id.as_deref().and_then(Group::from_remote_id).ok_or("not a Google Chat conversation")?;
    handle.world().huddles.remove(&group);
    state.events.emit("googlechatHuddle", json!({ "bufferId": buffer.id, "huddle": Value::Null }));
    Ok(())
}
