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

/// Fresh logins an account may make in any hour.
pub const PER_HOUR: usize = 3;
/// And in any day.
pub const PER_DAY: usize = 8;

const HOUR: i64 = 3600;
const DAY: i64 = 24 * HOUR;

/// Whether one more fresh login may be made at `now` (Unix seconds), given the times of the earlier ones. Answers
/// the number of seconds to wait when not. The list is kept as it should be saved: only what still counts.
pub fn admit(history: &mut Vec<i64>, now: i64) -> Result<(), i64> {
    history.retain(|t| now - t < DAY);
    history.sort_unstable();
    let in_hour: Vec<i64> = history.iter().copied().filter(|t| now - t < HOUR).collect();
    let mut wait = 0;
    // Over the hour's limit: when the one that makes room ages out.
    if in_hour.len() >= PER_HOUR {
        wait = wait.max(in_hour[in_hour.len() - PER_HOUR] + HOUR - now);
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

/// What the window says while a login is held.
pub fn explain(wait: i64) -> String {
    let at = chrono::Local::now() + chrono::Duration::seconds(wait);
    format!(
        "Holding the Discord connection: this account has already logged in {PER_HOUR} times in the last hour (or {PER_DAY} in a day), and many fresh logins can get an account flagged as spam. It will connect at {}.",
        at.format("%H:%M")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_few_logins_an_hour_are_let_through_and_the_next_waits_for_the_oldest_to_age_out() {
        let mut history = Vec::new();
        let start = 1_000_000;
        assert_eq!(admit(&mut history, start), Ok(()));
        assert_eq!(admit(&mut history, start + 60), Ok(()));
        assert_eq!(admit(&mut history, start + 120), Ok(()));
        // The fourth, ten minutes in: held until the first is an hour old.
        assert_eq!(admit(&mut history, start + 600), Err(HOUR - 600));
        assert_eq!(history.len(), 3, "a held login is not counted");
        // An hour after the first, there is room.
        assert_eq!(admit(&mut history, start + HOUR), Ok(()));
    }

    #[test]
    fn the_day_has_its_own_limit_however_the_hours_fall() {
        let mut history = Vec::new();
        let start = 5_000_000;
        // Three an hour for three hours is nine; the eighth is held until the first is a day old.
        let mut made = 0;
        for hour in 0..3 {
            for n in 0..3 {
                if admit(&mut history, start + hour * HOUR + n).is_ok() {
                    made += 1;
                }
            }
        }
        assert_eq!(made, PER_DAY);
        let wait = admit(&mut history, start + 3 * HOUR).unwrap_err();
        assert_eq!(wait, DAY - 3 * HOUR);
    }

    #[test]
    fn old_logins_are_forgotten() {
        let mut history = vec![1, 2, 3, 4, 5];
        assert_eq!(admit(&mut history, 10 * DAY), Ok(()));
        assert_eq!(history, vec![10 * DAY]);
    }
}
