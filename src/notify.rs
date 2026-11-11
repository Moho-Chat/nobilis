//! How much a conversation is allowed to interrupt somebody.
//!
//! One answer per conversation, the same four everywhere: what the service
//! would do unasked, every message, only what is aimed at you, or nothing.
//! Element offers exactly these for a room, and Discord, Stoat and Steam have
//! the same four under other names - so a frontend asks one question and the
//! services differ only in where the answer is kept.
//!
//! Two places keep it, and which one is a fact about the service rather than a
//! choice. Matrix, Discord, Stoat, Steam and Telegram hold it on the account,
//! so a room quietened on a phone is quiet here and the other way round: those
//! backends read it, tell the runtime (`Runtime::set_notify_level`), and write
//! it back when asked. Everything else has no such idea, and the level is
//! this daemon's own - `NotifyStore`, below - which every frontend on this
//! machine then shares. A window keeping its own copy was the old behaviour
//! and it is why a mute lifted anywhere else never reached it.
//!
//! The decision itself lives here too (`NotifyLevel::announces`) because the
//! daemon is the only place that sees a message before it becomes a
//! notification: a window can decline to show one, and cannot make one that
//! was never sent.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// What a conversation is set to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyLevel {
    /// Nothing was said for this conversation, so the account's or the
    /// service's own default stands: every direct message, and in a channel
    /// whatever mentions you.
    #[default]
    Default,
    /// Every message.
    All,
    /// Only what mentions you, or matches a word you asked to hear about -
    /// in a direct message as well as in a channel.
    Mentions,
    /// Nothing announces itself. What is waiting still counts.
    Mute,
}

impl NotifyLevel {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "default" => Some(Self::Default),
            "all" => Some(Self::All),
            "mentions" => Some(Self::Mentions),
            "mute" => Some(Self::Mute),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::All => "all",
            Self::Mentions => "mentions",
            Self::Mute => "mute",
        }
    }

    /// For `skip_serializing_if`: a conversation left alone says nothing.
    pub fn is_default(&self) -> bool {
        *self == Self::Default
    }

    /// Whether a message that has been read, and is not ours, is announced.
    ///
    /// `Default` is the rule the daemon has always had - a direct message
    /// always, a channel only when it names you - which is also what Matrix and
    /// Discord do unasked, so a conversation nobody touched behaves the same
    /// as before this existed.
    pub fn announces(self, is_dm: bool, is_highlight: bool) -> bool {
        match self {
            Self::Mute => false,
            Self::All => true,
            Self::Mentions => is_highlight,
            Self::Default => is_dm || is_highlight,
        }
    }
}

/// Whether the account itself keeps this conversation's level, by who owns it.
///
/// The same list the window uses to decide what to offer: a service on it
/// reads and writes the setting on the account, and is asked to when the level
/// changes. Anything else is kept here.
pub fn held_on_account(account_id: &str) -> bool {
    ["matrix:", "discord:", "stoat:", "steam:", "telegram:"].iter().any(|prefix| account_id.starts_with(prefix))
}

/// Sets one conversation's level, wherever its service keeps it.
///
/// A service that holds the answer on the account is asked to write it there,
/// and what it says back is what the conversation then shows: the buffer is
/// updated by the backend from the server's own reply, not from what was
/// asked, so a change the server did not take is reported rather than drawn.
/// Everything else keeps it here.
///
/// A service that cannot express a level says so rather than rounding it to
/// the nearest one it can. Telegram has no "mentions only" for a chat, and a
/// menu that offered it would be promising a setting that would quietly be
/// something else.
pub async fn set_level(state: &crate::state::AppState, buffer: &crate::model::Buffer, level: NotifyLevel) -> Result<(), String> {
    use crate::backend;
    let account = buffer.account_id.as_str();
    // An XMPP room keeps it in its bookmark, for every client of the account (XEP-0492); a conversation with a person has no
    // such place and keeps it here.
    if account.starts_with("xmpp:") && buffer.kind == "channel" {
        return backend::xmpp::bookmarks::set_notify(state, buffer, level).await;
    }
    if !held_on_account(account) {
        state.runtime.set_local_notify(state, &buffer.id, level);
        return Ok(());
    }
    let plain = |e: anyhow::Error| format!("{e:#}");
    if account.starts_with("matrix:") {
        backend::matrix::set_room_level(state, account, &buffer.id, level).await.map_err(plain)
    } else if account.starts_with("discord:") {
        backend::discord::mutes::set_level(state, account, &buffer.id, level).await.map_err(plain)
    } else if account.starts_with("stoat:") {
        let handle = state.runtime.stoat_handle(account).ok_or("that Stoat account is not connected")?;
        backend::stoat::settings::set_level(state, &handle, buffer, level).await.map_err(plain)
    } else if account.starts_with("steam:") {
        // A room of a group has the three; a conversation with a friend has
        // only whether their messages notify at all.
        if let Some((group, _)) = backend::steam::chatgroups::ids_of(buffer) {
            return backend::steam::chatgroups::set_level(state, account, group, level).await;
        }
        let friend = buffer
            .remote_id
            .as_deref()
            .and_then(|id| id.parse::<u64>().ok())
            .and_then(backend::steam::steamid::account_of)
            .ok_or("this conversation is not with a Steam account")?;
        if !matches!(level, NotifyLevel::Default | NotifyLevel::Mute) {
            return Err("Steam keeps only whether a friend's messages notify you, so there is nothing between default and mute".to_string());
        }
        let link = state.runtime.steam_link(account).ok_or("this Steam account is not connected")?;
        backend::steam::prefs::set_muted(&link, friend, level == NotifyLevel::Mute).await?;
        state.runtime.set_notify_level(state, &buffer.id, level, level);
        Ok(())
    } else {
        if !matches!(level, NotifyLevel::Default | NotifyLevel::Mute) {
            return Err("Telegram keeps only whether a chat is muted, so there is nothing between default and mute".to_string());
        }
        backend::telegram::notify::set_muted(state, buffer, level == NotifyLevel::Mute).await
    }
}

/// The file's contents, at `~/.config/nobilis/notify.toml`.
#[derive(Serialize, Deserialize, Clone, Default)]
struct NotifyPrefs {
    /// Buffer id -> the level it was set to. A conversation at `Default` has
    /// no row: absent and default mean the same thing, and only one of them
    /// leaves something in the file.
    #[serde(default)]
    levels: BTreeMap<String, NotifyLevel>,
}

/// The levels of conversations whose service has nowhere to keep one.
pub struct NotifyStore {
    path: PathBuf,
    inner: Mutex<NotifyPrefs>,
}

impl NotifyStore {
    pub fn open(path: PathBuf) -> Self {
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            // A file that will not parse must not stop the daemon starting:
            // every conversation at its default is a working configuration.
            .unwrap_or_default();
        Self { path, inner: Mutex::new(inner) }
    }

    pub fn get(&self, buffer_id: &str) -> NotifyLevel {
        self.inner.lock().unwrap().levels.get(buffer_id).copied().unwrap_or_default()
    }

    /// Sets one conversation's level, and says whether that changed anything.
    pub fn set(&self, buffer_id: &str, level: NotifyLevel) -> bool {
        let mut changed = false;
        self.update(|prefs| {
            changed = prefs.levels.get(buffer_id).copied().unwrap_or_default() != level;
            if level.is_default() {
                prefs.levels.remove(buffer_id);
            } else {
                prefs.levels.insert(buffer_id.to_string(), level);
            }
        });
        changed
    }

    /// Drops everything kept for an account, for one being removed.
    pub fn forget_account(&self, account_id: &str) {
        let prefix = format!("{account_id}|");
        self.update(|prefs| prefs.levels.retain(|id, _| !id.starts_with(&prefix)));
    }

    fn update(&self, edit: impl FnOnce(&mut NotifyPrefs)) {
        let mut prefs = self.inner.lock().unwrap();
        edit(&mut prefs);
        let Ok(text) = toml::to_string_pretty(&*prefs) else { return };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Through a temporary file, so an interrupted write cannot leave half
        // a file behind.
        let tmp = self.path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_each_level_announces() {
        // (level, dm, highlight) -> announced
        let cases = [
            (NotifyLevel::Default, true, false, true),
            (NotifyLevel::Default, false, false, false),
            (NotifyLevel::Default, false, true, true),
            (NotifyLevel::All, false, false, true),
            (NotifyLevel::All, true, false, true),
            // Mentions only means it in a direct message too.
            (NotifyLevel::Mentions, true, false, false),
            (NotifyLevel::Mentions, true, true, true),
            (NotifyLevel::Mentions, false, true, true),
            (NotifyLevel::Mute, true, true, false),
            (NotifyLevel::Mute, false, true, false),
        ];
        for (level, dm, highlight, announced) in cases {
            assert_eq!(level.announces(dm, highlight), announced, "{level:?} dm={dm} highlight={highlight}");
        }
    }

    #[test]
    fn the_names_round_trip_and_unknown_ones_are_refused() {
        for level in [NotifyLevel::Default, NotifyLevel::All, NotifyLevel::Mentions, NotifyLevel::Mute] {
            assert_eq!(NotifyLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(NotifyLevel::parse("loud"), None);
        assert_eq!(NotifyLevel::parse(""), None);
    }

    #[test]
    fn only_services_with_a_setting_of_their_own_keep_it_on_the_account() {
        for held in ["matrix:@a:b", "discord:1", "stoat:x", "steam:7656", "telegram:42"] {
            assert!(held_on_account(held), "{held}");
        }
        for local in ["irc:libera", "twitch:x", "kick:y", "sneedchat:z"] {
            assert!(!held_on_account(local), "{local}");
        }
    }

    #[test]
    fn a_level_is_kept_across_a_restart_and_default_leaves_no_row() {
        let dir = std::env::temp_dir().join(format!("nobilis-notify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("notify.toml");

        let store = NotifyStore::open(path.clone());
        assert_eq!(store.get("irc:a|#one"), NotifyLevel::Default);
        assert!(store.set("irc:a|#one", NotifyLevel::Mute));
        assert!(!store.set("irc:a|#one", NotifyLevel::Mute), "no change, and says so");
        assert!(store.set("irc:a|#two", NotifyLevel::Mentions));

        let reopened = NotifyStore::open(path.clone());
        assert_eq!(reopened.get("irc:a|#one"), NotifyLevel::Mute);
        assert_eq!(reopened.get("irc:a|#two"), NotifyLevel::Mentions);

        assert!(reopened.set("irc:a|#one", NotifyLevel::Default));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("#one"), "default is absence: {text}");

        reopened.forget_account("irc:a");
        assert_eq!(NotifyStore::open(path).get("irc:a|#two"), NotifyLevel::Default);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_will_not_parse_is_an_empty_one() {
        let dir = std::env::temp_dir().join(format!("nobilis-notify-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("notify.toml");
        std::fs::write(&path, "levels = [not toml").unwrap();
        assert_eq!(NotifyStore::open(path).get("anything"), NotifyLevel::Default);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
