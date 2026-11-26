//! How often an account may log in to Discord afresh.
//!
//! A Discord client keeps one session open for days and, when the connection drops, *resumes* it. A fresh login (an
//! IDENTIFY) is what happens when a client starts or its session is lost, and a client that does it many times a day
//! looks like automation to Discord's spam detection. Restarting moho is exactly that, so the number of fresh logins an
//! account makes is kept - across restarts, in a file - and when it is over the limit the connection is held until it
//! is under again, with the window told why. A resume costs nothing and is never held.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Mutex;

// Half of what arikawa, the library under Dissent, allows any account: it
// waits five seconds between logins and allows a thousand a day.

/// Seconds there must be between one fresh login and the next.
pub const MIN_GAP: i64 = 10;
/// Fresh logins an account may make in any day.
pub const PER_DAY: usize = 500;

const DAY: i64 = 24 * 3600;

/// Whether one more fresh login may be made at `now` (Unix seconds), given the times of the earlier ones. Answers
/// the number of seconds to wait when not. The list is kept as it should be saved: only what still counts.
pub fn admit(history: &mut Vec<i64>, now: i64) -> Result<(), i64> {
    history.retain(|t| now - t < DAY);
    history.sort_unstable();
    let mut wait = 0;
    // Too soon after the last one.
    if let Some(&last) = history.last() {
        wait = wait.max(last + MIN_GAP - now);
    }
    // Over the day's.
    if history.len() >= PER_DAY {
        wait = wait.max(history[history.len() - PER_DAY] + DAY - now);
    }
    if wait > 0 {
        return Err(wait.max(1));
    }
    history.push(now);
    Ok(())
}

#[derive(Default, Serialize, Deserialize)]
struct Saved {
    /// Account id -> when its fresh logins were made.
    accounts: HashMap<String, Vec<i64>>,
}

fn file() -> std::path::PathBuf {
    crate::default_data_dir().join("discord-logins.json")
}

fn held() -> &'static Mutex<Option<Saved>> {
    static H: Mutex<Option<Saved>> = Mutex::new(None);
    &H
}

/// Asks to make a fresh login now. `Err` carries how many seconds to wait first; `Ok` means it has been counted.
pub fn ask(account_id: &str) -> Result<(), i64> {
    let now = chrono::Utc::now().timestamp();
    let mut slot = held().lock().unwrap_or_else(|e| e.into_inner());
    let saved = slot.get_or_insert_with(|| std::fs::read_to_string(file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default());
    let history = saved.accounts.entry(account_id.to_string()).or_default();
    let answer = admit(history, now);
    if answer.is_ok() {
        if let Ok(text) = serde_json::to_string(saved) {
            if let Some(dir) = file().parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(file(), text);
        }
    }
    answer
}

/// The accounts whose login is being held now, each with the way to let it through.
type Held = Mutex<HashMap<String, std::sync::Arc<tokio::sync::Notify>>>;

fn waiting() -> &'static Held {
    static W: std::sync::OnceLock<Held> = std::sync::OnceLock::new();
    W.get_or_init(Default::default)
}

/// Marks an account as being held, answering what to wait on besides the clock.
pub fn begin_hold(account_id: &str) -> std::sync::Arc<tokio::sync::Notify> {
    waiting().lock().unwrap_or_else(|e| e.into_inner()).entry(account_id.to_string()).or_default().clone()
}

/// The hold is over, by the clock or by the person.
pub fn end_hold(account_id: &str) {
    waiting().lock().unwrap_or_else(|e| e.into_inner()).remove(account_id);
}

/// Lets a held login through now. `false` when nothing was being held, so that pressing the button twice, or after the
/// hold ended, cannot leave a pass waiting for some later hold.
pub fn connect_anyway(account_id: &str) -> bool {
    match waiting().lock().unwrap_or_else(|e| e.into_inner()).get(account_id) {
        Some(note) => {
            note.notify_one();
            true
        }
        None => false,
    }
}

/// Counts a login that was let through past the limit: it still counts towards the next ones.
pub fn record(account_id: &str) {
    let now = chrono::Utc::now().timestamp();
    let mut slot = held().lock().unwrap_or_else(|e| e.into_inner());
    let saved = slot.get_or_insert_with(|| std::fs::read_to_string(file()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default());
    let history = saved.accounts.entry(account_id.to_string()).or_default();
    history.retain(|t| now - t < DAY);
    history.push(now);
    if let Ok(text) = serde_json::to_string(saved) {
        let _ = std::fs::write(file(), text);
    }
}

/// What the window says while a login is held.
pub fn explain(wait: i64) -> String {
    let at = chrono::Local::now() + chrono::Duration::seconds(wait);
    format!(
        "Holding the Discord connection: this account has logged in afresh too often (no more than once every {MIN_GAP} seconds, or {PER_DAY} times a day), and many fresh logins can get an account flagged as spam. It will connect at {}.",
        at.format("%H:%M:%S")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logins_are_spaced_out() {
        let mut history = Vec::new();
        let start = 1_000_000;
        assert_eq!(admit(&mut history, start), Ok(()));
        // Three seconds later: held until ten have passed.
        assert_eq!(admit(&mut history, start + 3), Err(MIN_GAP - 3));
        assert_eq!(history.len(), 1, "a held login is not counted");
        assert_eq!(admit(&mut history, start + MIN_GAP), Ok(()));
    }

    #[test]
    fn the_day_has_its_own_limit() {
        let start = 5_000_000;
        let mut history: Vec<i64> = (0..PER_DAY as i64).map(|n| start + n * MIN_GAP).collect();
        let now = start + PER_DAY as i64 * MIN_GAP;
        // Held until the first of the day's logins is a day old.
        assert_eq!(admit(&mut history, now), Err(start + DAY - now));
        assert_eq!(admit(&mut history, start + DAY), Ok(()));
    }

    #[test]
    fn a_pass_is_only_given_to_a_login_that_is_being_held() {
        assert!(!connect_anyway("discord:test-never-held"), "nothing is held, so nothing is let through");
        let _pass = begin_hold("discord:test-held");
        assert!(connect_anyway("discord:test-held"));
        end_hold("discord:test-held");
        assert!(!connect_anyway("discord:test-held"), "after the hold ends there is nothing to let through");
    }

    #[test]
    fn old_logins_are_forgotten() {
        let mut history = vec![1, 2, 3, 4, 5];
        assert_eq!(admit(&mut history, 10 * DAY), Ok(()));
        assert_eq!(history, vec![10 * DAY]);
    }
}
