//! The rooms an account is in, changed while it stays connected.
//!
//! One socket carries every room (see chat.rs), so opening a room is a `join`
//! on it and closing one a `leave`; nothing else is touched, and nobody signs
//! in again. An account that is not connected picks its list up when it is.

use super::*;

/// Brings the connection's rooms into line with the account's saved ones.
pub fn sync_rooms(state: &AppState, account_id: &str) {
    let Some(config) = state.accounts.get_sneedchat(account_id) else { return };
    super::chat::chat(account_id).want_rooms(&config.rooms);
}

/// Leaves the room a buffer shows, and deletes it and its history.
///
/// Answers false for a buffer that is not one of the account's rooms - a
/// whisper conversation - which has nothing to leave.
pub fn leave_room(state: &AppState, account_id: &str, buffer_name: &str) -> Result<bool> {
    let config = state.accounts.get_sneedchat(account_id).ok_or_else(|| anyhow!("no such account"))?;
    let name = room_name_of(buffer_name);
    if !config.rooms.iter().any(|r| r.name == name) {
        return Ok(false);
    }
    let remaining: Vec<SneedChatRoom> = config.rooms.into_iter().filter(|r| r.name != name).collect();
    state.accounts.set_sneedchat_rooms(account_id, remaining)?;
    sync_rooms(state, account_id);
    forget_room(state, account_id, buffer_name);
    Ok(true)
}

/// Sets an account's rooms outright - the Join page's list - opening and
/// closing what changed, and deleting what was closed along with its history.
pub fn set_rooms(state: &AppState, account_id: &str, rooms: Vec<SneedChatRoom>) -> Result<bool> {
    let Some(before) = state.accounts.get_sneedchat(account_id) else { return Ok(false) };
    let closed: Vec<SneedChatRoom> = before.rooms.iter().filter(|old| !rooms.iter().any(|r| r.id == old.id)).cloned().collect();
    if !state.accounts.set_sneedchat_rooms(account_id, rooms)? {
        return Ok(false);
    }
    sync_rooms(state, account_id);
    for room in closed {
        forget_room(state, account_id, &room_buffer_name(&room.name));
    }
    Ok(true)
}

/// Takes a closed room's buffer away and deletes everything kept for it: its
/// scrollback, and the attachments cached for its messages.
///
/// A closed room is not coming back with its history - opening it again
/// starts from what the site replays - so keeping the history would only be
/// disk filling up with conversations nobody can reach.
fn forget_room(state: &AppState, account_id: &str, buffer_name: &str) {
    let buffer_id = crate::model::buffer_id(account_id, buffer_name);
    state.runtime.remove_buffer(state, &buffer_id);
    let bodies = match state.store.forget_buffer(&buffer_id) {
        Ok(bodies) => bodies,
        Err(e) => {
            tracing::warn!("sneedchat[{account_id}]: deleting the history of {buffer_name}: {e:#}");
            return;
        }
    };
    let cache = attachment_cache_dir();
    let mut removed = 0;
    for path in cached_files_named_in(&bodies, &cache) {
        if std::fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    tracing::info!("sneedchat[{account_id}]: closed {buffer_name}: {} message(s) and {removed} cached file(s) deleted", bodies.len());
    // Deleted rows only free pages inside the file; this hands them back.
    let store = state.store.clone();
    tokio::task::spawn_blocking(move || {
        let _ = store.incremental_vacuum();
    });
}

/// The files under `cache` that these message bodies point at - attachments
/// fetched for them and written into the body as a `file://` link (see
/// `spawn_attachment_resolve`). Only files inside `cache`: a body is text from
/// the site, and nothing in it decides what gets deleted outside the cache.
pub(super) fn cached_files_named_in(bodies: &[String], cache: &std::path::Path) -> Vec<std::path::PathBuf> {
    let prefix = format!("file://{}/", cache.display());
    let mut found = Vec::new();
    for body in bodies {
        let mut rest = body.as_str();
        while let Some(at) = rest.find(&prefix) {
            let tail = &rest[at + prefix.len()..];
            let name: String = tail.chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')).collect();
            if !name.is_empty() && !name.starts_with('.') {
                let path = cache.join(&name);
                if !found.contains(&path) {
                    found.push(path);
                }
            }
            rest = tail;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_files_inside_the_cache_are_named() {
        let cache = std::path::Path::new("/home/u/.cache/nobilis/sneedchat-attachments");
        let bodies = vec![
            format!("look file://{}/9609686.png and again file://{}/9609686.png", cache.display(), cache.display()),
            format!("[img]file://{}/42.webp[/img]", cache.display()),
            "file:///etc/passwd and file://somewhere/else.png".to_string(),
            format!("file://{}/../../secret", cache.display()),
        ];
        let found = cached_files_named_in(&bodies, cache);
        assert_eq!(found, vec![cache.join("9609686.png"), cache.join("42.webp")]);
    }
}
