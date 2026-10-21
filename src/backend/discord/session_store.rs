//! A gateway session kept on disk, so a restart can pick it up instead of logging in afresh.
//!
//! Discord keeps a dropped session for a few minutes and replays what it missed to a client that resumes it; its own
//! client resumes only when the last heartbeat was acknowledged under three minutes ago, and logs in afresh otherwise.
//! A restart of moho is a dropped connection like any other, so it does the same - for which the session has to
//! survive the process: what to resume (`Position`), and what a resume does *not* send again, because Discord sends
//! READY and GUILD_CREATE only to a fresh login (`State`). The state is replayed through the very code that handles
//! them live, so a resumed start ends up with the same friends, servers, mutes and plan as a fresh one.
//!
//! Two files per account, because the position changes every heartbeat and the state hardly ever.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::PathBuf;

/// How old the last acknowledged heartbeat may be for a session to be worth resuming. Discord's client uses three
/// minutes; a little under, since connecting takes time and a refused resume costs a fresh login.
pub const FRESH_FOR: i64 = 150;

/// Past this the state is not kept (and so nothing is resumed): a few huge servers are not worth a file that size
/// rewritten on every change.
const MAX_STATE_BYTES: usize = 24 * 1024 * 1024;

#[derive(Serialize, Deserialize, Clone)]
pub struct Position {
    pub user_id: String,
    /// Which token this session belongs to, as a hash - the token itself is not written here.
    pub token: String,
    pub session_id: String,
    pub url: String,
    pub seq: u64,
    /// When a heartbeat was last acknowledged, Unix seconds.
    pub acked_at: i64,
}

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    pub session_id: String,
    /// READY's payload, with the direct messages found over REST added as `moho_dms`.
    pub ready: Value,
    /// Each server's GUILD_CREATE payload, by id.
    pub guilds: HashMap<String, Value>,
}

/// One account's session files, and what is known of them in memory.
#[derive(Default)]
pub struct SessionFile {
    /// Where the files go; the daemon's data directory unless a test says otherwise.
    root: Option<PathBuf>,
    account: String,
    user_id: String,
    token: String,
    position: Option<Position>,
    state: Option<State>,
    state_dirty: bool,
}

pub fn token_hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().take(12).map(|b| format!("{b:02x}")).collect()
}

fn default_dir() -> PathBuf {
    crate::default_data_dir().join("discord-sessions")
}

/// Whether a saved position can still be resumed at `now`, by this token.
pub fn resumable(position: &Position, user_id: &str, token: &str, now: i64) -> bool {
    position.user_id == user_id && position.token == token_hash(token) && now - position.acked_at < FRESH_FOR && position.acked_at <= now + 5
}

impl SessionFile {
    fn dir(&self) -> PathBuf {
        self.root.clone().unwrap_or_else(default_dir)
    }

    fn path(&self, what: &str) -> PathBuf {
        let safe: String = self.account.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        self.dir().join(format!("{safe}.{what}.json"))
    }

    pub fn new(account: &str, user_id: &str, token: &str) -> Self {
        SessionFile { account: account.to_string(), user_id: user_id.to_string(), token: token_hash(token), ..Default::default() }
    }

    /// The saved session as READY and GUILD_CREATE dispatches in the order they were first received, with the
    /// position to resume from; `None` when there is nothing fresh enough to resume.
    pub fn load(&mut self, token: &str, now: i64) -> Option<(Position, Vec<Value>)> {
        let position: Position = serde_json::from_str(&std::fs::read_to_string(self.path("pos")).ok()?).ok()?;
        if !resumable(&position, &self.user_id, token, now) {
            return None;
        }
        let state: State = serde_json::from_str(&std::fs::read_to_string(self.path("state")).ok()?).ok()?;
        if state.session_id != position.session_id || state.ready.is_null() {
            return None;
        }
        let mut replay = vec![dispatch("READY", position.seq, state.ready.clone())];
        replay.extend(state.guilds.values().map(|g| dispatch("GUILD_CREATE", position.seq, g.clone())));
        self.position = Some(position.clone());
        self.state = Some(state);
        Some((position, replay))
    }

    /// A fresh READY: a new session, with nothing of the last one worth keeping.
    pub fn ready(&mut self, session_id: &str, url: &str, seq: u64, ready: Value) {
        self.position = Some(Position {
            user_id: self.user_id.clone(),
            token: self.token.clone(),
            session_id: session_id.to_string(),
            url: url.to_string(),
            seq,
            acked_at: chrono::Utc::now().timestamp(),
        });
        self.state = Some(State { session_id: session_id.to_string(), ready, guilds: HashMap::new() });
        self.state_dirty = true;
    }

    /// The direct messages listed over REST at READY, kept with it: a resume does not list them again.
    pub fn dms(&mut self, list: Vec<Value>) {
        if let Some(state) = self.state.as_mut() {
            state.ready["moho_dms"] = Value::Array(list);
            self.state_dirty = true;
        }
    }

    pub fn guild(&mut self, id: &str, payload: &Value) {
        if let Some(state) = self.state.as_mut() {
            state.guilds.insert(id.to_string(), payload.clone());
            self.state_dirty = true;
        }
    }

    pub fn guild_gone(&mut self, id: &str) {
        if let Some(state) = self.state.as_mut() {
            if state.guilds.remove(id).is_some() {
                self.state_dirty = true;
            }
        }
    }

    /// A heartbeat was acknowledged: the session is alive as of now, and has seen events up to `seq`.
    pub fn acked(&mut self, seq: u64) {
        let Some(position) = self.position.as_mut() else { return };
        position.seq = seq;
        position.acked_at = chrono::Utc::now().timestamp();
        self.write();
    }

    /// The session is gone (refused, or ended by Discord): nothing is left to resume.
    pub fn clear(&mut self) {
        self.position = None;
        self.state = None;
        self.state_dirty = false;
        let _ = std::fs::remove_file(self.path("pos"));
        let _ = std::fs::remove_file(self.path("state"));
    }

    fn write(&mut self) {
        let (Some(position), Some(state)) = (self.position.as_ref(), self.state.as_ref()) else { return };
        let _ = std::fs::create_dir_all(self.dir());
        if self.state_dirty {
            match serde_json::to_string(state) {
                Ok(text) if text.len() <= MAX_STATE_BYTES => {
                    if write_private(&self.path("state"), &text) {
                        self.state_dirty = false;
                    }
                }
                _ => {
                    // Too big to keep: this session cannot be resumed after a restart, which is the cost of that
                    // many servers.
                    let _ = std::fs::remove_file(self.path("pos"));
                    self.position = None;
                    self.state = None;
                    return;
                }
            }
        }
        if let Ok(text) = serde_json::to_string(position) {
            write_private(&self.path("pos"), &text);
        }
    }
}

/// Removes whatever is kept for an account that has been taken away.
pub fn forget(account: &str) {
    let mut file = SessionFile { account: account.to_string(), ..Default::default() };
    file.clear();
}

fn dispatch(kind: &str, seq: u64, d: Value) -> Value {
    serde_json::json!({ "op": 0, "t": kind, "s": seq, "d": d, "moho_replay": true })
}

/// Written to a temporary file and renamed, so a restart in the middle of it never reads half a file; readable by
/// this user alone, as the session is as good as a token for as long as it lives.
fn write_private(path: &std::path::Path, text: &str) -> bool {
    let tmp = path.with_extension("tmp");
    let ok = (|| -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(text.as_bytes())?;
        }
        #[cfg(not(unix))]
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    })();
    ok.is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A directory of its own, gone when the test is.
    struct Scratch(PathBuf);
    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("nobilis-session-store-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            Scratch(dir)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn in_dir(dir: &std::path::Path) -> SessionFile {
        SessionFile { root: Some(dir.to_path_buf()), ..SessionFile::new("discord:7", "7", "tok") }
    }

    #[test]
    fn a_saved_session_comes_back_as_the_dispatches_it_was_made_of() {
        let dir = Scratch::new("comes_back");
        let mut first = in_dir(dir.path());
        first.ready("s", "wss://x", 3, json!({ "session_id": "s", "guilds": [] }));
        first.dms(vec![json!({ "id": "9" })]);
        first.guild("g1", &json!({ "id": "g1" }));
        first.acked(42);

        let (position, replay) = in_dir(dir.path()).load("tok", chrono::Utc::now().timestamp()).expect("fresh enough");
        assert_eq!((position.session_id.as_str(), position.seq), ("s", 42));
        // READY first, then the servers, each marked as saved and numbered where the session had got to.
        assert_eq!(replay[0]["t"], "READY");
        assert_eq!(replay[0]["d"]["moho_dms"][0]["id"], "9");
        assert_eq!(replay[1]["t"], "GUILD_CREATE");
        assert!(replay.iter().all(|m| m["s"] == 42 && m["moho_replay"] == true));
    }

    #[test]
    fn a_session_cleared_or_of_another_token_is_not_resumed() {
        let dir = Scratch::new("cleared");
        let mut file = in_dir(dir.path());
        file.ready("s", "wss://x", 1, json!({}));
        file.acked(2);
        let now = chrono::Utc::now().timestamp();
        assert!(in_dir(dir.path()).load("other", now).is_none());
        file.clear();
        assert!(in_dir(dir.path()).load("tok", now).is_none());
    }

    #[test]
    fn a_session_that_was_never_acknowledged_is_not_saved() {
        let dir = Scratch::new("never_acked");
        let mut file = in_dir(dir.path());
        file.ready("s", "wss://x", 1, json!({}));
        assert!(in_dir(dir.path()).load("tok", chrono::Utc::now().timestamp()).is_none());
    }

    fn pos(acked_at: i64) -> Position {
        Position { user_id: "7".into(), token: token_hash("tok"), session_id: "s".into(), url: "wss://x".into(), seq: 3, acked_at }
    }

    #[test]
    fn a_session_is_resumed_only_while_its_last_ack_is_fresh() {
        assert!(resumable(&pos(1000), "7", "tok", 1000 + FRESH_FOR - 1));
        assert!(!resumable(&pos(1000), "7", "tok", 1000 + FRESH_FOR));
        assert!(!resumable(&pos(1000), "7", "tok", 1000 + 3600));
    }

    #[test]
    fn a_session_belongs_to_one_account_and_one_token() {
        assert!(!resumable(&pos(1000), "8", "tok", 1010));
        assert!(!resumable(&pos(1000), "7", "other", 1010));
        // A clock set back is not a fresh session.
        assert!(!resumable(&pos(5000), "7", "tok", 1000));
    }

    #[test]
    fn the_token_itself_is_not_what_is_written() {
        assert!(!token_hash("tok").contains("tok"));
        assert_eq!(token_hash("tok"), token_hash("tok"));
        assert_ne!(token_hash("tok"), token_hash("tok2"));
    }
}
