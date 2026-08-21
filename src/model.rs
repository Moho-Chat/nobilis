use serde::{Deserialize, Serialize};

/// Matches the wire contract's Account JSON shape exactly (see
/// daemon/nobilis/model.c's nobilis_account_json) - the frontend renders these
/// fields directly, so names/shape here are not negotiable.
#[derive(Serialize, Clone, Debug)]
pub struct Account {
    pub id: String,
    pub service: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    pub state: String,
    pub autojoin: String,
    #[serde(rename = "hasNickservPassword")]
    pub has_nickserv_password: bool,
    #[serde(rename = "saslEnabled")]
    pub sasl_enabled: bool,
    #[serde(rename = "saslUsername")]
    pub sasl_username: String,
    #[serde(rename = "allowPlaintextSasl")]
    pub allow_plaintext_sasl: bool,
    #[serde(rename = "hasPassword")]
    pub has_password: bool,
    /// A real avatar image URL (currently Discord's CDN only - IRC has no
    /// such concept) for the account switcher/sidebar to render instead of
    /// a generic colored initial.
    #[serde(rename = "avatarUrl", skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Currently-enabled Sneedchat rooms (empty for every other service) -
    /// see accounts.rs's SockChatRoom / the setSockChatRooms RPC. There's
    /// no server-side "list every room" endpoint wired up yet, so the
    /// settings UI's own known-channel catalog cross-references against
    /// this to know which toggles should show as on.
    #[serde(rename = "sockchatRooms", skip_serializing_if = "Vec::is_empty")]
    pub sockchat_rooms: Vec<SockChatRoomInfo>,
    #[serde(rename = "torMode", skip_serializing_if = "Option::is_none")]
    pub tor_mode: Option<String>,
    #[serde(rename = "torProxy", skip_serializing_if = "Option::is_none")]
    pub tor_proxy: Option<String>,
    /// IRC-only: tunnel this account's connection through a SOCKS5 proxy
    /// (an external Tor daemon/Tor Browser, not the embedded Arti client -
    /// see accounts.rs's IrcAccountConfig::use_tor doc comment). Always
    /// false for every other service.
    #[serde(rename = "useTor")]
    pub use_tor: bool,
    /// Matrix-only: whether this account currently has server-side room
    /// key backup (a recovery key) set up - see backend/matrix/backup.rs.
    /// Always false for every other service.
    #[serde(rename = "hasKeyBackup")]
    pub has_key_backup: bool,
}

/// One room in a Sneedchat account's currently-enabled list, as exposed on
/// `Account.sockchatRooms` - see accounts.rs's own SockChatRoom (the
/// persisted config shape this mirrors).
#[derive(Serialize, Clone, Debug)]
pub struct SockChatRoomInfo {
    pub id: u32,
    pub name: String,
}

/// Matches Buffer JSON (daemon/nobilis/model.c's nobilis_buffer_json).
/// `id` = "<accountId>|<name>"; `kind` is "channel"|"dm"|"server".
#[derive(Serialize, Clone, Debug)]
pub struct Buffer {
    pub id: String,
    #[serde(rename = "accountId")]
    pub account_id: String,
    pub kind: String,
    pub name: String,
    /// Unix timestamp of the buffer's most recent message (0 if none yet) -
    /// seeded from persisted scrollback when the buffer is (re)created, then
    /// bumped live on every new message so the frontend can sort buffers by
    /// recent activity without querying history itself.
    #[serde(rename = "lastActivityTs", default)]
    pub last_activity_ts: i64,
    /// The room's own avatar (Matrix only - a resolved local file:// path,
    /// same reasoning as Message::avatar_url; IRC/XMPP/Discord channels
    /// have no per-channel avatar concept). See Runtime::
    /// set_matrix_room_avatar.
    #[serde(rename = "avatarUrl", skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Whether this room is end-to-end encrypted (Matrix only - absent,
    /// not `false`, for every other protocol, since "encrypted" isn't a
    /// meaningful concept for them at all). Drives the lock/unlock
    /// indicator in the message input - see Runtime::
    /// mark_matrix_room_encrypted.
    #[serde(rename = "encrypted", skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<bool>,
}

/// A cached snapshot of the message being replied to, taken at receive
/// time - not a live reference. Discord hands us the referenced message's
/// author/content inline with the reply itself (`referenced_message`), so
/// there's no need to look anything up, and the preview still means
/// something even if the original later scrolls out of local history or
/// gets deleted. `id` is what the frontend's "jump to" click targets if
/// the original happens to already be loaded.
#[derive(Serialize, Clone, Debug)]
pub struct ReplyPreview {
    pub id: String,
    pub from: String,
    pub body: String,
}

/// One emoji's reaction tally on a message. `me` is whether *this*
/// account is among the reactors - Discord's REACTION_ADD/REMOVE events
/// are per-user, so this is accumulated incrementally rather than
/// snapshotted (see backend/discord.rs's reaction handling).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Reaction {
    pub emoji: String,
    pub count: i64,
    pub me: bool,
    /// Only meaningful for a custom Discord emoji (see backend/discord.rs's
    /// extract_reactions) - whether the frontend should build its CDN
    /// image URL with a `.gif` extension instead of `.png`. Always false
    /// for a plain Unicode emoji, which the frontend renders as text and
    /// never looks at this for.
    #[serde(default)]
    pub animated: bool,
}

/// A Discord rich embed (title/description/color/timestamp box - a
/// webhook's or bot's own formatted content, distinct from the plain
/// `body` text) - see backend/discord.rs's extract_embeds. `color` is
/// Discord's own decimal RGB value (e.g. 15844367); the frontend renders
/// it as a colored accent bar down the embed's left edge, same as
/// Discord's own client does, and re-renders it live if a later edit
/// changes it (a webhook can restyle an existing embed - status-bridge
/// bots commonly do this to signal connected/degraded/down).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Embed {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// A file attached to a message, described rather than inlined.
///
/// Modelled on how Matrix itself carries media and how matrix-rust-sdk hands
/// it to a client: the metadata travels beside the message, and the bytes are
/// resolved separately into a local cache file whose path is reported here.
/// The message body keeps whatever text the protocol actually sent (for Matrix
/// media that is the filename), so a frontend never has to reverse-engineer an
/// attachment back out of prose.
///
/// Every protocol that has attachments already describes them this way -
/// Matrix in `content.info`, Discord in its `attachments` array - so this is
/// mostly a matter of not throwing that structure away.
///
/// `path`/`thumbnail_path` are local `file://` URLs, present once the daemon
/// has fetched the bytes. They are the only route a frontend has to media
/// behind Tor, a Matrix access token, or E2EE decryption - the same reason
/// matrix-rust-sdk returns a temp-file handle rather than a URL.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Attachment {
    /// "image" | "video" | "audio" | "file" - the broad shape, so a frontend
    /// can choose a renderer without parsing mimetypes itself.
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mimetype: Option<String>,
    /// The original filename, where the protocol supplies one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// Intrinsic pixel dimensions, when known. A frontend can reserve layout
    /// space with these before any bytes arrive, which is what stops a message
    /// list reflowing as images load.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// A compact placeholder to paint while the real image loads (Matrix's
    /// blurhash, where the sender provided one).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blurhash: Option<String>,
    /// Locally cached full-size media.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Locally cached thumbnail, when the protocol offers one. Preferring this
    /// for previews avoids pulling a full-size original over Tor or off a
    /// homeserver just to draw a small image.
    #[serde(rename = "thumbnailPath", skip_serializing_if = "Option::is_none")]
    pub thumbnail_path: Option<String>,
    /// The remote URL, for "open the original" and for re-fetching after a
    /// cache sweep. Not directly loadable by a frontend for Sneedchat (Tor)
    /// or Matrix (auth), which is what `path` is for.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Matches Message JSON (daemon/nobilis/uiops_conv.c + store.c's row shape).
#[derive(Serialize, Clone, Debug)]
pub struct Message {
    pub id: String,
    #[serde(rename = "bufferId")]
    pub buffer_id: String,
    pub from: String,
    pub body: String,
    pub ts: i64,
    #[serde(rename = "isAction")]
    pub is_action: bool,
    #[serde(rename = "isHighlight")]
    pub is_highlight: bool,
    pub kind: String,
    #[serde(rename = "replyTo", skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<ReplyPreview>,
    #[serde(default)]
    pub edited: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reactions: Vec<Reaction>,
    /// Whether the account viewing this sent it - the same identity check
    /// already used for highlight detection, just also exposed so the
    /// frontend can gate Edit/Delete without fragile name-matching of its
    /// own (a local display-name override shouldn't break that check).
    #[serde(rename = "isOwn", default)]
    pub is_own: bool,
    /// The sender's real avatar image URL (Discord only - see backend/
    /// discord.rs's author_avatar_url; IRC/XMPP have no such concept).
    /// Covers the account's own messages too, same mechanism, no special
    /// casing - Discord's gateway echo of a self-sent message carries the
    /// full `author` object same as anyone else's.
    #[serde(rename = "avatarUrl", skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Structured rich embeds attached to this message (Discord only) -
    /// see the Embed doc comment. Replaces the old behavior of just
    /// dumping an embed's title/description into `body` as plain text.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub embeds: Vec<Embed>,
    /// Files attached to this message, described rather than inlined into
    /// `body` - see Attachment.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    /// The sender's real protocol-level id (Matrix only - a full MXID like
    /// `@user:server`, as opposed to `from`'s display name, which can
    /// collide between users or be a locally-set nickname unrelated to
    /// their real identity). Needed to target moderation actions (redact-
    /// others/kick/ban/mute - see backend/matrix/moderation.rs) at the
    /// right user regardless of what display name they're currently
    /// showing.
    #[serde(rename = "senderId", skip_serializing_if = "Option::is_none")]
    pub sender_id: Option<String>,
}

pub fn buffer_id(account_id: &str, name: &str) -> String {
    format!("{account_id}|{name}")
}

/// A fresh, process-unique message id (daemon/nobilis/model.c's
/// nobilis_next_message_id: "<unix-ts>.<seq>").
pub fn next_message_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{ts}.{seq}")
}

/// "~"/"@"/"%"/"+"/"" by descending rank (daemon/nobilis/model.c's
/// nobilis_member_prefix).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MemberRank {
    None,
    Voice,
    HalfOp,
    Op,
    Founder,
}

impl MemberRank {
    pub fn prefix(self) -> &'static str {
        match self {
            MemberRank::Founder => "~",
            MemberRank::Op => "@",
            MemberRank::HalfOp => "%",
            MemberRank::Voice => "+",
            MemberRank::None => "",
        }
    }
}
