//! The account's own profile: what others see of it, and changing that - display name, pronouns, the line under the
//! name, the picture, and how present it is.

use super::media;
use super::Handle;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// A text field as Stoat has it: some words, or nothing.
pub fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        // A list of them is written as one line.
        Value::Array(list) => {
            let joined: Vec<&str> = list.iter().filter_map(Value::as_str).collect();
            (!joined.is_empty()).then(|| joined.join(", "))
        }
        _ => None,
    }
}

/// Stoat's name for each of moho's four statuses. Its "Focus" is the one that only wants to hear when mentioned, which
/// is nearest to moho's do-not-disturb; "Busy" asks for no notifications at all, which is what that means here.
pub fn presence_of(status: &str) -> &'static str {
    match status {
        "idle" => "Idle",
        "focus" => "Focus",
        "dnd" => "Busy",
        "invisible" => "Invisible",
        _ => "Online",
    }
}

/// moho's word for a presence Stoat says of the account.
pub fn status_from(presence: &str) -> Option<&'static str> {
    Some(match presence {
        "Online" => "online",
        "Idle" => "idle",
        "Focus" => "focus",
        "Busy" => "dnd",
        "Invisible" => "invisible",
        _ => return None,
    })
}

/// How the account looks to others, with what may be changed.
pub async fn get(handle: &Handle) -> Result<Value> {
    let own = handle.world.lock().unwrap_or_else(|e| e.into_inner()).own_id.clone();
    let profile_path = format!("/users/{own}/profile");
    let (me, bio) = tokio::join!(handle.api.get("/users/@me", &[]), handle.api.get(&profile_path, &[]));
    let me = me.map_err(|e| anyhow!("{e}"))?;
    let avatar = me["avatar"]["_id"].as_str().and_then(|f| media::file_url(&handle.instance, "avatars", f)).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, None));
    Ok(json!({
        "accountId": handle.account_id,
        "userId": own,
        "handle": format!("{}#{}", me["username"].as_str().unwrap_or(""), me["discriminator"].as_str().unwrap_or("")),
        "displayName": me["display_name"].as_str().unwrap_or(""),
        "pronouns": text(&me["pronouns"]).unwrap_or_default(),
        "bio": bio.ok().and_then(|b| text(&b["content"])).unwrap_or_default(),
        "statusText": me["status"]["text"].as_str().unwrap_or(""),
        "avatarUrl": avatar,
    }))
}

/// What a change of the profile asks of Stoat: the fields to set, and the ones to take away (a field emptied is removed,
/// since Stoat would take an empty string as a value).
pub fn change(display_name: Option<&str>, pronouns: Option<&str>, bio: Option<&str>, status_text: Option<&str>) -> Value {
    let mut body = json!({});
    let mut remove: Vec<&str> = Vec::new();
    let mut field = |value: Option<&str>, set: &mut dyn FnMut(&str), gone: &'static str| {
        if let Some(v) = value.map(str::trim) {
            if v.is_empty() {
                remove.push(gone);
            } else {
                set(v);
            }
        }
    };
    field(display_name, &mut |v| body["display_name"] = json!(v), "DisplayName");
    field(pronouns, &mut |v| body["pronouns"] = json!(v.chars().take(24).collect::<String>()), "Pronouns");
    field(bio, &mut |v| body["profile"] = json!({ "content": v }), "ProfileContent");
    field(status_text, &mut |v| body["status"] = json!({ "text": v }), "StatusText");
    if !remove.is_empty() {
        body["remove"] = json!(remove);
    }
    body
}

/// Changes the profile. Only what is given is touched.
pub async fn set(handle: &Handle, display_name: Option<&str>, pronouns: Option<&str>, bio: Option<&str>, status_text: Option<&str>) -> Result<()> {
    let body = change(display_name, pronouns, bio, status_text);
    if body.as_object().is_some_and(|o| o.is_empty()) {
        return Ok(());
    }
    handle.api.patch("/users/@me", &body).await.map_err(|e| anyhow!("{e}"))?;
    Ok(())
}

/// Puts a picture from a file on the account.
pub async fn set_avatar(handle: &Handle, path: &str) -> Result<String> {
    let server = handle.instance.autumn.clone().ok_or_else(|| anyhow!("this instance has no file server"))?;
    let name = super::send::sent_name(path, false);
    let size = std::fs::metadata(path).map_err(|e| anyhow!("{path}: {e}"))?.len();
    super::limits::may_use_picture("avatar", size, handle.limits()).map_err(|e| anyhow!(e))?;
    let bytes = tokio::fs::read(path).await.map_err(|e| anyhow!("reading {path}: {e}"))?;
    let id = handle.api.upload(&server, "avatars", &name, super::send::content_type_of(&name), bytes).await.map_err(|e| anyhow!("{e}"))?;
    handle.api.patch("/users/@me", &json!({ "avatar": id })).await.map_err(|e| anyhow!("{e}"))?;
    Ok(media::file_url(&handle.instance, "avatars", &id).map(|u| media::cached(&handle.account_id, media::AVATARS, &u, None, None)).unwrap_or_default())
}

/// Takes the picture off.
pub async fn clear_avatar(handle: &Handle) -> Result<()> {
    handle.api.patch("/users/@me", &json!({ "remove": ["Avatar"] })).await.map(|_| ()).map_err(|e| anyhow!("{e}"))
}

/// Says how present the account is, and what it says beside that where it has been given something to say.
pub async fn apply_status(handle: &Handle, status: &str, text: Option<&str>) -> bool {
    let mut body = change(None, None, None, text);
    body["status"] = {
        let mut s = body["status"].take();
        if !s.is_object() {
            s = json!({});
        }
        s["presence"] = json!(presence_of(status));
        s
    };
    handle.api.patch("/users/@me", &body).await.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_given_is_set_and_one_emptied_is_removed_and_one_not_given_is_left() {
        let body = change(Some(" Ada "), Some("she, her"), Some(""), None);
        assert_eq!(body["display_name"], "Ada");
        assert_eq!(body["pronouns"], json!("she, her"), "one line, and at most 24 characters");
        assert_eq!(change(None, Some(&"x".repeat(40)), None, None)["pronouns"], json!("x".repeat(24)));
        assert_eq!(body["remove"], json!(["ProfileContent"]));
        assert!(body.get("status").is_none() && body.get("profile").is_none());
        assert_eq!(change(None, None, None, None), json!({}));
        assert_eq!(change(None, None, None, Some("out"))["status"], json!({ "text": "out" }));
        assert_eq!(change(None, None, None, Some("  "))["remove"], json!(["StatusText"]));
    }

    #[test]
    fn moho_s_statuses_are_stoat_s() {
        assert_eq!((presence_of("online"), presence_of("idle"), presence_of("dnd"), presence_of("invisible")), ("Online", "Idle", "Busy", "Invisible"));
        assert_eq!(presence_of("focus"), "Focus");
        for word in ["online", "idle", "focus", "dnd", "invisible"] {
            assert_eq!(status_from(presence_of(word)), Some(word), "{word} comes back as itself");
        }
        assert_eq!(status_from("Something"), None);
    }

    #[test]
    fn pronouns_may_come_as_one_string_or_a_list() {
        assert_eq!(text(&json!(["he", "him"])).as_deref(), Some("he, him"));
        assert_eq!(text(&json!("they/them")).as_deref(), Some("they/them"));
        assert_eq!(text(&json!([])), None);
    }
}
