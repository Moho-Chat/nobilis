//! Who somebody is, and what this account shows of itself.

use super::*;

/// Who somebody is, as far as Matrix will say.
///
/// Three sources, because Matrix keeps them apart: the profile endpoint has
/// the name and the picture, the room's power levels have their standing in
/// this room, and presence has when they were last seen. Presence is often
/// refused - a server may not federate it, or may have it switched off - and
/// that is not an error, just an absence.
pub async fn profile(state: &AppState, account_id: &str, buffer_id: &str, user_id: &str) -> Value {
    let mut profile = crate::profile::pending("matrix", account_id, user_id);
    profile["pending"] = serde_json::json!(false);
    profile["id"] = Value::String(user_id.to_string());
    profile["handle"] = Value::String(user_id.to_string());
    profile["name"] = Value::String(protocol::mxid_localpart(user_id));

    let Some(account) = state.accounts.get_matrix(account_id) else { return profile };
    let base = account.homeserver_url.trim_end_matches('/');
    let encoded_user = url::form_urlencoded::byte_serialize(user_id.as_bytes()).collect::<String>();

    if let Ok(resp) = http::get_json(&format!("{base}/_matrix/client/v3/profile/{encoded_user}"), &account.access_token).await {
        if let Some(name) = resp["displayname"].as_str().filter(|n| !n.is_empty()) {
            profile["name"] = Value::String(name.to_string());
        }
        if let Some(mxc) = resp["avatar_url"].as_str() {
            if let Some(path) = cached_media_path(&account.homeserver_url, &account.access_token, mxc, "").await {
                profile["avatarUrl"] = Value::String(path);
            }
        }
    }

    // Their standing in this room, from what the sync already carries.
    if let Some(room_id) = state.runtime.get_matrix_room(buffer_id) {
        if let Some(levels) = state.runtime.get_matrix_power_levels(account_id, &room_id) {
            let creators = state.runtime.matrix_room_creators(account_id, &room_id);
            let version = state.runtime.matrix_room_version(account_id, &room_id).unwrap_or_default();
            let level = moderation::effective_power(&levels, user_id, &creators, &version);
            // Matrix's own conventional names for the two ranks anybody
            // recognises. A room can set any number, so a level that is
            // neither is reported as itself rather than rounded to a word.
            let role = match level {
                // The room's own creator, who from version 12 outranks every
                // number rather than holding a large one - so it is named
                // rather than printed, which would be a wall of digits.
                moderation::CREATOR_POWER => Some("Owner".to_string()),
                100 => Some("Admin".to_string()),
                50 => Some("Moderator".to_string()),
                0 => None,
                other => Some(format!("Power level {other}")),
            };
            if let Some(role) = role {
                profile["roles"] = serde_json::json!([role]);
            }
            profile["isModerator"] = serde_json::json!(level >= 50);
        }
    }

    // When they were last seen. `last_active_ago` is milliseconds, and is the
    // only "last seen" Matrix has - there is no join date to be had without
    // walking the room's whole state history.
    if let Ok(resp) = http::get_json(&format!("{base}/_matrix/client/v3/presence/{encoded_user}/status"), &account.access_token).await {
        if let Some(status) = resp["presence"].as_str() {
            profile["status"] = Value::String(status.to_string());
        }
        if let Some(ago) = resp["last_active_ago"].as_i64() {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            profile["lastActiveTs"] = serde_json::json!(now - ago / 1000);
        }
        if let Some(message) = resp["status_msg"].as_str().filter(|m| !m.is_empty()) {
            crate::profile::note(&mut profile, "Status", message);
        }
    }

    profile
}

/// Uploads a local file to the homeserver's media repository and builds
/// the corresponding `m.room.message` content (`m.image`/`m.video`/
/// `m.audio`/`m.file`) - unencrypted rooms only, see
/// Changes what this account is called, for everyone.
///
/// The homeserver's copy rather than the local rename beside it in the
/// account panel: one is how moho refers to you and the other is what every
/// room you are in shows. Both exist on purpose, and only this one leaves the
/// machine.
pub async fn set_own_display_name(state: &AppState, account_id: &str, name: &str) -> Result<()> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/profile/{}/displayname",
        url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>()
    );
    http::put_json(&url, &account.access_token, serde_json::json!({ "displayname": name }))
        .await
        .context("changing your display name")?;
    Ok(())
}

/// Changes this account's picture, for everyone.
///
/// Uploaded unencrypted on purpose, unlike an attachment: a profile picture
/// is shown to anybody who can see the account at all, including in rooms
/// this client has never been in, and there is nobody to share a key with.
pub async fn set_own_avatar(state: &AppState, account_id: &str, path: &str) -> Result<String> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let bytes = tokio::fs::read(path).await.context("reading the picture")?;
    let (filename, ext) = file_extension(path);
    let (_msgtype, mime) = media_msgtype_and_mime(&ext);
    let upload_url = format!(
        "{base}/_matrix/media/v3/upload?filename={}",
        url::form_urlencoded::byte_serialize(filename.as_bytes()).collect::<String>()
    );
    let resp = http_client_post_bytes(&upload_url, &account.access_token, mime, bytes)
        .await
        .context("uploading the picture")?;
    let content_uri = resp["content_uri"].as_str().context("the server took the picture but did not say where")?;

    let url = format!(
        "{base}/_matrix/client/v3/profile/{}/avatar_url",
        url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>()
    );
    http::put_json(&url, &account.access_token, serde_json::json!({ "avatar_url": content_uri }))
        .await
        .context("setting your picture")?;
    Ok(content_uri.to_string())
}

/// What the homeserver currently says this account is called and looks like.
pub async fn own_profile(state: &AppState, account_id: &str) -> Result<Value> {
    let account = state.accounts.get_matrix(account_id).context("account not connected")?;
    let base = account.homeserver_url.trim_end_matches('/');
    let url = format!(
        "{base}/_matrix/client/v3/profile/{}",
        url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>()
    );
    let profile = http::get_json(&url, &account.access_token).await.context("reading your profile")?;
    let avatar = match profile["avatar_url"].as_str() {
        Some(mxc) => cached_media_path(&account.homeserver_url, &account.access_token, mxc, "").await,
        None => None,
    };
    Ok(serde_json::json!({
        "accountId": account_id,
        "userId": account.user_id,
        "displayName": profile["displayname"].as_str().unwrap_or_default(),
        "avatarUrl": avatar,
    }))
}

/// The full member list of a room, needed to know who to establish Olm
/// sessions with / share the Megolm session with before sending into an
/// encrypted room. Fetched fresh per send rather than tracked incrementally
/// from `m.room.member` timeline events - simpler, and an extra round trip
/// per encrypted send is an acceptable cost for v1 (this project's other
/// backends make comparable per-send round trips already, e.g. Discord's
/// own REST send call).
pub(super) async fn joined_member_ids(base: &str, access_token: &str, room_id: &str) -> Result<Vec<ruma_common::OwnedUserId>> {
    let encoded_room_id = url::form_urlencoded::byte_serialize(room_id.as_bytes()).collect::<String>();
    let resp = http::get_json(&format!("{base}/_matrix/client/v3/rooms/{encoded_room_id}/joined_members"), access_token)
        .await
        .context("fetching joined_members")?;
    let members = resp["joined"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.keys())
        .filter_map(|id| ruma_common::OwnedUserId::try_from(id.as_str()).ok())
        .collect();
    Ok(members)
}

/// Pushes a status to the homeserver.
///
/// Matrix has three presence values - online, unavailable and offline. Idle
/// maps to unavailable, which is the closest honest answer to "am I here".
pub async fn apply_status(state: &AppState, config: &MatrixAccountConfig, status: &str) -> Result<()> {
    // Re-read rather than trusting the config passed in: a re-login rotates
    // the token, and a stale one fails with a bare 401.
    let account = state
        .accounts
        .get_matrix(&config.account_id())
        .context("account is no longer configured")?;
    let access_token = account.access_token;
    // Matrix has three, and they do not line up one for one. Do-not-disturb
    // is somebody present who does not want interrupting, which is closest to
    // unavailable; invisible has no equivalent at all, and offline is the
    // honest answer - it is what invisible means to everybody looking.
    let presence = match status {
        "idle" | "dnd" => "unavailable",
        "invisible" => "offline",
        _ => "online",
    };
    let user = url::form_urlencoded::byte_serialize(account.user_id.as_bytes()).collect::<String>();
    let base = account.homeserver_url.trim_end_matches('/');
    http::put_json(
        &format!("{base}/_matrix/client/v3/presence/{user}/status"),
        &access_token,
        serde_json::json!({ "presence": presence }),
    )
    .await
    .context("setting presence")?;
    Ok(())
}
