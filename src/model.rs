use serde::{Deserialize, Serialize};

/// Matches the wire contract's Account JSON shape exactly (see
/// daemon/nobilis/model.c's nobilis_account_json) - the frontend renders these
/// fields directly, so names/shape here are not negotiable.
#[derive(Serialize, Clone, Debug)]
pub struct Account {
    pub id: String,
    pub service: String,
    /// How the user is presenting themselves: "online" or "idle".
    /// Distinct from `state`, which is whether the connection is up - a
    /// disconnected account still remembers the status it will reconnect with.
    #[serde(default)]
    pub status: String,
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
    /// Which SASL mechanism this account is pinned to, or empty for the
    /// default. IRC-only; empty everywhere else.
    #[serde(rename = "saslMechanism")]
    pub sasl_mechanism: String,
    /// Whether a TLS client certificate is configured for SASL EXTERNAL. The
    /// path itself is not reported - a frontend never needs it, and it names a
    /// file on the machine the daemon is running on rather than this one.
    #[serde(rename = "hasSaslCertificate")]
    pub has_sasl_certificate: bool,
    /// The words that make a message here worth being told about, besides
    /// this account's own name. Reported with the account so the field that
    /// edits them can be filled in without a second question.
    #[serde(rename = "highlightKeywords", default)]
    pub highlight_keywords: Vec<String>,
    /// The nick this account is actually using right now.
    ///
    /// Not the same as the configured one: a session that landed on the alt
    /// nick is using that until GHOST reclaims the real one. Reported because
    /// a client cannot otherwise tell which row in a member list is the person
    /// using it - and therefore cannot tell whether they hold op.
    ///
    /// Empty for every service that has no such concept.
    #[serde(rename = "currentNick")]
    pub current_nick: String,
    /// Whether this account's connection is encrypted.
    ///
    /// Reported so a frontend can tell the truth about what sending
    /// credentials over it would mean. SASL PLAIN on a cleartext link is the
    /// password in the clear, and a settings pane that cannot see the
    /// difference can only nag about it always or never mention it at all.
    /// Every other service is encrypted by construction - they are all HTTPS
    /// or WSS - so only IRC ever reports false.
    pub ssl: bool,
    #[serde(rename = "hasPassword")]
    pub has_password: bool,
    /// A real avatar image URL (currently Discord's CDN only - IRC has no
    /// such concept) for the account switcher/sidebar to render instead of
    /// a generic colored initial.
    #[serde(rename = "avatarUrl", skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// Currently-enabled Sneedchat rooms (empty for every other service) -
    /// see accounts.rs's SneedChatRoom / the setSneedChatRooms RPC. There's
    /// no server-side "list every room" endpoint wired up yet, so the
    /// settings UI's own known-channel catalog cross-references against
    /// this to know which toggles should show as on.
    #[serde(rename = "sneedchatRooms", skip_serializing_if = "Vec::is_empty")]
    pub sneedchat_rooms: Vec<SneedChatRoomInfo>,
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
/// `Account.sneedchatRooms` - see accounts.rs's own SneedChatRoom (the
/// persisted config shape this mirrors).
#[derive(Serialize, Clone, Debug)]
pub struct SneedChatRoomInfo {
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
    /// The buffer's own picture: a room's avatar for Matrix (a resolved local
    /// file:// path, same reasoning as Message::avatar_url), or for a direct
    /// message the other person's. Absent for channels, which have no
    /// per-channel avatar concept outside Matrix. See Runtime::
    /// set_buffer_avatar.
    #[serde(rename = "avatarUrl", skip_serializing_if = "Option::is_none")]
    pub avatar_url: Option<String>,
    /// The heading this buffer sits under, where the service has such a thing
    /// - a Discord category. Absent for everything else, and for channels the
    /// server left uncategorised, which belong above the first heading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Where the service puts this buffer in its own ordering.
    ///
    /// One number rather than a category rank and a channel rank, because a
    /// client only ever needs to sort by it: it is the category's position and
    /// the channel's position folded together, so channels sort within their
    /// heading and headings sort against each other.
    #[serde(default)]
    pub position: i64,
    /// Set while this buffer exists but the service has not yet told us what
    /// is in it - a Matrix room between joining it and its first sync.
    ///
    /// Carried so a room can appear the moment somebody joins rather than
    /// whenever the server gets round to mentioning it, which on a large room
    /// is many seconds later and reads as the join having done nothing.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub syncing: bool,
    /// Whether this room is end-to-end encrypted (Matrix only - absent,
    /// not `false`, for every other protocol, since "encrypted" isn't a
    /// meaningful concept for them at all). Drives the lock/unlock
    /// indicator in the message input - see Runtime::
    /// mark_matrix_room_encrypted.
    #[serde(rename = "encrypted", skip_serializing_if = "Option::is_none")]
    pub encrypted: Option<bool>,
    /// The channel's own modes, as the letters the server writes them with -
    /// "+mnt". IRC only, and absent until the server has said, which it does
    /// on join and whenever they change.
    ///
    /// Carried so a client can say why a message will not send. A moderated
    /// channel refuses one with an error that names a numeric, and "you cannot
    /// speak here" is a better thing to have known beforehand.
    #[serde(rename = "channelModes", skip_serializing_if = "Option::is_none")]
    pub channel_modes: Option<String>,
    /// Which BufferGroup this buffer belongs to - a Discord guild, a Matrix
    /// space, or the account itself for protocols with no such concept.
    /// Absent only while a backend has not yet placed it.
    #[serde(rename = "groupId", skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// The service's own identifier for this buffer - a Discord channel id.
    ///
    /// Every other field here is named the way a person would say it, which is
    /// deliberate; this one is the exception because message bodies are not. A
    /// Discord message linking a channel carries the raw `<#id>` token and
    /// nothing else, so a frontend that wants to draw that as a channel name -
    /// and open it when clicked - needs the id to match against. Kept as a
    /// field rather than resolved on the way in so that scrollback stored
    /// before the channel was ever seen still reads correctly once it is.
    #[serde(rename = "remoteId", skip_serializing_if = "Option::is_none")]
    pub remote_id: Option<String>,
}

/// The rail entry a buffer belongs to when its protocol has no grouping of
/// its own - IRC and Sneedchat today, Matrix until spaces are read. Backends
/// that do have grouping (Discord guilds) override it per buffer, so this is
/// the default rather than a special case.
pub fn account_group_id(account_id: &str) -> String {
    format!("account:{account_id}")
}

/// One entry in a frontend's server rail: a Discord guild, a Matrix space, an
/// account's direct messages, or - for IRC and Sneedchat, which have no such
/// concept - the account itself.
///
/// Deliberately one generic shape rather than per-protocol fields. What a
/// frontend needs to draw a rail is the same regardless of where the grouping
/// came from, and a protocol that gains grouping later (Matrix spaces) becomes
/// a backend change with no wire change and no frontend change.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct BufferGroup {
    /// Stable across restarts and unique across accounts, so a frontend can
    /// persist which entry was selected.
    pub id: String,
    #[serde(rename = "accountId")]
    pub account_id: String,
    /// "irc" | "discord" | "sneedchat" | "matrix" - what brand mark to fall
    /// back to when there is no icon.
    pub service: String,
    /// "guild" | "space" | "dms" | "account".
    pub kind: String,
    pub name: String,
    /// A local `file://` path once fetched, absent when the group has no icon
    /// of its own. Remote URLs are never handed out: a Discord guild icon is
    /// on a CDN the client can reach, but a Matrix space avatar is behind an
    /// access token and a Sneedchat one is only reachable over Tor, so
    /// resolving them here is what keeps every frontend uniform.
    #[serde(rename = "iconUrl", skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,
    /// Rail ordering. Ties break on name, so the order is stable rather than
    /// whatever the gateway happened to send.
    #[serde(default)]
    pub position: i64,
    /// Set while this account has joined but cannot yet speak - Discord's
    /// membership screening, where a server makes you agree to its rules
    /// first. Carried on the group rather than fetched per guild because a
    /// client needs it to say why the box is refusing, and the group is
    /// already the thing it has in hand.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pending: bool,
}

/// A cached snapshot of the message being replied to, taken at receive
/// time - not a live reference. Discord hands us the referenced message's
/// author/content inline with the reply itself (`referenced_message`), so
/// there's no need to look anything up, and the preview still means
/// something even if the original later scrolls out of local history or
/// gets deleted. `id` is what the frontend's "jump to" click targets if
/// the original happens to already be loaded.
#[derive(Serialize, Clone, Debug, Default)]
pub struct ReplyPreview {
    pub id: String,
    pub from: String,
    pub body: String,
    /// Whether `id` is a thread rather than a single message being answered.
    ///
    /// The two are the same field because they are the same fact from the
    /// protocol's side - Matrix says both with `m.relates_to`, and a threaded
    /// message names its thread where a reply names its target. What differs
    /// is what a client should do with it: a reply points somewhere, a thread
    /// is somewhere, and only the second can be opened and continued.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub thread: bool,
    /// Whether this names a message brought here rather than one answered.
    ///
    /// Discord says a reply and a forward the same way - a reference to
    /// another message - and only the reference's own type tells them apart.
    /// Read as a reply, a forward drew the arrow a reply gets with nothing
    /// beside it, which is how a forwarded message announced itself as
    /// something else entirely.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forwarded: bool,
}

/// How a service says a sender should look.
///
/// One value rather than two more parameters on `record_message_at`, which was
/// already carrying seventeen: colour and badges always travel together,
/// always come from the same place, and are always absent together for the
/// protocols that have no such idea.
#[derive(Clone, Debug, Default)]
pub struct SenderStyle {
    pub color: Option<String>,
    pub badges: Vec<crate::backend::kick::api::Badge>,
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

/// Something on a message that can be pressed.
///
/// Discord's own name for these is components, and a great many servers now
/// work through them: a bot posts a message and the thing it is for is the
/// button underneath, not the words. A client that draws only the words shows
/// half of what was sent - a role picker with no roles, a ticket panel with no
/// way to open a ticket.
///
/// Deliberately not modelled after Discord's wire shape. Their type numbers
/// (2 is a button, 3 is a select) mean nothing to a frontend and would leak an
/// integer nobody can read into every other protocol that grows the same idea.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Component {
    /// "button" or "select".
    pub kind: String,
    /// What the service calls this control, sent back when it is used. A link
    /// button has none: pressing it opens a page rather than telling anybody.
    #[serde(rename = "customId", skip_serializing_if = "Option::is_none")]
    pub custom_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// primary, secondary, success, danger or link - which is what the colour
    /// means, rather than the number the service sends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Where a link button goes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub disabled: bool,
    /// The emoji on the button, as this client writes emoji elsewhere.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// What a select offers. Empty for a button.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ComponentOption>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Which row it was on, so a client can draw them as they were laid out.
    #[serde(default)]
    pub row: i64,
}

/// One choice in a select menu.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ComponentOption {
    pub value: String,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
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
    /// The colour this service says the sender's name should be.
    ///
    /// Kick gives everybody one and it is half of how a busy chat is read.
    /// A client that has none falls back to colouring the nick itself, which
    /// is what IRC has always needed - so this overrides that rather than
    /// replacing it.
    #[serde(rename = "senderColor", skip_serializing_if = "Option::is_none")]
    pub sender_color: Option<String>,
    /// What the sender has earned in this channel: moderator, subscriber,
    /// verified, and so on. Empty for services that have no such idea.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub badges: Vec<crate::backend::kick::api::Badge>,
    /// The sender's own formatted version of `body`, where the protocol
    /// carries one (Matrix's `formatted_body`). Restricted HTML, and still
    /// untrusted - a frontend must put it through the same sanitiser it
    /// uses for everything else rather than treating it as safe because it
    /// arrived structured. `body` stays the plain-text fallback, and is
    /// what search reads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub html: Option<String>,
    /// Buttons and menus on the message, where the service has them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<Component>,
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

/// Which service an account id belongs to.
///
/// Account ids are prefixed by their service - "matrix:@a:b", "kick:name" -
/// except IRC, whose ids are "nick@host" and predate the convention. That

/// Whether this kind of line is the room reporting itself rather than
/// somebody speaking.
///
/// Somebody arrived, somebody left, the topic changed, a nick changed. Nobody
/// typed these and nobody can be addressed in one - which matters because
/// they are written *about* people and therefore contain their names. A quit
/// line says "Salastil has quit", and a client looking for your name in every
/// line will find it there.
///
/// Found the hard way: closing the client sends a QUIT, the server reports it
/// back in every channel, and every one of those became a notification and an
/// entry in the mentions inbox - a farewell from yourself, once per channel,
/// every time you shut the thing down.
///
/// A list of what to exclude rather than of what to include, deliberately:
/// the kinds that are neither speech nor membership - a subscription, a raid,
/// a moderator's action - are somebody doing something to somebody, and
/// whether those should carry a mention is a separate question from this one.
/// Left as they were.
pub fn is_room_event(kind: &str) -> bool {
    matches!(
        kind,
        "join"
            | "part"
            | "quit"
            | "nick"
            | "mode"
            | "topic"
            | "system"
            | "matrixJoin"
            | "matrixInvite"
            | "matrixKick"
            | "matrixQuit"
    )
}

/// exception is why this exists rather than each caller splitting on a colon.
pub fn service_of(account_id: &str) -> &'static str {
    match account_id.split_once(':').map(|(prefix, _)| prefix) {
        Some("matrix") => "matrix",
        Some("discord") => "discord",
        Some("kick") => "kick",
        Some("sneedchat") => "sneedchat",
        Some("jabber") => "jabber",
        Some("slack") => "slack",
        _ => "irc",
    }
}

#[cfg(test)]
mod service_tests {
    use super::service_of;

    #[test]
    fn an_account_id_names_its_service() {
        assert_eq!(service_of("matrix:@a:example.org"), "matrix");
        assert_eq!(service_of("discord:167790743988076545"), "discord");
        assert_eq!(service_of("kick:someone"), "kick");
        assert_eq!(service_of("sneedchat:Ancient Pioneer"), "sneedchat");
        // IRC is the one without a prefix, and a host with a port in it must
        // not be read as one.
        assert_eq!(service_of("Salastil@irc.libera.chat"), "irc");
        assert_eq!(service_of("Salastil@irc.example.net:6697"), "irc");
    }
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

    /// What the rank is called, for somewhere there is room to say it. None
    /// for an ordinary member, who has no rank to name.
    pub fn title(self) -> Option<&'static str> {
        match self {
            MemberRank::Founder => Some("Founder"),
            MemberRank::Op => Some("Operator"),
            MemberRank::HalfOp => Some("Half-operator"),
            MemberRank::Voice => Some("Voiced"),
            MemberRank::None => None,
        }
    }

    /// Whether this rank can act on other people. Voice is the right to
    /// speak in a moderated channel, not the right to moderate one.
    pub fn can_moderate(self) -> bool {
        matches!(self, MemberRank::Founder | MemberRank::Op | MemberRank::HalfOp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire contract is camelCase throughout, and a frontend reading the
    /// wrong key sees nothing rather than an error - a mismatch here is silent
    /// on both sides, so it gets a test rather than trust.
    #[test]
    fn attachment_serializes_with_camelcase_wire_names() {
        let a = Attachment {
            kind: "image".into(),
            mimetype: Some("image/png".into()),
            filename: Some("x.png".into()),
            size: Some(1),
            width: Some(2),
            height: Some(3),
            blurhash: Some("b".into()),
            path: Some("file:///x".into()),
            thumbnail_path: Some("file:///t".into()),
            url: Some("https://x".into()),
        };
        let v = serde_json::to_value(&a).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert!(keys.contains(&"thumbnailPath"), "got {keys:?}");
        assert!(!keys.iter().any(|k| k.contains('_')), "snake_case leaked: {keys:?}");
    }

    /// Absent fields are omitted rather than sent as null, so a frontend can
    /// treat presence as meaning "known".
    #[test]
    fn empty_attachment_fields_are_omitted() {
        let a = Attachment { kind: "file".into(), ..Default::default() };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v.as_object().unwrap().keys().collect::<Vec<_>>(), vec!["kind"]);
    }

    fn a_buffer() -> Buffer {
        Buffer {
            id: "acct|Guild/#general".into(),
            account_id: "acct".into(),
            kind: "channel".into(),
            name: "Guild/#general".into(),
            last_activity_ts: 0,
            avatar_url: None,
            category: None,
            position: 0,
            syncing: false,
            encrypted: None,
            channel_modes: None,
            group_id: None,
            remote_id: None,
        }
    }

    /// The channel id a frontend matches `<#id>` against. Under the wrong key
    /// every channel link in every message silently renders as unknown, which
    /// is precisely the symptom this field exists to fix.
    #[test]
    fn a_buffers_channel_id_goes_out_as_remote_id() {
        let b = Buffer { remote_id: Some("1393001234568164748".into()), ..a_buffer() };
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["remoteId"], "1393001234568164748");
    }

    /// Only Discord channels have one. An IRC channel sending `remoteId: null`
    /// would have a frontend indexing null as a channel id.
    #[test]
    fn a_buffer_with_no_channel_id_omits_the_key() {
        let v = serde_json::to_value(a_buffer()).unwrap();
        assert!(!v.as_object().unwrap().contains_key("remoteId"));
    }
}
