//! What this client will say on Discord, and when.
//!
//! Discord judges an account by what it sends, and it judges the first thing said somewhere hardest. Accounts that
//! start talking in a server, or to a person, from a client that is not Discord's own have been flagged for spam - and
//! logged out of everywhere - on the strength of that one message, however ordinary it was; the same account's later
//! messages, in places it has already spoken, are left alone. So this client does not say the first word:
//!
//! - **A server** where the account has never spoken since it joined is locked, every channel in it. Whether it has
//!   spoken is asked of what is stored here and then of Discord's own search (one request a server, the answer kept),
//!   and the lock lifts as soon as there is a message of the account's there - including one sent from Discord's own
//!   app while this one is running.
//! - **A conversation** with no messages in it at all is locked the same way, and lifts when one arrives.
//!
//! The lock is the daemon's, not the window's: a send to a locked place is refused here, whoever asks. The window shows
//! the reason where the message box would be, through the conversation's `read_only`.
//!
//! The rest is how what is sent goes out: a `nonce` on every message and an explicit `allowed_mentions`, as Discord's
//! clients send; a file checked against what the account may upload before any of it leaves; and a pause between a
//! message and a file, since a file sent straight after a message is what has been flagged too.

use super::*;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Shown where the box would be in a server the account has not spoken in.
pub const FIRST_WORD: &str =
    "Send your first message in this server from Discord's own app or website. Discord flags accounts that start talking in a server from another client.";

/// Shown while that is being found out.
pub const CHECKING: &str = "Checking whether you have spoken in this server before…";

/// Shown in a conversation with no messages in it.
pub const EMPTY_DM: &str =
    "Start this conversation from Discord's own app or website. Discord flags accounts that send the first message to somebody from another client.";

/// Why a place is closed to this client, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    None,
    Checking,
    FirstWord,
    EmptyDm,
}

impl Lock {
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Lock::None => None,
            Lock::Checking => Some(CHECKING),
            Lock::FirstWord => Some(FIRST_WORD),
            Lock::EmptyDm => Some(EMPTY_DM),
        }
    }
}

/// What is known, per process. Plain data and plain methods so it can be tested without a connection.
#[derive(Default)]
pub struct Guard {
    /// Account -> the servers it is known to have spoken in.
    spoken: HashMap<String, HashSet<String>>,
    /// Servers being looked into now.
    checking: HashSet<(String, String)>,
    /// Servers already looked into this run, whatever came of it: a server is not searched again every time it is read.
    checked: HashSet<(String, String)>,
    /// When the account joined each server (Unix seconds), where Discord said.
    joined: HashMap<(String, String), i64>,
    /// Each server's boost tier, which raises what may be uploaded there.
    tiers: HashMap<(String, String), u8>,
    /// Why a conversation is read-only for a reason that is not ours (no permission, Discord's own notices).
    permission: HashMap<String, Option<&'static str>>,
    /// Conversations that have no messages in them.
    empty_dms: HashSet<String>,
    /// When this account last sent a message, and whether it carried a file.
    last_sent: HashMap<String, (Instant, bool)>,
    /// The messages this client has sent, by id. They are the account's own, but they are not what the lock asks for:
    /// it wants a first word from Discord's own client.
    sent: HashSet<String>,
    sent_order: std::collections::VecDeque<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    accounts: HashMap<String, Vec<String>>,
    /// Ids of messages this client sent, oldest first.
    #[serde(default)]
    sent: Vec<String>,
}

/// How many sent messages are remembered: enough to outlast the rest of the history that could be mistaken for a
/// first word, and no more.
const REMEMBER_SENT: usize = 5000;

impl Guard {
    pub fn is_spoken(&self, account: &str, guild: &str) -> bool {
        self.spoken.get(account).is_some_and(|g| g.contains(guild))
    }

    /// What closes a place: `guild` is None for a conversation.
    pub fn lock_of(&self, account: &str, guild: Option<&str>, buffer: &str) -> Lock {
        match guild {
            Some(guild) if self.is_spoken(account, guild) => Lock::None,
            Some(guild) if self.checking.contains(&(account.to_string(), guild.to_string())) => Lock::Checking,
            Some(_) => Lock::FirstWord,
            None if self.empty_dms.contains(buffer) => Lock::EmptyDm,
            None => Lock::None,
        }
    }

    /// The reason a conversation is read-only: a permission the account lacks first, since that is the truer thing to
    /// say, then the lock.
    pub fn effective(&self, account: &str, guild: Option<&str>, buffer: &str) -> Option<&'static str> {
        self.permission.get(buffer).copied().flatten().or_else(|| self.lock_of(account, guild, buffer).reason())
    }
}

impl Guard {
    pub fn note_sent(&mut self, id: &str) -> bool {
        if !self.sent.insert(id.to_string()) {
            return false;
        }
        self.sent_order.push_back(id.to_string());
        while self.sent_order.len() > REMEMBER_SENT {
            if let Some(old) = self.sent_order.pop_front() {
                self.sent.remove(&old);
            }
        }
        true
    }

    /// Whether any of these message ids is one this client did not send.
    pub fn any_not_sent_here<'a>(&self, ids: impl IntoIterator<Item = &'a str>) -> bool {
        ids.into_iter().any(|id| !self.sent.contains(id))
    }
}

/// A message this client has just sent, from what Discord answered: remembered as sent here.
pub(super) async fn note_sent(resp: reqwest::Response) {
    if let Ok(body) = resp.json::<Value>().await {
        if let Some(id) = body["id"].as_str() {
            with(|g| {
                if g.note_sent(id) {
                    save(g);
                }
            });
        }
    }
}

/// The most characters a message may hold: 4,000 with Nitro, 2,000 without.
pub fn message_limit(premium: u8) -> usize {
    if premium == 2 {
        4000
    } else {
        2000
    }
}

/// Refuses text Discord would refuse, or nothing at all, before it is sent. A request that is bound to be turned down
/// is one more request from an account that is being watched, and says nothing a sentence here does not say better.
pub fn check_text(content: &str, premium: u8, may_be_empty: bool) -> Result<()> {
    let limit = message_limit(premium);
    let length = content.chars().count();
    if length > limit {
        bail!("That message is {length} characters long and Discord takes {limit}{}", if premium == 2 { "" } else { " (4,000 with Nitro)" });
    }
    if !may_be_empty && content.trim().is_empty() {
        bail!("There is nothing to send");
    }
    Ok(())
}

/// The most a file may be where it is sent: what the account's Nitro allows, or what the server's boosts do, whichever
/// is more.
pub fn upload_limit(premium: u8, guild_tier: u8) -> u64 {
    const MIB: u64 = 1024 * 1024;
    let account = match premium {
        2 => 500 * MIB,
        1 | 3 => 50 * MIB,
        _ => 10 * MIB,
    };
    let guild = match guild_tier {
        3 => 100 * MIB,
        2 => 50 * MIB,
        _ => 10 * MIB,
    };
    account.max(guild)
}

fn file() -> std::path::PathBuf {
    crate::default_data_dir().join("discord-spoken.json")
}

static GUARD: Mutex<Option<Guard>> = Mutex::new(None);

/// Runs `f` on the process's state, loading what was kept the first time.
fn with<R>(f: impl FnOnce(&mut Guard) -> R) -> R {
    let mut slot = GUARD.lock().unwrap_or_else(|e| e.into_inner());
    let guard = slot.get_or_insert_with(|| {
        let saved: Saved = std::fs::read_to_string(file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
        Guard {
            spoken: saved.accounts.into_iter().map(|(a, g)| (a, g.into_iter().collect())).collect(),
            sent: saved.sent.iter().cloned().collect(),
            sent_order: saved.sent.into_iter().collect(),
            ..Default::default()
        }
    });
    f(guard)
}

fn save(guard: &Guard) {
    let saved = Saved {
        accounts: guard.spoken.iter().map(|(a, g)| (a.clone(), g.iter().cloned().collect())).collect(),
        sent: guard.sent_order.iter().cloned().collect(),
    };
    if let Ok(text) = serde_json::to_string(&saved) {
        if let Some(dir) = file().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(file(), text);
    }
}

/// Says why a conversation is read-only for a reason of its own (or that it is not), and applies what the lock adds.
pub(super) fn set_permission(state: &AppState, account: &str, buffer: &str, reason: Option<&'static str>) {
    let guild = state.runtime.get_discord_guild(buffer);
    let effective = with(|g| {
        g.permission.insert(buffer.to_string(), reason);
        g.effective(account, guild.as_deref(), buffer)
    });
    state.runtime.set_buffer_read_only(state, buffer, effective);
}

/// Puts every conversation in a server back to what it should say now.
fn reapply_guild(state: &AppState, account: &str, guild: &str) {
    for buffer in state.runtime.discord_buffers_in_guild(account, guild) {
        let effective = with(|g| g.effective(account, Some(guild), &buffer));
        state.runtime.set_buffer_read_only(state, &buffer, effective);
    }
}

/// A server is being registered: note what is wanted of it, and start finding out whether the account has spoken in
/// it. Called before its channels are registered, so they begin locked rather than open.
pub(super) fn open_guild(state: &AppState, config: &DiscordAccountConfig, guild: &Value) {
    let Some(guild_id) = guild["id"].as_str() else { return };
    let account = config.account_id();
    let key = (account.clone(), guild_id.to_string());
    let joined = guild["joined_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp());
    let start = with(|g| {
        if let Some(at) = joined {
            g.joined.insert(key.clone(), at);
        }
        g.tiers.insert(key.clone(), guild["premium_tier"].as_u64().unwrap_or(0).min(3) as u8);
        if g.is_spoken(&account, guild_id) || g.checking.contains(&key) || g.checked.contains(&key) {
            return false;
        }
        g.checking.insert(key.clone());
        true
    });
    if start {
        spawn_check(state.clone(), config.clone(), guild_id.to_string());
    }
}

/// An account has said something in a server: it has spoken there.
pub(super) fn spoke_in_guild(state: &AppState, account: &str, guild: &str) {
    let changed = with(|g| {
        let key = (account.to_string(), guild.to_string());
        let was_checking = g.checking.remove(&key);
        let new = g.spoken.entry(account.to_string()).or_default().insert(guild.to_string());
        if new {
            save(g);
        }
        new || was_checking
    });
    if changed {
        reapply_guild(state, account, guild);
    }
}

/// A server is gone (left, or removed from): speaking there again, if it is joined again, is a first word again.
pub(super) fn forget_guild(account: &str, guild: &str) {
    with(|g| {
        if let Some(set) = g.spoken.get_mut(account) {
            if set.remove(guild) {
                save(g);
            }
        }
        let key = (account.to_string(), guild.to_string());
        g.checking.remove(&key);
        g.checked.remove(&key);
        g.joined.remove(&key);
        g.tiers.remove(&key);
    });
}

/// An account is removed: nothing of it is kept.
pub fn forget_account(account: &str) {
    with(|g| {
        if g.spoken.remove(account).is_some() {
            save(g);
        }
        g.checking.retain(|(a, _)| a != account);
        g.checked.retain(|(a, _)| a != account);
        g.joined.retain(|(a, _), _| a != account);
        g.tiers.retain(|(a, _), _| a != account);
        g.last_sent.remove(account);
    });
}

/// A conversation as it was registered: empty ones are locked.
pub(super) fn note_dm(state: &AppState, account: &str, buffer: &str, channel: &Value) {
    // Only a channel that says it has no messages: one that does not say is not known to be empty.
    let empty = channel.get("last_message_id").is_some_and(Value::is_null) && state.store.newest_msg_id(buffer).ok().flatten().is_none();
    with(|g| {
        if empty {
            g.empty_dms.insert(buffer.to_string());
        } else {
            g.empty_dms.remove(buffer);
        }
    });
    let effective = with(|g| g.effective(account, None, buffer));
    state.runtime.set_buffer_read_only(state, buffer, effective);
}

/// A message has arrived in a conversation, so it is not empty any more.
pub(super) fn dm_has_messages(state: &AppState, account: &str, buffer: &str) {
    let was_empty = with(|g| g.empty_dms.remove(buffer));
    if was_empty {
        let effective = with(|g| g.effective(account, None, buffer));
        state.runtime.set_buffer_read_only(state, buffer, effective);
    }
}

/// A dispatch for a message was received in a place this connection knows: whatever it means for the locks.
pub(super) fn heard(state: &AppState, config: &DiscordAccountConfig, d: &Value, buffer: &str, is_dm: bool) {
    let account = config.account_id();
    if is_dm {
        dm_has_messages(state, &account, buffer);
    } else if d["author"]["id"].as_str() == Some(config.user_id.as_str()) {
        if let Some(guild) = d["guild_id"].as_str() {
            spoke_in_guild(state, &account, guild);
        }
    }
}

/// Refuses a send to a place the account has not spoken in.
pub(super) fn ensure_may_speak(state: &AppState, buffer: &str) -> Result<()> {
    let account = state.runtime.get_buffer(buffer).map(|b| b.account_id).unwrap_or_default();
    let guild = state.runtime.get_discord_guild(buffer);
    match with(|g| g.lock_of(&account, guild.as_deref(), buffer)).reason() {
        Some(reason) => bail!("{reason}"),
        None => Ok(()),
    }
}

/// Refuses a file Discord would refuse, before any of it is sent. What Discord does with one that is too big is cut
/// the upload off, which looks like nothing a client does; saying so first is the better answer.
pub(super) fn ensure_fits(state: &AppState, buffer: &str, files: &[(String, String, u64)]) -> Result<()> {
    let account = state.runtime.get_buffer(buffer).map(|b| b.account_id).unwrap_or_default();
    let guild = state.runtime.get_discord_guild(buffer);
    let tier = guild.as_deref().and_then(|g| with(|s| s.tiers.get(&(account.clone(), g.to_string())).copied())).unwrap_or(0);
    let limit = upload_limit(state.runtime.discord_premium(&account), tier);
    for (_, name, len) in files {
        if *len > limit {
            bail!("{name} is {} and the most you can send here is {}", human(*len), human(limit));
        }
    }
    Ok(())
}

fn human(bytes: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    format!("{:.1} MB", bytes as f64 / MIB)
}

/// How long to wait before this account sends another message, given what it sent last: a file straight after a
/// message, or a message straight after a file, is closer together than anybody working a client sends them.
pub fn gap_needed(last: Option<(Duration, bool)>, attachment: bool) -> Duration {
    const FILE_AFTER_MESSAGE: Duration = Duration::from_secs(5);
    const MESSAGE_AFTER_FILE: Duration = Duration::from_secs(3);
    let Some((since, last_was_file)) = last else { return Duration::ZERO };
    let needed = match (attachment, last_was_file) {
        (true, _) => FILE_AFTER_MESSAGE,
        (false, true) => MESSAGE_AFTER_FILE,
        (false, false) => Duration::ZERO,
    };
    needed.saturating_sub(since)
}

/// Waits out the pause, and takes this send's turn.
pub(super) async fn take_turn(account: &str, attachment: bool) {
    let wait = with(|g| {
        let last = g.last_sent.get(account).map(|(at, file)| (at.elapsed(), *file));
        let wait = gap_needed(last, attachment);
        g.last_sent.insert(account.to_string(), (Instant::now() + wait, attachment));
        wait
    });
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

/// A fresh message id, as Discord's clients make one: a snowflake from now, so it sorts with the messages around it
/// and cannot repeat.
pub fn nonce() -> String {
    const EPOCH_MS: i64 = 1_420_070_400_000;
    let now = chrono::Utc::now().timestamp_millis();
    let low: u64 = rand::random::<u64>() & ((1 << 22) - 1);
    ((((now - EPOCH_MS).max(0) as u64) << 22) | low).to_string()
}

/// What every message carries beyond its words: the nonce, and who may be mentioned, said outright.
pub(super) fn stamp(payload: &mut Value) {
    if payload.get("nonce").is_none() {
        payload["nonce"] = Value::from(nonce());
    }
    if payload.get("allowed_mentions").is_none() {
        let mut allowed = json!({ "parse": ["users", "roles", "everyone"] });
        if payload.get("message_reference").is_some() {
            allowed["replied_user"] = Value::Bool(true);
        }
        payload["allowed_mentions"] = allowed;
    }
}

// --- finding out whether the account has spoken in a server ---------------------------------------------------------

/// One server at a time, a little apart: a search is the heaviest thing this asks of Discord.
static TURN: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const SPACING: Duration = Duration::from_millis(2500);
const INDEXING_TRIES: usize = 6;

/// What Discord's search said of the account's messages in a server.
#[derive(Debug, PartialEq, Eq)]
pub enum Searched {
    Spoke,
    Never,
    /// Still indexing, or asked to slow down: try again after this long.
    Wait(Duration),
}

/// Reads a search answer. The count is Discord's own; a 202 or a 429 is not an answer. Messages that this client sent
/// are the account's but are not the first word the lock waits for: it is `Spoke` only if one of the hits was sent
/// from somewhere else, or if there are more results than the hits shown (which cannot all be ours).
pub fn read_search(status: u16, retry_after: Option<f64>, body: &Value, sent_here: &HashSet<String>) -> Result<Searched> {
    match status {
        200 => {
            let total = body["total_results"].as_u64().unwrap_or(0);
            let hits: Vec<&str> = body["messages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|group| group.as_array()?.iter().find(|m| m["hit"].as_bool() == Some(true)).or_else(|| group.as_array()?.first()))
                .filter_map(|m| m["id"].as_str())
                .collect();
            let elsewhere = hits.iter().any(|id| !sent_here.contains(*id));
            Ok(if total > 0 && (elsewhere || total as usize > hits.len()) { Searched::Spoke } else { Searched::Never })
        }
        202 | 429 => {
            let seconds = retry_after.or_else(|| body["retry_after"].as_f64()).unwrap_or(5.0).clamp(1.0, 60.0);
            Ok(Searched::Wait(Duration::from_secs_f64(seconds)))
        }
        other => bail!("Discord answered {other} to the search"),
    }
}

fn spawn_check(state: AppState, config: DiscordAccountConfig, guild_id: String) {
    tokio::spawn(async move {
        // The server's channels are registered a moment after this is started.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let account = config.account_id();
        let key = (account.clone(), guild_id.clone());
        let since = with(|g| g.joined.get(&key).copied());

        // What is kept here first: no request for what is already known, and no waiting behind the searches.
        let buffers = state.runtime.discord_buffers_in_guild(&account, &guild_id);
        let kept = state.store.own_message_ids_since(&buffers, since.unwrap_or(0), 200).unwrap_or_default();
        if with(|g| g.any_not_sent_here(kept.iter().map(String::as_str))) {
            spoke_in_guild(&state, &account, &guild_id);
            return;
        }

        // Then Discord's own search, one server at a time.
        let _turn = TURN.lock().await;
        let mut spoke = false;
        for attempt in 0..INDEXING_TRIES {
            match search_own(&config, &guild_id, since).await {
                Ok(Searched::Spoke) => {
                    spoke = true;
                    break;
                }
                Ok(Searched::Never) => break,
                Ok(Searched::Wait(wait)) => {
                    tracing::debug!("discord[{account}]: searching {guild_id}, try {attempt}: waiting {}s", wait.as_secs());
                    tokio::time::sleep(wait).await;
                }
                Err(e) => {
                    // Not known either way. Left locked - which is the cautious answer - and tried again at the next start.
                    tracing::warn!("discord[{account}]: could not look for your messages in {guild_id}: {e:#}");
                    break;
                }
            }
        }
        if spoke {
            spoke_in_guild(&state, &account, &guild_id);
        } else {
            with(|g| {
                g.checking.remove(&key);
                g.checked.insert(key.clone());
            });
            reapply_guild(&state, &account, &guild_id);
            tracing::info!("discord[{account}]: no message of yours in {guild_id} since joining: locked until you send one from Discord's own client");
        }
        tokio::time::sleep(SPACING).await;
    });
}

/// Asks Discord whether the account has any message in a server since it joined.
async fn search_own(config: &DiscordAccountConfig, guild_id: &str, since: Option<i64>) -> Result<Searched> {
    let mut query = vec![("author_id".to_string(), config.user_id.clone()), ("include_nsfw".to_string(), "true".to_string())];
    if let Some(at) = since {
        query.push(("min_id".to_string(), super::search::snowflake_at(at)));
    }
    let resp = http_client_for(&config.token)
        .get(format!("{API_BASE}/guilds/{guild_id}/messages/search"))
        .query(&query)
        .header("Authorization", &config.token)
        .send()
        .await
        .context("searching for your messages")?;
    let status = resp.status().as_u16();
    let retry = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    let sent_here = with(|g| g.sent.clone());
    read_search(status, retry, &body, &sent_here)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guard() -> Guard {
        Guard::default()
    }

    #[test]
    fn a_server_never_spoken_in_is_locked_and_one_spoken_in_is_not() {
        let mut g = guard();
        assert_eq!(g.lock_of("a", Some("g"), "b"), Lock::FirstWord);
        g.checking.insert(("a".into(), "g".into()));
        assert_eq!(g.lock_of("a", Some("g"), "b"), Lock::Checking);
        g.spoken.entry("a".into()).or_default().insert("g".into());
        assert_eq!(g.lock_of("a", Some("g"), "b"), Lock::None);
    }

    #[test]
    fn speaking_in_one_account_does_not_open_another() {
        let mut g = guard();
        g.spoken.entry("a".into()).or_default().insert("g".into());
        assert_eq!(g.lock_of("other", Some("g"), "b"), Lock::FirstWord);
        assert_eq!(g.lock_of("a", Some("elsewhere"), "b"), Lock::FirstWord);
    }

    #[test]
    fn an_empty_conversation_is_locked_and_one_with_messages_is_not() {
        let mut g = guard();
        assert_eq!(g.lock_of("a", None, "dm"), Lock::None);
        g.empty_dms.insert("dm".into());
        assert_eq!(g.lock_of("a", None, "dm"), Lock::EmptyDm);
    }

    #[test]
    fn a_missing_permission_is_said_before_the_lock() {
        let mut g = guard();
        g.permission.insert("b".into(), Some("You may not send here."));
        assert_eq!(g.effective("a", Some("g"), "b"), Some("You may not send here."));
        g.permission.insert("b".into(), None);
        assert_eq!(g.effective("a", Some("g"), "b"), Some(FIRST_WORD));
        g.spoken.entry("a".into()).or_default().insert("g".into());
        assert_eq!(g.effective("a", Some("g"), "b"), None);
    }

    #[test]
    fn what_may_be_uploaded_is_the_more_of_the_account_and_the_server() {
        const MIB: u64 = 1024 * 1024;
        assert_eq!(upload_limit(0, 0), 10 * MIB);
        assert_eq!(upload_limit(2, 0), 500 * MIB);
        assert_eq!(upload_limit(3, 0), 50 * MIB);
        assert_eq!(upload_limit(0, 2), 50 * MIB);
        assert_eq!(upload_limit(0, 3), 100 * MIB);
        assert_eq!(upload_limit(3, 3), 100 * MIB);
        assert_eq!(upload_limit(2, 3), 500 * MIB);
    }

    #[test]
    fn a_file_waits_after_a_message_and_a_message_after_a_file() {
        let s = Duration::from_secs;
        assert_eq!(gap_needed(None, true), Duration::ZERO);
        assert_eq!(gap_needed(Some((s(1), false)), true), s(4));
        assert_eq!(gap_needed(Some((s(9), false)), true), Duration::ZERO);
        assert_eq!(gap_needed(Some((s(1), true)), false), s(2));
        assert_eq!(gap_needed(Some((s(1), true)), true), s(4));
        // Messages one after the other are talking, and are not slowed here.
        assert_eq!(gap_needed(Some((Duration::ZERO, false)), false), Duration::ZERO);
    }

    #[test]
    fn text_discord_would_refuse_is_refused_here() {
        assert!(check_text("hello", 0, false).is_ok());
        assert!(check_text(&"a".repeat(2000), 0, false).is_ok());
        let long = check_text(&"a".repeat(2001), 0, false).unwrap_err().to_string();
        assert!(long.contains("2001") && long.contains("2000") && long.contains("Nitro"), "{long}");
        assert!(check_text(&"a".repeat(4000), 2, false).is_ok(), "Nitro takes more");
        assert!(check_text(&"a".repeat(4001), 2, false).is_err());
        // Counted in characters, not bytes.
        assert!(check_text(&"é".repeat(2000), 0, false).is_ok());
        assert!(check_text("  \n ", 0, false).is_err(), "nothing is nothing");
        assert!(check_text("", 0, true).is_ok(), "a caption may be empty");
    }

    #[test]
    fn a_nonce_is_a_number_that_does_not_repeat() {
        let a = nonce();
        let b = nonce();
        assert_ne!(a, b);
        assert!(a.parse::<u64>().is_ok());
        assert!(a.len() >= 18, "{a}");
    }

    #[test]
    fn every_message_is_stamped_and_what_is_already_said_is_kept() {
        let mut plain = json!({ "content": "hi" });
        stamp(&mut plain);
        assert!(plain["nonce"].as_str().is_some());
        assert_eq!(plain["allowed_mentions"]["parse"], json!(["users", "roles", "everyone"]));
        assert!(plain["allowed_mentions"].get("replied_user").is_none());

        let mut reply = json!({ "content": "hi", "message_reference": { "message_id": "1" } });
        stamp(&mut reply);
        assert_eq!(reply["allowed_mentions"]["replied_user"], json!(true));

        let mut quiet = json!({ "content": "hi", "message_reference": { "message_id": "1" }, "allowed_mentions": { "parse": [], "replied_user": false }, "nonce": "7" });
        stamp(&mut quiet);
        assert_eq!(quiet["nonce"], json!("7"));
        assert_eq!(quiet["allowed_mentions"]["replied_user"], json!(false));
    }

    fn hit(id: &str) -> Value {
        json!([{ "id": id, "hit": true }])
    }

    #[test]
    fn a_search_answer_is_read_as_spoke_never_or_wait() {
        let none = HashSet::new();
        let one = json!({ "total_results": 1, "messages": [hit("9")] });
        assert_eq!(read_search(200, None, &one, &none).unwrap(), Searched::Spoke);
        assert_eq!(read_search(200, None, &json!({ "total_results": 0, "messages": [] }), &none).unwrap(), Searched::Never);
        assert_eq!(read_search(202, None, &json!({ "retry_after": 7.0 }), &none).unwrap(), Searched::Wait(Duration::from_secs(7)));
        assert_eq!(read_search(429, Some(2.0), &Value::Null, &none).unwrap(), Searched::Wait(Duration::from_secs(2)));
        // Not an answer: neither "never" nor "spoke".
        assert!(read_search(403, None, &Value::Null, &none).is_err());
        assert!(read_search(500, None, &Value::Null, &none).is_err());
        // A wait is never an hour.
        assert_eq!(read_search(202, Some(9999.0), &Value::Null, &none).unwrap(), Searched::Wait(Duration::from_secs(60)));
    }

    #[test]
    fn what_this_client_sent_is_not_the_first_word_the_lock_waits_for() {
        let sent: HashSet<String> = ["9".to_string()].into();
        let mine = json!({ "total_results": 1, "messages": [hit("9")] });
        assert_eq!(read_search(200, None, &mine, &sent).unwrap(), Searched::Never);
        let other = json!({ "total_results": 2, "messages": [hit("9"), hit("10")] });
        assert_eq!(read_search(200, None, &other, &sent).unwrap(), Searched::Spoke);
        // More results than hits shown: they cannot all be the one this client sent.
        let many = json!({ "total_results": 40, "messages": [hit("9")] });
        assert_eq!(read_search(200, None, &many, &sent).unwrap(), Searched::Spoke);
    }

    #[test]
    fn sent_messages_are_remembered_a_bounded_number() {
        let mut g = guard();
        assert!(g.note_sent("1"));
        assert!(!g.note_sent("1"), "twice is once");
        assert!(g.any_not_sent_here(["1", "2"]));
        assert!(!g.any_not_sent_here(["1"]));
        for i in 0..(REMEMBER_SENT + 10) {
            g.note_sent(&format!("m{i}"));
        }
        assert_eq!(g.sent.len(), REMEMBER_SENT);
        assert!(!g.sent.contains("1") && !g.sent.contains("m0") && g.sent.contains(&format!("m{}", REMEMBER_SENT + 9)));
    }
}
