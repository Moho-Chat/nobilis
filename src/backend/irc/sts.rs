//! Strict Transport Security for IRC, as IRCv3 defines it.
//!
//! A network advertises `sts` in its `CAP LS` with two parameters, and which
//! of them applies depends entirely on how the connection carrying the
//! advertisement was made:
//!
//! - Over **plaintext**, only `port=` counts. It means "come back on this port
//!   over TLS". Nothing is remembered from a plaintext connection, because an
//!   attacker who can rewrite the plaintext stream can write the policy too,
//!   and a remembered forgery would outlive the attack.
//! - Over **TLS**, only `duration=` counts. It means "for this many seconds,
//!   refuse to talk to me any other way". That is the half worth persisting,
//!   and it is what protects the *next* connection from being talked down to
//!   plaintext by somebody sitting in the middle of it.
//!
//! `duration=0` over TLS is a network withdrawing its policy, and is obeyed -
//! it is the only way out for a network that turns TLS off, and ignoring it
//! would strand every client that ever connected.
//!
//! Kept in a file of its own rather than in the account, because it is not a
//! setting somebody chose: it is something a network said, with an expiry, and
//! it has to survive a client that is upgraded or reinstalled between two
//! connections or it protects nothing.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// What a server's `sts=` value said. Both fields are optional because a
/// server sends the one that applies to the connection it is speaking over.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct StsAdvert {
    pub duration: Option<u64>,
    pub port: Option<u16>,
}

/// Reads the value half of an `sts=...` capability.
///
/// The parameters are comma-separated `key=value` pairs, in either order, and
/// a server may send keys this does not know - those are skipped rather than
/// making the whole advertisement unreadable, which is what lets the spec add
/// one later without breaking clients that predate it.
pub fn parse_sts(value: &str) -> StsAdvert {
    let mut out = StsAdvert::default();
    for part in value.split(',') {
        let Some((key, val)) = part.split_once('=') else { continue };
        match key.trim() {
            "duration" => out.duration = val.trim().parse().ok(),
            "port" => out.port = val.trim().parse().ok(),
            _ => {}
        }
    }
    out
}

/// Finds the `sts` token in a `CAP LS` list and reads its value.
///
/// Returns `None` for a server that does not advertise it, and for one that
/// advertises a bare `sts` with no parameters - which says nothing actionable
/// and is treated as if it had not been said.
pub fn sts_from_caps(list: &str) -> Option<StsAdvert> {
    list.split_whitespace()
        .find_map(|tok| tok.strip_prefix("sts="))
        .map(parse_sts)
        .filter(|a| a.duration.is_some() || a.port.is_some())
}

/// One network's policy: the port it wants to be reached on, and when the
/// promise runs out.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct StsPolicy {
    pub port: u16,
    /// Unix seconds. Past this, the policy is gone and a plaintext connection
    /// is somebody's own business again.
    pub expires_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Default)]
pub struct StsPrefs {
    #[serde(default)]
    pub hosts: BTreeMap<String, StsPolicy>,
}

pub struct StsStore {
    path: PathBuf,
    inner: Mutex<StsPrefs>,
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl StsStore {
    pub fn open(path: PathBuf) -> Self {
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| toml::from_str(&t).ok())
            // A file that will not parse must not stop the daemon starting.
            // No policies is a working configuration, and the worst it costs
            // is one connection made the way the account was configured.
            .unwrap_or_default();
        Self { path, inner: Mutex::new(inner) }
    }

    /// This host's policy, if it has one that has not run out.
    ///
    /// Host names are matched case-insensitively, because a policy learned
    /// from `irc.Example.NET` has to apply when somebody types the same name
    /// in lowercase - the two are one network, and an STS that could be
    /// stepped around by changing capitalisation would not be worth having.
    pub fn policy(&self, host: &str) -> Option<StsPolicy> {
        let policy = *self.inner.lock().unwrap().hosts.get(&host.to_ascii_lowercase())?;
        (policy.expires_at > now_secs()).then_some(policy)
    }

    /// Records what a network said over TLS. A zero duration withdraws it.
    pub fn remember(&self, host: &str, port: u16, duration: u64) {
        self.update(|prefs| {
            let key = host.to_ascii_lowercase();
            if duration == 0 {
                prefs.hosts.remove(&key);
            } else {
                prefs.hosts.insert(key, StsPolicy { port, expires_at: now_secs().saturating_add(duration) });
            }
        });
    }

    fn update(&self, f: impl FnOnce(&mut StsPrefs)) {
        let snapshot = {
            let mut prefs = self.inner.lock().unwrap();
            f(&mut prefs);
            // Expired policies are dropped whenever the file is rewritten, so
            // it does not accumulate a row per network ever connected to.
            let now = now_secs();
            prefs.hosts.retain(|_, p| p.expires_at > now);
            prefs.clone()
        };
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = toml::to_string_pretty(&snapshot) {
            let _ = std::fs::write(&self.path, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_parameters_in_either_order() {
        assert_eq!(parse_sts("duration=2592000,port=6697"), StsAdvert { duration: Some(2592000), port: Some(6697) });
        assert_eq!(parse_sts("port=6697,duration=300"), StsAdvert { duration: Some(300), port: Some(6697) });
    }

    /// A key this does not know does not make the rest unreadable - the spec
    /// is allowed to grow one.
    #[test]
    fn an_unknown_parameter_is_stepped_over() {
        assert_eq!(parse_sts("preload,duration=60,something=else"), StsAdvert { duration: Some(60), port: None });
    }

    /// Zero is a real value, not a missing one: it is how a network withdraws
    /// its policy, and reading it as "unset" would make that impossible.
    #[test]
    fn a_zero_duration_is_a_value() {
        assert_eq!(parse_sts("duration=0").duration, Some(0));
    }

    #[test]
    fn finds_sts_among_other_capabilities() {
        let caps = "server-time sts=duration=60,port=6697 away-notify";
        assert_eq!(sts_from_caps(caps), Some(StsAdvert { duration: Some(60), port: Some(6697) }));
        assert_eq!(sts_from_caps("server-time away-notify"), None);
        // A bare `sts` says nothing actionable.
        assert_eq!(sts_from_caps("server-time sts away-notify"), None);
    }

    /// Each test gets its own directory: they run in one process, so a
    /// shared path would have them reading each other's policies.
    fn store(name: &str) -> (StsStore, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("moho-sts-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        (StsStore::open(dir.join("irc-sts.toml")), dir)
    }

    #[test]
    fn a_policy_is_remembered_and_read_back_whatever_the_capitalisation() {
        let (s, dir) = store("caps");
        s.remember("irc.Example.NET", 6697, 3600);
        assert_eq!(s.policy("irc.example.net").map(|p| p.port), Some(6697));
        assert_eq!(s.policy("IRC.EXAMPLE.NET").map(|p| p.port), Some(6697));
        assert!(s.policy("irc.other.net").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The only way out for a network that turns TLS off.
    #[test]
    fn a_zero_duration_withdraws_the_policy() {
        let (s, dir) = store("withdraw");
        s.remember("irc.example.net", 6697, 3600);
        s.remember("irc.example.net", 6697, 0);
        assert!(s.policy("irc.example.net").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_policy_that_has_run_out_is_not_a_policy() {
        let (s, dir) = store("expired");
        s.remember("irc.example.net", 6697, 3600);
        // Reach in and age it rather than sleeping.
        s.inner.lock().unwrap().hosts.get_mut("irc.example.net").unwrap().expires_at = 1;
        assert!(s.policy("irc.example.net").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_policy_survives_being_written_and_reopened() {
        let dir = std::env::temp_dir().join(format!("moho-sts-{}-reopen", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("irc-sts.toml");
        StsStore::open(path.clone()).remember("irc.example.net", 6697, 3600);
        assert_eq!(StsStore::open(path).policy("irc.example.net").map(|p| p.port), Some(6697));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
