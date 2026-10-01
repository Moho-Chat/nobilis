//! The rooms an account is in, changed while it stays signed in.
//!
//! An account signs in once. Its session - the cookies the sign-in captured,
//! or the ones a browser handed over - is kept here with what it takes to
//! reach the site, and every room is a task of its own that borrows it: to
//! join, and again after any interruption, when a room reconnects on its own
//! and refreshes the session if the site has let it lapse. So opening a room
//! is starting one task and closing one is stopping it. Nothing else is
//! touched: the other rooms stay connected, and nobody signs in again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::*;

/// One signed-in account's means of reaching the site, and its rooms.
pub(super) struct Live {
    transport: Transport,
    session: Session,
    host: String,
    username: String,
    password: String,
    totp_secret: Option<String>,
    /// The room whose socket records whispers, or zero for none.
    ///
    /// Every room's socket receives every whisper - they are not
    /// room-scoped - so exactly one of them writes them down. Moved to
    /// another room when that one closes, and none when no room is open.
    whisper_room: Arc<AtomicU32>,
    rooms: Mutex<HashMap<u32, tokio::task::AbortHandle>>,
}

fn registry() -> &'static Mutex<HashMap<String, Arc<Live>>> {
    static LIVE: OnceLock<Mutex<HashMap<String, Arc<Live>>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Stops an account's rooms when the connection that owns them ends.
pub(super) struct LiveGuard {
    account_id: String,
    live: Arc<Live>,
}

impl Drop for LiveGuard {
    fn drop(&mut self) {
        let mut all = registry().lock().unwrap();
        // Only if it is still this connection's: a newer one may already
        // have taken the account over, and its rooms are not ours to stop.
        if all.get(&self.account_id).is_some_and(|current| Arc::ptr_eq(current, &self.live)) {
            all.remove(&self.account_id);
        }
        drop(all);
        for (_, task) in self.live.rooms.lock().unwrap().drain() {
            task.abort();
        }
    }
}

/// Registers a freshly signed-in account. Its rooms are started by
/// `sync_rooms`, which the caller runs next.
pub(super) fn install(account_id: &str, transport: Transport, session: Session, host: String, config: &SneedChatAccountConfig) -> LiveGuard {
    let live = Arc::new(Live {
        transport,
        session,
        host,
        username: config.username.clone(),
        password: config.password.clone(),
        totp_secret: config.totp_secret.clone(),
        whisper_room: Arc::new(AtomicU32::new(0)),
        rooms: Mutex::new(HashMap::new()),
    });
    registry().lock().unwrap().insert(account_id.to_string(), live.clone());
    LiveGuard { account_id: account_id.to_string(), live }
}

/// Brings an account's running rooms into line with its configured ones:
/// starts the ones newly chosen, stops the ones dropped, and leaves the rest
/// exactly as they are. Nothing happens for an account that is not signed in;
/// it picks the list up when it is.
///
/// The stopping is done here, before this returns, so a caller that goes on
/// to remove a room's buffer knows no message from it can arrive afterwards
/// and bring the buffer back.
pub fn sync_rooms(state: &AppState, account_id: &str) {
    let Some(live) = registry().lock().unwrap().get(account_id).cloned() else { return };
    let Some(config) = state.accounts.get_sneedchat(account_id) else { return };
    let wanted = config.rooms;

    let mut running = live.rooms.lock().unwrap();
    running.retain(|id, task| {
        let keep = wanted.iter().any(|r| r.id == *id);
        if !keep {
            task.abort();
        }
        keep
    });
    live.whisper_room.store(wanted.first().map(|r| r.id).unwrap_or(0), Ordering::Relaxed);
    for room in wanted {
        if running.contains_key(&room.id) {
            continue;
        }
        let (state, live2, account_id) = (state.clone(), live.clone(), account_id.to_string());
        let id = room.id;
        let task = tokio::spawn(async move {
            let creds = Credentials { username: live2.username.clone(), password: live2.password.clone() };
            let two_factor = match &live2.totp_secret {
                Some(secret) => match totp::decode_secret(secret) {
                    Ok(bytes) => TwoFactor::Totp(bytes),
                    Err(_) => TwoFactor::None,
                },
                None => TwoFactor::None,
            };
            run_room(&state, &live2.transport, &live2.session, &creds, &two_factor, &account_id, &live2.host, &room, live2.whisper_room.clone()).await;
        });
        running.insert(id, task.abort_handle());
    }
    tracing::info!("sneedchat[{account_id}]: {} room(s) open", running.len());
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
