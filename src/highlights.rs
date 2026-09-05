//! The words that make a message worth being told about, besides your name.
//!
//! Kept here rather than in the client because this is where the decision is
//! made: `Runtime::record_message` marks a message highlighted, and that one
//! flag feeds the stored column the mentions inbox reads, the event the client
//! styles from, and the notification. A keyword held in a window's own
//! preferences could colour a line and could never notify anybody, which is
//! the whole point of having one.
//!
//! Two lists. The global one is for words that mean the same wherever they are
//! said - a surname, a project - and the per-account ones for words that only
//! matter on one network, which is most of them: a name that reaches you on
//! IRC is somebody else's ordinary vocabulary on Discord.
//!
//! A side table keyed by account id rather than a field on each account,
//! because there are five account structs and `display_name` already shows
//! what that costs - a five-branch fan-out in the setter and another in the
//! getter. The price of this shape is that removing an account has to remove
//! its words too; `removeAccount` does that.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The file's contents, at `~/.config/nobilis/highlights.toml`.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct HighlightPrefs {
    /// Words that count on every account.
    #[serde(default)]
    pub global: Vec<String>,
    /// Words that count on one account, by account id.
    #[serde(default)]
    pub accounts: BTreeMap<String, Vec<String>>,
}

pub struct HighlightStore {
    path: PathBuf,
    inner: Mutex<HighlightPrefs>,
}

impl HighlightStore {
    pub fn open(path: PathBuf) -> Self {
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            // A file that will not parse must not stop the daemon starting.
            // No keywords is a working configuration - it is what everybody
            // has until they add one.
            .unwrap_or_default();
        Self { path, inner: Mutex::new(inner) }
    }

    /// The words that count on every account.
    pub fn global(&self) -> Vec<String> {
        self.inner.lock().unwrap().global.clone()
    }

    /// The words this one account has of its own.
    pub fn account(&self, account_id: &str) -> Vec<String> {
        self.inner.lock().unwrap().accounts.get(account_id).cloned().unwrap_or_default()
    }

    /// Everything that counts in a conversation on this account: the global
    /// words and its own, together, since a message only has to match one.
    pub fn for_account(&self, account_id: &str) -> Vec<String> {
        let prefs = self.inner.lock().unwrap();
        let mut out = prefs.global.clone();
        if let Some(own) = prefs.accounts.get(account_id) {
            out.extend(own.iter().cloned());
        }
        out
    }

    pub fn set_global(&self, words: Vec<String>) {
        self.update(|prefs| prefs.global = tidy(words));
    }

    /// Sets one account's words, and forgets the account entirely when the
    /// list is emptied - an empty list and no list mean the same thing, and
    /// only one of them leaves a row behind.
    pub fn set_account(&self, account_id: &str, words: Vec<String>) {
        self.update(|prefs| {
            let words = tidy(words);
            if words.is_empty() {
                prefs.accounts.remove(account_id);
            } else {
                prefs.accounts.insert(account_id.to_string(), words);
            }
        });
    }

    /// Drops an account's words, for an account that is being removed.
    pub fn forget(&self, account_id: &str) {
        self.update(|prefs| {
            prefs.accounts.remove(account_id);
        });
    }

    /// Applies a change and writes the file out, through a temporary file so
    /// an interrupted write cannot leave a half-written one behind.
    fn update(&self, edit: impl FnOnce(&mut HighlightPrefs)) {
        let mut prefs = self.inner.lock().unwrap();
        edit(&mut prefs);
        let Ok(text) = toml::to_string_pretty(&*prefs) else { return };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = self.path.with_extension("toml.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// What a typed list actually contains once the typing is taken out: no blank
/// entries, no surrounding spaces, nothing said twice.
fn tidy(words: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in words {
        let word = word.trim().to_string();
        if word.is_empty() || out.iter().any(|w| w.eq_ignore_ascii_case(&word)) {
            continue;
        }
        out.push(word);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_list_loses_its_blanks_and_its_repeats() {
        let words = tidy(vec!["  moho ".into(), "".into(), "MOHO".into(), "nobilis".into(), "   ".into()]);
        assert_eq!(words, vec!["moho".to_string(), "nobilis".to_string()]);
    }

    #[test]
    fn an_accounts_words_are_its_own_plus_everybodys() {
        let dir = std::env::temp_dir().join(format!("moho-highlights-{}", std::process::id()));
        let store = HighlightStore::open(dir.join("highlights.toml"));
        store.set_global(vec!["salastil".into()]);
        store.set_account("irc:coreirc", vec!["moho".into()]);

        let mut both = store.for_account("irc:coreirc");
        both.sort();
        assert_eq!(both, vec!["moho".to_string(), "salastil".to_string()]);
        // Another account gets the global words and nobody else's.
        assert_eq!(store.for_account("discord:1"), vec!["salastil".to_string()]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn emptying_a_list_removes_it_rather_than_storing_nothing() {
        let dir = std::env::temp_dir().join(format!("moho-highlights-empty-{}", std::process::id()));
        let store = HighlightStore::open(dir.join("highlights.toml"));
        store.set_account("irc:coreirc", vec!["moho".into()]);
        store.set_account("irc:coreirc", vec![]);
        assert!(store.inner.lock().unwrap().accounts.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The words have to survive a restart, which is the only thing the file
    /// is for.
    #[test]
    fn what_was_written_is_read_back() {
        let dir = std::env::temp_dir().join(format!("moho-highlights-rw-{}", std::process::id()));
        let path = dir.join("highlights.toml");
        {
            let store = HighlightStore::open(path.clone());
            store.set_global(vec!["salastil".into()]);
            store.set_account("kick:me", vec!["raid".into(), "gifted".into()]);
        }
        let reopened = HighlightStore::open(path);
        assert_eq!(reopened.global(), vec!["salastil".to_string()]);
        assert_eq!(reopened.account("kick:me"), vec!["raid".to_string(), "gifted".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
