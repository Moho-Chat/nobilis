use crate::model::{Embed, Message, Reaction, ReplyPreview};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

/// Scrollback store - same schema/path convention as daemon/nobilis/store.c's
/// messages table, so an existing scrollback.db opens with zero migration.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open(db_path)
            .with_context(|| format!("opening scrollback db at {}", db_path.display()))?;
        // Only takes effect for a brand-new file (or after a full VACUUM,
        // which isn't done automatically here since that could be slow on
        // an existing multi-month scrollback.db) - lets incremental_vacuum
        // actually hand freed pages back to the OS after prune_old_messages/
        // delete_message, instead of SQLite just quietly reusing them
        // in-place forever without the file ever shrinking on disk.
        let _ = conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                msg_id TEXT,
                buffer_id TEXT NOT NULL,
                from_nick TEXT,
                body TEXT,
                ts INTEGER NOT NULL,
                is_action INTEGER NOT NULL DEFAULT 0,
                is_highlight INTEGER NOT NULL DEFAULT 0,
                kind TEXT NOT NULL DEFAULT 'chat'
            );
            CREATE INDEX IF NOT EXISTS idx_messages_buffer_ts ON messages(buffer_id, ts);",
        )?;
        // Defensive no-ops for a scrollback.db predating these columns,
        // same as store.c's post-hoc ALTER TABLE. Ignore "duplicate column".
        for stmt in [
            "ALTER TABLE messages ADD COLUMN kind TEXT NOT NULL DEFAULT 'chat'",
            "ALTER TABLE messages ADD COLUMN reply_to_id TEXT",
            "ALTER TABLE messages ADD COLUMN reply_to_from TEXT",
            "ALTER TABLE messages ADD COLUMN reply_to_body TEXT",
            "ALTER TABLE messages ADD COLUMN edited INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE messages ADD COLUMN reactions TEXT NOT NULL DEFAULT '[]'",
            "ALTER TABLE messages ADD COLUMN is_own INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE messages ADD COLUMN avatar_url TEXT",
            "ALTER TABLE messages ADD COLUMN embeds TEXT NOT NULL DEFAULT '[]'",
            "ALTER TABLE messages ADD COLUMN sender_id TEXT",
        ] {
            let _ = conn.execute(stmt, []);
        }
        Ok(Self { conn: Mutex::new(conn) })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn append_message(
        &self,
        buffer_id: &str,
        msg_id: &str,
        from: &str,
        body: &str,
        ts: i64,
        is_action: bool,
        is_highlight: bool,
        kind: &str,
        reply_to: Option<&ReplyPreview>,
        initial_reactions: &[Reaction],
        is_own: bool,
        avatar_url: Option<&str>,
        embeds: &[Embed],
        sender_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        // Live messages are always freshly created with no reactions yet
        // (the column's own schema default, '[]', already covers that) -
        // this only matters for history backfill, which sees whatever
        // Discord's message-list response already reports.
        let reactions_json = if initial_reactions.is_empty() { None } else { Some(serde_json::to_string(initial_reactions)?) };
        let embeds_json = if embeds.is_empty() { None } else { Some(serde_json::to_string(embeds)?) };
        conn.execute(
            "INSERT INTO messages (msg_id, buffer_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, reactions, is_own, avatar_url, embeds, sender_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, COALESCE(?12, '[]'), ?13, ?14, COALESCE(?15, '[]'), ?16)",
            params![
                msg_id,
                buffer_id,
                from,
                body,
                ts,
                is_action as i32,
                is_highlight as i32,
                kind,
                reply_to.map(|r| r.id.as_str()),
                reply_to.map(|r| r.from.as_str()),
                reply_to.map(|r| r.body.as_str()),
                reactions_json,
                is_own as i32,
                avatar_url,
                embeds_json,
                sender_id,
            ],
        )?;
        Ok(())
    }

    /// A live edit (Discord's MESSAGE_UPDATE) - updates the body of an
    /// already-recorded message in place and flags it edited. Returns
    /// false (a harmless no-op for the caller) if the message was never
    /// recorded locally in the first place - editing something outside
    /// our loaded history isn't something there's a visible row to update.
    pub fn update_message_body(&self, buffer_id: &str, msg_id: &str, body: &str, embeds: &[Embed]) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let embeds_json = serde_json::to_string(embeds)?;
        let rows = conn.execute(
            "UPDATE messages SET body = ?1, edited = 1, embeds = ?4 WHERE buffer_id = ?2 AND msg_id = ?3",
            params![body, buffer_id, msg_id, embeds_json],
        )?;
        Ok(rows > 0)
    }

    /// Like update_message_body, but doesn't set `edited` - for backend-
    /// internal rewrites of a message's own body (e.g. backend/sockchat's
    /// attachment-link resolution swapping in a Tor-fetched local copy
    /// once ready) that aren't a real user edit and shouldn't show an
    /// "(edited)" label.
    pub fn update_message_body_silent(&self, buffer_id: &str, msg_id: &str, body: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute("UPDATE messages SET body = ?1 WHERE buffer_id = ?2 AND msg_id = ?3", params![body, buffer_id, msg_id])?;
        Ok(rows > 0)
    }

    /// Discord's MESSAGE_DELETE - removes the row outright (Discord's own
    /// clients don't show a tombstone either, they just remove it).
    pub fn delete_message(&self, buffer_id: &str, msg_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let rows = conn.execute("DELETE FROM messages WHERE buffer_id = ?1 AND msg_id = ?2", params![buffer_id, msg_id])?;
        Ok(rows > 0)
    }

    /// Applies one reaction add/remove to a message's stored tally and
    /// returns the updated reaction list (None if the message isn't
    /// recorded locally). Discord's REACTION_ADD/REMOVE events are
    /// per-user-per-emoji, not a full snapshot, so this accumulates
    /// incrementally rather than replacing the whole list each time.
    pub fn update_reaction(&self, buffer_id: &str, msg_id: &str, emoji: &str, is_me: bool, add: bool) -> Result<Option<Vec<Reaction>>> {
        let conn = self.conn.lock().unwrap();
        let existing: Option<String> = conn
            .query_row("SELECT reactions FROM messages WHERE buffer_id = ?1 AND msg_id = ?2", params![buffer_id, msg_id], |row| row.get(0))
            .optional()?;
        let Some(existing) = existing else { return Ok(None) };
        let mut reactions: Vec<Reaction> = serde_json::from_str(&existing).unwrap_or_default();

        if let Some(r) = reactions.iter_mut().find(|r| r.emoji == emoji) {
            if add {
                r.count += 1;
            } else {
                r.count -= 1;
            }
            if is_me {
                r.me = add;
            }
        } else if add {
            // animated defaults false here rather than being threaded
            // through from the live gateway event - a brand-new reaction
            // (never seen on this message before) is a rare first-add,
            // and the next full backlog fetch/reconnect re-syncs it with
            // the real value from the message's own snapshot (see
            // extract_reactions in backend/discord.rs, which does know
            // it) - not worth widening this incremental-update path just
            // for that narrow a window.
            reactions.push(Reaction { emoji: emoji.to_string(), count: 1, me: is_me, animated: false });
        }
        reactions.retain(|r| r.count > 0);

        let json = serde_json::to_string(&reactions)?;
        conn.execute("UPDATE messages SET reactions = ?1 WHERE buffer_id = ?2 AND msg_id = ?3", params![json, buffer_id, msg_id])?;
        Ok(Some(reactions))
    }

    /// Whether this buffer has ever recorded anything - the seed-once guard
    /// for Discord's history backfill (see backend/discord.rs): a buffer
    /// with any messages already (from a prior backfill or from having
    /// seen live traffic) is left alone rather than re-fetched, since
    /// there's no per-message dedup against Discord's own message ids here.
    pub fn has_messages(&self, buffer_id: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE buffer_id = ?1)", params![buffer_id], |row| row.get(0))?;
        Ok(exists)
    }

    /// Most recent message timestamp for a buffer, 0 if none - seeds
    /// Buffer::last_activity_ts when a buffer is (re)created so activity
    /// sorting reflects persisted scrollback immediately, not just
    /// messages seen since the current daemon process started.
    pub fn last_activity(&self, buffer_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let ts: i64 = conn.query_row("SELECT COALESCE(MAX(ts), 0) FROM messages WHERE buffer_id = ?1", params![buffer_id], |row| row.get(0))?;
        Ok(ts)
    }

    /// The oldest stored message's own protocol id for a buffer (Discord's
    /// backfill/extend_history use this as the `msg_id` in their own
    /// snowflake form) - what backend::discord::extend_history pages
    /// further back from. None if the buffer has nothing stored yet.
    pub fn oldest_msg_id(&self, buffer_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let id = conn
            .query_row("SELECT msg_id FROM messages WHERE buffer_id = ?1 ORDER BY ts ASC LIMIT 1", params![buffer_id], |row| row.get(0))
            .optional()?;
        Ok(id)
    }

    /// Oldest-first, matching store.c's getBacklog (query is DESC+LIMIT for
    /// "most recent N", then reversed before returning).
    pub fn get_backlog(&self, buffer_id: &str, before: i64, limit: i64) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let limit = if limit > 0 { limit } else { 200 };
        let mut stmt = conn.prepare(
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id
             FROM messages
             WHERE buffer_id = ?1 AND (?2 <= 0 OR ts < ?2)
             ORDER BY ts DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![buffer_id, before, limit], |row| {
            let reply_to_id: Option<String> = row.get(7)?;
            let reply_to_from: Option<String> = row.get(8)?;
            let reply_to_body: Option<String> = row.get(9)?;
            let reply_to = reply_to_id.map(|id| ReplyPreview {
                id,
                from: reply_to_from.unwrap_or_default(),
                body: reply_to_body.unwrap_or_default(),
            });
            let reactions_json: String = row.get::<_, Option<String>>(11)?.unwrap_or_else(|| "[]".to_string());
            let reactions: Vec<Reaction> = serde_json::from_str(&reactions_json).unwrap_or_default();
            let embeds_json: String = row.get::<_, Option<String>>(14)?.unwrap_or_else(|| "[]".to_string());
            let embeds: Vec<Embed> = serde_json::from_str(&embeds_json).unwrap_or_default();
            Ok(Message {
                id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                buffer_id: buffer_id.to_string(),
                from: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                body: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ts: row.get(3)?,
                is_action: row.get::<_, i64>(4)? != 0,
                is_highlight: row.get::<_, i64>(5)? != 0,
                kind: row.get::<_, Option<String>>(6)?.unwrap_or_else(|| "chat".to_string()),
                reply_to,
                edited: row.get::<_, i64>(10)? != 0,
                reactions,
                is_own: row.get::<_, i64>(12)? != 0,
                avatar_url: row.get(13)?,
                embeds,
                sender_id: row.get(15)?,
            })
        })?;
        let mut out: Vec<Message> = rows.collect::<rusqlite::Result<_>>()?;
        out.reverse();
        Ok(out)
    }

    /// A single message by id - used to build a ReplyPreview locally for
    /// protocols that (unlike Discord's `referenced_message`) only give a
    /// reply's target event id, not its content, inline with the reply
    /// itself (see backend/matrix/mod.rs's reply handling). Returns None
    /// both when the buffer/id genuinely doesn't exist and when it's
    /// aged out of local scrollback (prune_old_messages) - either way,
    /// callers fall back to a minimal reply preview with no body/from.
    pub fn get_message(&self, buffer_id: &str, msg_id: &str) -> Result<Option<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id
             FROM messages
             WHERE buffer_id = ?1 AND msg_id = ?2",
        )?;
        let mut rows = stmt.query_map(params![buffer_id, msg_id], |row| {
            let reply_to_id: Option<String> = row.get(7)?;
            let reply_to_from: Option<String> = row.get(8)?;
            let reply_to_body: Option<String> = row.get(9)?;
            let reply_to = reply_to_id.map(|id| ReplyPreview {
                id,
                from: reply_to_from.unwrap_or_default(),
                body: reply_to_body.unwrap_or_default(),
            });
            let reactions_json: String = row.get::<_, Option<String>>(11)?.unwrap_or_else(|| "[]".to_string());
            let reactions: Vec<Reaction> = serde_json::from_str(&reactions_json).unwrap_or_default();
            let embeds_json: String = row.get::<_, Option<String>>(14)?.unwrap_or_else(|| "[]".to_string());
            let embeds: Vec<Embed> = serde_json::from_str(&embeds_json).unwrap_or_default();
            Ok(Message {
                id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                buffer_id: buffer_id.to_string(),
                from: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                body: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                ts: row.get(3)?,
                is_action: row.get::<_, i64>(4)? != 0,
                is_highlight: row.get::<_, i64>(5)? != 0,
                kind: row.get::<_, Option<String>>(6)?.unwrap_or_else(|| "chat".to_string()),
                reply_to,
                edited: row.get::<_, i64>(10)? != 0,
                reactions,
                is_own: row.get::<_, i64>(12)? != 0,
                avatar_url: row.get(13)?,
                embeds,
                sender_id: row.get(15)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Keeps only the `keep_per_buffer` most recent rows in each buffer -
    /// without this, scrollback grows forever (a busy channel left running
    /// for months is genuinely unbounded, not a one-time cost). Called
    /// periodically from main.rs, not just once at startup, so a buffer
    /// that's been open and active the whole time this process has been
    /// running still gets trimmed. `getBacklog`'s pagination is what
    /// re-fetches older history on demand were it ever needed beyond
    /// this - this only trims what's kept locally, it doesn't affect the
    /// server-side history for protocols that have their own (Discord).
    /// Returns the number of rows actually deleted.
    pub fn prune_old_messages(&self, keep_per_buffer: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        let deleted = conn.execute(
            "DELETE FROM messages WHERE id IN (
                SELECT id FROM (
                    SELECT id, ROW_NUMBER() OVER (PARTITION BY buffer_id ORDER BY ts DESC) AS rn
                    FROM messages
                ) WHERE rn > ?1
            )",
            params![keep_per_buffer],
        )?;
        Ok(deleted)
    }

    /// Hands freed pages back to the OS a couple hundred at a time, rather
    /// than a single large blocking VACUUM - a no-op on a database that
    /// predates `auto_vacuum = INCREMENTAL` (see `open`'s doc comment).
    pub fn incremental_vacuum(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute_batch("PRAGMA incremental_vacuum(200);")?;
        Ok(())
    }
}
