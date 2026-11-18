//! The account's own presence: the status chosen in moho, published on the gateway (op 3, `gateway/commands.md`) with
//! the line beside it as the custom status. Fluxer keeps five of these per twenty seconds per connection and drops the
//! rest, so a change is sent once, not repeated.

use super::{media, Handle};
use anyhow::{anyhow, Result};
use base64::Engine;
use serde_json::{json, Value};

/// Fluxer's word for a status chosen in moho.
pub fn status_of(status: &str) -> &'static str {
    match status {
        "idle" | "away" => "idle",
        "dnd" | "busy" => "dnd",
        "invisible" | "offline" => "invisible",
        _ => "online",
    }
}

/// The presence command: the status, and the custom status when a line is given (an empty line clears it).
pub fn command(status: &str, text: Option<&str>) -> Value {
    let mut d = json!({ "status": status_of(status), "afk": false, "mobile": false });
    if let Some(text) = text {
        let text = text.trim();
        d["custom_status"] = if text.is_empty() { Value::Null } else { json!({ "text": text.chars().take(128).collect::<String>() }) };
    }
    json!({ "op": 3, "d": d })
}

pub async fn apply_status(handle: &Handle, status: &str, text: Option<&str>) -> bool {
    handle.gateway.send(command(status, text).to_string()).is_ok()
}

/// The account's own profile, as the account tools in the window show it (the same shape as Stoat's).
pub async fn get(handle: &Handle) -> Result<Value> {
    let me = handle.api.get("/users/@me", &[]).await.map_err(|e| anyhow!("{e}"))?;
    let own = me["id"].as_str().unwrap_or("").to_string();
    let avatar = me["avatar"].as_str().and_then(|h| media::asset_url(&handle.instance, "avatars", &own, h)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp")));
    let (_, custom) = handle.world.lock().unwrap_or_else(|e| e.into_inner()).presence.get(&own).cloned().unwrap_or(("online".to_string(), None));
    Ok(json!({
        "accountId": handle.account_id,
        "userId": own,
        "handle": super::login::username_of(&me, &handle.instance),
        "displayName": me["global_name"].as_str().unwrap_or(""),
        "pronouns": me["pronouns"].as_str().unwrap_or(""),
        "bio": me["bio"].as_str().unwrap_or(""),
        "statusText": custom.unwrap_or_default(),
        "avatarUrl": avatar,
    }))
}

/// What a change of the profile asks of Fluxer: a field given is set, one emptied is cleared (null), one not given is left.
pub fn change(display_name: Option<&str>, pronouns: Option<&str>, bio: Option<&str>) -> Value {
    let mut body = json!({});
    for (key, value, most) in [("global_name", display_name, 32usize), ("pronouns", pronouns, 40), ("bio", bio, 1000)] {
        if let Some(v) = value.map(str::trim) {
            body[key] = if v.is_empty() { Value::Null } else { json!(v.chars().take(most).collect::<String>()) };
        }
    }
    body
}

/// Changes the profile; the line beside the status is the presence's custom status.
pub async fn set(handle: &Handle, display_name: Option<&str>, pronouns: Option<&str>, bio: Option<&str>, status_text: Option<&str>) -> Result<()> {
    let body = change(display_name, pronouns, bio);
    if body.as_object().is_some_and(|o| !o.is_empty()) {
        handle.api.patch("/users/@me", &body).await.map_err(|e| anyhow!("{e}"))?;
    }
    if let Some(text) = status_text {
        let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
        let status = handle.world.lock().unwrap_or_else(|e| e.into_inner()).presence.get(&own).map(|p| p.0.clone()).unwrap_or_else(|| "online".to_string());
        handle.world.lock().unwrap_or_else(|e| e.into_inner()).presence.insert(own, (status.clone(), Some(text.trim().to_string()).filter(|t| !t.is_empty())));
        apply_status(handle, &status, Some(text)).await;
    }
    Ok(())
}

/// Puts a picture on the account (from a file), and answers its address.
pub async fn set_avatar(handle: &Handle, path: &str) -> Result<Option<String>> {
    let bytes = tokio::fs::read(path).await.map_err(|e| anyhow!("reading {path}: {e}"))?;
    let name = super::send::sent_name(path, false);
    let uri = format!("data:{};base64,{}", crate::backend::stoat::send::content_type_of(&name), base64::engine::general_purpose::STANDARD.encode(bytes));
    let me = handle.api.patch("/users/@me", &json!({ "avatar": uri })).await.map_err(|e| anyhow!("{e}"))?;
    let own = me["id"].as_str().unwrap_or("").to_string();
    Ok(me["avatar"].as_str().and_then(|h| media::asset_url(&handle.instance, "avatars", &own, h)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, Some("image/webp"))))
}

pub async fn clear_avatar(handle: &Handle) -> Result<()> {
    handle.api.patch("/users/@me", &json!({ "avatar": null })).await.map(|_| ()).map_err(|e| anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_is_said_in_fluxers_words_and_an_empty_line_clears_the_custom_status() {
        assert_eq!(command("dnd", None), json!({ "op": 3, "d": { "status": "dnd", "afk": false, "mobile": false } }));
        assert_eq!(command("invisible", Some(""))["d"]["custom_status"], Value::Null);
        assert_eq!(command("online", Some(" lunch "))["d"]["custom_status"]["text"], "lunch");
        assert_eq!(status_of("whatever"), "online");
    }

    #[test]
    fn a_profile_field_given_is_set_one_emptied_is_cleared_and_one_not_given_is_left() {
        let body = change(Some(" Ada "), Some(""), None);
        assert_eq!(body, json!({ "global_name": "Ada", "pronouns": null }));
        assert_eq!(change(None, None, None), json!({}));
    }
}
