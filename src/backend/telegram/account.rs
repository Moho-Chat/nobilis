//! The account itself: its profile as everybody sees it (name, bio, @username, picture), the sessions signed in to
//! it (moho's among them), and whether it shows as online.
//!
//! The profile is Telegram's, not moho's: the local name moho calls the account by is separate and stays as it was.
//! Sessions are the list Telegram's own Settings, Devices shows; ending one there and here is the same request.
//!
//! # Online
//!
//! Telegram shows somebody online while one of their apps says so and for a few minutes after; an app repeats it
//! while it is in use. Moho says it every few minutes while the account is set to online and the person has not
//! stepped away, and says "offline" once when either changes. Invisible is offline: Telegram has no state for being
//! present but unseen, and "last seen" then shows the moment moho last said it.

use super::send::explain;
use super::Handle;
use crate::state::AppState;
use grammers_client::tl;
use serde_json::{json, Value};
use std::time::Duration;

/// How often moho repeats that the account is online: inside the five minutes Telegram keeps it.
pub const ONLINE_EVERY: Duration = Duration::from_secs(210);

fn handle_of(state: &AppState, account_id: &str) -> Result<Handle, String> {
    state.runtime.telegram_handle(account_id).ok_or_else(|| "that Telegram account is not connected".to_string())
}

/// The profile as Telegram has it now.
pub async fn own_profile(state: &AppState, account_id: &str) -> Result<Value, String> {
    let handle = handle_of(state, account_id)?;
    let me = handle.client.get_me().await.map_err(|e| explain(&e))?;
    let full = handle.client.invoke(&tl::functions::users::GetFullUser { id: tl::enums::InputUser::UserSelf }).await.map_err(|e| explain(&e))?;
    let tl::enums::users::UserFull::Full(full) = full;
    let tl::enums::UserFull::Full(full) = full.full_user;
    let tl::enums::User::User(raw) = &me.raw else { return Err("Telegram did not describe this account".to_string()) };
    let peer = grammers_client::peer::Peer::User(me.clone());
    Ok(json!({
        "firstName": raw.first_name.clone().unwrap_or_default(),
        "lastName": raw.last_name.clone().unwrap_or_default(),
        "bio": full.about.unwrap_or_default(),
        "username": raw.username.clone().unwrap_or_default(),
        "phone": raw.phone.clone().map(|p| format!("+{p}")).unwrap_or_default(),
        "avatarUrl": super::media::avatar_url(handle.own.user_id, &peer),
    }))
}

/// What Telegram allows in a profile, counted as it counts.
const NAME_MAX: usize = 64;
const BIO_MAX: usize = 70;

/// The profile's fields, checked before they are sent: a first name is required, the rest may be empty.
pub fn checked_profile(first: Option<&str>, last: Option<&str>, bio: Option<&str>) -> Result<(Option<String>, Option<String>, Option<String>), String> {
    let first = first.map(|s| s.trim().to_string());
    let last = last.map(|s| s.trim().to_string());
    let bio = bio.map(|s| s.trim().to_string());
    if first.as_deref() == Some("") {
        return Err("Telegram needs a first name".to_string());
    }
    if [&first, &last].into_iter().flatten().any(|n| n.chars().count() > NAME_MAX) {
        return Err(format!("A name can be at most {NAME_MAX} characters"));
    }
    if bio.as_ref().is_some_and(|b| b.chars().count() > BIO_MAX) {
        return Err(format!("Telegram keeps a bio to {BIO_MAX} characters"));
    }
    Ok((first, last, bio))
}

/// Changes the name or the bio; what is not given stays as it is.
pub async fn set_profile(state: &AppState, account_id: &str, first: Option<&str>, last: Option<&str>, bio: Option<&str>) -> Result<(), String> {
    let (first_name, last_name, about) = checked_profile(first, last, bio)?;
    let handle = handle_of(state, account_id)?;
    handle.client.invoke(&tl::functions::account::UpdateProfile { first_name, last_name, about }).await.map_err(|e| explain(&e))?;
    // The account's name as moho shows it follows: it is Telegram's.
    if let Ok(me) = handle.client.get_me().await {
        let name = me.full_name();
        let _ = state.accounts.mutate_telegram(account_id, |a| a.name = name.clone());
        state.runtime.set_own_identity(account_id, &name);
    }
    Ok(())
}

/// Whether a @username is one Telegram would take: 5 to 32 letters, digits and underscores, starting with a letter,
/// not ending in an underscore. Empty is taking it off.
pub fn valid_username(name: &str) -> bool {
    name.is_empty()
        || ((5..=32).contains(&name.len())
            && name.starts_with(|c: char| c.is_ascii_alphabetic())
            && !name.ends_with('_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

/// Sets the account's @username, or takes it off when empty.
pub async fn set_username(state: &AppState, account_id: &str, name: &str) -> Result<(), String> {
    let name = name.trim().trim_start_matches('@');
    if !valid_username(name) {
        return Err("A username is 5 to 32 letters, digits and underscores, starting with a letter".to_string());
    }
    let handle = handle_of(state, account_id)?;
    handle.client.invoke(&tl::functions::account::UpdateUsername { username: name.to_string() }).await.map_err(|e| match &e {
        grammers_client::InvocationError::Rpc(r) if r.name == "USERNAME_OCCUPIED" => format!("@{name} is taken"),
        grammers_client::InvocationError::Rpc(r) if r.name == "USERNAME_INVALID" => "Telegram does not accept that username".to_string(),
        grammers_client::InvocationError::Rpc(r) if r.name == "USERNAME_NOT_MODIFIED" => "That is already the username".to_string(),
        grammers_client::InvocationError::Rpc(r) if r.name == "USERNAME_PURCHASE_AVAILABLE" => format!("@{name} is only sold on Fragment"),
        _ => explain(&e),
    })?;
    let _ = state.accounts.mutate_telegram(account_id, |a| a.username = Some(name.to_string()).filter(|n| !n.is_empty()));
    Ok(())
}

/// Puts a picture on the profile from a file, or takes the current one off when `path` is empty.
pub async fn set_photo(state: &AppState, account_id: &str, path: &str) -> Result<(), String> {
    let handle = handle_of(state, account_id)?;
    if path.is_empty() {
        handle
            .client
            .invoke(&tl::functions::photos::UpdateProfilePhoto { fallback: false, bot: None, id: tl::enums::InputPhoto::Empty })
            .await
            .map_err(|e| explain(&e))?;
        return Ok(());
    }
    let uploaded = handle.client.upload_file(path).await.map_err(|e| format!("could not read that picture: {e}"))?;
    handle
        .client
        .invoke(&tl::functions::photos::UploadProfilePhoto { fallback: false, bot: None, file: Some(uploaded.raw), video: None, video_start_ts: None, video_emoji_markup: None })
        .await
        .map_err(|e| match &e {
            grammers_client::InvocationError::Rpc(r) if r.name.starts_with("PHOTO_") || r.name == "IMAGE_PROCESS_FAILED" => "Telegram could not use that picture".to_string(),
            _ => explain(&e),
        })?;
    Ok(())
}

/// One signed-in session, in the shape the window lists.
fn session_of(a: &tl::types::Authorization) -> Value {
    let device = [a.device_model.trim(), a.platform.trim(), a.system_version.trim()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
    let app = format!("{} {}", a.app_name.trim(), a.app_version.trim()).trim().to_string();
    let place = [a.region.trim(), a.country.trim()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
    json!({
        // Telegram's own number for the session, as text: it is a 64-bit value and the window's numbers are not.
        "id": a.hash.to_string(),
        "app": app,
        "device": device,
        "place": place,
        "ip": a.ip,
        "lastActive": a.date_active,
        "created": a.date_created,
        "current": a.current,
        "official": a.official_app,
        "unconfirmed": a.unconfirmed,
    })
}

/// The sessions signed in to the account, this one first.
pub async fn sessions(state: &AppState, account_id: &str) -> Result<Value, String> {
    let handle = handle_of(state, account_id)?;
    let tl::enums::account::Authorizations::Authorizations(list) = handle.client.invoke(&tl::functions::account::GetAuthorizations {}).await.map_err(|e| explain(&e))?;
    Ok(Value::Array(ordered(list.authorizations.iter().map(|a| {
        let tl::enums::Authorization::Authorization(a) = a;
        session_of(a)
    }))))
}

/// The current session first, then the most recently used.
fn ordered(list: impl Iterator<Item = Value>) -> Vec<Value> {
    let mut list: Vec<Value> = list.collect();
    list.sort_by_key(|s| (!s["current"].as_bool().unwrap_or(false), -s["lastActive"].as_i64().unwrap_or(0)));
    list
}

/// Ends another session: it is signed out wherever it is.
pub async fn end_session(state: &AppState, account_id: &str, id: &str) -> Result<(), String> {
    let hash: i64 = id.trim().parse().map_err(|_| "that is not one of the account's sessions".to_string())?;
    if hash == 0 {
        return Err("This session is moho's own: remove the account to end it".to_string());
    }
    let handle = handle_of(state, account_id)?;
    handle.client.invoke(&tl::functions::account::ResetAuthorization { hash }).await.map_err(|e| match &e {
        // Telegram keeps a new session from ending others for its first day.
        grammers_client::InvocationError::Rpc(r) if r.name == "FRESH_RESET_AUTHORISATION_FORBIDDEN" => "Telegram does not let a session signed in less than a day ago end others yet".to_string(),
        grammers_client::InvocationError::Rpc(r) if r.name == "HASH_INVALID" => "That session has already ended".to_string(),
        _ => explain(&e),
    })?;
    Ok(())
}

/// Whether what the account shows as now is online to Telegram: online and do-not-disturb are, the rest are not.
pub fn shows_online(effective: &str) -> bool {
    matches!(effective, "online" | "dnd")
}

/// Tells Telegram whether the account is online now, from what it is set to and whether the person is away.
pub async fn apply_presence(state: &AppState, account_id: &str, handle: &Handle) {
    let online = shows_online(&state.runtime.effective_status(state, account_id));
    if let Err(e) = handle.client.invoke(&tl::functions::account::UpdateStatus { offline: !online }).await {
        tracing::debug!("telegram[{account_id}]: saying the account is {}: {}", if online { "online" } else { "offline" }, explain(&e));
    }
}

/// The same, from outside the connection (a status chosen, the person away or back): applied if it is connected.
pub fn presence_changed(state: &AppState, account_id: &str) -> bool {
    let Some(handle) = state.runtime.telegram_handle(account_id) else { return false };
    let (state, account_id) = (state.clone(), account_id.to_string());
    tokio::spawn(async move { apply_presence(&state, &account_id, &handle).await });
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_profile_needs_a_first_name_and_keeps_to_telegrams_lengths() {
        assert_eq!(checked_profile(Some(" Ada "), None, Some(" hi ")), Ok((Some("Ada".into()), None, Some("hi".into()))));
        assert!(checked_profile(Some("  "), None, None).is_err(), "an empty first name");
        assert!(checked_profile(None, Some(""), None).is_ok(), "an empty last name takes it off");
        assert!(checked_profile(Some(&"x".repeat(65)), None, None).is_err());
        assert!(checked_profile(None, None, Some(&"é".repeat(71))).is_err());
        assert!(checked_profile(None, None, Some(&"é".repeat(70))).is_ok(), "counted in characters, not bytes");
    }

    #[test]
    fn usernames_follow_telegrams_rules() {
        for good in ["", "ada_l", "Ada12345", "a1234"] {
            assert!(valid_username(good), "{good}");
        }
        for bad in ["ada", "1adal", "ada__", "ada l", "ada-l", &"a".repeat(33)] {
            assert!(!valid_username(bad), "{bad}");
        }
    }

    fn auth(hash: i64, current: bool, active: i32) -> tl::types::Authorization {
        tl::types::Authorization {
            current,
            official_app: !current,
            password_pending: false,
            encrypted_requests_disabled: false,
            call_requests_disabled: false,
            unconfirmed: false,
            hash,
            device_model: "Pixel 9a".into(),
            platform: "Android".into(),
            system_version: "".into(),
            api_id: 1,
            app_name: "Telegram Android".into(),
            app_version: "11.0".into(),
            date_created: 1,
            date_active: active,
            ip: "192.0.2.1".into(),
            country: "Nowhere".into(),
            region: "".into(),
        }
    }

    #[test]
    fn sessions_list_this_one_first_then_the_most_recent() {
        let list = ordered([auth(5, false, 10), auth(0, true, 1), auth(7, false, 30)].iter().map(session_of));
        let ids: Vec<&str> = list.iter().map(|s| s["id"].as_str().unwrap()).collect();
        assert_eq!(ids, ["0", "7", "5"]);
        assert_eq!(list[1]["device"], "Pixel 9a · Android", "an empty part is left out");
        assert_eq!(list[1]["app"], "Telegram Android 11.0");
        assert_eq!(list[1]["place"], "Nowhere");
    }

    #[test]
    fn online_and_do_not_disturb_show_as_online_and_nothing_else_does() {
        assert!(shows_online("online") && shows_online("dnd"));
        assert!(!shows_online("idle") && !shows_online("invisible"));
    }
}
