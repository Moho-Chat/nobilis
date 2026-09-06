//! People whose messages should not arrive, on the services that will not do
//! it for you.
//!
//! Matrix keeps an ignore list on the homeserver, which is the right place for
//! one: it follows the account to every client and the server stops sending
//! the messages at all. Nothing else here has that. Discord has blocking,
//! which is a real relationship and visible to the person blocked, but its
//! gateway still delivers what they say - the official client is what hides
//! it. IRC, Kick and Sneedchat have nothing whatsoever.
//!
//! So this is the local half: a list per account, applied where every message
//! already passes - `Runtime::record_message_at`. Held in the daemon rather
//! than in a window for the same reason the highlight keywords are: a window
//! could hide a line and could not stop it counting as unread, notifying, or
//! being written into the log.
//!
//! Matched on the name and on the protocol id where there is one. A pattern
//! may carry `*`, because "everything from this bot family" is a real thing
//! to want on IRC - though a nick is only ever a rented name there, and
//! somebody who changes it walks straight back through. Say so rather than
//! pretending otherwise: this is the ignore every IRC client has had for
//! thirty years, and it has always had that hole in it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The file's contents, at `~/.config/nobilis/ignores.toml`.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct IgnorePrefs {
    /// Who is ignored, by account id.
    #[serde(default)]
    pub accounts: BTreeMap<String, Vec<String>>,
}

pub struct IgnoreStore {
    path: PathBuf,
    inner: Mutex<IgnorePrefs>,
}

impl IgnoreStore {
    pub fn open(path: PathBuf) -> Self {
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            // A file that will not parse must not stop the daemon starting -
            // ignoring nobody is a working configuration.
            .unwrap_or_default();
        Self { path, inner: Mutex::new(inner) }
    }

    pub fn for_account(&self, account_id: &str) -> Vec<String> {
        self.inner.lock().unwrap().accounts.get(account_id).cloned().unwrap_or_default()
    }

    /// Adds or removes one entry, and says whether anything changed.
    pub fn set(&self, account_id: &str, target: &str, ignored: bool) -> bool {
        let target = target.trim();
        if target.is_empty() {
            return false;
        }
        let mut changed = false;
        self.update(|prefs| {
            let list = prefs.accounts.entry(account_id.to_string()).or_default();
            let known = list.iter().position(|t| t.eq_ignore_ascii_case(target));
            match (known, ignored) {
                (None, true) => {
                    list.push(target.to_string());
                    changed = true;
                }
                (Some(at), false) => {
                    list.remove(at);
                    changed = true;
                }
                _ => {}
            }
            // An empty list and no list mean the same thing, and only one of
            // them leaves a row in the file behind.
            if list.is_empty() {
                prefs.accounts.remove(account_id);
            }
        });
        changed
    }

    /// Drops an account's list, for an account being removed.
    pub fn forget(&self, account_id: &str) {
        self.update(|prefs| {
            prefs.accounts.remove(account_id);
        });
    }

    fn update(&self, edit: impl FnOnce(&mut IgnorePrefs)) {
        let mut prefs = self.inner.lock().unwrap();
        edit(&mut prefs);
        let Ok(text) = toml::to_string_pretty(&*prefs) else { return };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        // Through a temporary file, so an interrupted write cannot leave half
        // a list behind.
        let tmp = self.path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// Whether this sender is on the list.
///
/// The name and the protocol id are both checked, because which one somebody
/// ignored depends on where they pressed it: a member list knows the id, and
/// a typed `/ignore` knows only the name.
pub fn is_ignored(list: &[String], from: &str, sender_id: Option<&str>) -> bool {
    list.iter().any(|pattern| {
        matches_pattern(pattern, from) || sender_id.is_some_and(|id| matches_pattern(pattern, id))
    })
}

/// One pattern against one name, case-insensitively, with `*` for anything.
///
/// Written out rather than pulled in as a dependency: the whole grammar is one
/// wildcard, and a glob crate would be a dependency for six lines.
fn matches_pattern(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern.eq_ignore_ascii_case(name);
    }
    let pattern = pattern.to_lowercase();
    let name = name.to_lowercase();
    let mut rest = name.as_str();
    let parts: Vec<&str> = pattern.split('*').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            None => return false,
            Some(at) => {
                // A pattern not starting with `*` is anchored at the front,
                // or "*bot" would match "robot" and "bottle" alike.
                if i == 0 && at != 0 {
                    return false;
                }
                rest = &rest[at + part.len()..];
            }
        }
    }
    // And anchored at the end unless it finishes with a wildcard.
    parts.last().is_some_and(|last| last.is_empty() || rest.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_or_an_id_is_enough() {
        let list = vec!["Spammer".to_string(), "1234567890".to_string()];
        assert!(is_ignored(&list, "spammer", None));
        assert!(is_ignored(&list, "somebody", Some("1234567890")));
        assert!(!is_ignored(&list, "somebody", Some("999")));
        assert!(!is_ignored(&[], "spammer", None));
    }

    #[test]
    fn a_star_stands_for_anything() {
        assert!(matches_pattern("spam*", "spambot9000"));
        assert!(matches_pattern("*bot", "helpbot"));
        assert!(matches_pattern("*", "anybody at all"));
        assert!(matches_pattern("a*c", "abbbc"));
    }

    #[test]
    fn a_star_does_not_stand_for_everything() {
        // Anchored at both ends unless the pattern says otherwise, or
        // "*bot" would take "bottle" with it.
        assert!(!matches_pattern("bot*", "robot"));
        assert!(!matches_pattern("*bot", "bottle"));
        assert!(!matches_pattern("spam", "spambot"));
    }

    #[test]
    fn adding_twice_changes_nothing_the_second_time() {
        let dir = std::env::temp_dir().join(format!("moho-ignores-{}", std::process::id()));
        let store = IgnoreStore::open(dir.join("ignores.toml"));
        assert!(store.set("irc:a", "Spammer", true));
        assert!(!store.set("irc:a", "spammer", true));
        assert_eq!(store.for_account("irc:a"), vec!["Spammer"]);
        // And removing it leaves no row behind, so the file does not grow a
        // list of empty lists.
        assert!(store.set("irc:a", "SPAMMER", false));
        assert!(store.for_account("irc:a").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
