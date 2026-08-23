use crate::model::{Attachment, Embed, Message, Reaction, ReplyPreview};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
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
            "ALTER TABLE messages ADD COLUMN attachments TEXT NOT NULL DEFAULT '[]'",
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
        attachments: &[Attachment],
        sender_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        // Live messages are always freshly created with no reactions yet
        // (the column's own schema default, '[]', already covers that) -
        // this only matters for history backfill, which sees whatever
        // Discord's message-list response already reports.
        let reactions_json = if initial_reactions.is_empty() { None } else { Some(serde_json::to_string(initial_reactions)?) };
        let embeds_json = if embeds.is_empty() { None } else { Some(serde_json::to_string(embeds)?) };
        let attachments_json = if attachments.is_empty() { None } else { Some(serde_json::to_string(attachments)?) };
        conn.execute(
            "INSERT INTO messages (msg_id, buffer_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, reactions, is_own, avatar_url, embeds, sender_id, attachments)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, COALESCE(?12, '[]'), ?13, ?14, COALESCE(?15, '[]'), ?16, COALESCE(?17, '[]'))",
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
                attachments_json,
            ],
        )?;
        Ok(())
    }

    /// A live edit (Discord's MESSAGE_UPDATE) - updates the body of an
    /// already-recorded message in place and flags it edited. Returns
    /// false (a harmless no-op for the caller) if the message was never
    /// recorded locally in the first place - editing something outside
    /// our loaded history isn't something there's a visible row to update.
    pub fn update_message_body(&self, buffer_id: &str, msg_id: &str, body: &str, embeds: &[Embed], attachments: &[Attachment]) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let embeds_json = serde_json::to_string(embeds)?;
        let attachments_json = serde_json::to_string(attachments)?;
        let rows = conn.execute(
            "UPDATE messages SET body = ?1, edited = 1, embeds = ?4, attachments = ?5 WHERE buffer_id = ?2 AND msg_id = ?3",
            params![body, buffer_id, msg_id, embeds_json, attachments_json],
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

    /// Replaces a message's attachment list without touching its text or
    /// marking it edited - for a cached preview arriving, or links being
    /// re-signed after they expired. Neither is a change the sender made.
    pub fn update_message_attachments(&self, buffer_id: &str, msg_id: &str, attachments: &[Attachment]) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let json = serde_json::to_string(attachments)?;
        let rows = conn.execute(
            "UPDATE messages SET attachments = ?1 WHERE buffer_id = ?2 AND msg_id = ?3",
            params![json, buffer_id, msg_id],
        )?;
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

    /// Which of `ids` this buffer already stores.
    ///
    /// `msg_id` carries no uniqueness constraint - append_message is a plain
    /// INSERT, because every normal write path is already known not to
    /// overlap (live gateway messages are new by definition, and
    /// extend_history pages strictly older than the oldest stored id).
    /// Re-reading a range that was already stored is the one case that can
    /// overlap, so it has to ask first or it would duplicate the lot.
    pub fn existing_msg_ids(&self, buffer_id: &str, ids: &[&str]) -> Result<HashSet<String>> {
        if ids.is_empty() {
            return Ok(HashSet::new());
        }
        let conn = self.conn.lock().unwrap();
        let placeholders = std::iter::repeat("?").take(ids.len()).collect::<Vec<_>>().join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT msg_id FROM messages WHERE buffer_id = ?1 AND msg_id IN ({placeholders})"
        ))?;
        let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
        params.push(&buffer_id);
        for id in ids {
            params.push(id);
        }
        let found = stmt
            .query_map(params.as_slice(), |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<String>>>()?;
        Ok(found)
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
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id, attachments
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
            let attachments_json: String = row.get::<_, Option<String>>(16)?.unwrap_or_else(|| "[]".to_string());
            let mut attachments: Vec<Attachment> = serde_json::from_str(&attachments_json).unwrap_or_default();
            drop_missing_local_copies(&mut attachments);
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
                attachments,
                sender_id: row.get(15)?,
            })
        })?;
        let mut out: Vec<Message> = rows.collect::<rusqlite::Result<_>>()?;
        out.reverse();
        Ok(out)
    }
}

/// Forgets local copies that are no longer on disk.
///
/// The caches these paths point into are swept when they outgrow their size
/// caps, and the message that named a file keeps naming it long after it was
/// deleted. Handing a client a path to a file that is gone makes a perfectly
/// good attachment look broken, and - worse - look expired, since a missing
/// preview is indistinguishable from a lapsed link at the far end.
fn drop_missing_local_copies(attachments: &mut [Attachment]) {
    let gone = |p: &Option<String>| -> bool {
        let Some(p) = p.as_deref() else { return false };
        let Some(rest) = p.strip_prefix("file://") else { return false };
        !std::path::Path::new(rest).exists()
    };
    for att in attachments.iter_mut() {
        if gone(&att.thumbnail_path) {
            att.thumbnail_path = None;
        }
        if gone(&att.path) {
            att.path = None;
        }
    }
}

impl Store {
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
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id, attachments
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
            let attachments_json: String = row.get::<_, Option<String>>(16)?.unwrap_or_else(|| "[]".to_string());
            let mut attachments: Vec<Attachment> = serde_json::from_str(&attachments_json).unwrap_or_default();
            drop_missing_local_copies(&mut attachments);
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
                attachments,
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

#[cfg(test)]
mod tests {
    use super::{drop_missing_local_copies, Store};
    use crate::model::Attachment;

    fn store() -> (Store, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "nobilis-store-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scrollback.db");
        let _ = std::fs::remove_file(&path);
        (Store::open(&path).expect("opening store"), dir)
    }

    fn append(s: &Store, buffer: &str, id: &str) {
        s.append_message(buffer, id, "someone", "hi", 1, false, false, "chat", None, &[], false, None, &[], &[], None)
            .expect("appending");
    }

    #[test]
    fn a_swept_thumbnail_is_not_reported_as_still_being_there() {
        // The thumbnail cache is capped and sweeps its oldest files, while the
        // message that named one goes on naming it. A client told about a file
        // that is gone shows a broken picture and, worse, reports the link as
        // expired - when the link is fine and the local copy simply aged out.
        let dir = std::env::temp_dir().join(format!("nobilis-store-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let present = dir.join("kept");
        std::fs::write(&present, b"x").unwrap();

        let mut attachments = vec![
            Attachment {
                thumbnail_path: Some(format!("file://{}", present.display())),
                path: Some(format!("file://{}", dir.join("swept").display())),
                ..Default::default()
            },
            Attachment {
                thumbnail_path: Some(format!("file://{}", dir.join("also-swept").display())),
                ..Default::default()
            },
        ];
        drop_missing_local_copies(&mut attachments);

        assert!(attachments[0].thumbnail_path.is_some(), "a file that exists was dropped");
        assert!(attachments[0].path.is_none(), "a swept original was still advertised");
        assert!(attachments[1].thumbnail_path.is_none(), "a swept thumbnail was still advertised");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_remote_url_is_left_alone() {
        // Only local copies are checked; a https link is not this code's
        // business and must survive untouched.
        let mut attachments = vec![Attachment {
            url: Some("https://cdn.discordapp.com/attachments/1/2/a.png".to_string()),
            thumbnail_path: None,
            ..Default::default()
        }];
        drop_missing_local_copies(&mut attachments);
        assert_eq!(attachments[0].url.as_deref(), Some("https://cdn.discordapp.com/attachments/1/2/a.png"));
    }

    #[test]
    fn reports_only_the_ids_this_buffer_already_stores() {
        let (s, dir) = store();
        append(&s, "discord:a|#chan", "100");
        append(&s, "discord:a|#chan", "101");

        let known = s.existing_msg_ids("discord:a|#chan", &["100", "101", "102"]).unwrap();
        assert_eq!(known.len(), 2);
        assert!(known.contains("100") && known.contains("101"));
        // 102 is what a refill would go on to store.
        assert!(!known.contains("102"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn does_not_confuse_the_same_id_in_another_buffer() {
        // Discord snowflakes are unique, but IRC/Sneedchat ids are not
        // globally so - a refill keyed on id alone would skip storing a
        // message because some other buffer happens to hold that id.
        let (s, dir) = store();
        append(&s, "discord:a|#one", "100");

        assert!(s.existing_msg_ids("discord:a|#two", &["100"]).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn an_empty_request_asks_the_database_nothing() {
        let (s, dir) = store();
        assert!(s.existing_msg_ids("discord:a|#chan", &[]).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
