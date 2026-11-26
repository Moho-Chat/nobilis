//! The card for a person: their name, handle, picture, pronouns and what they wrote about themselves, their badges,
//! when they made the account and joined the guild, their roles there, and what they have in common with this account
//! (`GET /users/{id}/profile`, with the guild and the mutual lists asked for).

use super::members::status_word;
use super::world::Kind;
use super::{media, messages};
use crate::profile;
use crate::state::AppState;
use serde_json::{json, Value};

/// The person a card is for: the other one in a direct message, or the member the window named by id.
fn target_of(state: &AppState, buffer_id: &str, user_id: &str, own: &str) -> Option<String> {
    let buffer = state.runtime.get_buffer(buffer_id)?;
    if buffer.kind == "dm" && user_id.parse::<u64>().is_err() {
        let handle = state.runtime.fluxer_handle(&buffer.account_id)?;
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let channel = world.channels.get(buffer.remote_id.as_deref()?)?;
        if channel.kind == Kind::Direct {
            return channel.recipients.iter().find(|r| *r != own).cloned();
        }
    }
    user_id.parse::<u64>().is_ok().then(|| user_id.to_string())
}

pub async fn build(state: &AppState, account_id: &str, buffer_id: &str, who: &str, user_id: &str) -> Value {
    let mut card = profile::pending("fluxer", account_id, who);
    card["pending"] = json!(false);
    let Some(handle) = state.runtime.fluxer_handle(account_id) else { return card };
    let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
    let Some(target) = target_of(state, buffer_id, user_id, &own) else { return card };
    card["id"] = json!(target);
    if let Some(secs) = messages::snowflake_secs(&target) {
        card["createdTs"] = json!(secs);
    }
    let guild = state.runtime.get_buffer(buffer_id).and_then(|b| b.remote_id).and_then(|c| handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(&c).and_then(|ch| ch.guild.clone()));
    let mut query = vec![("with_mutual_friends", "true".to_string()), ("with_mutual_guilds", "true".to_string())];
    if let Some(g) = &guild {
        query.push(("guild_id", g.clone()));
    }
    // What is already known of them, so the card says who they are (and what can be done about them) even when Fluxer
    // will not give the full profile - as it will not for somebody blocked who shares nothing with the account.
    if let Some(user) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.get(&target).cloned() {
        card["name"] = json!(user.shown());
        if !user.username.is_empty() {
            card["handle"] = json!(match Some(user.discriminator.as_str()).filter(|d| !d.is_empty() && d.chars().any(|c| c != '0')) {
                Some(tag) => format!("{}#{tag}", user.username),
                None => user.username.clone(),
            });
        }
        let avatar = user.avatar.as_deref().and_then(|h| media::asset_url(&handle.instance, "avatars", &target, h)).map(|u| media::cached(account_id, media::AVATARS, &u, None, Some("image/webp")));
        profile::set(&mut card, "avatarUrl", avatar.map(|u| json!(u)));
    }
    // What they are to this account, for the card's friend and block buttons.
    {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let relation = if target == world.own_id {
            None
        } else if world.friends.contains(&target) {
            Some("Friend")
        } else if world.blocked.contains(&target) {
            Some("Blocked")
        } else if world.incoming.contains(&target) {
            Some("Incoming")
        } else if world.outgoing.contains(&target) {
            Some("Outgoing")
        } else {
            Some("None")
        };
        profile::set(&mut card, "relationship", relation.map(|r| json!(r)));
        if target != world.own_id {
            card["userNote"] = json!(world.notes.get(&target).cloned().unwrap_or_default());
        }
    }
    let Ok(full) = handle.api.get(&format!("/users/{target}/profile"), &query).await else { return card };
    let user = &full["user"];
    if let Some((id, parsed)) = super::world::parse_user(user) {
        card["name"] = json!(parsed.shown());
        card["handle"] = json!(super::login::username_of(user, &handle.instance));
        let avatar = parsed.avatar.as_deref().and_then(|h| media::asset_url(&handle.instance, "avatars", &id, h)).map(|u| media::cached(account_id, media::AVATARS, &u, None, Some("image/webp")));
        profile::set(&mut card, "avatarUrl", avatar.map(|u| json!(u)));
        if parsed.bot {
            profile::note(&mut card, "Account", "A bot");
        }
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).users.insert(id, parsed);
    }
    let (status, custom) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).presence.get(&target).cloned().unwrap_or(("offline".to_string(), None));
    card["status"] = json!(status_word(&status));
    profile::set(&mut card, "activity", custom.filter(|t| !t.is_empty()).map(|t| json!({ "kind": "custom", "text": t })));
    let guild_profile = &full["guild_member_profile"];
    let account_profile = &full["user_profile"];
    let pick = |key: &str| guild_profile[key].as_str().filter(|s| !s.is_empty()).or_else(|| account_profile[key].as_str().filter(|s| !s.is_empty()));
    if let Some(pronouns) = pick("pronouns") {
        profile::note(&mut card, "Pronouns", pronouns);
    }
    if let Some(bio) = pick("bio") {
        profile::note(&mut card, "About", bio);
    }
    let badges: Vec<String> = full["profile_badges"].as_array().into_iter().flatten().filter_map(|b| b["description"].as_str().or_else(|| b["id"].as_str()).map(str::to_string)).collect();
    if !badges.is_empty() {
        profile::note(&mut card, "Badges", badges.join(", "));
    }
    let count = |key: &str| full[key].as_array().map(Vec::len).unwrap_or(0);
    if count("mutual_guilds") > 0 {
        profile::note(&mut card, "Communities in common", count("mutual_guilds").to_string());
    }
    if count("mutual_friends") > 0 {
        profile::note(&mut card, "Friends in common", count("mutual_friends").to_string());
    }
    let member = &full["guild_member"];
    if let Some(t) = member["joined_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
        card["joinedTs"] = json!(t.timestamp());
    }
    if let Some(g) = &guild {
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(gl) = world.guilds.get(g) {
            let mut held: Vec<(i64, String)> = member["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).filter_map(|r| gl.roles.get(r).map(|role| (-role.position, role.name.clone()))).collect();
            held.sort();
            if !held.is_empty() {
                card["roles"] = json!(held.into_iter().map(|(_, n)| n).collect::<Vec<_>>());
            }
        }
    }
    card
}

/// Sets or clears this account's private note on another Fluxer user (`PUT /users/@me/notes/{id}`).
pub async fn set_note(handle: &super::Handle, user_id: &str, note: &str) -> anyhow::Result<()> {
    let trimmed = note.trim();
    handle.api.put(&format!("/users/@me/notes/{user_id}"), Some(&json!({ "note": trimmed }))).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
    if trimmed.is_empty() {
        world.notes.remove(user_id);
    } else {
        world.notes.insert(user_id.to_string(), trimmed.to_string());
    }
    Ok(())
}
