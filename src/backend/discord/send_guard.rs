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
pub const CHECKING: &str = "Checking whether you have spoken here before…";

/// Shown in a conversation with no messages in it.
pub const EMPTY_DM: &str =
    "Start this conversation from Discord's own app or website. Discord flags accounts that send the first message to somebody from another client.";

/// Shown where Discord has asked for a captcha, which this client does not answer.
pub const CAPTCHA: &str =
    "Discord asked for a captcha here, and moho does not answer those - doing so is what gets an account flagged. Say something here from Discord's own app; this opens again then, or in half an hour.";

/// Shown in a server channel that nobody has ever written in.
pub const EMPTY_CHANNEL: &str = "Nobody has said anything in this channel yet. Send the first message from Discord's own app.";

/// Shown in a conversation that has few messages of the account's own.
pub const NEW_CONVERSATION: &str =
    "This conversation is new. Send at least 5 messages in it from Discord's own app first - Discord flags accounts that start talking to somebody from another client.";

pub const MESSAGE_REQUEST: &str = "This is a message request, not yet a conversation. Accept it in Discord's own app first.";

pub const SPAM_DM: &str = "Discord has marked this conversation as spam.";

/// How long a captcha closes the place it came from, unless the account speaks there from Discord's own app.
const CAPTCHA_HOLD: Duration = Duration::from_secs(30 * 60);

/// Why a place is closed to this client, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lock {
    None,
    Checking,
    FirstWord,
    EmptyDm,
    NewConversation,
    MessageRequest,
    SpamDm,
    EmptyChannel,
    Captcha,
    /// The server asks something of the account that it has not done, and will not do by waiting.
    Participation(&'static str),
}

impl Lock {
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Lock::None => None,
            Lock::Checking => Some(CHECKING),
            Lock::FirstWord => Some(FIRST_WORD),
            Lock::EmptyDm => Some(EMPTY_DM),
            Lock::NewConversation => Some(NEW_CONVERSATION),
            Lock::MessageRequest => Some(MESSAGE_REQUEST),
            Lock::SpamDm => Some(SPAM_DM),
            Lock::EmptyChannel => Some(EMPTY_CHANNEL),
            Lock::Captcha => Some(CAPTCHA),
            Lock::Participation(r) => Some(r),
        }
    }
}

/// What is known of a channel, by its buffer.
#[derive(Debug, Clone, Default)]
pub struct ChannelFacts {
    /// Seconds between messages, the channel's slowmode.
    pub slowmode: u64,
    /// Whether the account's permissions are not held to it.
    pub bypass_slowmode: bool,
    /// Every permission: the owner, an administrator.
    pub full: bool,
}

/// What is known of a direct message.
#[derive(Debug, Clone, Default)]
pub struct DmFacts {
    pub request: bool,
    pub spam: bool,
    /// The account's own messages in it that were not sent from here.
    pub own: usize,
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
    /// The nonces of messages sent from here: the echo of one is not the account speaking from elsewhere.
    nonces: HashSet<String>,
    nonce_order: std::collections::VecDeque<String>,
    /// Where a captcha closed a place: (account, server or conversation) -> until when.
    captcha: HashMap<(String, String), Instant>,
    /// Each server's rules and our membership of it.
    guilds: HashMap<(String, String), (rules::GuildFacts, Option<rules::MemberFacts>)>,
    /// What the server's onboarding said of itself, where it was asked: enabled or not, and when it was asked.
    onboarding: HashMap<(String, String), (Option<bool>, Instant)>,
    accounts: HashMap<String, rules::AccountFacts>,
    channels: HashMap<String, ChannelFacts>,
    /// Server channels nobody has written in.
    empty_channels: HashSet<String>,
    dms: HashMap<String, DmFacts>,
    /// Conversations that have enough of the account's own messages in them, kept once reached.
    established: HashSet<String>,
    /// Conversations being counted now, and counted this run.
    dm_checking: HashSet<String>,
    dm_checked: HashSet<String>,
    /// When the account last spoke in each channel, from here or from anywhere.
    last_own: HashMap<String, Instant>,
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    accounts: HashMap<String, Vec<String>>,
    /// Conversations that have been begun for good.
    #[serde(default)]
    established: Vec<String>,
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

    fn captcha_held(&self, account: &str, scope: &str, now: Instant) -> bool {
        self.captcha.get(&(account.to_string(), scope.to_string())).is_some_and(|until| *until > now)
    }

    /// Whether the server's rules hold the account back, as of `now` (Unix seconds).
    pub fn restriction(&self, account: &str, guild: &str, buffer: &str, now: i64) -> Option<rules::Restriction> {
        let key = (account.to_string(), guild.to_string());
        let (facts, member) = self.guilds.get(&key)?;
        let account_facts = self.accounts.get(account)?;
        let full = self.channels.get(buffer).is_some_and(|c| c.full);
        let onboarding = self.onboarding.get(&key).and_then(|(enabled, _)| *enabled);
        // Discord's green new-member badge, for the first week: what the server asks comes first, as it is the more
        // actionable thing to be told.
        rules::participation(facts, member.as_ref(), account_facts, full, onboarding, now).or_else(|| rules::new_member(facts, member.as_ref(), self.joined.get(&key).copied(), now))
    }

    /// What closes a place: `guild` is None for a conversation.
    pub fn lock_of(&self, account: &str, guild: Option<&str>, buffer: &str) -> Lock {
        let now = Instant::now();
        match guild {
            Some(guild) => {
                let secs = chrono::Utc::now().timestamp();
                if let Some(reason) = self.restriction(account, guild, buffer, secs).and_then(|r| r.banner()) {
                    return Lock::Participation(reason);
                }
                if self.captcha_held(account, guild, now) {
                    return Lock::Captcha;
                }
                if !self.is_spoken(account, guild) {
                    return if self.checking.contains(&(account.to_string(), guild.to_string())) { Lock::Checking } else { Lock::FirstWord };
                }
                if self.empty_channels.contains(buffer) {
                    return Lock::EmptyChannel;
                }
                Lock::None
            }
            None => {
                if let Some(dm) = self.dms.get(buffer) {
                    if dm.spam {
                        return Lock::SpamDm;
                    }
                    if dm.request {
                        return Lock::MessageRequest;
                    }
                }
                if self.captcha_held(account, buffer, now) {
                    return Lock::Captcha;
                }
                if self.established.contains(buffer) {
                    return Lock::None;
                }
                if self.dm_checking.contains(buffer) {
                    return Lock::Checking;
                }
                if self.empty_dms.contains(buffer) {
                    return Lock::EmptyDm;
                }
                match self.dms.get(buffer) {
                    Some(dm) if dm.own < rules::ESTABLISHED_DM_MESSAGES => Lock::NewConversation,
                    _ => Lock::None,
                }
            }
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
            established: saved.established.into_iter().collect(),
            ..Default::default()
        }
    });
    f(guard)
}

fn save(guard: &Guard) {
    let saved = Saved {
        accounts: guard.spoken.iter().map(|(a, g)| (a.clone(), g.iter().cloned().collect())).collect(),
        sent: guard.sent_order.iter().cloned().collect(),
        established: guard.established.iter().cloned().collect(),
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

/// Servers whose new-member badge has a task waiting for its end.
static BADGE_WAITS: Mutex<Option<HashSet<(String, String)>>> = Mutex::new(None);

/// Opens the places the green new-member badge closed when the week is up. Re-evaluating is all it does: the lock is
/// worked out from the clock each time, so what is needed is only a moment at which to look again.
fn watch_badge_end(state: &AppState, account: &str, guild: &str, joined: i64) {
    let left = joined + rules::NEW_MEMBER_SECS - chrono::Utc::now().timestamp();
    if left <= 0 {
        return;
    }
    let key = (account.to_string(), guild.to_string());
    let fresh = BADGE_WAITS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).insert(key.clone());
    if !fresh {
        return;
    }
    // Said so that the week can be checked against the day the badge actually goes.
    tracing::info!("discord[{account}]: joined server {guild} {} days ago: the new-member lock lasts until {}", (chrono::Utc::now().timestamp() - joined) / 86_400, chrono::DateTime::from_timestamp(joined + rules::NEW_MEMBER_SECS, 0).map(|t| t.to_rfc3339()).unwrap_or_default());
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(left as u64 + 5)).await;
        BADGE_WAITS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(Default::default).remove(&key);
        reapply_guild(&state, &key.0, &key.1);
    });
}

/// A server is being registered: note what is wanted of it, and start finding out whether the account has spoken in
/// it. Called before its channels are registered, so they begin locked rather than open.
pub(super) fn open_guild(state: &AppState, config: &DiscordAccountConfig, guild: &Value) {
    let Some(guild_id) = guild["id"].as_str() else { return };
    let account = config.account_id();
    let key = (account.clone(), guild_id.to_string());
    let joined = guild["joined_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp());
    if let Some(at) = joined {
        watch_badge_end(state, &account, guild_id, at);
    }
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
    // How many of the account's own messages are in it that were not sent from here.
    let kept = state.store.own_message_ids_since(&[buffer.to_string()], 0, 50).unwrap_or_default();
    with(|g| {
        let own = kept.iter().filter(|id| !g.sent.contains(id.as_str())).count();
        g.dms.insert(
            buffer.to_string(),
            DmFacts { request: channel["is_message_request"].as_bool() == Some(true), spam: channel["is_spam"].as_bool() == Some(true), own },
        );
        if own >= rules::ESTABLISHED_DM_MESSAGES && g.established.insert(buffer.to_string()) {
            save(g);
        }
        if empty {
            g.empty_dms.insert(buffer.to_string());
        } else {
            g.empty_dms.remove(buffer);
        }
    });
    // Not enough of the account's own messages here to know it has talked in it: asked of Discord, once a run.
    let start = with(|g| {
        let counted = g.dms.get(buffer).is_some_and(|f| f.own >= rules::ESTABLISHED_DM_MESSAGES);
        if counted || g.established.contains(buffer) || empty || g.dm_checked.contains(buffer) || g.dm_checking.contains(buffer) {
            return false;
        }
        g.dm_checking.insert(buffer.to_string());
        true
    });
    let effective = with(|g| g.effective(account, None, buffer));
    state.runtime.set_buffer_read_only(state, buffer, effective);
    if start {
        if let (Some(config), Some(channel)) = (state.accounts.get_discord(account), channel["id"].as_str()) {
            spawn_dm_count(state.clone(), config, buffer.to_string(), channel.to_string());
        }
    }
}

/// A message has arrived in a conversation, so it is not empty any more.
pub(super) fn dm_has_messages(state: &AppState, account: &str, buffer: &str) {
    let was_empty = with(|g| g.empty_dms.remove(buffer));
    if was_empty {
        let effective = with(|g| g.effective(account, None, buffer));
        state.runtime.set_buffer_read_only(state, buffer, effective);
    }
}

/// The account itself, from READY's `user`: what it can say of itself to a server's rules.
pub(super) fn note_account(account: &str, user: &Value) {
    let id = user["id"].as_str().and_then(|i| i.parse::<u64>().ok()).unwrap_or(0);
    let created_at = if id == 0 { 0 } else { (((id >> 22) + 1_420_070_400_000) / 1000) as i64 };
    with(|g| {
        g.accounts.insert(
            account.to_string(),
            rules::AccountFacts {
                email_verified: user["verified"].as_bool(),
                // A phone is a verified one: Discord does not keep an unverified number on an account.
                phone_verified: Some(user["phone"].as_str().is_some_and(|p| !p.is_empty())),
                created_at,
            },
        );
    });
}

/// A server's rules, from its payload.
pub(super) fn note_guild(account: &str, guild: &Value, user_id: &str) {
    let Some(id) = guild["id"].as_str() else { return };
    // Some payloads keep the server's own fields under `properties`.
    let own = guild.get("properties").filter(|p| p.is_object()).unwrap_or(guild);
    let facts = rules::GuildFacts {
        verification: own["verification_level"].as_u64().unwrap_or(0).min(4) as u8,
        community: own["features"].as_array().is_some_and(|f| f.iter().any(|x| x.as_str() == Some("COMMUNITY"))),
        owner: own["owner_id"].as_str() == Some(user_id),
    };
    with(|g| {
        let key = (account.to_string(), id.to_string());
        let member = g.guilds.remove(&key).and_then(|(_, m)| m);
        g.guilds.insert(key, (facts, member));
    });
}

/// The account's own membership of a server, as its member object says (at login, and when it changes).
pub(super) fn note_member(state: &AppState, account: &str, guild_id: &str, member: Option<&Value>) {
    let Some(member) = member else { return };
    let when = |v: &Value| v.as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp());
    let facts = rules::MemberFacts {
        pending: member["pending"].as_bool(),
        flags: member["flags"].as_u64(),
        has_roles: member["roles"].as_array().is_some_and(|r| !r.is_empty()),
        joined_at: when(&member["joined_at"]),
        timed_out_until: when(&member["communication_disabled_until"]),
    };
    if let Some(at) = facts.joined_at {
        watch_badge_end(state, account, guild_id, at);
    }
    let changed = with(|g| {
        let key = (account.to_string(), guild_id.to_string());
        match g.guilds.get_mut(&key) {
            Some((_, slot)) => {
                let changed = slot.as_ref().map(|m| (m.pending, m.flags, m.has_roles, m.timed_out_until)) != Some((facts.pending, facts.flags, facts.has_roles, facts.timed_out_until));
                *slot = Some(facts);
                changed
            }
            None => false,
        }
    });
    if changed {
        reapply_guild(state, account, guild_id);
    }
}

/// A server channel, as it is registered: its slowmode, whether the account's permissions are held to it, whether it
/// has ever been written in.
pub(super) fn note_channel(state: &AppState, buffer: &str, channel: &Value, permissions: u64, is_owner: bool) {
    const ADMINISTRATOR: u64 = 1 << 3;
    const MANAGE_CHANNELS: u64 = 1 << 4;
    const MANAGE_MESSAGES: u64 = 1 << 13;
    let empty = channel.get("last_message_id").is_some_and(Value::is_null) && state.store.newest_msg_id(buffer).ok().flatten().is_none();
    with(|g| {
        g.channels.insert(
            buffer.to_string(),
            ChannelFacts {
                slowmode: channel["rate_limit_per_user"].as_u64().unwrap_or(0),
                bypass_slowmode: is_owner || permissions & (ADMINISTRATOR | MANAGE_CHANNELS | MANAGE_MESSAGES) != 0,
                full: is_owner || permissions & ADMINISTRATOR != 0,
            },
        );
        if empty {
            g.empty_channels.insert(buffer.to_string());
        } else {
            g.empty_channels.remove(buffer);
        }
    });
}

/// A channel's slowmode changed.
pub(super) fn set_slowmode(buffer: &str, seconds: u64) {
    with(|g| {
        if let Some(facts) = g.channels.get_mut(buffer) {
            facts.slowmode = seconds;
        }
    });
}

/// Whether a message is somebody speaking, and not Discord saying something about them. A system message carries its
/// subject as its author - "the account joined this server" (type 7) is authored by the account - and is no word of
/// theirs: counting it unlocked a server the moment it was joined. Only an ordinary message (0) and a reply (19) are
/// what a person wrote.
pub fn is_speech(message: &Value) -> bool {
    matches!(message["type"].as_u64().unwrap_or(0), 0 | 19)
}

/// A dispatch for a message was received in a place this connection knows: whatever it means for the locks.
pub(super) fn heard(state: &AppState, config: &DiscordAccountConfig, d: &Value, buffer: &str, is_dm: bool) {
    let account = config.account_id();
    let own = d["author"]["id"].as_str() == Some(config.user_id.as_str()) && is_speech(d);
    // The echo of a message sent from here carries the nonce it was sent with: it is not the account speaking from
    // another client, whatever else it is.
    let from_here = d["nonce"].as_str().is_some_and(|n| with(|g| g.nonces.contains(n)));
    if own {
        with(|g| g.last_own.insert(buffer.to_string(), Instant::now()));
    }
    // Something has been said in a channel that nobody had written in.
    let was_empty = with(|g| g.empty_channels.remove(buffer));
    if was_empty {
        let effective = with(|g| g.effective(&account, state.runtime.get_discord_guild(buffer).as_deref(), buffer));
        state.runtime.set_buffer_read_only(state, buffer, effective);
    }
    if is_dm {
        dm_has_messages(state, &account, buffer);
        if own && !from_here {
            let reached = with(|g| {
                let own = {
                    let facts = g.dms.entry(buffer.to_string()).or_default();
                    facts.own += 1;
                    facts.own
                };
                let reached = own >= rules::ESTABLISHED_DM_MESSAGES && g.established.insert(buffer.to_string());
                if reached {
                    save(g);
                }
                // Said from another client, here: a captcha that held this conversation is over.
                g.captcha.remove(&(account.clone(), buffer.to_string()));
                reached
            });
            if reached {
                let effective = with(|g| g.effective(&account, None, buffer));
                state.runtime.set_buffer_read_only(state, buffer, effective);
            }
        }
    } else if own && !from_here {
        if let Some(guild) = d["guild_id"].as_str() {
            with(|g| g.captcha.remove(&(account.clone(), guild.to_string())));
            spoke_in_guild(state, &account, guild);
        }
    }
}

/// Discord asked for a captcha where this was sent: that place is closed to this client for a while.
pub(super) fn hold_captcha(state: &AppState, buffer: &str) {
    let account = state.runtime.get_buffer(buffer).map(|b| b.account_id).unwrap_or_default();
    let scope = state.runtime.get_discord_guild(buffer).unwrap_or_else(|| buffer.to_string());
    with(|g| g.captcha.insert((account.clone(), scope.clone()), Instant::now() + CAPTCHA_HOLD));
    match state.runtime.get_discord_guild(buffer) {
        Some(guild) => reapply_guild(state, &account, &guild),
        None => {
            let effective = with(|g| g.effective(&account, None, buffer));
            state.runtime.set_buffer_read_only(state, buffer, effective);
        }
    }
}

/// A message was sent from here: the slowmode counts from now, and its echo is told from another client's.
pub(super) fn record_own_send(buffer: &str) {
    with(|g| g.last_own.insert(buffer.to_string(), Instant::now()));
}

/// Refuses a send to a place this client does not speak first in, or where the server's own rules (a level, a
/// timeout, a slowmode) say the account may not speak yet. Asked of every send; it makes no request unless a server
/// has never been asked whether it has onboarding.
pub(super) async fn ensure_may_speak(state: &AppState, buffer: &str) -> Result<()> {
    let account = state.runtime.get_buffer(buffer).map(|b| b.account_id).unwrap_or_default();
    let guild = state.runtime.get_discord_guild(buffer);
    if let Some(reason) = with(|g| g.lock_of(&account, guild.as_deref(), buffer)).reason() {
        bail!("{reason}");
    }
    if let Some(guild) = guild.as_deref() {
        learn_onboarding(state, &account, guild, buffer).await;
        let now = chrono::Utc::now().timestamp();
        if let Some(restriction) = with(|g| g.restriction(&account, guild, buffer, now)) {
            bail!("{}", restriction.sentence());
        }
    }
    let slowmode = with(|g| g.channels.get(buffer).map(|c| rules::slowmode_remaining(c.slowmode, g.last_own.get(buffer).copied(), c.bypass_slowmode, Instant::now())));
    if let Some(Some(wait)) = slowmode {
        bail!("This channel has slowmode on: wait {} more seconds.", wait.as_secs().max(1) + 1);
    }
    Ok(())
}

/// Asks a server whether it has onboarding on, once, but only where it could matter: a community server whose
/// onboarding this member has not completed. Everywhere else the answer is no request at all.
async fn learn_onboarding(state: &AppState, account: &str, guild: &str, buffer: &str) {
    const COMPLETED_ONBOARDING: u64 = 1 << 1;
    let key = (account.to_string(), guild.to_string());
    let needed = with(|g| {
        let Some((facts, member)) = g.guilds.get(&key) else { return false };
        let completed = member.as_ref().and_then(|m| m.flags).is_some_and(|f| f & COMPLETED_ONBOARDING != 0);
        let full = g.channels.get(buffer).is_some_and(|c| c.full);
        let recent = g.onboarding.get(&key).is_some_and(|(_, at)| at.elapsed() < Duration::from_secs(3600));
        facts.community && !facts.owner && !completed && !full && !recent
    });
    if !needed {
        return;
    }
    let Some(config) = state.accounts.get_discord(account) else { return };
    let enabled = async {
        let resp = http_client_for(&config.token).get(format!("{API_BASE}/guilds/{guild}/onboarding")).header("Authorization", &config.token).send_paced().await.ok()?;
        if !resp.status().is_success() {
            return None;
        }
        resp.json::<Value>().await.ok()?["enabled"].as_bool()
    }
    .await;
    with(|g| g.onboarding.insert(key, (enabled, Instant::now())));
    reapply_guild(state, account, guild);
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

type ChannelLocks = HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>;
static CHANNEL_LOCKS: Mutex<Option<ChannelLocks>> = Mutex::new(None);

/// Waits until nothing else is being sent to this channel, and holds the channel until it is dropped: two messages
/// to one channel do not overlap, as they cannot from a person at a keyboard.
pub(super) async fn channel_turn(channel_id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut slot = CHANNEL_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        slot.get_or_insert_with(HashMap::new).entry(channel_id.to_string()).or_default().clone()
    };
    lock.lock_owned().await
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
        let nonce = nonce();
        with(|g| {
            g.nonces.insert(nonce.clone());
            g.nonce_order.push_back(nonce.clone());
            while g.nonce_order.len() > 500 {
                if let Some(old) = g.nonce_order.pop_front() {
                    g.nonces.remove(&old);
                }
            }
        });
        payload["nonce"] = Value::from(nonce);
    }
    // Said outright, as Discord's own clients say them.
    if payload.get("tts").is_none() {
        payload["tts"] = Value::Bool(false);
    }
    if payload.get("flags").is_none() {
        payload["flags"] = Value::from(0);
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
                .filter(|m| is_speech(m))
                .filter_map(|m| m["id"].as_str())
                .collect();
            let elsewhere = hits.iter().any(|id| !sent_here.contains(*id));
            // A hit that is not speech (a join message) is not a word of theirs, and does not count among the results
            // either: only when more were found than the hits shown, and the shown ones are all speech, can the rest
            // be taken for speech too.
            let shown = body["messages"].as_array().map(|g| g.len()).unwrap_or(0);
            let all_speech = hits.len() == shown;
            Ok(if total > 0 && (elsewhere || (all_speech && total as usize > hits.len())) { Searched::Spoke } else { Searched::Never })
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

/// Counts the account's own messages in a conversation by asking Discord's search - the store holds only what has been
/// read here - so a conversation of years is not taken for a new one. One request, queued behind the others.
fn spawn_dm_count(state: AppState, config: DiscordAccountConfig, buffer: String, channel_id: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let _turn = TURN.lock().await;
        let account = config.account_id();
        let mut own = None;
        for _ in 0..INDEXING_TRIES {
            let resp = http_client_for(&config.token)
                .get(format!("{API_BASE}/channels/{channel_id}/messages/search"))
                .query(&[("author_id", config.user_id.as_str())])
                .header("Authorization", &config.token)
                .send_paced()
                .await;
            let Ok(resp) = resp else { break };
            let status = resp.status().as_u16();
            let retry = resp.headers().get("retry-after").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<f64>().ok());
            let body: Value = resp.json().await.unwrap_or(Value::Null);
            match status {
                200 => {
                    own = Some(count_own(&body, &with(|g| g.sent.clone())));
                    break;
                }
                202 | 429 => {
                    let wait = retry.or_else(|| body["retry_after"].as_f64()).unwrap_or(5.0).clamp(1.0, 60.0);
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                }
                _ => break,
            }
        }
        with(|g| {
            g.dm_checking.remove(&buffer);
            g.dm_checked.insert(buffer.clone());
            if let Some(own) = own {
                g.dms.entry(buffer.clone()).or_default().own = own;
                if own >= rules::ESTABLISHED_DM_MESSAGES && g.established.insert(buffer.clone()) {
                    save(g);
                }
            }
        });
        let effective = with(|g| g.effective(&account, None, &buffer));
        state.runtime.set_buffer_read_only(&state, &buffer, effective);
        tokio::time::sleep(SPACING).await;
    });
}

/// How many of the account's own messages a conversation's search answer says there are, not counting any that this
/// client sent (of those shown): the total is Discord's, the sent ones are known by id.
pub fn count_own(body: &Value, sent_here: &HashSet<String>) -> usize {
    let total = body["total_results"].as_u64().unwrap_or(0) as usize;
    let ours = body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|group| group.as_array()?.iter().find(|m| m["hit"].as_bool() == Some(true)).or_else(|| group.as_array()?.first()))
        .filter(|m| m["id"].as_str().is_some_and(|id| sent_here.contains(id)))
        .count();
    total.saturating_sub(ours)
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
        assert_eq!(plain["tts"], json!(false));
        assert_eq!(plain["flags"], json!(0));
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
    fn a_system_message_about_the_account_is_not_the_account_speaking() {
        assert!(is_speech(&json!({ "type": 0 })));
        assert!(is_speech(&json!({ "type": 19 })), "a reply is");
        assert!(is_speech(&json!({})), "no type is the ordinary one");
        // User join (7), boosts (8-11), pins (6), thread created (18), automod (24): Discord's, not theirs.
        for t in [1, 3, 4, 6, 7, 8, 18, 20, 21, 24] {
            assert!(!is_speech(&json!({ "type": t })), "type {t}");
        }
        // The same in a search: the join message of the account is a hit and is not a word of theirs.
        let sent = HashSet::new();
        let joined = json!({ "total_results": 1, "messages": [[{ "id": "5", "type": 7, "hit": true }]] });
        assert_eq!(read_search(200, None, &joined, &sent).unwrap(), Searched::Never);
        let spoke = json!({ "total_results": 2, "messages": [[{ "id": "5", "type": 7, "hit": true }], [{ "id": "6", "type": 0, "hit": true }]] });
        assert_eq!(read_search(200, None, &spoke, &sent).unwrap(), Searched::Spoke);
    }

    #[test]
    fn a_conversations_own_messages_are_counted_without_the_ones_sent_here() {
        let sent: HashSet<String> = ["2".to_string()].into();
        let body = json!({ "total_results": 7, "messages": [[{ "id": "1", "hit": true }], [{ "id": "2", "hit": true }]] });
        assert_eq!(count_own(&body, &sent), 6);
        assert_eq!(count_own(&json!({ "total_results": 0, "messages": [] }), &sent), 0);
        assert_eq!(count_own(&Value::Null, &sent), 0);
    }

    #[test]
    fn a_new_conversation_is_locked_until_enough_is_the_accounts_own_and_then_for_good() {
        let mut g = guard();
        assert_eq!(g.lock_of("a", None, "dm"), Lock::None, "nothing known of it");
        g.dms.insert("dm".into(), DmFacts { own: 2, ..Default::default() });
        assert_eq!(g.lock_of("a", None, "dm"), Lock::NewConversation);
        g.dm_checking.insert("dm".into());
        assert_eq!(g.lock_of("a", None, "dm"), Lock::Checking);
        g.dm_checking.clear();
        g.dms.insert("dm".into(), DmFacts { own: 5, ..Default::default() });
        g.established.insert("dm".into());
        assert_eq!(g.lock_of("a", None, "dm"), Lock::None);
        // Established is for good, but a message request or spam mark is Discord's word on it.
        g.dms.insert("dm".into(), DmFacts { own: 9, request: true, ..Default::default() });
        assert_eq!(g.lock_of("a", None, "dm"), Lock::MessageRequest);
        g.dms.insert("dm".into(), DmFacts { own: 9, spam: true, ..Default::default() });
        assert_eq!(g.lock_of("a", None, "dm"), Lock::SpamDm);
    }

    #[test]
    fn an_empty_channel_and_a_captcha_close_a_server_the_account_has_spoken_in() {
        let mut g = guard();
        g.spoken.entry("a".into()).or_default().insert("g".into());
        assert_eq!(g.lock_of("a", Some("g"), "b"), Lock::None);
        g.empty_channels.insert("b".into());
        assert_eq!(g.lock_of("a", Some("g"), "b"), Lock::EmptyChannel);
        g.empty_channels.clear();
        g.captcha.insert(("a".into(), "g".into()), Instant::now() + Duration::from_secs(60));
        assert_eq!(g.lock_of("a", Some("g"), "b"), Lock::Captcha);
        g.captcha.insert(("a".into(), "g".into()), Instant::now() - Duration::from_secs(1));
        assert_eq!(g.lock_of("a", Some("g"), "b"), Lock::None, "it ends");
    }

    #[test]
    fn a_server_that_asks_for_a_phone_closes_its_channels_until_there_is_one() {
        let mut g = guard();
        g.spoken.entry("a".into()).or_default().insert("g".into());
        g.accounts.insert("a".into(), rules::AccountFacts { email_verified: Some(true), phone_verified: Some(false), created_at: 1 });
        g.guilds.insert(
            ("a".into(), "g".into()),
            (rules::GuildFacts { verification: 4, ..Default::default() }, Some(rules::MemberFacts { pending: Some(false), flags: Some(0), ..Default::default() })),
        );
        assert!(matches!(g.lock_of("a", Some("g"), "b"), Lock::Participation(r) if r.contains("phone")));
        g.accounts.get_mut("a").unwrap().phone_verified = Some(true);
        assert_eq!(g.lock_of("a", Some("g"), "b"), Lock::None);
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
