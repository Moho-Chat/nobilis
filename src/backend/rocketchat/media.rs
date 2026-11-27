//! Rocket.Chat file uploads (`POST /api/v1/rooms.upload/{rid}`), voice notes,
//! authenticated file downloads (`/file-upload/{id}/{name}`), and custom server emoji (`emoji-custom.list`).

use super::world::CustomEmoji;
use super::Handle;
use crate::model::{EmojiEntry, EmojiSource};
use crate::state::AppState;
use anyhow::Result;
use serde_json::Value;

/// Upload a file or recorded voice note to a Rocket.Chat room (`POST /api/v1/rooms.upload/{rid}`).
/// For E2EE rooms, encrypts file bytes with AES-GCM before uploading and attaches encryption metadata.
pub async fn upload_file(
    handle: &Handle,
    room_id: &str,
    filename: &str,
    mime_type: &str,
    bytes: Vec<u8>,
    description: Option<&str>,
    thread_id: Option<&str>,
) -> Result<Value> {
    let is_encrypted = handle
        .world
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .rooms
        .get(room_id)
        .is_some_and(|r| r.encrypted);

    let plaintext_len = bytes.len();
    let (upload_bytes, upload_name, encryption_meta) = if is_encrypted {
        let (ct, meta) = super::e2ee::encrypt_file_bytes(&bytes)?;
        use sha2::Digest;
        let hashed_name = hex::encode(sha2::Sha256::digest(filename.as_bytes()));
        (ct, hashed_name, Some(meta))
    } else {
        (bytes, filename.to_string(), None)
    };

    // Rocket.Chat v7+/v8+/v9 uses two-step `rooms.media/{rid}` + `rooms.mediaConfirm/{rid}/{fileId}`;
    // older v6 servers use single-step `rooms.upload/{rid}`.
    let part = reqwest::multipart::Part::bytes(upload_bytes.clone())
        .file_name(upload_name.clone())
        .mime_str(mime_type)?;
    let media_form = reqwest::multipart::Form::new().part("file", part);
    match handle
        .api
        .upload_multipart(&format!("/api/v1/rooms.media/{room_id}"), media_form)
        .await
    {
        Ok(up) => {
            if let Some(file_id) = up["file"]["_id"].as_str() {
                let file_url = up["file"]["url"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("/file-upload/{file_id}/{upload_name}"));
                let mut confirm = serde_json::json!({});
                if let Some(tmid) = thread_id.filter(|s| !s.is_empty()) {
                    confirm["tmid"] = serde_json::json!(tmid);
                }
                if let Some(meta) = encryption_meta {
                    // Encrypt both `content` (attachments/files/msg) and `fileContent` under Ks (`rc.v2.aes-sha2`),
                    // matching Rocket.Chat v8/v9 `processMessageUploads.ts` so official clients decrypt & render it.
                    let room_key = handle
                        .e2ee
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .room_keys
                        .get(room_id)
                        .cloned();
                    if let Some((kid, ks)) = room_key {
                        let ext = filename.rsplit('.').next().unwrap_or("");
                        let type_group = mime_type.split('/').next().unwrap_or("application");
                        let mut attachment = serde_json::json!({
                            "title": filename,
                            "type": "file",
                            "title_link": file_url,
                            "title_link_download": true,
                            "encryption": meta["encryption"],
                            "hashes": meta["hashes"],
                            "fileId": file_id,
                            "size": plaintext_len,
                            "format": ext,
                        });
                        if matches!(type_group, "image" | "audio" | "video") {
                            attachment[format!("{type_group}_url")] = serde_json::json!(file_url);
                            attachment[format!("{type_group}_type")] = serde_json::json!(mime_type);
                            attachment[format!("{type_group}_size")] = serde_json::json!(plaintext_len);
                        }
                        let file_obj = serde_json::json!({
                            "_id": file_id,
                            "name": filename,
                            "type": mime_type,
                            "size": plaintext_len,
                            "format": ext,
                        });
                        let msg_inner = serde_json::json!({
                            "attachments": [attachment],
                            "files": [file_obj],
                            "file": file_obj,
                            "msg": description.unwrap_or(""),
                        });
                        let file_meta_inner = serde_json::json!({
                            "type": mime_type,
                            "typeGroup": type_group,
                            "name": filename,
                            "encryption": meta["encryption"],
                            "hashes": meta["hashes"],
                        });
                        if let (Ok(enc_content), Ok(enc_file_content)) = (
                            super::e2ee::encrypt_message_v2(&ks, &kid, &msg_inner),
                            super::e2ee::encrypt_message_v2(&ks, &kid, &file_meta_inner),
                        ) {
                            confirm["t"] = serde_json::json!("e2e");
                            confirm["msg"] = serde_json::json!("");
                            confirm["content"] = enc_content;
                            confirm["fileContent"] = enc_file_content;
                        }
                    }
                } else if let Some(desc) = description.filter(|s| !s.is_empty()) {
                    confirm["description"] = serde_json::json!(desc);
                }
                let confirmed = handle
                    .api
                    .post(&format!("/api/v1/rooms.mediaConfirm/{room_id}/{file_id}"), &confirm)
                    .await?;
                return Ok(confirmed);
            }
            Ok(up)
        }
        Err(_) => {
            let part2 = reqwest::multipart::Part::bytes(upload_bytes)
                .file_name(filename.to_string())
                .mime_str(mime_type)?;
            let mut form = reqwest::multipart::Form::new().part("file", part2);
            if let Some(desc) = description.filter(|s| !s.is_empty()) {
                form = form.text("description", desc.to_string());
            }
            if let Some(tmid) = thread_id.filter(|s| !s.is_empty()) {
                form = form.text("tmid", tmid.to_string());
            }
            if let Some(meta) = encryption_meta {
                form = form.text("content", meta.to_string());
            }
            Ok(handle
                .api
                .upload_multipart(&format!("/api/v1/rooms.upload/{room_id}"), form)
                .await?)
        }
    }
}

/// Download an authenticated Rocket.Chat file or avatar.
pub async fn download_file(handle: &Handle, path_or_url: &str) -> Result<Option<(String, Vec<u8>)>> {
    Ok(handle.api.get_bytes(path_or_url).await?)
}

/// Fetch custom server emoji (`GET /api/v1/emoji-custom.list`) and register them with moho's emoji picker.
pub async fn sync_custom_emoji(state: &AppState, handle: &Handle) -> Result<usize> {
    let resp = handle.api.get("/api/v1/emoji-custom.list", &[]).await?;
    let list = resp["emojis"]["update"]
        .as_array()
        .or_else(|| resp["emojis"].as_array())
        .cloned()
        .unwrap_or_default();

    let base = handle.api.base().to_string();
    let mut entries = Vec::with_capacity(list.len());
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.custom_emoji.clear();
        for item in &list {
            let Some(name) = item["name"].as_str().filter(|s| !s.is_empty()) else {
                continue;
            };
            let id = item["_id"].as_str().unwrap_or(name).to_string();
            let ext = item["extension"].as_str().unwrap_or("png").to_string();
            let aliases: Vec<String> = item["aliases"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let url = format!("{base}/emoji-custom/{name}.{ext}");
            let animated = ext.eq_ignore_ascii_case("gif") || ext.eq_ignore_ascii_case("webp");
            world.custom_emoji.insert(
                name.to_string(),
                CustomEmoji {
                    id: id.clone(),
                    name: name.to_string(),
                    aliases,
                    extension: ext,
                },
            );
            entries.push(EmojiEntry {
                id: format!(":{name}:"),
                name: name.to_string(),
                url: Some(url),
                animated,
                locked: false,
            });
        }
    }

    let count = entries.len();
    state.runtime.set_emoji_source(
        &handle.account_id,
        EmojiSource {
            id: format!("{}:custom", handle.account_id),
            name: handle.server.site_name.clone(),
            service: "rocketchat".to_string(),
            kind: "text".to_string(),
            icon_url: None,
            buffers: Vec::new(),
            sendable_anywhere: true,
            emoji: entries,
        },
    );
    Ok(count)
}
