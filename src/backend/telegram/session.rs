//! What grammers remembers between runs: the authorization key Telegram issued, the datacenters it found, the people
//! and chats it has met (their access hashes, without which a chat cannot be addressed again) and how far the
//! account's updates have been read.
//!
//! The library offers a SQLite store, which would bring a second SQLite beside the two this daemon already links,
//! and a memory store that cannot be saved. This is its own: the same bookkeeping as the memory store, kept in a file
//! of its own in the daemon's data directory (one per account, private, written whole and renamed into place). The file is
//! a credential: the authorization key in it is the account.
//!
//! It is written when something changed and a few seconds have passed (`save_if_dirty`, called from the connection's
//! loop), and once more when the connection ends or a sign-in finishes.

use anyhow::{Context, Result};
use grammers_session::types::{ChannelState, DcOption, PeerId, PeerInfo, UpdateState, UpdatesState};
use grammers_session::{BoxFuture, Session, SessionData};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard};

/// Where an account's session lives.
pub fn path_for(user_id: i64) -> PathBuf {
    folder().join(format!("{user_id}.session"))
}

/// A sign-in that has not got as far as having an account yet keeps its session here until it has.
pub fn path_for_login(login_id: &str) -> PathBuf {
    let safe: String = login_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
    folder().join(format!("login-{safe}.session"))
}

fn folder() -> PathBuf {
    crate::default_data_dir().join("telegram")
}

/// The file's form: what the library's own type holds, with the maps as lists (a JSON object cannot be keyed by a
/// peer).
#[derive(Serialize, Deserialize, Default)]
struct Saved {
    home_dc: i32,
    dc_options: Vec<DcOption>,
    peers: Vec<PeerInfo>,
    updates: UpdatesState,
}

pub struct FileSession {
    path: Mutex<PathBuf>,
    data: Mutex<SessionData>,
    dirty: AtomicBool,
}

impl FileSession {
    /// A new session, with Telegram's datacenters as the library knows them and no key yet.
    pub fn fresh(path: PathBuf) -> Self {
        Self { path: Mutex::new(path), data: Mutex::new(SessionData::default()), dirty: AtomicBool::new(false) }
    }

    /// The session a file holds, or a fresh one if there is no file.
    pub fn open(path: PathBuf) -> Result<Self> {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::fresh(path)),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let saved: Saved = serde_json::from_str(&text).with_context(|| format!("reading the session in {}", path.display()))?;
        let mut data = SessionData::default();
        if saved.home_dc != 0 {
            data.home_dc = saved.home_dc;
        }
        for option in saved.dc_options {
            data.dc_options.insert(option.id, option);
        }
        data.peer_infos = saved.peers.into_iter().map(|p| (p.id(), p)).collect();
        data.updates_state = saved.updates;
        Ok(Self { path: Mutex::new(path), data: Mutex::new(data), dirty: AtomicBool::new(false) })
    }

    fn data(&self) -> MutexGuard<'_, SessionData> {
        // Nothing here can leave the data half-changed, so a lock a panic left poisoned is still good.
        self.data.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the key Telegram issued for the home datacenter is there: what a signed-in session has.
    pub fn has_key(&self) -> bool {
        let data = self.data();
        data.dc_options.get(&data.home_dc).is_some_and(|o| o.auth_key.is_some())
    }

    /// Moves the file, when a sign-in learns whose session this is.
    pub fn move_to(&self, path: PathBuf) -> Result<()> {
        let old = std::mem::replace(&mut *self.path.lock().unwrap_or_else(|e| e.into_inner()), path);
        self.save()?;
        if old != *self.path.lock().unwrap_or_else(|e| e.into_inner()) {
            let _ = std::fs::remove_file(old);
        }
        Ok(())
    }

    /// Writes the session, whole and private, and renames it over the last.
    pub fn save(&self) -> Result<()> {
        let path = self.path.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let saved = {
            let data = self.data();
            Saved { home_dc: data.home_dc, dc_options: data.dc_options.values().cloned().collect(), peers: data.peer_infos.values().cloned().collect(), updates: data.updates_state.clone() }
        };
        write_private(&path, serde_json::to_string(&saved)?.as_bytes())?;
        self.dirty.store(false, Ordering::Relaxed);
        Ok(())
    }

    /// Writes it if anything changed since the last time.
    pub fn save_if_dirty(&self) -> Result<()> {
        if self.dirty.load(Ordering::Relaxed) {
            self.save()?;
        }
        Ok(())
    }

    /// Takes the file away: an account that was removed, or a sign-in that was given up.
    pub fn delete(&self) {
        let path = self.path.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let _ = std::fs::remove_file(path);
        self.dirty.store(false, Ordering::Relaxed);
    }

    fn touch(&self) {
        self.dirty.store(true, Ordering::Relaxed);
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("making {}", parent.display()))?;
    }
    let tmp = path.with_extension("session.tmp");
    {
        // Private from the moment it exists, as the accounts file is.
        let mut file = crate::secure::create_private_file(&tmp)?;
        file.write_all(bytes)?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

impl Session for FileSession {
    type Error = Infallible;

    fn home_dc_id(&self) -> Result<i32, Infallible> {
        Ok(self.data().home_dc)
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, Result<(), Infallible>> {
        Box::pin(async move {
            self.data().home_dc = dc_id;
            self.touch();
            Ok(())
        })
    }

    fn dc_option(&self, dc_id: i32) -> Result<Option<DcOption>, Infallible> {
        Ok(self.data().dc_options.get(&dc_id).cloned())
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, Result<(), Infallible>> {
        let dc_option = dc_option.clone();
        Box::pin(async move {
            self.data().dc_options.insert(dc_option.id, dc_option);
            self.touch();
            Ok(())
        })
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Result<Option<PeerInfo>, Infallible>> {
        Box::pin(async move { Ok(self.data().peer_infos.get(&peer).cloned()) })
    }

    fn cache_peer(&self, peer: PeerInfo) -> BoxFuture<'_, Result<(), Infallible>> {
        Box::pin(async move {
            let changed = {
                let mut data = self.data();
                let known = data.peer_infos.entry(peer.id()).or_insert_with(|| peer.clone());
                known.extend_info(&peer)
            };
            if changed {
                self.touch();
            }
            Ok(())
        })
    }

    fn updates_state(&self) -> BoxFuture<'_, Result<UpdatesState, Infallible>> {
        Box::pin(async move { Ok(self.data().updates_state.clone()) })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, Result<(), Infallible>> {
        Box::pin(async move {
            {
                let mut data = self.data();
                match update {
                    UpdateState::All(state) => data.updates_state = state,
                    UpdateState::Primary { pts, date, seq } => {
                        data.updates_state.pts = pts;
                        data.updates_state.date = date;
                        data.updates_state.seq = seq;
                    }
                    UpdateState::Secondary { qts } => data.updates_state.qts = qts,
                    UpdateState::Channel { id, pts } => {
                        data.updates_state.channels.retain(|c| c.id != id);
                        data.updates_state.channels.push(ChannelState { id, pts });
                    }
                }
            }
            self.touch();
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nobilis-tg-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn a_session_is_written_private_and_comes_back_whole() {
        let dir = dir("roundtrip");
        let path = dir.join("1.session");
        let session = FileSession::fresh(path.clone());
        assert!(!session.has_key());
        let mut option = session.dc_option(2).unwrap().unwrap();
        option.auth_key = Some([9u8; 256]);
        session.set_dc_option(&option).await.unwrap();
        session.set_home_dc_id(2).await.unwrap();
        session.set_update_state(UpdateState::Primary { pts: 10, date: 20, seq: 30 }).await.unwrap();
        session.set_update_state(UpdateState::Channel { id: 5, pts: 7 }).await.unwrap();
        assert!(session.has_key());
        session.save_if_dirty().unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600, "the key in it is the account");
        }
        let back = FileSession::open(path.clone()).unwrap();
        assert!(back.has_key());
        assert_eq!(back.dc_option(2).unwrap().unwrap().auth_key, Some([9u8; 256]));
        let state = back.updates_state().await.unwrap();
        assert_eq!((state.pts, state.date, state.seq), (10, 20, 30));
        assert_eq!(state.channels.iter().map(|c| (c.id, c.pts)).collect::<Vec<_>>(), vec![(5, 7)]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn nothing_is_written_until_something_changed_and_a_missing_file_is_a_fresh_session() {
        let dir = dir("clean");
        let path = dir.join("2.session");
        let session = FileSession::open(path.clone()).unwrap();
        session.save_if_dirty().unwrap();
        assert!(!path.exists(), "an unchanged session writes nothing");
        session.set_home_dc_id(4).await.unwrap();
        session.save_if_dirty().unwrap();
        assert!(path.exists());
        let moved = dir.join("3.session");
        session.move_to(moved.clone()).unwrap();
        assert!(moved.exists() && !path.exists(), "a sign-in's file moves to the account's name");
        session.delete();
        assert!(!moved.exists());
        std::fs::remove_dir_all(dir).ok();
    }
}
