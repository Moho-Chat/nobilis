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

/// A transfer as it is kept on disk.
///
/// Deliberately not `runtime::DccTransfer`: that one carries a cancel flag and
/// the offer it came from, which are about a transfer that is happening rather
/// than one that happened, and have no meaning once it is written down.
#[derive(Clone, Debug)]
pub struct TransferRow {
    pub id: String,
    pub account_id: String,
    pub outgoing: bool,
    pub peer: String,
    pub file_name: String,
    pub raw_name: String,
    pub size: u64,
    pub received: u64,
    pub state: String,
    pub path: Option<String>,
    pub error: Option<String>,
    pub ts: i64,
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
            CREATE INDEX IF NOT EXISTS idx_messages_buffer_ts ON messages(buffer_id, ts);
            CREATE TABLE IF NOT EXISTS transfers (
                id TEXT PRIMARY KEY,
                account_id TEXT NOT NULL,
                outgoing INTEGER NOT NULL DEFAULT 0,
                peer TEXT NOT NULL,
                file_name TEXT NOT NULL,
                raw_name TEXT NOT NULL,
                size INTEGER NOT NULL DEFAULT 0,
                received INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL,
                path TEXT,
                error TEXT,
                ts INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_transfers_ts ON transfers(ts);
            /* Polls and predictions, kept so they can be read back after the
               fact. The card on screen is a live thing, but what was voted
               on last night outlives the minute the vote was open - and
               outlives this process, which is why it is kept here rather
               than in the runtime memory.

               One row per card, replaced as the votes come in: card_id is
               whatever the service uses to mean the same poll, or the moment
               it started where the service names nothing. */
            CREATE TABLE IF NOT EXISTS live_cards (
                buffer_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                card_id TEXT NOT NULL,
                title TEXT NOT NULL,
                body TEXT NOT NULL,
                ts INTEGER NOT NULL,
                PRIMARY KEY (buffer_id, kind, card_id)
            );
            CREATE INDEX IF NOT EXISTS idx_live_cards_ts ON live_cards(buffer_id, ts);",
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
            // How the sender looked, where the service says: their colour and
            // what they have earned. Persisted rather than live-only, or
            // reopening a conversation would strip a chat of the thing that
            // makes it readable.
            "ALTER TABLE messages ADD COLUMN sender_color TEXT",
            "ALTER TABLE messages ADD COLUMN badges TEXT NOT NULL DEFAULT '[]'",
            "ALTER TABLE messages ADD COLUMN sender_id TEXT",
            // Matrix sends a formatted body alongside the plain one. Kept
            // separate rather than replacing body: the plain text is the
            // fallback every other protocol uses and the one search reads.
            "ALTER TABLE messages ADD COLUMN html TEXT",
            // Buttons and menus on a message. Kept rather than shown live and
            // forgotten: a role picker or a ticket panel is posted once and
            // pressed for months, so a client that lost them on restart would
            // be showing a message whose whole point had gone.
            "ALTER TABLE messages ADD COLUMN components TEXT NOT NULL DEFAULT '[]'",
            // Which kind of conversation this was said in.
            //
            // Buffers live only as long as the daemon does - they are rebuilt
            // as connections come back and rejoin - so a query that scoped
            // itself to the ones currently known could not see anything said
            // in a channel not yet rejoined. That is why mentions appeared to
            // vanish over a restart: the messages were here all along, and
            // nothing could name the conversations they belonged to.
            "ALTER TABLE messages ADD COLUMN buffer_kind TEXT",
            // Whether reply_to_id names a thread rather than a single message.
            // Same column pair for both because the protocol says both the
            // same way; what differs is that a thread can be opened and
            // continued, and a reply target cannot.
            "ALTER TABLE messages ADD COLUMN reply_is_thread INTEGER NOT NULL DEFAULT 0",
            // Whether that reference is a message brought here rather than
            // one answered - see model::ReplyPreview::forwarded.
            "ALTER TABLE messages ADD COLUMN reply_forwarded INTEGER NOT NULL DEFAULT 0",
        ] {
            let _ = conn.execute(stmt, []);
        }

        // Every arrival and departure that happened to contain your name was
        // recorded as a mention of you - see model::is_room_event for how, and
        // for the shutdown that made it obvious. New ones no longer are; these
        // are the ones already written down, and they are the difference
        // between a mentions inbox and a list of everyone who has ever walked
        // past you. Measured on the store that reported this: 486 of them
        // against 48 real mentions.
        let _ = conn.execute(
            "UPDATE messages SET is_highlight = 0 WHERE is_highlight = 1 AND kind IN \
             ('join','part','quit','nick','mode','topic','system','matrixJoin','matrixInvite','matrixKick','matrixQuit')",
            [],
        );

        // Sneedchat's account ids used to be spelled "sockchat:", after the
        // implementation this backend was written by studying rather than
        // after the chat itself. Buffer ids are built from the account id, so
        // every message ever stored for it carries the old spelling - and
        // without this they would all belong to conversations that no longer
        // exist under any name.
        let _ = conn.execute(
            "UPDATE messages SET buffer_id = 'sneedchat:' || substr(buffer_id, 10) WHERE buffer_id LIKE 'sockchat:%'",
            [],
        );

        // A message id identifies a message, so recording one twice is always
        // a mistake - but nothing said so, and a backend that replays history
        // on reconnect quietly stacked up copies. Sneedchat did: seven
        // identical rows for the same id, one per reconnect.
        //
        // That was not merely wasted space. A frontend keys its rendered rows
        // by message id, and duplicate keys leave a list that cannot be
        // reconciled - rows from the last conversation survive being switched
        // away from and sit above the new one's, which reads as one channel's
        // history bleeding into another's.
        //
        // Deduplicated before the index is added, since it cannot be created
        // while the rows it forbids are still there. The copies are identical
        // and the oldest row of each set is kept, so nothing is lost. NULL
        // msg_ids are left alone: SQLite treats them as distinct, which is
        // right - a row with no id makes no claim about being the same
        // message as any other.
        let _ = conn.execute(
            "DELETE FROM messages WHERE id NOT IN (
                 SELECT MIN(id) FROM messages WHERE msg_id IS NOT NULL GROUP BY buffer_id, msg_id
             ) AND msg_id IS NOT NULL",
            [],
        );
        let _ = conn.execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_unique ON messages(buffer_id, msg_id)",
            [],
        );
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
        html: Option<&str>,
        buffer_kind: &str,
        sender_color: Option<&str>,
        badges: &[crate::backend::kick::api::Badge],
    ) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        // Live messages are always freshly created with no reactions yet
        // (the column's own schema default, '[]', already covers that) -
        // this only matters for history backfill, which sees whatever
        // Discord's message-list response already reports.
        let reactions_json = if initial_reactions.is_empty() { None } else { Some(serde_json::to_string(initial_reactions)?) };
        let embeds_json = if embeds.is_empty() { None } else { Some(serde_json::to_string(embeds)?) };
        let attachments_json = if attachments.is_empty() { None } else { Some(serde_json::to_string(attachments)?) };
        let inserted = conn.execute(
            // OR IGNORE against the (buffer_id, msg_id) index: recording the
            // same message twice is a backend replaying history it already
            // has, and the right answer is to keep the copy already stored.
            "INSERT OR IGNORE INTO messages (msg_id, buffer_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, reactions, is_own, avatar_url, embeds, sender_id, attachments, html, buffer_kind, sender_color, badges, reply_is_thread, reply_forwarded)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, COALESCE(?12, '[]'), ?13, ?14, COALESCE(?15, '[]'), ?16, COALESCE(?17, '[]'), ?18, ?19, ?20, COALESCE(?21, '[]'), ?22, ?23)",
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
                html,
                buffer_kind,
                sender_color,
                serde_json::to_string(badges).ok(),
                reply_to.is_some_and(|r| r.thread) as i32,
                reply_to.is_some_and(|r| r.forwarded) as i32,
            ],
        )?;
        // Whether this was actually new. The insert has always ignored a
        // repeat; saying so is what lets a caller tell "stored" from "already
        // had it", which is the difference between announcing a message once
        // and announcing it again every time a backend replays its history.
        Ok(inserted > 0)
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
    /// internal rewrites of a message's own body (e.g. backend/sneedchat's
    /// attachment-link resolution swapping in a Tor-fetched local copy
    /// once ready) that aren't a real user edit and shouldn't show an
    /// "(edited)" label.
    /// Renames the thing a message points at, without touching the message.
    ///
    /// For a forward, whose author is not in what the service sent and has to
    /// be read afterwards - see backend/discord's name_forward.
    pub fn rename_reply_from(&self, buffer_id: &str, msg_id: &str, from: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE messages SET reply_to_from = ?1 WHERE buffer_id = ?2 AND msg_id = ?3",
            params![from, buffer_id, msg_id],
        )?;
        Ok(changed > 0)
    }

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
    /// Replaces a message's reactions outright. Answers whether the message
    /// was there to change, so a caller can stay quiet about one it does not
    /// have - a moderator clearing reactions on something older than this
    /// client's scrollback is not an error.
    pub fn set_reactions(&self, buffer_id: &str, msg_id: &str, reactions: &[Reaction]) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let json = serde_json::to_string(reactions)?;
        let changed = conn.execute(
            "UPDATE messages SET reactions = ?3 WHERE buffer_id = ?1 AND msg_id = ?2",
            params![buffer_id, msg_id, json],
        )?;
        Ok(changed > 0)
    }

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

    /// The most recent message this buffer holds, for asking a service what
    /// has happened since.
    pub fn newest_msg_id(&self, buffer_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let id = conn
            .query_row("SELECT msg_id FROM messages WHERE buffer_id = ?1 ORDER BY ts DESC LIMIT 1", params![buffer_id], |row| row.get(0))
            .optional()?;
        Ok(id)
    }

    /// Oldest-first, matching store.c's getBacklog (query is DESC+LIMIT for
    /// "most recent N", then reversed before returning).
    /// One row of the message columns, read by name.
    ///
    /// By name rather than by position deliberately: this was positional, and
    /// adding a column silently moved every index after it - which broke the
    /// mentions query twice, each time by exactly one, each time invisibly.
    /// Every query below selects these columns under these names, so a column
    /// added at the end of a select list now costs nothing here.
    fn row_to_message(buffer_id: &str, row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
        let reply_to_id: Option<String> = row.get("reply_to_id")?;
        let reply_to = reply_to_id.map(|id| ReplyPreview {
            id,
            from: row.get::<_, Option<String>>("reply_to_from").ok().flatten().unwrap_or_default(),
            body: row.get::<_, Option<String>>("reply_to_body").ok().flatten().unwrap_or_default(),
            thread: row.get::<_, Option<i64>>("reply_is_thread").ok().flatten().unwrap_or(0) != 0,
            forwarded: row.get::<_, Option<i64>>("reply_forwarded").ok().flatten().unwrap_or(0) != 0 });
        let json_column = |name: &str| -> String {
            row.get::<_, Option<String>>(name).ok().flatten().unwrap_or_else(|| "[]".to_string())
        };
        let reactions: Vec<Reaction> = serde_json::from_str(&json_column("reactions")).unwrap_or_default();
        let embeds: Vec<Embed> = serde_json::from_str(&json_column("embeds")).unwrap_or_default();
        let mut attachments: Vec<Attachment> = serde_json::from_str(&json_column("attachments")).unwrap_or_default();
        drop_missing_local_copies(&mut attachments);
        let badges = serde_json::from_str(&json_column("badges")).unwrap_or_default();
        let components = serde_json::from_str(&json_column("components")).unwrap_or_default();
        Ok(Message {
            id: row.get::<_, Option<String>>("msg_id")?.unwrap_or_default(),
            buffer_id: buffer_id.to_string(),
            from: row.get::<_, Option<String>>("from_nick")?.unwrap_or_default(),
            body: row.get::<_, Option<String>>("body")?.unwrap_or_default(),
            ts: row.get("ts")?,
            is_action: row.get::<_, i64>("is_action")? != 0,
            is_highlight: row.get::<_, i64>("is_highlight")? != 0,
            kind: row.get::<_, Option<String>>("kind")?.unwrap_or_else(|| "chat".to_string()),
            reply_to,
            edited: row.get::<_, i64>("edited")? != 0,
            reactions,
            is_own: row.get::<_, i64>("is_own")? != 0,
            avatar_url: row.get("avatar_url")?,
            embeds,
            attachments,
            sender_id: row.get("sender_id")?,
            sender_color: row.get("sender_color")?,
            badges,
            html: row.get("html")?,
            components,
        })
    }

    pub fn get_backlog(&self, buffer_id: &str, before: i64, limit: i64) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let limit = if limit > 0 { limit } else { 200 };
        let mut stmt = conn.prepare(
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id, attachments, html, sender_color, badges, reply_is_thread, reply_forwarded, components
             FROM messages
             WHERE buffer_id = ?1 AND (?2 <= 0 OR ts < ?2)
             ORDER BY ts DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![buffer_id, before, limit], |row| Self::row_to_message(buffer_id, row))?;
        let mut out: Vec<Message> = rows.collect::<rusqlite::Result<_>>()?;
        out.reverse();
        Ok(out)
    }

    /// Puts the buttons and menus on a message that already exists.
    ///
    /// Its own call rather than another argument to `append_message`, which
    /// takes nineteen already: these belong to one service, are written by
    /// that service's backend immediately after the message lands, and adding
    /// a twentieth parameter would make every other backend say "no
    /// components" in a language none of them speak.
    pub fn set_components(&self, buffer_id: &str, msg_id: &str, components: &[crate::model::Component]) -> Result<()> {
        if components.is_empty() {
            return Ok(());
        }
        let json = serde_json::to_string(components)?;
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE messages SET components = ?3 WHERE buffer_id = ?1 AND msg_id = ?2",
            params![buffer_id, msg_id, json],
        )?;
        Ok(())
    }

    /// The names that have spoken here lately, most recent first.
    ///
    /// Wanted because a roster is not always there to ask: a service may not
    /// send one until somebody joins or leaves, and a name with a space in it
    /// cannot be picked out of typed text without a list of the names it
    /// could be. Whoever has spoken recently is the list that matters anyway
    /// - they are who somebody is answering.
    pub fn recent_senders(&self, buffer_id: &str, limit: i64) -> Result<Vec<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT from_nick FROM messages
             WHERE buffer_id = ?1 AND from_nick IS NOT NULL AND from_nick != ''
             GROUP BY from_nick ORDER BY MAX(ts) DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![buffer_id, limit.max(1)], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Who somebody is, from the last thing they said.
    ///
    /// Search filters are typed as names - "from:coty1911" - and Discord wants
    /// an id. Nothing else here keeps a name-to-id table, but every stored
    /// message carries both, so the most recent one somebody sent under that
    /// name is the answer. Scoped by buffer prefix so an account only ever
    /// resolves names against its own conversations.
    pub fn sender_id_by_nick(&self, buffer_prefix: &str, nick: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT sender_id FROM messages
             WHERE buffer_id LIKE ?1 || '%' AND from_nick = ?2 COLLATE NOCASE AND sender_id IS NOT NULL
             ORDER BY ts DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(params![buffer_prefix, nick])?;
        Ok(rows.next()?.map(|row| row.get::<_, String>(0)).transpose()?)
    }

    /// The conversation from one moment forward, oldest first.
    ///
    /// `get_backlog` reads backwards because that is what scrolling up wants.
    /// This is what a reader working their way back towards the present wants,
    /// after arriving in the middle of a conversation.
    pub fn messages_after(&self, buffer_id: &str, after: i64, limit: i64) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let limit = if limit > 0 { limit } else { 200 };
        let mut stmt = conn.prepare(
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id, attachments, html, sender_color, badges, reply_is_thread, reply_forwarded, components
             FROM messages
             WHERE buffer_id = ?1 AND ts > ?2
             ORDER BY ts ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![buffer_id, after, limit], |row| Self::row_to_message(buffer_id, row))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The conversation either side of one moment, oldest first.
    ///
    /// `get_backlog` only ever reads backwards, which is right for scrolling
    /// up and wrong for arriving somewhere: a message shown with nothing after
    /// it looks like the end of the conversation, and the reader has no way to
    /// tell that it is not. Both halves rather than a wider backward read for
    /// the same reason.
    pub fn messages_around(&self, buffer_id: &str, ts: i64, span: i64) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let span = if span > 0 { span } else { 50 };
        const COLUMNS: &str = "msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id, attachments, html, sender_color, badges, reply_is_thread, reply_forwarded, components";

        let mut older = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM messages WHERE buffer_id = ?1 AND ts <= ?2 ORDER BY ts DESC LIMIT ?3"
        ))?;
        let rows = older.query_map(params![buffer_id, ts, span], |row| Self::row_to_message(buffer_id, row))?;
        let mut out: Vec<Message> = rows.collect::<rusqlite::Result<_>>()?;
        out.reverse();

        let mut newer = conn.prepare(&format!(
            "SELECT {COLUMNS} FROM messages WHERE buffer_id = ?1 AND ts > ?2 ORDER BY ts ASC LIMIT ?3"
        ))?;
        let rows = newer.query_map(params![buffer_id, ts, span], |row| Self::row_to_message(buffer_id, row))?;
        out.extend(rows.collect::<rusqlite::Result<Vec<Message>>>()?);
        Ok(out)
    }
}

impl Store {
    /// Every message that mentioned this account, newest first, across all of
    /// its conversations.
    ///
    /// One query rather than a walk of every buffer: what makes an inbox worth
    /// having is that it answers "what wanted me" in one place, and the
    /// highlight flag is already recorded per message at the point it arrives
    /// - by the backend that knows whether a mention is real, which for
    /// Discord is its own resolved mentions array rather than a guess at the
    /// nickname.
    /// Mentions, from the messages themselves rather than from whatever
    /// conversations happen to be open.
    ///
    /// `known_channels` covers the rows written before messages recorded which
    /// kind of conversation they were said in - those have no `buffer_kind`,
    /// so the only thing that can vouch for them is the live buffer list, as
    /// this always used to do. Everything written since stands on its own and
    /// survives a restart, which is the point.
    pub fn mentions(&self, known_channels: &[String], limit: i64) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let limit = if limit > 0 { limit } else { 100 };
        let places = std::iter::repeat("?").take(known_channels.len()).collect::<Vec<_>>().join(",");
        let legacy = if known_channels.is_empty() {
            String::new()
        } else {
            format!(" OR (buffer_kind IS NULL AND buffer_id IN ({places}))")
        };
        let sql = format!(
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id, attachments, html, sender_color, badges, reply_is_thread, reply_forwarded, components, buffer_id
             FROM messages
             WHERE is_highlight = 1 AND is_own = 0 AND (buffer_kind = 'channel'{legacy})
             ORDER BY ts DESC LIMIT ?"
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<&dyn rusqlite::ToSql> = known_channels.iter().map(|b| b as &dyn rusqlite::ToSql).collect();
        params.push(&limit);
        let rows = stmt.query_map(params.as_slice(), |row| {
            // The buffer is read back per row rather than stamped from a
            // parameter, since unlike every other query here this one spans
            // conversations and each answer belongs to a different one.
            //
            // Index 20, after the twenty columns row_to_message reads - and a
            // number that has now been wrong twice. It said 17 (`html`), which
            // made every mention on a service that sends no HTML fail the
            // whole query with "Invalid column type Null"; adding sender_color
            // and badges moved it again. Nothing shows it: the page fills from
            // live messages too, so mentions appear to work and then vanish on
            // the next restart.
            //
            // The real fix is to stop counting - a named column, or a struct
            // that owns its own column list - and it is worth doing the next
            // time this query is touched.
            let buffer_id: String = row.get("buffer_id")?;
            Self::row_to_message(&buffer_id, row)
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Writes a transfer down, or updates the one already there.
    ///
    /// Every state it passes through, not only the end: a transfer interrupted
    /// by the daemon stopping should still be in the list afterwards, saying
    /// what happened to it, rather than having never existed.
    pub fn record_transfer(&self, t: &TransferRow) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO transfers (id, account_id, outgoing, peer, file_name, raw_name, size, received, state, path, error, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET received = ?8, state = ?9, path = ?10, error = ?11",
            params![
                t.id,
                t.account_id,
                t.outgoing as i32,
                t.peer,
                t.file_name,
                t.raw_name,
                t.size as i64,
                t.received as i64,
                t.state,
                t.path,
                t.error,
                t.ts,
            ],
        )?;
        Ok(())
    }

    /// Writes a poll or prediction down, replacing the last state of it.
    ///
    /// The card on screen changes every few seconds while people vote; what
    /// is kept is the latest of it, so what is read back afterwards is how it
    /// finished rather than how it opened.
    pub fn record_live_card(&self, buffer_id: &str, kind: &str, card_id: &str, title: &str, body: &str, ts: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO live_cards (buffer_id, kind, card_id, title, body, ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(buffer_id, kind, card_id) DO UPDATE SET title = ?4, body = ?5",
            params![buffer_id, kind, card_id, title, body, ts],
        )?;
        Ok(())
    }

    /// The polls or predictions this conversation has seen, newest first.
    pub fn live_cards(&self, buffer_id: &str, kind: &str, limit: i64) -> Result<Vec<(String, i64)>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT body, ts FROM live_cards WHERE buffer_id = ?1 AND kind = ?2 ORDER BY ts DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![buffer_id, kind, limit], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// The transfers worth remembering, newest first.
    pub fn recent_transfers(&self, limit: i64) -> Result<Vec<TransferRow>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, account_id, outgoing, peer, file_name, raw_name, size, received, state, path, error, ts
             FROM transfers ORDER BY ts DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], |row| {
            Ok(TransferRow {
                id: row.get(0)?,
                account_id: row.get(1)?,
                outgoing: row.get::<_, i32>(2)? != 0,
                peer: row.get(3)?,
                file_name: row.get(4)?,
                raw_name: row.get(5)?,
                size: row.get::<_, i64>(6)? as u64,
                received: row.get::<_, i64>(7)? as u64,
                state: row.get(8)?,
                path: row.get(9)?,
                error: row.get(10)?,
                ts: row.get(11)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// Drops the oldest transfers beyond `keep`.
    pub fn prune_transfers(&self, keep: i64) -> Result<usize> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.execute(
            "DELETE FROM transfers WHERE id NOT IN (SELECT id FROM transfers ORDER BY ts DESC LIMIT ?1)",
            params![keep],
        )?)
    }

    /// Messages in one buffer whose text contains `query`.
    ///
    /// Deliberately a substring match rather than a word index: scrollback
    /// here is one SQLite table per client, the thing people search for is
    /// usually a fragment they half-remember, and a full-text index would have
    /// to be kept in step with every edit and deletion to earn its keep.
    ///
    /// Newest first, since a search for something said recently is the common
    /// case and a caller showing ten results wants those ten.
    pub fn search_messages(&self, buffer_id: &str, query: &str, limit: i64) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let limit = if limit > 0 { limit } else { 50 };
        // LIKE's own wildcards have to be neutralised, or searching for "50%"
        // silently matches everything beginning "50".
        let escaped = query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let mut stmt = conn.prepare(
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id, attachments, html, sender_color, badges, reply_is_thread, reply_forwarded, components
             FROM messages
             WHERE buffer_id = ?1 AND body LIKE ?2 ESCAPE '\\'
             ORDER BY ts DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![buffer_id, pattern, limit], |row| Self::row_to_message(buffer_id, row))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
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
    /// The id of the newest message stored for a buffer.
    ///
    /// Used to say how far a room has been read, which is a claim about a
    /// specific message rather than about a time - so it has to be the id the
    /// service itself gave, not one generated here.
    pub fn newest_message_id(&self, buffer_id: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT msg_id FROM messages WHERE buffer_id = ?1 ORDER BY ts DESC, rowid DESC LIMIT 1",
        )?;
        let mut rows = stmt.query_map(params![buffer_id], |row| row.get::<_, Option<String>>(0))?;
        Ok(rows.next().transpose()?.flatten())
    }

    /// A thread: the message it grew from, then everything said in it, oldest
    /// first.
    ///
    /// The root is included because a thread without the thing it is about is
    /// half a conversation - and it is fetched separately rather than by a
    /// clever join, because a room read into local scrollback may hold the
    /// replies without holding the root, and the reverse.
    pub fn thread_messages(&self, buffer_id: &str, root_id: &str) -> Result<Vec<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id, attachments, html, sender_color, badges, reply_is_thread, reply_forwarded, components
             FROM messages
             WHERE buffer_id = ?1 AND ((reply_to_id = ?2 AND reply_is_thread = 1) OR msg_id = ?2)
             ORDER BY ts ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![buffer_id, root_id], |row| Self::row_to_message(buffer_id, row))?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    pub fn get_message(&self, buffer_id: &str, msg_id: &str) -> Result<Option<Message>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT msg_id, from_nick, body, ts, is_action, is_highlight, kind, reply_to_id, reply_to_from, reply_to_body, edited, reactions, is_own, avatar_url, embeds, sender_id, attachments, html, sender_color, badges, reply_is_thread, reply_forwarded, components
             FROM messages
             WHERE buffer_id = ?1 AND msg_id = ?2",
        )?;
        let mut rows = stmt.query_map(params![buffer_id, msg_id], |row| Self::row_to_message(buffer_id, row))?;
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
        s.append_message(buffer, id, "someone", "hi", 1, false, false, "chat", None, &[], false, None, &[], &[], None, None, "channel", None, &[])
            .expect("appending");
    }

    /// Like `append`, but with text worth searching for.
    fn append_saying(s: &Store, buffer: &str, id: &str, body: &str) {
        s.append_message(buffer, id, "someone", body, 1, false, false, "chat", None, &[], false, None, &[], &[], None, None, "channel", None, &[])
            .expect("appending");
    }

    fn mention(s: &Store, buffer: &str, id: &str, kind: &str) {
        s.append_message(buffer, id, "someone", "hey you", 1, false, true, "chat", None, &[], false, None, &[], &[], None, None, kind, None, &[])
            .expect("appending");
    }

    /// Like `append`, but at a time - so "newest" can be checked.
    fn append_at(s: &Store, buffer: &str, id: &str, ts: i64) {
        s.append_message(buffer, id, "someone", "hi", ts, false, false, "chat", None, &[], false, None, &[], &[], None, None, "channel", None, &[])
            .expect("appending");
    }

    #[test]
    fn finds_the_newest_message_to_mark_a_room_read_up_to() {
        let (s, _dir) = store();
        assert_eq!(s.newest_message_id("b").unwrap(), None, "an empty room has nothing to have read");

        append_at(&s, "b", "$one", 100);
        append_at(&s, "b", "$two", 300);
        append_at(&s, "b", "$mid", 200);
        // Newest by time, not by arrival: history arriving after a live
        // message must not move the marker backwards.
        assert_eq!(s.newest_message_id("b").unwrap().as_deref(), Some("$two"));
        // And it is that room's newest, not the store's.
        append_at(&s, "other", "$elsewhere", 999);
        assert_eq!(s.newest_message_id("b").unwrap().as_deref(), Some("$two"));
    }

    #[test]
    fn a_mention_is_found_without_the_conversation_being_open() {
        // The whole point: buffers live only as long as the daemon, so a
        // mention that could only be found by naming a live one disappeared
        // over a restart even though it was on disk the whole time.
        let (st, dir) = store();
        mention(&st, "irc|#chan", "1", "channel");

        let found = st.mentions(&[], 50).unwrap();
        assert_eq!(found.len(), 1, "a mention must not need its channel to be open");
        assert_eq!(found[0].buffer_id, "irc|#chan");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_direct_message_is_not_a_mention() {
        // It already has its own conversation and its own row in the list;
        // collecting it here would also drag in every service robot that says
        // your name in a query.
        let (st, dir) = store();
        mention(&st, "irc|NickServ", "1", "dm");
        assert!(st.mentions(&[], 50).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_mention_written_before_this_change_still_needs_its_channel_named() {
        // Rows from before messages recorded which kind of conversation they
        // were in have nothing to vouch for them but the live buffer list,
        // which is exactly what this used to rely on.
        let (st, dir) = store();
        st.append_message("irc|#old", "1", "someone", "hey you", 1, false, true, "chat", None, &[], false, None, &[], &[], None, None, "channel", None, &[])
            .unwrap();
        st.conn.lock().unwrap().execute("UPDATE messages SET buffer_kind = NULL", []).unwrap();

        assert!(st.mentions(&[], 50).unwrap().is_empty(), "nothing vouches for it");
        assert_eq!(st.mentions(&["irc|#old".to_string()], 50).unwrap().len(), 1, "the live list still does");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_transfer_is_remembered_and_updated_in_place() {
        let (st, dir) = store();
        let mut row = super::TransferRow {
            id: "t1".into(),
            account_id: "irc|a".into(),
            outgoing: false,
            peer: "someone".into(),
            file_name: "film.mkv".into(),
            raw_name: "film.mkv".into(),
            size: 100,
            received: 0,
            state: "receiving".into(),
            path: None,
            error: None,
            ts: 42,
        };
        st.record_transfer(&row).unwrap();
        row.received = 100;
        row.state = "done".into();
        row.path = Some("/tmp/film.mkv".into());
        st.record_transfer(&row).unwrap();

        let back = st.recent_transfers(50).unwrap();
        assert_eq!(back.len(), 1, "the same transfer must not become two");
        assert_eq!(back[0].state, "done");
        assert_eq!(back[0].received, 100);
        assert_eq!(back[0].path.as_deref(), Some("/tmp/film.mkv"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_message_stored_twice_is_only_new_once() {
        // What a reconnect looks like: the backend replays a room's recent
        // history, and every message in it arrives again. Saying so is what
        // stops a mention in that history notifying on every reconnect - the
        // same one, over and over, for a message read hours ago.
        let (st, dir) = store();
        let first = st
            .append_message("room", "uuid-1", "someone", "hi", 1, false, true, "chat", None, &[], false, None, &[], &[], None, None, "channel", None, &[])
            .expect("appending");
        let again = st
            .append_message("room", "uuid-1", "someone", "hi", 1, false, true, "chat", None, &[], false, None, &[], &[], None, None, "channel", None, &[])
            .expect("appending again");

        assert!(first, "the first time is new");
        assert!(!again, "the second time is a replay, not a message");
        assert_eq!(st.get_backlog("room", i64::MAX, 50).unwrap().len(), 1, "and only one was kept");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_same_id_in_two_rooms_is_two_messages() {
        // The index is on the pair, not the id: a whisper and a room message
        // can carry the same uuid, and treating the second as a replay would
        // silently drop it.
        let (st, dir) = store();
        assert!(st
            .append_message("room", "shared", "someone", "hi", 1, false, false, "chat", None, &[], false, None, &[], &[], None, None, "channel", None, &[])
            .unwrap());
        assert!(st
            .append_message("Whispers", "shared", "someone", "hi", 1, false, false, "whisper", None, &[], false, None, &[], &[], None, None, "channel", None, &[])
            .unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn search_finds_a_fragment_and_stays_inside_its_buffer() {
        let (st, dir) = store();
        append_saying(&st, "a", "1", "just need to broil some broc");
        append_saying(&st, "a", "2", "unrelated chatter");
        append_saying(&st, "b", "3", "broil something else entirely");

        let hits = st.search_messages("a", "broil", 50).unwrap();
        assert_eq!(hits.len(), 1, "expected one hit, got {hits:?}");
        assert_eq!(hits[0].id, "1");

        // A search is for a fragment, not a whole word.
        assert_eq!(st.search_messages("a", "roil so", 50).unwrap().len(), 1);
        // And it must not reach into another conversation.
        assert_eq!(st.search_messages("b", "broil", 50).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_wildcard_in_the_query_is_not_a_wildcard() {
        // Searching for "50%" must not match every message starting "50".
        let (st, dir) = store();
        append_saying(&st, "a", "1", "down 50% this quarter");
        append_saying(&st, "a", "2", "50 people showed up");

        let hits = st.search_messages("a", "50%", 50).unwrap();
        assert_eq!(hits.len(), 1, "the percent sign was treated as a wildcard: {hits:?}");
        assert_eq!(hits[0].id, "1");
        let _ = std::fs::remove_dir_all(dir);
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

#[cfg(test)]
mod dedupe_tests {
    use super::*;

    fn store() -> Store {
        Store::open(std::path::Path::new(":memory:")).unwrap()
    }

    fn put(s: &Store, buffer: &str, id: &str, body: &str) {
        let _ = s.append_message(buffer, id, "nick", body, 1, false, false, "chat", None, &[], false, None, &[], &[], None, None, "channel", None, &[]);
    }

    /// A backend replaying history it already has - Sneedchat does this on
    /// every reconnect - must not stack up copies. Seven of them is what made
    /// a frontend's keyed rows collide.
    #[test]
    fn recording_the_same_message_twice_stores_it_once() {
        let s = store();
        for _ in 0..7 {
            put(&s, "acct|#room", "abc", "hello");
        }
        assert_eq!(s.get_backlog("acct|#room", 0, 100).unwrap().len(), 1);
    }

    /// The first copy wins, so a replay cannot rewrite what was said.
    #[test]
    fn a_replay_does_not_overwrite_the_stored_copy() {
        let s = store();
        put(&s, "acct|#room", "abc", "what was said");
        put(&s, "acct|#room", "abc", "something else");
        let rows = s.get_backlog("acct|#room", 0, 100).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].body, "what was said");
    }

    /// Two conversations can hold the same id without either shadowing the
    /// other - a bridged message legitimately exists in both.
    #[test]
    fn the_same_id_in_two_buffers_is_two_messages() {
        let s = store();
        put(&s, "acct|#one", "abc", "hello");
        put(&s, "acct|#two", "abc", "hello");
        assert_eq!(s.get_backlog("acct|#one", 0, 100).unwrap().len(), 1);
        assert_eq!(s.get_backlog("acct|#two", 0, 100).unwrap().len(), 1);
    }
}
