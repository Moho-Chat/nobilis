//! Which messages are going to disappear, and when.
//!
//! A message with a disappearing timer is removed from the conversation, and from the account's store, when its time
//! comes. For what the account sent the time runs from when it was sent. For what it received it runs from when the
//! conversation is looked at (the way Signal's own clients do it: a message must not vanish before it was seen), so a
//! row waits with no deadline until then.

use super::{Result, SignalStore};
use rusqlite::params;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expiry {
    pub buffer_id: String,
    pub msg_id: String,
    /// The conversation, as `ids::remote_id` writes it.
    pub thread_remote: String,
    /// The time the sender stamped the message with, which is how the library's store finds it.
    pub sent_ms: u64,
    pub seconds: u32,
    /// Seconds since the epoch, `None` until the timer has started.
    pub deadline: Option<i64>,
}

impl SignalStore {
    /// Remembers a message that will disappear. Remembered twice it keeps the earlier deadline.
    pub fn expiry_add(&self, e: &Expiry) -> Result<()> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO expiries (buffer_id, msg_id, thread_remote, sent_ms, seconds, deadline) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT (buffer_id, msg_id) DO NOTHING",
                params![e.buffer_id, e.msg_id, e.thread_remote, e.sent_ms as i64, e.seconds, e.deadline],
            )?;
            Ok(())
        })
    }

    /// Starts the timers of what was waiting to be read in a conversation. Answers how many started.
    pub fn expiry_start(&self, buffer_id: &str, now: i64) -> Result<usize> {
        self.write(|tx| Ok(tx.execute("UPDATE expiries SET deadline = ?2 + seconds WHERE buffer_id = ?1 AND deadline IS NULL", params![buffer_id, now])?))
    }

    /// What is due.
    pub fn expiry_due(&self, now: i64) -> Result<Vec<Expiry>> {
        self.read(|conn| {
            let mut stmt = conn.prepare("SELECT buffer_id, msg_id, thread_remote, sent_ms, seconds, deadline FROM expiries WHERE deadline IS NOT NULL AND deadline <= ?1 ORDER BY deadline")?;
            let rows = stmt.query_map([now], |row| {
                Ok(Expiry { buffer_id: row.get(0)?, msg_id: row.get(1)?, thread_remote: row.get(2)?, sent_ms: row.get::<_, i64>(3)? as u64, seconds: row.get(4)?, deadline: row.get(5)? })
            })?;
            Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
        })
    }

    /// Forgets one, because the message is gone.
    pub fn expiry_done(&self, buffer_id: &str, msg_id: &str) -> Result<()> {
        self.write(|tx| {
            tx.execute("DELETE FROM expiries WHERE buffer_id = ?1 AND msg_id = ?2", params![buffer_id, msg_id])?;
            Ok(())
        })
    }

    /// How many are being kept track of.
    pub fn expiry_count(&self) -> Result<usize> {
        self.read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM expiries", [], |row| row.get::<_, i64>(0))? as usize))
    }
}
