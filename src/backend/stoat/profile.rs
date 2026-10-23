//! The card for a person: their name, handle, picture, what they wrote about themselves, the badges they hold, when
//! they made the account and joined the server, and who and what they have in common with this account.

use super::members::status_of;
use super::world::{Kind, User};
use super::{media, messages};
use crate::profile;
use crate::state::AppState;
use serde_json::{json, Value};

/// What Stoat's badge bits stand for.
pub fn badge_names(flags: u64) -> Vec<&'static str> {
    [
        (1, "Stoat developer"),
        (2, "Translator"),
        (4, "Supporter"),
        (8, "Responsible disclosure"),
        (16, "Founder"),
        (32, "Platform moderator"),
        (64, "Active supporter"),
        (128, "Paw"),
        (256, "Early adopter"),
    ]
    .iter()
    .filter(|(bit, _)| flags & bit != 0)
    .map(|(_, name)| *name)
    .collect()
}

/// The person a card is for: the other one in a direct message, or the member the window named by id.
fn target_of(state: &AppState, buffer_id: &str, user_id: &str, own: &str) -> Option<String> {
    let buffer = state.runtime.get_buffer(buffer_id)?;
    if buffer.kind == "dm" {
        let handle = state.runtime.stoat_handle(&buffer.account_id)?;
        let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        let channel = world.channels.get(buffer.remote_id.as_deref()?)?;
        if channel.kind == Kind::Direct {
            return channel.recipients.iter().find(|r| *r != own).cloned();
        }
    }
    (user_id.len() == 26).then(|| user_id.to_string())
}

pub async fn build(state: &AppState, account_id: &str, buffer_id: &str, who: &str, user_id: &str) -> Value {
    let mut card = profile::pending("stoat", account_id, who);
    card["pending"] = json!(false);
    let Some(handle) = state.runtime.stoat_handle(account_id) else { return card };
    let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
    let Some(target) = target_of(state, buffer_id, user_id, &own) else { return card };
    card["id"] = json!(target);
    if let Some(ms) = messages::ulid_ms(&target) {
        card["createdTs"] = json!(ms / 1000);
    }
    let (path, profile_path, mutual_path) = (format!("/users/{target}"), format!("/users/{target}/profile"), format!("/users/{target}/mutual"));
    let (full, bio, mutual) = tokio::join!(handle.api.get(&path, &[]), handle.api.get(&profile_path, &[]), handle.api.get(&mutual_path, &[]));
    if let Ok(user) = &full {
        if let Some((id, parsed)) = super::world::parse_user(user) {
            let handle_text = format!("{}#{}", user["username"].as_str().unwrap_or(""), user["discriminator"].as_str().unwrap_or(""));
            card["name"] = json!(parsed.name);
            card["handle"] = json!(handle_text);
            let avatar = parsed.avatar.as_deref().and_then(|f| media::file_url(&handle.instance, "avatars", f)).map(|u| media::cached(account_id, media::AVATARS, &u, None, None));
            profile::set(&mut card, "avatarUrl", avatar.map(|u| json!(u)));
            let known: User = {
                let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
                world.users.entry(id).and_modify(|u| u.online = parsed.online).or_insert(parsed.clone());
                parsed
            };
            card["status"] = json!(status_of(&known));
            profile::set(&mut card, "activity", known.status_text.as_ref().map(|t| json!({ "kind": "custom", "text": t })));
            if known.bot {
                profile::note(&mut card, "Account", "A bot");
            }
        }
        let badges = badge_names(user["badges"].as_u64().unwrap_or(0));
        if !badges.is_empty() {
            profile::note(&mut card, "Badges", badges.join(", "));
        }
        if let Some(text) = user["status"]["text"].as_str() {
            profile::note(&mut card, "Status", text);
        }
    }
    if let Ok(bio) = &bio {
        if let Some(about) = bio["content"].as_str() {
            profile::note(&mut card, "About", about);
        }
    }
    if let Ok(mutual) = &mutual {
        let count = |key: &str| mutual[key].as_array().map(|l| l.len()).unwrap_or(0);
        if count("servers") > 0 {
            profile::note(&mut card, "Servers in common", count("servers").to_string());
        }
        if count("users") > 0 {
            profile::note(&mut card, "Friends in common", count("users").to_string());
        }
    }
    // When they joined the server this was asked in.
    let server = state.runtime.get_buffer(buffer_id).and_then(|b| b.remote_id).and_then(|c| handle.world.lock().unwrap_or_else(|e| e.into_inner()).channels.get(&c).and_then(|ch| ch.server.clone()));
    if let Some(server) = server {
        if let Ok(member) = handle.api.get(&format!("/servers/{server}/members/{target}"), &[]).await {
            if let Some(t) = member["joined_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
                card["joinedTs"] = json!(t.timestamp());
            }
            let world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = world.servers.get(&server) {
                let mut held: Vec<(i64, String)> = member["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str()).filter_map(|r| s.roles.get(r).map(|role| (role.rank, role.name.clone()))).collect();
                held.sort();
                if !held.is_empty() {
                    card["roles"] = json!(held.into_iter().map(|(_, n)| n).collect::<Vec<_>>());
                }
            }
        }
    }
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_are_named_by_their_bits() {
        assert_eq!(badge_names(0), Vec::<&str>::new());
        assert_eq!(badge_names(1 | 16), vec!["Stoat developer", "Founder"]);
        assert_eq!(badge_names(256), vec!["Early adopter"]);
        assert_eq!(badge_names(1 << 20), Vec::<&str>::new(), "a bit nobody has named is not a badge");
    }
}
