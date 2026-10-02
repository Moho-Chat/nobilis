//! Every media cache the daemon keeps, in one place.
//!
//! Pictures, avatars, emotes and sounds are fetched once and kept on disk,
//! because the window cannot reach most of them itself - over Tor, behind a
//! Matrix bearer token, encrypted. Kept forever, that is a folder that only
//! grows. So every cache is listed here with the same two limits - a size, and
//! an age - and swept against both.
//!
//! Expiring a file is only safe because a missing one can be fetched again.
//! Scrollback names cached files by path, so a picture swept out of a cache
//! used to be a broken box in history for good. `restore` is the other half:
//! the window asks for a file it could not load, and the protocol that cached
//! it fetches it again from where it came from. Most file names say where that
//! was (a Matrix media id, a Sneedchat attachment id); what a name cannot say
//! - an encrypted file's key, an avatar's URL - is recorded with `record`.
//!
//! The same list is what Settings reports and clears.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use crate::state::AppState;
use crate::store::Store;

/// One cache: its folder under the cache root, which group Settings shows it
/// in, and the most it may hold.
pub struct Cache {
    pub dir: &'static str,
    pub group: &'static str,
    pub max_bytes: u64,
}

const MB: u64 = 1024 * 1024;

pub const CACHES: &[Cache] = &[
    Cache { dir: "matrix-media", group: "media", max_bytes: 250 * MB },
    Cache { dir: "sneedchat-attachments", group: "media", max_bytes: 250 * MB },
    Cache { dir: "discord-thumbnails", group: "media", max_bytes: 100 * MB },
    Cache { dir: "sneedchat-avatars", group: "avatars", max_bytes: 100 * MB },
    Cache { dir: "discord-icons", group: "avatars", max_bytes: 16 * MB },
    Cache { dir: "kick-emotes", group: "emotes", max_bytes: 64 * MB },
    Cache { dir: "discord-stickers", group: "emotes", max_bytes: 32 * MB },
    Cache { dir: "discord-sounds", group: "emotes", max_bytes: 16 * MB },
];

/// How long a cached file is kept after it was written, whatever the size.
///
/// A month: long enough that what somebody is reading this week is never
/// fetched twice, short enough that a cache stops being a record of
/// everything ever seen. Anything older that is looked at again is fetched
/// again.
pub const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// How often the caches are held to their limits.
pub const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// The daemon's cache root, `~/.cache/nobilis`.
pub fn root() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".cache")).join("nobilis")
}

pub fn dir(name: &str) -> PathBuf {
    root().join(name)
}

static STORE: OnceLock<Arc<Store>> = OnceLock::new();
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Gives `record` and `source` somewhere to keep things, and `usage` the
/// folder the scrollback is in. Called once at startup, so the code that
/// caches a file does not need the whole state threaded through to it.
pub fn init(store: Arc<Store>, data_dir: PathBuf) {
    let _ = STORE.set(store);
    let _ = DATA_DIR.set(data_dir);
}

/// Notes where a cached file came from, when its name cannot say.
pub fn record(path: &Path, source: Value) {
    if let Some(store) = STORE.get() {
        if let Err(e) = store.record_media_source(&path.to_string_lossy(), &source) {
            tracing::debug!("media cache: recording where {} came from: {e}", path.display());
        }
    }
}

/// Where a cached file came from, if it was recorded.
pub fn source(path: &Path) -> Option<Value> {
    STORE.get()?.media_source(&path.to_string_lossy()).ok().flatten()
}

/// Every file in a folder, with its size and when it was written.
async fn files_in(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
    let mut out = Vec::new();
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else { return out };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let Ok(meta) = entry.metadata().await else { continue };
        if meta.is_file() {
            out.push((entry.path(), meta.len(), meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH)));
        }
    }
    out
}

/// Holds every cache to its limits: files past `MAX_AGE` go, and then the
/// oldest go until the folder is under its size.
pub async fn sweep_all() {
    for cache in CACHES {
        let dir = dir(cache.dir);
        let mut expired = 0usize;
        for (path, _, written) in files_in(&dir).await {
            if written.elapsed().is_ok_and(|age| age > MAX_AGE) && tokio::fs::remove_file(&path).await.is_ok() {
                expired += 1;
            }
        }
        if expired > 0 {
            tracing::info!("{}: {expired} file(s) older than {} days expired", cache.dir, MAX_AGE.as_secs() / 86_400);
        }
        crate::backend::sneedchat::sweep_cache_dir(&dir, cache.max_bytes, cache.dir).await;
    }
}

async fn bytes_in(dir: &Path) -> u64 {
    files_in(dir).await.iter().map(|(_, size, _)| size).sum()
}

/// The scrollback database and the files SQLite keeps beside it.
fn history_files() -> Vec<PathBuf> {
    let Some(dir) = DATA_DIR.get() else { return Vec::new() };
    ["scrollback.db", "scrollback.db-wal", "scrollback.db-shm"].iter().map(|f| dir.join(f)).collect()
}

/// How much each group holds, for Settings.
pub async fn usage() -> Value {
    let mut groups: Vec<(&str, u64)> = vec![("history", 0), ("media", 0), ("avatars", 0), ("emotes", 0)];
    for path in history_files() {
        groups[0].1 += tokio::fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
    }
    for cache in CACHES {
        let bytes = bytes_in(&dir(cache.dir)).await;
        if let Some(group) = groups.iter_mut().find(|(id, _)| *id == cache.group) {
            group.1 += bytes;
        }
    }
    json!(groups.into_iter().map(|(id, bytes)| json!({ "id": id, "bytes": bytes })).collect::<Vec<_>>())
}

/// Empties one group. Media, avatars and emotes are fetched again as they
/// are looked at; history is gone except where the service keeps it.
pub async fn clear(state: &AppState, group: &str) -> Result<()> {
    if group == "history" {
        let store = state.store.clone();
        let removed = tokio::task::spawn_blocking(move || store.clear_history()).await??;
        tracing::info!("scrollback: cleared {removed} message(s) on request");
        state.events.emit("historyCleared", json!({}));
        return Ok(());
    }
    let caches: Vec<&Cache> = CACHES.iter().filter(|c| c.group == group).collect();
    if caches.is_empty() {
        bail!("there is no cache called {group}");
    }
    for cache in caches {
        let mut removed = 0usize;
        for (path, _, _) in files_in(&dir(cache.dir)).await {
            if tokio::fs::remove_file(&path).await.is_ok() {
                removed += 1;
            }
        }
        tracing::info!("{}: cleared {removed} file(s) on request", cache.dir);
    }
    Ok(())
}

/// Fetches a cached file again, after it expired or was cleared.
///
/// `buffer_id` and `message_id` are the message that names it, where the
/// window knows one: a Discord thumbnail is named by a hash of its URL, and
/// only the message says what that URL was. Answers the file's local URL,
/// which is usually the same path and is not always - an avatar comes back
/// named after what its bytes turned out to be.
pub async fn restore(state: &AppState, path: &str, buffer_id: Option<&str>, message_id: Option<&str>) -> Result<String> {
    let (cache, path) = cache_for(&root(), path)?;
    let account_hint = buffer_id.and_then(|b| state.runtime.get_buffer(b)).map(|b| b.account_id);

    let restored = match cache.dir {
        "matrix-media" => crate::backend::matrix::restore_cached(state, &path, account_hint.as_deref()).await?,
        "sneedchat-attachments" | "sneedchat-avatars" => crate::backend::sneedchat::restore_cached(&path).await?,
        "discord-thumbnails" => {
            let (Some(buffer_id), Some(message_id)) = (buffer_id, message_id) else {
                bail!("a Discord picture is fetched again from its message");
            };
            crate::backend::discord::restore_thumbnail(state, &path, buffer_id, message_id).await?
        }
        "discord-icons" => crate::backend::discord::restore_guild_icon(&path).await?,
        "kick-emotes" => crate::backend::kick::emotecache::restore_cached(&path).await?,
        _ => bail!("{} is fetched again by what uses it, not by name", cache.dir),
    };
    Ok(format!("file://{}", restored.display()))
}

/// Which cache a path names a file in, refusing anything else.
///
/// Only ever a file directly inside one of the caches: the path comes from
/// the window, and anywhere else is not somewhere to fetch to. Compared as
/// text after the `file://` is taken off, with `..` and hidden names refused
/// outright rather than resolved.
fn cache_for(root: &Path, raw: &str) -> Result<(&'static Cache, PathBuf)> {
    let path = PathBuf::from(raw.strip_prefix("file://").unwrap_or(raw).split('#').next().unwrap_or(raw));
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir | std::path::Component::CurDir)) {
        bail!("not a cached file");
    }
    let parent = path.parent().context("not a cached file")?;
    let cache = CACHES
        .iter()
        .find(|c| parent == root.join(c.dir))
        .context("that file is not in a media cache")?;
    let name = path.file_name().and_then(|n| n.to_str()).context("not a cached file")?;
    if name.starts_with('.') {
        bail!("not a cached file");
    }
    Ok((cache, path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_file_directly_in_a_cache_is_restored() {
        let root = Path::new("/home/someone/.cache/nobilis");
        let (cache, path) = cache_for(root, "file:///home/someone/.cache/nobilis/matrix-media/a.org_xyz.png#123").unwrap();
        assert_eq!(cache.dir, "matrix-media");
        assert_eq!(path, root.join("matrix-media/a.org_xyz.png"));

        for refused in [
            "/etc/passwd",
            "/home/someone/.cache/nobilis/accounts.toml",
            "/home/someone/.cache/nobilis/matrix-media/../../x",
            "/home/someone/.cache/nobilis/matrix-media/sub/a.png",
            "/home/someone/.cache/nobilis/matrix-media/.hidden",
            "/home/someone/.cache/nobilis/not-a-cache/a.png",
        ] {
            assert!(cache_for(root, refused).is_err(), "{refused} should be refused");
        }
    }

    #[test]
    fn every_cache_belongs_to_a_group_settings_shows() {
        for cache in CACHES {
            assert!(["media", "avatars", "emotes"].contains(&cache.group), "{} is in no group", cache.dir);
        }
    }
}
