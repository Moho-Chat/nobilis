use crate::accounts::irc_account_to_json;
use crate::model::{self, Account, Attachment, Buffer, Embed, Message, ReplyPreview};
use crate::state::AppState;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// What somebody is doing in a voice channel, beyond being in it.
///
/// All four ride on the voice state itself - Discord sends no separate
/// dispatch for starting a screen share or turning a camera on, so a voice
/// state that is thrown away except for its channel id is a screen share
/// nobody can be told about.
///
/// `streaming` is Go Live, a screen or window shared into the channel.
/// `video` is a camera. They are independent: somebody can do both, and the
/// two look nothing alike to the person watching.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoiceFlags {
    pub streaming: bool,
    pub video: bool,
    pub muted: bool,
    pub deafened: bool,
}

impl VoiceFlags {
    /// Reads the four flags off a voice state as Discord sends it.
    pub fn from_voice_state(d: &Value) -> Self {
        Self {
            streaming: d["self_stream"].as_bool().unwrap_or(false),
            video: d["self_video"].as_bool().unwrap_or(false),
            // Either kind of mute counts. A server mute and muting yourself
            // are different in who can undo them and identical in whether
            // anything is audible, and this says whether anything is audible.
            muted: d["self_mute"].as_bool().unwrap_or(false) || d["mute"].as_bool().unwrap_or(false),
            deafened: d["self_deaf"].as_bool().unwrap_or(false) || d["deaf"].as_bool().unwrap_or(false),
        }
    }
}

/// A file somebody has offered, from the moment it is offered to the moment
/// it is on disk or has failed.
#[derive(Clone, Debug)]
pub struct DccTransfer {
    pub id: String,
    pub account_id: String,
    /// Which way it is going. Sending and receiving share everything except
    /// this and who the other person is, so they share a list too - what
    /// somebody wants to see is "my transfers", not two of them.
    pub outgoing: bool,
    /// Who offered it, or who it is being sent to.
    pub from: String,
    /// What we would call it on disk - already made safe.
    pub file_name: String,
    /// What they called it. Shown alongside where the two differ, so a name
    /// that had to be changed is visible rather than silently substituted.
    pub raw_name: String,
    pub size: u64,
    pub received: u64,
    /// Bytes a second over the last interval, while it is running.
    pub rate: u64,
    pub state: DccState,
    /// Set once it is on disk.
    pub path: Option<String>,
    /// Why it failed, or why it was refused.
    pub error: Option<String>,
    /// Raised to stop a transfer that is already running.
    pub cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// The offer itself, kept so accepting does not have to read anything
    /// off the network a second time. Dropped once it is settled.
    pub offer: Option<crate::backend::irc_dcc::DccSend>,
    /// When it started, so the order survives being written down and read
    /// back - a list kept only in memory can rely on its own order, and one
    /// that outlives the process cannot.
    pub started_at: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DccState {
    /// Waiting on an answer - or, when sending, waiting for them to accept.
    Offered,
    Receiving,
    Sending,
    Done,
    Declined,
    Failed,
}

impl DccState {
    /// The inverse of `as_str`, for a transfer read back from disk.
    ///
    /// Anything unrecognised is treated as failed rather than as still
    /// running: a state this version does not know about came from another
    /// one, and a row that cannot be acted on should not look live.
    pub fn from_str(s: &str) -> Self {
        match s {
            "done" => DccState::Done,
            "declined" => DccState::Declined,
            _ => DccState::Failed,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            DccState::Offered => "offered",
            DccState::Receiving => "receiving",
            DccState::Sending => "sending",
            DccState::Done => "done",
            DccState::Declined => "declined",
            DccState::Failed => "failed",
        }
    }

    /// Still going, so it counts against the concurrency limit.
    pub fn active(self) -> bool {
        matches!(self, DccState::Offered | DccState::Receiving | DccState::Sending)
    }
}

/// How many transfers are remembered at once.
///
/// The finished ones are kept only so the list has some history in it; a
/// person who wants the file wants the file, not a ledger.
pub const DCC_KEEP: usize = 100;

/// How many unanswered offers one person may have outstanding.
///
/// Without this, anybody who can message you can put an unbounded queue of
/// prompts on your screen, which is a nuisance at best and a way to hide a
/// real prompt among fifty fake ones at worst.
const DCC_PENDING_PER_NICK: usize = 3;

/// Somebody in a voice channel, as a call view needs them.
///
/// A struct rather than the tuple this used to be: it grew a picture, and a
/// four-field tuple whose third element is an optional URL is the kind of
/// thing that gets passed in the wrong order once and then silently shows
/// everyone the same face.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VoiceRosterEntry {
    pub user_id: String,
    pub name: String,
    pub avatar_url: Option<String>,
    pub flags: VoiceFlags,
}

/// Where somebody is, and what they are doing there.
#[derive(Clone, Debug)]
struct VoicePresence {
    channel_id: String,
    flags: VoiceFlags,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Connected,
    /// Distinct from Disconnected: the saved credential itself was
    /// rejected (confirmed for Discord's gateway close code 4004 - a
    /// revoked/invalidated token, not a network hiccup), so retrying with
    /// the same stored credential can never succeed. The retry loop stops
    /// entirely rather than backing off and trying again (see
    /// backend/discord.rs's run_gateway_with_retry) - the only way out is
    /// a fresh login, which upserts the existing account in place (same
    /// account id, same buffers/scrollback) rather than creating a new one.
    AuthFailed,
}

impl ConnState {
    fn as_str(&self) -> &'static str {
        match self {
            ConnState::Disconnected => "disconnected",
            ConnState::Connecting => "connecting",
            ConnState::Connected => "connected",
            ConnState::AuthFailed => "auth_failed",
        }
    }
}

/// Per-connected-account handle the RPC layer uses to act on a live IRC
/// connection (join/part/send) - see backend/irc.rs for what populates
/// this. `nick` is the *current* nick (may differ from the account's
/// configured nick after a collision-driven rename, though that's not
/// handled yet - out of scope for this milestone).
pub struct IrcHandle {
    pub sender: irc::client::Sender,
    pub nick: String,
}

/// Live, in-memory state that sits alongside the persisted AccountStore:
/// connection state per account, active IRC senders, and the buffer
/// registry. Mirrors what libpurple itself tracked at runtime
/// (purple_accounts_get_all()/purple_get_conversations()) that nobilis's C
/// version queried directly - here it's ours to maintain explicitly.
pub struct Runtime {
    conn_states: Mutex<HashMap<String, ConnState>>,
    irc_handles: Mutex<HashMap<String, IrcHandle>>,
    buffers: Mutex<HashMap<String, Buffer>>,
    /// Rail entries by group id - Discord guilds, Matrix spaces, and the
    /// account-level entry every other protocol gets. Held here rather than
    /// per-backend so listBufferGroups is one lookup regardless of protocol.
    buffer_groups: Mutex<HashMap<String, crate::model::BufferGroup>>,
    /// Registered the instant a connection task is spawned - before there's
    /// any IrcHandle/Sender to gracefully QUIT with. Lets setAccountConnected
    /// (and removeAccount) actually stop a connection attempt that's still
    /// stuck in DNS/TCP/TLS/registration, not just an already-connected one.
    task_handles: Mutex<HashMap<String, tokio::task::AbortHandle>>,
    /// Timestamp of the last connection *attempt* per account (IRC only
    /// for now) - a real IRC network's own anti-flood/throttling can
    /// silently drop new connection attempts for a while after too many
    /// in quick succession from the same source, which manifests as a
    /// TCP connect that just hangs (confirmed live: disconnecting and
    /// immediately reconnecting a couple of times in a row was enough to
    /// trigger it against both Libera and Rizon). This isn't something a
    /// bigger timeout fixes - the fix is not re-triggering it, by pacing
    /// out attempts on our own end. See backend/irc.rs's spawn().
    last_connect_attempt: Mutex<HashMap<String, std::time::Instant>>,
    /// Guards against a real race: disconnect() on an already-connected
    /// account only sends QUIT and returns immediately - it does not wait
    /// for (or synchronously clean up after) the old task actually
    /// exiting, since that only happens once the server closes the
    /// connection in response. If the user reconnects before that old
    /// task finishes unwinding, spawn() starts a genuinely new task while
    /// the old one is still alive; when the old one *does* finally exit,
    /// its own cleanup (set state to Disconnected, remove the handles)
    /// would otherwise blindly stomp on whatever the new connection has
    /// since set up, by account_id alone with no way to tell old from
    /// new. Each spawn() takes a fresh generation number, and cleanup
    /// only actually applies if its generation is still the current one.
    connect_generation: Mutex<HashMap<String, u64>>,
    /// Accounts that are meant to be connected right now.
    ///
    /// IRC reconnects on its own, and a retry loop needs to tell "the
    /// connection dropped" from "somebody switched this account off" -
    /// which look identical from inside the loop, because disconnect() on a
    /// connected account only sends QUIT and returns. The session then ends
    /// the same way a dead link does, and without this the account would
    /// come straight back up seconds after being turned off.
    wants_connected: Mutex<std::collections::HashSet<String>>,
    /// Rooms this account has been invited to and not yet answered, by
    /// account id. Held rather than turned straight into buffers: an invite
    /// is not a room you are in, has none of its history, and answering it
    /// is a decision somebody has to make.
    matrix_invites: Mutex<HashMap<String, Vec<serde_json::Value>>>,
    /// Guilds this account could not speak in when it connected, by group id.
    ///
    /// Kept apart from the flag on the rail entry, which is about what to
    /// show. This is about what still needs doing: the entry's flag is
    /// cleared the moment the rules are agreed to, so the channel list -
    /// which is rebuilt when Discord confirms the change - would have nothing
    /// left to tell it the rebuild was owed.
    discord_gated: Mutex<std::collections::HashSet<String>>,
    /// Guilds whose channel list is being re-read, and whether another
    /// change arrived while it was. Role edits and permission changes come
    /// in bursts - a server being rearranged emits one event per channel -
    /// and each is two requests, so they collapse into one pass and a single
    /// follow-up that sees the settled result.
    discord_resync: Mutex<HashMap<String, bool>>,
    /// This account's own roles per guild, as last seen.
    ///
    /// Kept only to tell a member update that matters from one that does
    /// not: roles decide which channels are permitted, and a nickname change
    /// arrives on the same event and decides nothing.
    discord_own_roles: Mutex<HashMap<String, Vec<String>>>,
    /// Per (account, room) pagination token for reading older history.
    matrix_back_tokens: Mutex<HashMap<(String, String), String>>,
    /// A guild channel's member window in Discord's own order, by buffer.
    ///
    /// Kept beside the presence snapshot rather than instead of it because
    /// the two are ordered differently on purpose: the snapshot is sorted
    /// for reading, and Discord's incremental ops address this one by index,
    /// so sorting it would make every INSERT and DELETE land on the wrong
    /// person.
    discord_member_windows: Mutex<HashMap<String, Vec<serde_json::Value>>>,
    /// Last-known member list per buffer (the same JSON shape presenceChange
    /// events carry) - presenceChange itself is only ever *pushed* on a
    /// join/part/namreply, so a client subscribing afterward (reopening a
    /// buffer, or a fresh UI session entirely) would otherwise see an empty
    /// userlist until the next incremental change. Keeping the current
    /// snapshot here lets subscribe() replay it immediately - see
    /// rpc/methods.rs's subscribe handler.
    presence: Mutex<HashMap<String, serde_json::Value>>,
    /// Generic (protocol-agnostic) "what's my own display name on this
    /// account" - record_message's highlight check needs this for every
    /// backend, not just IRC (which already tracked it per-connection via
    /// IrcHandle.nick/irc_current_nick well before Discord existed).
    own_identity: Mutex<HashMap<String, String>>,
    /// Discord-specific: buffer id -> channel snowflake. Discord buffer
    /// *names* are human-friendly ("guild/#general", a DM recipient's
    /// username) for display, but sending requires the actual channel id -
    /// this is the only place that mapping is kept (IRC has no equivalent
    /// need since its buffer name already *is* the protocol target).
    discord_channels: Mutex<HashMap<String, String>>,
    /// Conversations currently ringing this machine, buffer id -> (account,
    /// channel).
    ///
    /// Kept here rather than left to each frontend because a ring outlives
    /// any one of them: the daemon stays connected while the window is shut,
    /// so a client opening mid-ring has to be able to ask what is ringing
    /// rather than having missed the event that said so.
    ringing_calls: Mutex<HashMap<String, (String, String)>>,
    /// Discord-specific: buffer id -> guild snowflake, for a guild channel
    /// only (absent entirely for a DM buffer, which has no guild). Only
    /// consumer is message_link's "open this message in a real Discord
    /// client" deep link (see rpc/methods.rs's getDiscordMessageLink) -
    /// the fallback for an attachment link whose signature expired, since
    /// this backend has no way to silently re-sign one itself (see
    /// backend/discord.rs's message_link doc comment).
    discord_guild_id: Mutex<HashMap<String, String>>,
    /// Guards backend::discord::extend_history against running twice at
    /// once for the same buffer - getBacklog can be called concurrently by
    /// more than one connected client (this project runs one ChatView
    /// instance per screen plus the popout window, all independently
    /// connected - see the QR-login multi-instance bug for how that bit
    /// once already), and without this a double-fetch would insert the
    /// same page of Discord history twice (no per-message dedup here).
    discord_history_inflight: Mutex<HashSet<String>>,
    /// Discord-specific: buffer id -> that channel's guild's usable custom
    /// emoji ({id, name, animated}) - see backend/discord.rs's
    /// register_guild_channels, populated straight from GUILD_CREATE with
    /// no extra request needed. Empty/absent for IRC and any Discord DM.
    discord_buffer_emojis: Mutex<HashMap<String, Vec<serde_json::Value>>>,
    /// Discord-specific: account id -> friends list ({userId, username,
    /// globalName, avatarUrl, status}), seeded from READY's `relationships`
    /// (type 1 = friend) + `presences`, kept current by live PRESENCE_UPDATE
    /// dispatches - see backend/discord.rs's READY/PRESENCE_UPDATE handling.
    /// Read synchronously by listDiscordFriends (no network round trip
    /// needed at query time, unlike Matrix's listMatrixDevices).
    discord_friends: Mutex<HashMap<String, Vec<serde_json::Value>>>,
    /// Sneedchat-specific: account id -> room id -> outgoing-frame sender.
    /// Unlike a single active-room connection, every configured room gets
    /// its own permanent websocket (see backend/sockchat/mod.rs), so
    /// sending has to target the specific room's own socket rather than
    /// one shared per-account sender.
    sockchat_senders: Mutex<HashMap<String, HashMap<u32, tokio::sync::mpsc::UnboundedSender<String>>>>,
    /// Matrix-specific: buffer id -> room id. Matrix buffer *names* are
    /// human-friendly (see backend/matrix/rooms.rs's naming fallback
    /// chain), but sending/reacting/etc. needs the real `!opaque:server`
    /// room id - same reasoning as discord_channels above, one connection
    /// per account covering every joined room rather than per-room senders
    /// like Sneedchat (Matrix's `/sync` doesn't need a per-room socket).
    matrix_rooms: Mutex<HashMap<String, String>>,
    /// Matrix-specific: (account id, room id) -> (buffer name, buffer
    /// kind), cached the first time a room is seen (see backend/matrix/
    /// mod.rs's process_sync_response) so every later event for that room
    /// can pass record_message the right buffer name without re-deriving
    /// it from that sync response's (often empty, incremental-only) state
    /// delta - see rooms.rs's own doc comment for why re-deriving on every
    /// event would risk silently fragmenting a room's messages across
    /// multiple wrongly-named buffers.
    matrix_room_names: Mutex<HashMap<(String, String), (String, String)>>,
    /// Matrix-specific: (account id, child room id) -> the rail group of the
    /// space listing it. Kept because a space and its children arrive in no
    /// particular order - a child's buffer may exist before the space that
    /// owns it has been seen, or the other way round - so both directions
    /// consult this rather than depending on which came first.
    matrix_space_parents: Mutex<HashMap<(String, String), String>>,
    /// account id -> "online" | "idle". Absent means online, which is what
    /// every backend does on connect anyway.
    account_status: Mutex<HashMap<String, String>>,
    discord_member_list_targets: Mutex<HashMap<(String, String), String>>,
    /// (account, guild) -> that guild's voice channels, as (id, name, limit).
    discord_voice_channels: Mutex<HashMap<(String, String), Vec<(String, String, u64)>>>,
    /// (account, user) -> the voice channel they are in. Absent means not in
    /// one; this is the only record of who is where, since Discord reports
    /// voice membership solely over the gateway.
    discord_voice_states: Mutex<HashMap<(String, String), VoicePresence>>,
    /// (account, user) -> what to call them. Filled from the member Discord
    /// attaches to a voice state, because someone in a voice channel is often
    /// in no loaded member list - a channel list showing raw snowflakes would
    /// be useless.
    discord_voice_names: Mutex<HashMap<(String, String), String>>,
    /// (account, user) -> their picture. Alongside the names and filled from
    /// the same places, because a call view is mostly faces: a row of coloured
    /// initials is legible but it is not who is in the room, and the member
    /// lists that would otherwise carry a picture do not cover a voice channel.
    discord_voice_avatars: Mutex<HashMap<(String, String), String>>,
    /// account -> the voice channel we are in, if any.
    discord_voice_self: Mutex<HashMap<String, String>>,
    /// Offered files waiting on an answer, and transfers already running,
    /// newest first. One list rather than two: a person watching this is
    /// watching one thing happen - a file arriving - and an offer is its
    /// first state, not a different kind of object.
    dcc_transfers: Mutex<Vec<DccTransfer>>,
    /// account -> the route its live IRC connection is using.
    ///
    /// Kept rather than rebuilt from the account config when a transfer is
    /// accepted, because the two can disagree: turning Tor off in settings
    /// does not move a connection that is already up, and rebuilding would
    /// then dial a transfer directly while its connection was still proxied.
    /// No entry means no live connection, and so nothing to transfer over.
    irc_transports: Mutex<HashMap<String, crate::net::tor::Transport>>,
    /// Discord-specific: account id -> its live gateway writer, so a status
    /// change can push a presence update on the existing connection instead of
    /// waiting for a reconnect.
    discord_gateway_senders: Mutex<HashMap<String, tokio::sync::mpsc::UnboundedSender<String>>>,
    /// Matrix-specific: account id -> the running OlmMachine wrapper (see
    /// backend/matrix/crypto.rs). Stored here, not just kept local to the
    /// sync-loop task, so sendMessage/toggleReaction RPC handlers - which
    /// run on a different task - can reach it to encrypt outgoing content
    /// and share room keys (phase 4).
    matrix_machines: Mutex<HashMap<String, std::sync::Arc<crate::backend::matrix::crypto::CryptoSession>>>,
    /// Matrix-specific: buffer ids whose room has ever received an
    /// `m.room.encryption` state event - sticky per room, per the C-S API
    /// spec (a room can't be un-encrypted once encryption is turned on).
    matrix_encrypted_rooms: Mutex<HashSet<String>>,
    /// Matrix-specific: (buffer id, message id, emoji) -> our own sent
    /// `m.reaction` event id - un-reacting on Matrix means redacting the
    /// specific reaction event we sent, unlike Discord's toggle-by-name
    /// endpoint (see toggle_reaction in backend/matrix/mod.rs).
    matrix_own_reactions: Mutex<HashMap<(String, String, String), String>>,
    /// Matrix-specific: reaction event id -> (buffer id, message id,
    /// emoji, is_me) - the reverse of matrix_own_reactions, but for
    /// *every* reaction seen (any sender), needed because an incoming
    /// `m.room.redaction` only names the event id being redacted; without
    /// this there'd be no way to know which reaction to decrement (or
    /// which message to delete, if the redacted id isn't in this map at
    /// all - see handle_timeline_event's redaction branch).
    matrix_reaction_targets: Mutex<HashMap<String, (String, String, String, bool)>>,
    /// Matrix-specific: verification id (generated - see backend/matrix/
    /// verification.rs's start_verification) -> in-progress SAS
    /// verification state. One at a time per account for v1 - a second
    /// start_verification replaces/cancels any existing entry for that
    /// account (see start_verification's own doc comment). Keyed by a
    /// generated id rather than matrix-sdk-crypto's own flow_id so the
    /// frontend never needs to know that shape, same reasoning as
    /// matrixLoginStatus's loginId.
    matrix_verifications: Mutex<HashMap<String, crate::backend::matrix::verification::ActiveVerification>>,
    /// Matrix-specific: account ids that currently have server-side key
    /// backup enabled - a synchronous cache of what's really tracked
    /// inside each account's `BackupMachine` (async-only to query), same
    /// pattern as matrix_encrypted_rooms above. Set on setup/restore, and
    /// re-derived once at connect time in run_sync from whatever's
    /// already persisted in the crypto store (a fresh process has to
    /// re-activate the backup key in the machine either way - see
    /// backup.rs's module doc - so re-deriving this alongside that is
    /// free).
    matrix_backup_enabled: Mutex<HashSet<String>>,
    /// Matrix-specific: (account id, user id) -> that user's avatar,
    /// already resolved+cached to a local `file://` path (see backend/
    /// matrix/roomstate.rs) - never a raw `mxc://` URI, same reasoning as
    /// matrix_room_names caching a *derived* name rather than raw state
    /// events. Global per user rather than per-room (a per-room profile
    /// override is a real but rare Matrix feature, not worth the extra
    /// key component for v1 - same "good enough, not exhaustive" call
    /// already made elsewhere in this backend, e.g. media_mxc_uri's own
    /// doc comment on encrypted media).
    matrix_member_avatars: Mutex<HashMap<(String, String), String>>,
    /// Matrix-specific: (account id, room id) -> that room's avatar,
    /// resolved+cached the same way. Applied to the room's buffer (see
    /// Buffer::avatar_url) and re-broadcast via bufferListChange -
    /// there's no separate "room avatar changed" event, matching how a
    /// buffer's own lastActivityTs bump already just re-sends the whole
    /// buffer object rather than a bespoke event per changed field.
    matrix_room_avatars: Mutex<HashMap<(String, String), String>>,
    /// Matrix-specific: (account id, room id) -> that room's current
    /// `m.room.power_levels` content, verbatim (not parsed into a struct -
    /// see backend/matrix/moderation.rs's own doc comment on why raw
    /// serde_json::Value manipulation matches this backend's existing
    /// convention). Used both to gate which moderation actions the
    /// frontend offers and by moderation.rs's own permission-check
    /// helpers - though the homeserver is the real authority either way;
    /// this is advisory/UI-gating only, an unauthorized action would be
    /// rejected server-side regardless of what's cached here.
    matrix_power_levels: Mutex<HashMap<(String, String), Value>>,
    /// Matrix-specific: (account id, room id) -> {user id -> display name}
    /// for every member currently *joined* to that room (see roomstate.rs's
    /// m.room.member handling - a leave/ban removes the entry entirely,
    /// unlike matrix_member_avatars above which deliberately keeps stale
    /// data around). This is the actual roster userlist.rs builds from,
    /// combined with matrix_power_levels (for sort order) and
    /// matrix_presence (for the online/offline split) - see emit_matrix_
    /// presence.
    matrix_room_members: Mutex<HashMap<(String, String), HashMap<String, String>>>,
    /// Matrix-specific: (account id, user id) -> whether /sync's top-level
    /// presence.events last reported them as "online". Absent (never seen
    /// a presence event for them) is treated as offline - Matrix has no
    /// bulk "get current presence for all these users" endpoint, and
    /// per-user polling would be expensive/often rate-limited, so this is
    /// purely reactive to whatever presence.events actually delivers,
    /// same honest-limitation call as most third-party Matrix clients
    /// make. Global per user, not per-room - presence isn't a per-room
    /// concept in the C-S API.
    matrix_presence: Mutex<HashMap<(String, String), bool>>,
}

impl Runtime {
    pub fn new() -> Self {
        Self {
            conn_states: Mutex::new(HashMap::new()),
            irc_handles: Mutex::new(HashMap::new()),
            task_handles: Mutex::new(HashMap::new()),
            last_connect_attempt: Mutex::new(HashMap::new()),
            connect_generation: Mutex::new(HashMap::new()),
            wants_connected: Mutex::new(std::collections::HashSet::new()),
            matrix_invites: Mutex::new(HashMap::new()),
            discord_gated: Mutex::new(std::collections::HashSet::new()),
            discord_resync: Mutex::new(HashMap::new()),
            discord_own_roles: Mutex::new(HashMap::new()),
            matrix_back_tokens: Mutex::new(HashMap::new()),
            discord_member_windows: Mutex::new(HashMap::new()),
            buffers: Mutex::new(HashMap::new()),
            presence: Mutex::new(HashMap::new()),
            own_identity: Mutex::new(HashMap::new()),
            discord_channels: Mutex::new(HashMap::new()),
            ringing_calls: Mutex::new(HashMap::new()),
            buffer_groups: Mutex::new(HashMap::new()),
            discord_guild_id: Mutex::new(HashMap::new()),
            discord_history_inflight: Mutex::new(HashSet::new()),
            discord_buffer_emojis: Mutex::new(HashMap::new()),
            discord_friends: Mutex::new(HashMap::new()),
            sockchat_senders: Mutex::new(HashMap::new()),
            matrix_rooms: Mutex::new(HashMap::new()),
            matrix_room_names: Mutex::new(HashMap::new()),
            matrix_space_parents: Mutex::new(HashMap::new()),
            account_status: Mutex::new(HashMap::new()),
            discord_member_list_targets: Mutex::new(HashMap::new()),
            discord_voice_channels: Mutex::new(HashMap::new()),
            discord_voice_states: Mutex::new(HashMap::new()),
            discord_voice_names: Mutex::new(HashMap::new()),
            discord_voice_avatars: Mutex::new(HashMap::new()),
            discord_voice_self: Mutex::new(HashMap::new()),
            dcc_transfers: Mutex::new(Vec::new()),
            irc_transports: Mutex::new(HashMap::new()),
            discord_gateway_senders: Mutex::new(HashMap::new()),
            matrix_machines: Mutex::new(HashMap::new()),
            matrix_encrypted_rooms: Mutex::new(HashSet::new()),
            matrix_own_reactions: Mutex::new(HashMap::new()),
            matrix_reaction_targets: Mutex::new(HashMap::new()),
            matrix_verifications: Mutex::new(HashMap::new()),
            matrix_backup_enabled: Mutex::new(HashSet::new()),
            matrix_member_avatars: Mutex::new(HashMap::new()),
            matrix_room_avatars: Mutex::new(HashMap::new()),
            matrix_power_levels: Mutex::new(HashMap::new()),
            matrix_room_members: Mutex::new(HashMap::new()),
            matrix_presence: Mutex::new(HashMap::new()),
        }
    }

    /// Returns true if this call acquired the guard (no fetch for this
    /// buffer was already in flight) - the caller must pair a `true`
    /// result with a later finish_discord_history_fetch() call.
    pub fn try_start_discord_history_fetch(&self, buffer_id: &str) -> bool {
        self.discord_history_inflight.lock().unwrap().insert(buffer_id.to_string())
    }

    pub fn finish_discord_history_fetch(&self, buffer_id: &str) {
        self.discord_history_inflight.lock().unwrap().remove(buffer_id);
    }

    pub fn list_accounts(&self, state: &AppState) -> Vec<Account> {
        let mut out = self.build_accounts(state);
        // The constructors have no view of live state, so the status the
        // runtime is actually holding is filled in here.
        for account in &mut out {
            account.status = self.account_status(&account.id);
        }
        out
    }

    fn build_accounts(&self, state: &AppState) -> Vec<Account> {
        let conn_states = self.conn_states.lock().unwrap();
        let mut out: Vec<Account> = state
            .accounts
            .all_irc()
            .iter()
            .map(|cfg| {
                let id = cfg.account_id();
                let conn = conn_states.get(&id).map(|s| s.as_str()).unwrap_or("disconnected");
                irc_account_to_json(cfg, conn)
            })
            .collect();
        out.extend(state.accounts.all_discord().iter().map(|cfg| {
            let id = cfg.account_id();
            let conn = conn_states.get(&id).map(|s| s.as_str()).unwrap_or("disconnected");
            crate::accounts::discord_account_to_json(cfg, conn)
        }));
        out.extend(state.accounts.all_sockchat().iter().map(|cfg| {
            let id = cfg.account_id();
            let conn = conn_states.get(&id).map(|s| s.as_str()).unwrap_or("disconnected");
            crate::accounts::sockchat_account_to_json(cfg, conn)
        }));
        out.extend(state.accounts.all_matrix().iter().map(|cfg| {
            let id = cfg.account_id();
            let conn = conn_states.get(&id).map(|s| s.as_str()).unwrap_or("disconnected");
            crate::accounts::matrix_account_to_json(cfg, conn, self.has_matrix_backup(&id))
        }));
        out
    }

    pub fn list_buffers(&self) -> Vec<Buffer> {
        self.buffers.lock().unwrap().values().cloned().collect()
    }

    /// What kind of conversation a buffer is, for a caller writing history
    /// into it directly.
    ///
    /// Falls back to "channel" rather than to nothing: history is only ever
    /// backfilled into a conversation that is open, so an absent answer means
    /// something has gone wrong upstream, and the fallback is the kind that
    /// makes a mention in it findable rather than invisible.
    pub fn buffer_kind_of(&self, buffer_id: &str) -> String {
        self.get_buffer(buffer_id).map(|b| b.kind).unwrap_or_else(|| "channel".to_string())
    }

    pub fn get_buffer(&self, buffer_id: &str) -> Option<Buffer> {
        self.buffers.lock().unwrap().get(buffer_id).cloned()
    }

    /// Rail entries, ordered the way a frontend should draw them: by the
    /// backend's own position, then name, so the order is stable across
    /// restarts rather than whatever order the gateway happened to send.
    pub fn list_buffer_groups(&self) -> Vec<crate::model::BufferGroup> {
        let mut out: Vec<_> = self.buffer_groups.lock().unwrap().values().cloned().collect();
        out.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.name.cmp(&b.name)));
        out
    }

    /// Registers or updates a rail entry, broadcasting only when something
    /// actually changed - a reconnect re-registers every guild it sees, and
    /// re-broadcasting identical entries would churn every connected frontend.
    pub fn upsert_buffer_group(&self, state: &AppState, group: crate::model::BufferGroup) {
        {
            let mut groups = self.buffer_groups.lock().unwrap();
            if groups.get(&group.id) == Some(&group) {
                return;
            }
            groups.insert(group.id.clone(), group.clone());
        }
        state.events.emit("bufferGroupChange", serde_json::to_value(&group).unwrap());
    }

    /// Lifts the "you cannot speak here yet" flag on a guild.
    ///
    /// Called after agreeing to a server's rules. Discord announces the
    /// change with GUILD_MEMBER_UPDATE, which this client does not listen
    /// for, so the group is corrected directly - otherwise the notice would
    /// sit there claiming a gate that is no longer closed.
    pub fn clear_discord_guild_pending(&self, state: &AppState, account_id: &str, guild_id: &str) {
        let group_id = crate::backend::discord::guild_group_id(account_id, guild_id);
        let updated = {
            let mut groups = self.buffer_groups.lock().unwrap();
            match groups.get_mut(&group_id) {
                Some(group) if group.pending => {
                    group.pending = false;
                    Some(group.clone())
                }
                _ => None,
            }
        };
        if let Some(group) = updated {
            state.events.emit("bufferGroupChange", serde_json::to_value(&group).unwrap());
        }
    }

    /// Files a buffer under a rail entry. Idempotent, and re-broadcasts the
    /// buffer so a frontend that already listed it moves it into place.
    pub fn set_buffer_group(&self, state: &AppState, buffer_id: &str, group_id: &str) {
        let updated = {
            let mut buffers = self.buffers.lock().unwrap();
            match buffers.get_mut(buffer_id) {
                Some(b) if b.group_id.as_deref() != Some(group_id) => {
                    b.group_id = Some(group_id.to_string());
                    Some(b.clone())
                }
                _ => None,
            }
        };
        if let Some(b) = updated {
            state.events.emit("bufferListChange", serde_json::to_value(&b).unwrap());
        }
    }

    /// Drops every rail entry for an account - for when it is removed, so its
    /// guilds don't sit in the rail forever with no buffers under them.
    pub fn clear_buffer_groups_for_account(&self, account_id: &str) {
        self.buffer_groups.lock().unwrap().retain(|_, g| g.account_id != account_id);
    }

    /// Re-reads a buffer's last-activity timestamp from persisted
    /// scrollback and re-broadcasts it - used after a Discord history
    /// backfill/pagination fetch inserts messages directly into the store
    /// (bypassing record_message's own live-bump), so a buffer whose only
    /// activity is freshly-fetched history still sorts correctly without
    /// waiting for the next daemon restart to re-seed it.
    pub fn refresh_buffer_activity(&self, state: &AppState, buffer_id: &str) {
        let ts = state.store.last_activity(buffer_id).unwrap_or(0);
        let updated = {
            let mut buffers = self.buffers.lock().unwrap();
            match buffers.get_mut(buffer_id) {
                Some(b) if b.last_activity_ts != ts => {
                    b.last_activity_ts = ts;
                    Some(b.clone())
                }
                _ => None,
            }
        };
        if let Some(b) = updated {
            state.events.emit("bufferListChange", serde_json::to_value(&b).unwrap());
        }
    }

    pub fn is_server_buffer(&self, buffer_id: &str) -> bool {
        self.buffers
            .lock()
            .unwrap()
            .get(buffer_id)
            .map(|b| b.kind == "server")
            .unwrap_or(false)
    }

    /// Called when an account is deleted - without this, its channels/DMs/
    /// server buffer would just linger in the registry (nothing else ever
    /// clears them; disconnecting alone doesn't) until the next full
    /// daemon restart, showing up as stale entries in listBuffers.
    pub fn remove_buffers_for_account(&self, state: &AppState, account_id: &str) {
        let ids: Vec<String> = {
            let buffers = self.buffers.lock().unwrap();
            buffers.values().filter(|b| b.account_id == account_id).map(|b| b.id.clone()).collect()
        };
        for id in ids {
            self.remove_buffer(state, &id);
        }
    }

    pub fn remove_buffer(&self, state: &AppState, buffer_id: &str) {
        self.buffers.lock().unwrap().remove(buffer_id);
        state.events.emit("bufferListChange", json!({ "id": buffer_id, "removed": true }));
    }

    /// Takes a rail entry and everything under it out of the client's view.
    ///
    /// For after leaving a guild or space for real: the server will stop
    /// mentioning it, but nothing already registered goes away on its own, so
    /// without this the entry sits in the column until the next restart.
    pub fn remove_buffer_group(&self, state: &AppState, group_id: &str) {
        let doomed: Vec<String> = self
            .buffers
            .lock()
            .unwrap()
            .values()
            .filter(|b| b.group_id.as_deref() == Some(group_id))
            .map(|b| b.id.clone())
            .collect();
        for id in doomed {
            self.remove_buffer(state, &id);
        }
        self.buffer_groups.lock().unwrap().remove(group_id);
        state.events.emit("bufferGroupChange", json!({ "id": group_id, "removed": true }));
    }

    /// Creates the buffer if it doesn't already exist and emits
    /// bufferListChange; no-ops (does not re-emit) if already present.
    pub fn ensure_buffer(&self, state: &AppState, account_id: &str, name: &str, kind: &str) -> Buffer {
        let id = model::buffer_id(account_id, name);
        let mut buffers = self.buffers.lock().unwrap();
        if let Some(existing) = buffers.get(&id) {
            return existing.clone();
        }
        let last_activity_ts = state.store.last_activity(&id).unwrap_or(0);
        // Defaults to the account's own rail entry; a backend with real
        // grouping moves it with set_buffer_group once it knows where it goes.
        let buffer = Buffer {
            id: id.clone(),
            account_id: account_id.to_string(),
            kind: kind.to_string(),
            name: name.to_string(),
            last_activity_ts,
            avatar_url: None,
            // Filled in by whichever backend knows: a Discord guild files its
            // channels under the server's own headings, and everything else
            // has none.
            category: None,
            position: 0,
            encrypted: None,
            group_id: Some(model::account_group_id(account_id)),
            remote_id: None,
        };
        buffers.insert(id, buffer.clone());
        state.events.emit("bufferListChange", serde_json::to_value(&buffer).unwrap());
        buffer
    }

    pub fn set_conn_state(&self, state: &AppState, account_id: &str, conn: ConnState, error: Option<&str>) {
        self.conn_states.lock().unwrap().insert(account_id.to_string(), conn.clone());
        let mut data = json!({ "accountId": account_id, "state": conn.as_str() });
        if let Some(e) = error {
            data["error"] = json!(e);
        }
        state.events.emit("connectionState", data);
    }

    /// A live progress update *within* the "connecting" state - DNS/TCP/TLS
    /// connect, SASL, waiting for the server's welcome, NickServ identify,
    /// etc. are each individually timeout-bounded (15-20s) but a real
    /// connection attempt can still visibly sit at "connecting" for the
    /// better part of a minute with no indication of what it's actually
    /// doing or how far it got - this exists so the account panel can show
    /// that live instead of a static "connecting" the whole time. Doesn't
    /// touch the stored ConnState (still "connecting" throughout), just
    /// re-broadcasts it with a human-readable `detail` string attached.
    pub fn report_progress(&self, state: &AppState, account_id: &str, detail: &str) {
        // Guards against exactly the bug this already caused once: calling
        // this after the real ConnState already advanced past Connecting
        // (e.g. NickServ identify, which runs after the connected
        // transition) would otherwise re-stamp the event with
        // state:"connecting" and incorrectly flip an already-connected
        // account's displayed state back, with nothing to ever correct it
        // afterward.
        if self.conn_states.lock().unwrap().get(account_id) != Some(&ConnState::Connecting) {
            return;
        }
        state.events.emit("connectionState", json!({ "accountId": account_id, "state": "connecting", "detail": detail }));
    }

    pub fn insert_irc_handle(&self, account_id: &str, handle: IrcHandle) {
        self.irc_handles.lock().unwrap().insert(account_id.to_string(), handle);
    }

    pub fn remove_irc_handle(&self, account_id: &str) {
        self.irc_handles.lock().unwrap().remove(account_id);
    }

    pub fn insert_task_handle(&self, account_id: &str, handle: tokio::task::AbortHandle) {
        self.task_handles.lock().unwrap().insert(account_id.to_string(), handle);
    }

    /// Records a connection attempt for this account and returns how much
    /// longer the caller should wait before actually starting it, if the
    /// previous attempt was too recent - see last_connect_attempt's doc
    /// comment for why this exists. Always records the (possibly delayed)
    /// attempt's timestamp as "now" up front, not after the wait, so a
    /// burst of rapid clicks spaces attempts out by min_interval each
    /// rather than all measuring against the same original timestamp and
    /// firing together once it elapses.
    pub fn throttle_connect_attempt(&self, account_id: &str, min_interval: std::time::Duration) -> std::time::Duration {
        let mut attempts = self.last_connect_attempt.lock().unwrap();
        let now = std::time::Instant::now();
        let delay = match attempts.get(account_id) {
            Some(last) => min_interval.saturating_sub(now.duration_since(*last)),
            None => std::time::Duration::ZERO,
        };
        attempts.insert(account_id.to_string(), now + delay);
        delay
    }

    /// Claims a fresh generation number for a new connection attempt -
    /// call once at the very start of spawn(), before doing anything
    /// else. The returned number must be threaded through to
    /// finish_connection() at that same attempt's end.
    pub fn next_generation(&self, account_id: &str) -> u64 {
        let mut gens = self.connect_generation.lock().unwrap();
        let next = gens.get(account_id).copied().unwrap_or(0).wrapping_add(1);
        gens.insert(account_id.to_string(), next);
        next
    }

    /// The end-of-task cleanup for a connection attempt - sets the final
    /// ConnState and removes this account's irc/task handles, but *only*
    /// if `generation` is still the current one for this account (see
    /// connect_generation's doc comment). A no-op otherwise: some later
    /// spawn() has already superseded this attempt, and that attempt owns
    /// the account's state now, not this stale one.
    /// Whether this attempt is still the one that owns the account.
    ///
    /// Same guard finish_connection applies, exposed so a retry loop can
    /// stand down when a newer spawn has taken over rather than reconnecting
    /// alongside it.
    pub fn is_current_generation(&self, account_id: &str, generation: u64) -> bool {
        self.connect_generation.lock().unwrap().get(account_id) == Some(&generation)
    }

    pub fn finish_connection(&self, state: &AppState, account_id: &str, generation: u64, new_state: ConnState, error: Option<&str>) {
        if self.connect_generation.lock().unwrap().get(account_id) != Some(&generation) {
            return;
        }
        self.set_conn_state(state, account_id, new_state, error);
        self.remove_irc_handle(account_id);
        self.remove_task_handle(account_id);
    }

    pub fn remove_task_handle(&self, account_id: &str) {
        self.task_handles.lock().unwrap().remove(account_id);
    }

    /// Records that this account should be connected, and should come back
    /// on its own if the link drops.
    pub fn set_wants_connected(&self, account_id: &str, wants: bool) {
        let mut w = self.wants_connected.lock().unwrap();
        if wants {
            w.insert(account_id.to_string());
        } else {
            w.remove(account_id);
        }
    }

    pub fn wants_connected(&self, account_id: &str) -> bool {
        self.wants_connected.lock().unwrap().contains(account_id)
    }

    /// Replaces this account's pending invitations, and says whether that
    /// changed anything.
    ///
    /// Every sync carries the full invite set, so this is a replace rather
    /// than a merge - an invite answered from another client simply stops
    /// being listed, and that is how it disappears here too. The changed
    /// answer is what keeps an event from being emitted on every sync tick
    /// for a set nobody has touched.
    pub fn set_matrix_invites(&self, account_id: &str, invites: Vec<serde_json::Value>) -> bool {
        let mut all = self.matrix_invites.lock().unwrap();
        match all.get(account_id) {
            Some(existing) if *existing == invites => false,
            _ => {
                all.insert(account_id.to_string(), invites);
                true
            }
        }
    }

    pub fn matrix_invites(&self, account_id: &str) -> Vec<serde_json::Value> {
        self.matrix_invites.lock().unwrap().get(account_id).cloned().unwrap_or_default()
    }

    /// Forcibly tears down any previous connection attempt/session for this
    /// account before a new one starts - called at the very top of
    /// backend::irc::spawn(), before next_generation(). Without this, an old
    /// task that's still mid-graceful-QUIT (see disconnect()) or still stuck
    /// connecting stayed alive and fully registered (its Sender still
    /// sitting in irc_handles, its AbortHandle still in task_handles) at the
    /// same time a new spawn() started a second, independent connection
    /// attempt under the same nick - two live connections to the same IRC
    /// server as the same nick is exactly the kind of thing a real network's
    /// anti-clone/anti-flood logic can silently wedge (confirmed live: this,
    /// not a plain slow network, was the actual cause behind accounts
    /// getting permanently stuck at "connecting" after a fast disconnect-
    /// then-reconnect - the generation guard on finish_connection() only
    /// ever protected the *cleanup* from a stale task stomping a newer one's
    /// state, it never stopped the two tasks from racing each other in the
    /// first place). Aborting here also fixes disconnect() picking the wrong
    /// branch: it decides whether to send a graceful QUIT purely by whether
    /// *any* irc_handle exists, including a stale one left by a superseded
    /// generation, which could make it try to gracefully quit a connection
    /// that isn't actually the live one, instead of aborting the task that
    /// really needs cancelling.
    pub fn reset_connection(&self, account_id: &str) {
        if let Some(handle) = self.irc_handles.lock().unwrap().remove(account_id) {
            let _ = handle.sender.send_quit("");
        }
        if let Some(handle) = self.task_handles.lock().unwrap().remove(account_id) {
            handle.abort();
        }
    }

    pub fn discord_member_window(&self, buffer_id: &str) -> Vec<serde_json::Value> {
        self.discord_member_windows.lock().unwrap().get(buffer_id).cloned().unwrap_or_default()
    }

    pub fn set_discord_member_window(&self, buffer_id: &str, members: Vec<serde_json::Value>) {
        self.discord_member_windows.lock().unwrap().insert(buffer_id.to_string(), members);
    }

    pub fn set_presence(&self, buffer_id: &str, members: serde_json::Value) {
        self.presence.lock().unwrap().insert(buffer_id.to_string(), members);
    }

    pub fn get_presence(&self, buffer_id: &str) -> Option<serde_json::Value> {
        self.presence.lock().unwrap().get(buffer_id).cloned()
    }

    pub fn set_discord_buffer_emojis(&self, buffer_id: &str, emojis: Vec<serde_json::Value>) {
        self.discord_buffer_emojis.lock().unwrap().insert(buffer_id.to_string(), emojis);
    }

    pub fn get_discord_buffer_emojis(&self, buffer_id: &str) -> Vec<serde_json::Value> {
        self.discord_buffer_emojis.lock().unwrap().get(buffer_id).cloned().unwrap_or_default()
    }

    pub fn set_own_identity(&self, account_id: &str, name: &str) {
        self.own_identity.lock().unwrap().insert(account_id.to_string(), name.to_string());
    }

    pub fn own_identity(&self, account_id: &str) -> Option<String> {
        self.own_identity.lock().unwrap().get(account_id).cloned()
    }

    /// Remembers which Discord channel a buffer is showing.
    ///
    /// Also published on the buffer itself as `remoteId`. Both, rather than
    /// one: the map is what the gateway dispatches are matched through and is
    /// keyed the other way round, while the field is what a frontend needs to
    /// turn a `<#id>` in a message body into the channel's name.
    pub fn set_discord_channel(&self, state: &AppState, buffer_id: &str, channel_id: &str) {
        self.discord_channels.lock().unwrap().insert(buffer_id.to_string(), channel_id.to_string());
        let updated = {
            let mut buffers = self.buffers.lock().unwrap();
            match buffers.get_mut(buffer_id) {
                Some(b) if b.remote_id.as_deref() != Some(channel_id) => {
                    b.remote_id = Some(channel_id.to_string());
                    Some(b.clone())
                }
                _ => None,
            }
        };
        // Only on a real change: this is called again for every channel on
        // every reconnect, and a frontend redrawing its whole list each time
        // would flicker.
        if let Some(b) = updated {
            state.events.emit("bufferListChange", serde_json::to_value(&b).unwrap());
        }
    }

    /// The buffer showing a Discord channel, if one exists - the reverse of
    /// discord_channels, needed because gateway dispatches are keyed by
    /// channel id while everything else here is keyed by buffer.
    /// (account, guild) -> the buffer whose member list was last asked for.
    ///
    /// GUILD_MEMBER_LIST_UPDATE identifies its list by guild and a
    /// permissions-derived id rather than by channel, so the reply cannot be
    /// matched back to a channel on its own - this remembers who asked.
    pub fn set_discord_member_list_target(&self, account_id: &str, guild_id: &str, buffer_id: &str) {
        self.discord_member_list_targets
            .lock()
            .unwrap()
            .insert((account_id.to_string(), guild_id.to_string()), buffer_id.to_string());
    }

    pub fn discord_member_list_target(&self, account_id: &str, guild_id: &str) -> Option<String> {
        self.discord_member_list_targets.lock().unwrap().get(&(account_id.to_string(), guild_id.to_string())).cloned()
    }

    pub fn set_discord_voice_channels(&self, account_id: &str, guild_id: &str, channels: Vec<(String, String, u64)>) {
        self.discord_voice_channels.lock().unwrap().insert((account_id.to_string(), guild_id.to_string()), channels);
    }

    pub fn discord_voice_channels(&self, account_id: &str, guild_id: &str) -> Vec<(String, String, u64)> {
        self.discord_voice_channels.lock().unwrap().get(&(account_id.to_string(), guild_id.to_string())).cloned().unwrap_or_default()
    }

    /// Records where someone is, or that they left. Returns the channel they
    /// were in before, so a caller can tell a move from an arrival.
    pub fn set_discord_voice_state(&self, account_id: &str, user_id: &str, channel_id: Option<&str>, name: Option<&str>) -> Option<String> {
        self.set_discord_voice_presence(account_id, user_id, channel_id, name, None, VoiceFlags::default())
    }

    /// The same, carrying what they are doing in there.
    ///
    /// Discord puts these on every voice state it sends, and they are the only
    /// notice anyone gets that a screen is being shared or a camera is on -
    /// there is no separate dispatch for it.
    pub fn set_discord_voice_presence(
        &self,
        account_id: &str,
        user_id: &str,
        channel_id: Option<&str>,
        name: Option<&str>,
        avatar_url: Option<&str>,
        flags: VoiceFlags,
    ) -> Option<String> {
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            self.remember_discord_name(account_id, user_id, name);
        }
        if let Some(url) = avatar_url.filter(|u| !u.is_empty()) {
            self.remember_discord_avatar(account_id, user_id, url);
        }
        let mut states = self.discord_voice_states.lock().unwrap();
        let key = (account_id.to_string(), user_id.to_string());
        match channel_id {
            Some(c) => states
                .insert(key, VoicePresence { channel_id: c.to_string(), flags })
                .map(|p| p.channel_id),
            None => states.remove(&key).map(|p| p.channel_id),
        }
    }

    /// Everyone currently in a voice channel, excluding `except`.
    pub fn discord_voice_occupants(&self, account_id: &str, channel_id: &str, except: &str) -> Vec<String> {
        self.discord_voice_states
            .lock()
            .unwrap()
            .iter()
            .filter(|((a, u), p)| a == account_id && p.channel_id == channel_id && u != except)
            .map(|((_, u), _)| u.clone())
            .collect()
    }

    /// Records what to call someone, from wherever their name was seen.
    ///
    /// Kept after they leave a channel: names change rarely, and having one
    /// ready matters more than the handful of bytes it costs.
    pub fn remember_discord_name(&self, account_id: &str, user_id: &str, name: &str) {
        if name.is_empty() {
            return;
        }
        self.discord_voice_names
            .lock()
            .unwrap()
            .insert((account_id.to_string(), user_id.to_string()), name.to_string());
    }

    /// Records somebody's picture, from wherever it was seen.
    ///
    /// Kept like the name is, and for a better reason: a voice state in a
    /// direct call carries no member object at all, so the only chance to
    /// learn a face there is a message they sent earlier.
    pub fn remember_discord_avatar(&self, account_id: &str, user_id: &str, url: &str) {
        if url.is_empty() {
            return;
        }
        self.discord_voice_avatars
            .lock()
            .unwrap()
            .insert((account_id.to_string(), user_id.to_string()), url.to_string());
    }

    pub fn discord_known_avatar(&self, account_id: &str, user_id: &str) -> Option<String> {
        self.discord_voice_avatars
            .lock()
            .unwrap()
            .get(&(account_id.to_string(), user_id.to_string()))
            .cloned()
    }

    /// What this account has previously learned to call somebody.
    ///
    /// A typing notice from a DM carries no member object to read a name
    /// out of, and "Someone is typing" is worse than the name we already
    /// saw on their last message.
    pub fn discord_known_name(&self, account_id: &str, user_id: &str) -> Option<String> {
        self.discord_voice_names
            .lock()
            .unwrap()
            .get(&(account_id.to_string(), user_id.to_string()))
            .cloned()
    }

    /// Everyone in a voice channel, as (user id, what to call them).
    ///
    /// Nobody is excluded here: a channel list has to show you your own
    /// presence, which is how you can tell you are in a call at all.
    pub fn discord_voice_members(&self, account_id: &str, channel_id: &str) -> Vec<(String, String)> {
        self.discord_voice_roster(account_id, channel_id)
            .into_iter()
            .map(|m| (m.user_id, m.name))
            .collect()
    }

    /// The same, with what each of them is doing.
    /// Records a new offer, unless this person already has too many waiting.
    ///
    /// Returns None when the offer was dropped, so the caller can say so in
    /// the log rather than the offer vanishing without explanation.
    pub fn add_dcc_offer(&self, transfer: DccTransfer) -> Option<DccTransfer> {
        let mut list = self.dcc_transfers.lock().unwrap();
        let waiting = list
            .iter()
            .filter(|t| t.account_id == transfer.account_id && t.from == transfer.from && t.state == DccState::Offered)
            .count();
        if waiting >= DCC_PENDING_PER_NICK {
            return None;
        }
        list.insert(0, transfer.clone());
        list.truncate(DCC_KEEP);
        Some(transfer)
    }

    pub fn set_irc_transport(&self, account_id: &str, transport: Option<crate::net::tor::Transport>) {
        let mut map = self.irc_transports.lock().unwrap();
        match transport {
            Some(t) => {
                map.insert(account_id.to_string(), t);
            }
            None => {
                map.remove(account_id);
            }
        }
    }

    pub fn irc_transport(&self, account_id: &str) -> Option<crate::net::tor::Transport> {
        self.irc_transports.lock().unwrap().get(account_id).cloned()
    }

    /// Puts a transfer back, without the checks a new offer goes through.
    ///
    /// For history read off disk: it has already happened, so there is nothing
    /// to rate-limit and nobody to refuse.
    pub fn push_dcc_transfer(&self, transfer: DccTransfer) {
        let mut list = self.dcc_transfers.lock().unwrap();
        if list.iter().any(|t| t.id == transfer.id) {
            return;
        }
        list.push(transfer);
        list.sort_by(|a, b| b.started_at.cmp(&a.started_at));
        list.truncate(DCC_KEEP);
    }

    pub fn dcc_transfers(&self) -> Vec<DccTransfer> {
        self.dcc_transfers.lock().unwrap().clone()
    }

    pub fn dcc_transfer(&self, id: &str) -> Option<DccTransfer> {
        self.dcc_transfers.lock().unwrap().iter().find(|t| t.id == id).cloned()
    }

    /// How many are offered or running, against the concurrency limit.
    pub fn dcc_active_count(&self) -> usize {
        self.dcc_transfers.lock().unwrap().iter().filter(|t| t.state.active()).count()
    }

    /// Applies `edit` to one transfer and hands back what it became.
    pub fn update_dcc(&self, id: &str, edit: impl FnOnce(&mut DccTransfer)) -> Option<DccTransfer> {
        let mut list = self.dcc_transfers.lock().unwrap();
        let t = list.iter_mut().find(|t| t.id == id)?;
        edit(t);
        Some(t.clone())
    }

    /// Stops everything belonging to an account that is going away.
    ///
    /// A transfer outliving the connection that carried its offer would be a
    /// socket nobody is watching any more.
    pub fn cancel_dcc_for_account(&self, account_id: &str) {
        for t in self.dcc_transfers.lock().unwrap().iter_mut() {
            if t.account_id == account_id && t.state.active() {
                t.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                if t.state == DccState::Offered {
                    t.state = DccState::Declined;
                    t.error = Some("disconnected before it was answered".to_string());
                }
            }
        }
    }

    pub fn discord_voice_roster(&self, account_id: &str, channel_id: &str) -> Vec<VoiceRosterEntry> {
        let names = self.discord_voice_names.lock().unwrap();
        let avatars = self.discord_voice_avatars.lock().unwrap();
        let mut out: Vec<VoiceRosterEntry> = self
            .discord_voice_states
            .lock()
            .unwrap()
            .iter()
            .filter(|((a, _), p)| a == account_id && p.channel_id == channel_id)
            .map(|((a, u), p)| {
                let key = (a.clone(), u.clone());
                VoiceRosterEntry {
                    user_id: u.clone(),
                    name: names.get(&key).cloned().unwrap_or_else(|| u.clone()),
                    avatar_url: avatars.get(&key).cloned(),
                    flags: p.flags,
                }
            })
            .collect();
        // Stable order, so a list does not reshuffle itself on every update.
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        out
    }

    /// Which guild a voice channel belongs to, if it is one we know about.
    pub fn discord_guild_of_voice_channel(&self, account_id: &str, channel_id: &str) -> Option<String> {
        self.discord_voice_channels
            .lock()
            .unwrap()
            .iter()
            .find(|((a, _), channels)| a == account_id && channels.iter().any(|(id, _, _)| id == channel_id))
            .map(|((_, g), _)| g.clone())
    }

    pub fn set_discord_voice_self(&self, account_id: &str, channel_id: Option<&str>) {
        let mut own = self.discord_voice_self.lock().unwrap();
        match channel_id {
            Some(c) => own.insert(account_id.to_string(), c.to_string()),
            None => own.remove(account_id),
        };
    }

    pub fn discord_voice_self(&self, account_id: &str) -> Option<String> {
        self.discord_voice_self.lock().unwrap().get(account_id).cloned()
    }

    /// Records that a conversation is ringing, or has stopped.
    ///
    /// Returns whether that was news. Discord repeats CALL_UPDATE for every
    /// change to a call - somebody muting, somebody's video - and each one
    /// carries the same ringing list, so without this a single call would
    /// re-announce itself several times and a frontend would restart its
    /// ringtone on each.
    pub fn set_ringing(&self, buffer_id: &str, account_id: &str, channel_id: &str, ringing: bool) -> bool {
        let mut calls = self.ringing_calls.lock().unwrap();
        if ringing {
            calls
                .insert(buffer_id.to_string(), (account_id.to_string(), channel_id.to_string()))
                .is_none()
        } else {
            calls.remove(buffer_id).is_some()
        }
    }

    /// Everything ringing right now, for a frontend that has just connected.
    pub fn ringing_calls(&self) -> Vec<(String, String, String)> {
        self.ringing_calls
            .lock()
            .unwrap()
            .iter()
            .map(|(buffer, (account, channel))| (buffer.clone(), account.clone(), channel.clone()))
            .collect()
    }

    pub fn discord_buffer_for_channel(&self, channel_id: &str) -> Option<String> {
        self.discord_channels.lock().unwrap().iter().find(|(_, c)| c.as_str() == channel_id).map(|(b, _)| b.clone())
    }

    pub fn get_discord_channel(&self, buffer_id: &str) -> Option<String> {
        self.discord_channels.lock().unwrap().get(buffer_id).cloned()
    }

    pub fn set_discord_guild(&self, buffer_id: &str, guild_id: &str) {
        self.discord_guild_id.lock().unwrap().insert(buffer_id.to_string(), guild_id.to_string());
    }

    pub fn get_discord_guild(&self, buffer_id: &str) -> Option<String> {
        self.discord_guild_id.lock().unwrap().get(buffer_id).cloned()
    }

    /// Every buffer belonging to a guild.
    ///
    /// For a guild that has gone away: leaving one from another client, or
    /// it being deleted, has to take its channels with it rather than
    /// leaving them sitting there looking joinable.
    pub fn discord_buffers_in_guild(&self, guild_id: &str) -> Vec<String> {
        self.discord_guild_id
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, g)| g.as_str() == guild_id)
            .map(|(buffer_id, _)| buffer_id.clone())
            .collect()
    }

    /// Replaces this account's whole friends snapshot - called once from
    /// READY (see backend/discord.rs), never incrementally, since that's
    /// the only point a full, authoritative relationships list exists.
    /// Live changes after that are per-user PRESENCE_UPDATE patches via
    /// update_discord_presence below, not further full replaces.
    pub fn set_discord_friends(&self, account_id: &str, friends: Vec<serde_json::Value>) {
        self.discord_friends.lock().unwrap().insert(account_id.to_string(), friends);
    }

    pub fn get_discord_friends(&self, account_id: &str) -> Vec<serde_json::Value> {
        self.discord_friends.lock().unwrap().get(account_id).cloned().unwrap_or_default()
    }

    /// Patches one friend's live status in place from a PRESENCE_UPDATE
    /// dispatch. Returns false (no-op) for a user_id not already in the
    /// friends list (a guild-mate's presence, not a friend's - Discord's
    /// gateway sends PRESENCE_UPDATE for both once subscribed) or when the
    /// status didn't actually change, so backend/discord.rs can skip
    /// emitting a redundant discordPresenceUpdate event.
    pub fn update_discord_presence(&self, account_id: &str, user_id: &str, status: &str) -> bool {
        let mut friends = self.discord_friends.lock().unwrap();
        let Some(list) = friends.get_mut(account_id) else { return false };
        let Some(friend) = list.iter_mut().find(|f| f["userId"] == user_id) else { return false };
        if friend["status"].as_str() == Some(status) {
            return false;
        }
        friend["status"] = serde_json::Value::String(status.to_string());
        true
    }

    pub fn set_matrix_room(&self, buffer_id: &str, room_id: &str) {
        self.matrix_rooms.lock().unwrap().insert(buffer_id.to_string(), room_id.to_string());
    }

    pub fn get_matrix_room(&self, buffer_id: &str) -> Option<String> {
        self.matrix_rooms.lock().unwrap().get(buffer_id).cloned()
    }

    /// Reverse of get_matrix_room - which buffer (if any) a room id is
    /// currently mapped to. Used by open_dm to detect and reconcile a
    /// race against the background sync loop, which runs concurrently and
    /// can independently discover (and buffer, under a worse name - see
    /// open_dm's own doc comment) the exact same freshly-created room.
    pub fn get_buffer_id_for_matrix_room(&self, room_id: &str) -> Option<String> {
        self.matrix_rooms.lock().unwrap().iter().find(|(_, r)| r.as_str() == room_id).map(|(b, _)| b.clone())
    }

    pub fn set_matrix_room_name(&self, account_id: &str, room_id: &str, name: &str, kind: &str) {
        self.matrix_room_names
            .lock()
            .unwrap()
            .insert((account_id.to_string(), room_id.to_string()), (name.to_string(), kind.to_string()));
    }

    pub fn get_matrix_room_name(&self, account_id: &str, room_id: &str) -> Option<(String, String)> {
        self.matrix_room_names.lock().unwrap().get(&(account_id.to_string(), room_id.to_string())).cloned()
    }

    /// An already-known "dm"-kind room this account shares with
    /// `target_user_id`, if any - used by backend/matrix/mod.rs's open_dm
    /// to reuse an existing DM instead of creating a duplicate one every
    /// time "Open DM" is clicked. A plain linear scan of this account's
    /// rooms (cheap at the scale of rooms a single account realistically
    /// has, same reasoning as matrix_rooms_containing_member).
    pub fn find_matrix_dm_room(&self, account_id: &str, target_user_id: &str) -> Option<String> {
        let names = self.matrix_room_names.lock().unwrap();
        let members = self.matrix_room_members.lock().unwrap();
        names
            .iter()
            .filter(|((acct, _), (_, kind))| acct == account_id && kind == "dm")
            .find(|((_, room_id), _)| {
                members
                    .get(&(account_id.to_string(), room_id.clone()))
                    .is_some_and(|roster| roster.contains_key(target_user_id))
            })
            .map(|((_, room_id), _)| room_id.clone())
    }

    pub fn set_matrix_machine(&self, account_id: &str, session: std::sync::Arc<crate::backend::matrix::crypto::CryptoSession>) {
        self.matrix_machines.lock().unwrap().insert(account_id.to_string(), session);
    }

    pub fn get_matrix_machine(&self, account_id: &str) -> Option<std::sync::Arc<crate::backend::matrix::crypto::CryptoSession>> {
        self.matrix_machines.lock().unwrap().get(account_id).cloned()
    }

    pub fn remove_matrix_machine(&self, account_id: &str) {
        self.matrix_machines.lock().unwrap().remove(account_id);
    }

    /// Records this sync's encryption check for a room and reflects it
    /// onto the buffer's own `encrypted` field (re-broadcast in place -
    /// same "re-send the whole buffer object" convention set_matrix_room_
    /// avatar/refresh_buffer_activity already use), driving the input
    /// field's lock/unlock indicator. `is_encrypted` only ever *sets*
    /// matrix_encrypted_rooms, never clears it - the spec forbids
    /// un-encrypting a room once turned on, so a stray `false` result on
    /// some later sync (there shouldn't be one, but this doesn't trust
    /// that) can't un-confirm it. The buffer field itself mirrors
    /// whichever is true.
    pub fn set_matrix_room_encrypted(&self, state: &AppState, buffer_id: &str, is_encrypted: bool) {
        if is_encrypted {
            self.matrix_encrypted_rooms.lock().unwrap().insert(buffer_id.to_string());
        }
        let confirmed = self.matrix_encrypted_rooms.lock().unwrap().contains(buffer_id);
        let updated = {
            let mut buffers = self.buffers.lock().unwrap();
            match buffers.get_mut(buffer_id) {
                Some(b) if b.encrypted != Some(confirmed) => {
                    b.encrypted = Some(confirmed);
                    Some(b.clone())
                }
                _ => None,
            }
        };
        if let Some(b) = updated {
            state.events.emit("bufferListChange", serde_json::to_value(&b).unwrap());
        }
    }

    pub fn is_matrix_room_encrypted(&self, buffer_id: &str) -> bool {
        self.matrix_encrypted_rooms.lock().unwrap().contains(buffer_id)
    }

    /// Records a reaction event we just learned about (ours or anyone
    /// else's) so a later `m.room.redaction` targeting it can be resolved
    /// - see matrix_reaction_targets's doc comment. `is_me` additionally
    /// populates matrix_own_reactions so our own un-react (toggleReaction)
    /// can find its event id by (buffer, message, emoji) without a
    /// reverse scan.
    pub fn record_matrix_reaction_event(&self, buffer_id: &str, msg_id: &str, emoji: &str, event_id: &str, is_me: bool) {
        self.matrix_reaction_targets
            .lock()
            .unwrap()
            .insert(event_id.to_string(), (buffer_id.to_string(), msg_id.to_string(), emoji.to_string(), is_me));
        if is_me {
            self.matrix_own_reactions
                .lock()
                .unwrap()
                .insert((buffer_id.to_string(), msg_id.to_string(), emoji.to_string()), event_id.to_string());
        }
    }

    pub fn get_matrix_own_reaction_event(&self, buffer_id: &str, msg_id: &str, emoji: &str) -> Option<String> {
        self.matrix_own_reactions.lock().unwrap().get(&(buffer_id.to_string(), msg_id.to_string(), emoji.to_string())).cloned()
    }

    /// Looks up and forgets a reaction event id in one step - a redaction
    /// only ever needs to resolve to its target once (the reaction is
    /// gone afterward either way), so removing it here also keeps these
    /// maps from growing unboundedly over a long-running buffer's life.
    pub fn take_matrix_reaction_target(&self, event_id: &str) -> Option<(String, String, String, bool)> {
        let target = self.matrix_reaction_targets.lock().unwrap().remove(event_id);
        if let Some((buffer_id, msg_id, emoji, true)) = &target {
            self.matrix_own_reactions.lock().unwrap().remove(&(buffer_id.clone(), msg_id.clone(), emoji.clone()));
        }
        target
    }

    pub fn insert_matrix_verification(&self, id: &str, v: crate::backend::matrix::verification::ActiveVerification) {
        self.matrix_verifications.lock().unwrap().insert(id.to_string(), v);
    }

    pub fn get_matrix_verification(&self, id: &str) -> Option<crate::backend::matrix::verification::ActiveVerification> {
        self.matrix_verifications.lock().unwrap().get(id).cloned()
    }

    pub fn update_matrix_verification(&self, id: &str, v: crate::backend::matrix::verification::ActiveVerification) {
        self.matrix_verifications.lock().unwrap().insert(id.to_string(), v);
    }

    pub fn remove_matrix_verification(&self, id: &str) {
        self.matrix_verifications.lock().unwrap().remove(id);
    }

    /// Every verification id currently tracked for one account - used both
    /// to enforce "one at a time per account" (start_verification cancels
    /// these first) and by the sync loop's per-account tick to know which
    /// entries to poll.
    pub fn matrix_verification_ids_for_account(&self, account_id: &str) -> Vec<String> {
        self.matrix_verifications
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, v)| v.account_id == account_id)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Every flow_id already known/tracked for this account - lets the
    /// sync loop's incoming-request scan (get_verification_requests) tell
    /// "an incoming request we don't know about yet" apart from ones
    /// already surfaced, without a separate reverse index.
    pub fn matrix_known_flow_ids(&self, account_id: &str) -> std::collections::HashSet<String> {
        self.matrix_verifications
            .lock()
            .unwrap()
            .values()
            .filter(|v| v.account_id == account_id)
            .map(|v| v.flow_id.clone())
            .collect()
    }

    pub fn mark_matrix_backup_enabled(&self, account_id: &str) {
        self.matrix_backup_enabled.lock().unwrap().insert(account_id.to_string());
    }

    pub fn has_matrix_backup(&self, account_id: &str) -> bool {
        self.matrix_backup_enabled.lock().unwrap().contains(account_id)
    }

    pub fn set_matrix_member_avatar(&self, account_id: &str, user_id: &str, avatar_url: &str) {
        self.matrix_member_avatars.lock().unwrap().insert((account_id.to_string(), user_id.to_string()), avatar_url.to_string());
    }

    pub fn get_matrix_member_avatar(&self, account_id: &str, user_id: &str) -> Option<String> {
        self.matrix_member_avatars.lock().unwrap().get(&(account_id.to_string(), user_id.to_string())).cloned()
    }

    /// Sets a room's resolved avatar and, if the room's buffer already
    /// exists, updates it in place and re-broadcasts bufferListChange -
    /// same "re-send the whole buffer object" convention refresh_buffer_
    /// activity already uses for a lastActivityTs bump, just for a
    /// different field. A no-op broadcast-wise if the buffer doesn't exist
    /// yet (the avatar is still cached for whenever ensure_buffer creates
    /// it - see get_matrix_room_avatar, checked at that point).
    pub fn set_account_status(&self, account_id: &str, status: &str) {
        self.account_status.lock().unwrap().insert(account_id.to_string(), status.to_string());
    }

    /// Defaults to online: an account that has never been set is online, which
    /// is what connecting does regardless.
    pub fn account_status(&self, account_id: &str) -> String {
        self.account_status.lock().unwrap().get(account_id).cloned().unwrap_or_else(|| "online".to_string())
    }

    pub fn set_discord_gateway_sender(&self, account_id: &str, sender: tokio::sync::mpsc::UnboundedSender<String>) {
        self.discord_gateway_senders.lock().unwrap().insert(account_id.to_string(), sender);
    }

    pub fn clear_discord_gateway_sender(&self, account_id: &str) {
        self.discord_gateway_senders.lock().unwrap().remove(account_id);
    }

    pub fn discord_gateway_sender(&self, account_id: &str) -> Option<tokio::sync::mpsc::UnboundedSender<String>> {
        self.discord_gateway_senders.lock().unwrap().get(account_id).cloned()
    }

    /// Records this account's roles in a guild, answering whether they
    /// differ from what was there before.
    ///
    /// The first sighting is not a change: it is the baseline, and treating
    /// it as one would re-read every guild on the first member update after
    /// connecting.
    pub fn set_discord_own_roles(&self, group_id: &str, roles: Vec<String>) -> bool {
        let mut sorted = roles;
        sorted.sort();
        let mut all = self.discord_own_roles.lock().unwrap();
        match all.insert(group_id.to_string(), sorted.clone()) {
            Some(before) => before != sorted,
            None => false,
        }
    }

    /// Claims the right to re-read a guild, or notes that one more change
    /// arrived while somebody else is already doing it.
    pub fn try_start_guild_resync(&self, group_id: &str) -> bool {
        let mut running = self.discord_resync.lock().unwrap();
        match running.get_mut(group_id) {
            Some(again) => {
                *again = true;
                false
            }
            None => {
                running.insert(group_id.to_string(), false);
                true
            }
        }
    }

    /// Finishes a pass, answering whether anything changed while it ran and
    /// so whether the result it just wrote may already be out of date.
    pub fn finish_guild_resync(&self, group_id: &str) -> bool {
        self.discord_resync.lock().unwrap().remove(group_id).unwrap_or(false)
    }

    /// Records that a guild's channel list will need rebuilding once its
    /// gate lifts, and answers whether it still does.
    pub fn note_discord_gated(&self, group_id: &str, gated: bool) {
        let mut set = self.discord_gated.lock().unwrap();
        if gated {
            set.insert(group_id.to_string());
        } else {
            set.remove(group_id);
        }
    }

    pub fn take_discord_gated(&self, group_id: &str) -> bool {
        self.discord_gated.lock().unwrap().remove(group_id)
    }

    /// A rail entry as it currently stands, for a caller rebuilding one.
    ///
    /// Rebuilding from a REST fetch loses whatever the gateway alone knew -
    /// a guild's position is the order somebody dragged their servers into
    /// and is not in the REST guild object at all, so re-registering without
    /// it would silently reshuffle the rail.
    pub fn get_buffer_group(&self, group_id: &str) -> Option<crate::model::BufferGroup> {
        self.buffer_groups.lock().unwrap().get(group_id).cloned()
    }

    pub fn has_buffer_group(&self, group_id: &str) -> bool {
        self.buffer_groups.lock().unwrap().contains_key(group_id)
    }

    pub fn set_matrix_space_parent(&self, account_id: &str, child_room_id: &str, group_id: &str) {
        self.matrix_space_parents
            .lock()
            .unwrap()
            .insert((account_id.to_string(), child_room_id.to_string()), group_id.to_string());
    }

    pub fn get_matrix_space_parent(&self, account_id: &str, child_room_id: &str) -> Option<String> {
        self.matrix_space_parents.lock().unwrap().get(&(account_id.to_string(), child_room_id.to_string())).cloned()
    }

    /// The buffer showing a Matrix room, if one has been created for it.
    pub fn matrix_buffer_for_room(&self, room_id: &str) -> Option<String> {
        self.matrix_rooms.lock().unwrap().iter().find(|(_, r)| r.as_str() == room_id).map(|(b, _)| b.clone())
    }

    pub fn set_matrix_room_avatar(&self, state: &AppState, account_id: &str, room_id: &str, avatar_url: &str) {
        self.matrix_room_avatars.lock().unwrap().insert((account_id.to_string(), room_id.to_string()), avatar_url.to_string());
        let Some(buffer_id) = self.matrix_rooms.lock().unwrap().iter().find(|(_, r)| r.as_str() == room_id).map(|(b, _)| b.clone()) else { return };
        let updated = {
            let mut buffers = self.buffers.lock().unwrap();
            match buffers.get_mut(&buffer_id) {
                Some(b) if b.avatar_url.as_deref() != Some(avatar_url) => {
                    b.avatar_url = Some(avatar_url.to_string());
                    Some(b.clone())
                }
                _ => None,
            }
        };
        if let Some(b) = updated {
            state.events.emit("bufferListChange", serde_json::to_value(&b).unwrap());
        }
    }

    /// Files a buffer under a heading, in the order the service puts it.
    ///
    /// Broadcast only when something changed: guild channels are re-registered
    /// on every reconnect, and a client that redrew its whole list each time
    /// would flicker.
    pub fn set_buffer_category(&self, state: &AppState, buffer_id: &str, category: Option<&str>, position: i64) {
        let updated = {
            let mut buffers = self.buffers.lock().unwrap();
            match buffers.get_mut(buffer_id) {
                Some(b) if b.category.as_deref() != category || b.position != position => {
                    b.category = category.map(String::from);
                    b.position = position;
                    Some(b.clone())
                }
                _ => None,
            }
        };
        if let Some(b) = updated {
            state.events.emit("bufferListChange", serde_json::to_value(&b).unwrap());
        }
    }

    /// Gives a buffer a picture, whatever protocol it came from.
    ///
    /// A room's avatar for Matrix, the other person's for a direct message.
    /// Broadcast so a list already on screen redraws, and only when it
    /// actually changed - a DM's avatar is set again on every reconnect, and
    /// a client that redraws its whole buffer list each time would flicker.
    pub fn set_buffer_avatar(&self, state: &AppState, buffer_id: &str, avatar_url: &str) {
        let updated = {
            let mut buffers = self.buffers.lock().unwrap();
            match buffers.get_mut(buffer_id) {
                Some(b) if b.avatar_url.as_deref() != Some(avatar_url) => {
                    b.avatar_url = Some(avatar_url.to_string());
                    Some(b.clone())
                }
                _ => None,
            }
        };
        if let Some(b) = updated {
            state.events.emit("bufferListChange", serde_json::to_value(&b).unwrap());
        }
    }

    pub fn get_matrix_room_avatar(&self, account_id: &str, room_id: &str) -> Option<String> {
        self.matrix_room_avatars.lock().unwrap().get(&(account_id.to_string(), room_id.to_string())).cloned()
    }

    pub fn set_matrix_power_levels(&self, account_id: &str, room_id: &str, content: Value) {
        self.matrix_power_levels.lock().unwrap().insert((account_id.to_string(), room_id.to_string()), content);
    }

    pub fn get_matrix_power_levels(&self, account_id: &str, room_id: &str) -> Option<Value> {
        self.matrix_power_levels.lock().unwrap().get(&(account_id.to_string(), room_id.to_string())).cloned()
    }

    /// Upserts a joined member's display name into a room's roster - see
    /// matrix_room_members's own doc comment. Called for `membership ==
    /// "join"`; anything else (leave/ban/invite) goes through
    /// remove_matrix_member instead, since only actual joins belong in a
    /// userlist.
    /// Where to carry on reading a room's history backwards from.
    ///
    /// Set once from the first sync that mentions the room and then moved by
    /// each page fetched, so it always means "older than everything already
    /// held". Set-if-absent on the sync side matters: a later sync's token
    /// points at newer history, and taking it would skip everything between.
    pub fn set_matrix_back_token(&self, account_id: &str, room_id: &str, token: &str, only_if_absent: bool) {
        let mut all = self.matrix_back_tokens.lock().unwrap();
        let key = (account_id.to_string(), room_id.to_string());
        if only_if_absent && all.contains_key(&key) {
            return;
        }
        all.insert(key, token.to_string());
    }

    pub fn matrix_back_token(&self, account_id: &str, room_id: &str) -> Option<String> {
        self.matrix_back_tokens.lock().unwrap().get(&(account_id.to_string(), room_id.to_string())).cloned()
    }

    pub fn set_matrix_member(&self, account_id: &str, room_id: &str, user_id: &str, display_name: &str) {
        self.matrix_room_members
            .lock()
            .unwrap()
            .entry((account_id.to_string(), room_id.to_string()))
            .or_default()
            .insert(user_id.to_string(), display_name.to_string());
    }

    pub fn remove_matrix_member(&self, account_id: &str, room_id: &str, user_id: &str) {
        if let Some(members) = self.matrix_room_members.lock().unwrap().get_mut(&(account_id.to_string(), room_id.to_string())) {
            members.remove(user_id);
        }
    }

    pub fn get_matrix_room_members(&self, account_id: &str, room_id: &str) -> HashMap<String, String> {
        self.matrix_room_members
            .lock()
            .unwrap()
            .get(&(account_id.to_string(), room_id.to_string()))
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_matrix_presence(&self, account_id: &str, user_id: &str, online: bool) {
        self.matrix_presence.lock().unwrap().insert((account_id.to_string(), user_id.to_string()), online);
    }

    pub fn is_matrix_user_online(&self, account_id: &str, user_id: &str) -> bool {
        self.matrix_presence.lock().unwrap().get(&(account_id.to_string(), user_id.to_string())).copied().unwrap_or(false)
    }

    /// Every room (by room id) this account currently has `user_id` as a
    /// joined member of - used when a presence.events update arrives for
    /// that user (a global event, not tied to one room) to know which
    /// rooms' userlists need re-emitting. A linear scan over this account's
    /// rooms is fine at the scale a single account's joined-room count
    /// realistically reaches.
    pub fn matrix_rooms_containing_member(&self, account_id: &str, user_id: &str) -> Vec<String> {
        self.matrix_room_members
            .lock()
            .unwrap()
            .iter()
            .filter(|((acct, _), members)| acct == account_id && members.contains_key(user_id))
            .map(|((_, room_id), _)| room_id.clone())
            .collect()
    }

    pub fn insert_sockchat_sender(&self, account_id: &str, room: u32, sender: tokio::sync::mpsc::UnboundedSender<String>) {
        self.sockchat_senders.lock().unwrap().entry(account_id.to_string()).or_default().insert(room, sender);
    }

    /// Only removes the entry if it's still exactly the sender being torn
    /// down (`same_channel`), not whatever's currently there. `.abort()`ing
    /// a room task (a respawn from settings changes, or run_with_retry's
    /// own account-level rebuild after a room's `?` returns an error)
    /// doesn't wait for that task to actually unwind - its own drop-guard
    /// cleanup can fire well after a *newer* connection attempt for the
    /// same room has already registered its own sender. An unconditional
    /// remove-by-key here would then delete that new, live sender out from
    /// under it: the room keeps receiving fine (its read loop doesn't
    /// touch this map at all) while sendMessage starts failing with "not
    /// currently connected to this room" for no visible reason - confirmed
    /// as the actual cause of a real "can't send to #fishtank, but still
    /// receiving" report.
    pub fn remove_sockchat_sender(&self, account_id: &str, room: u32, sender: &tokio::sync::mpsc::UnboundedSender<String>) {
        if let Some(rooms) = self.sockchat_senders.lock().unwrap().get_mut(account_id) {
            if rooms.get(&room).is_some_and(|current| current.same_channel(sender)) {
                rooms.remove(&room);
            }
        }
    }

    /// Drops every room's sender for this account - called when the whole
    /// account-level connection is being rebuilt from scratch (see
    /// backend::sockchat::run_with_retry), so a stale sender from a
    /// pre-restart room task can't be mistaken for a live one.
    pub fn clear_sockchat_senders(&self, account_id: &str) {
        self.sockchat_senders.lock().unwrap().remove(account_id);
    }

    pub fn sockchat_sender(&self, account_id: &str, room: u32) -> Option<tokio::sync::mpsc::UnboundedSender<String>> {
        self.sockchat_senders.lock().unwrap().get(account_id).and_then(|rooms| rooms.get(&room)).cloned()
    }

    /// Sends QUIT to every currently-connected account - called on
    /// shutdown (SIGTERM/SIGINT/Ctrl-C, see main.rs) so a `kill`'d or
    /// restarted daemon doesn't just drop its TCP connections, which can
    /// leave a "ghost" session holding the nick hostage server-side until
    /// the network's own ping-timeout notices the dead peer (confirmed
    /// live against Libera: this is exactly what caused a real reconnect
    /// failure - "Nickname is already in use" - after an abrupt kill).
    pub fn quit_all(&self, message: &str) {
        // Nothing should try to come back up on the way out. IRC reconnects
        // by itself now, and its loop reads this on the way out of a
        // session - a QUIT sent here ends one, so without clearing the
        // intent first the daemon would spend its shutdown reconnecting.
        self.wants_connected.lock().unwrap().clear();
        let senders: Vec<_> = self.irc_handles.lock().unwrap().values().map(|h| h.sender.clone()).collect();
        for sender in senders {
            let _ = sender.send_quit(message);
        }
    }

    /// Stops a connection: gracefully QUITs if it already reached the
    /// connected state (has a Sender), otherwise aborts the in-flight
    /// connection task directly - the only way to interrupt something
    /// still stuck in DNS/TCP/TLS/registration, which has no Sender yet.
    /// Returns false if this account has neither (nothing to stop).
    pub fn disconnect(&self, state: &AppState, account_id: &str) -> bool {
        // Before the QUIT, not after: the session ends as a result of it,
        // and an IRC retry loop reading this flag on the way out must see
        // that the ending was asked for.
        self.set_wants_connected(account_id, false);
        if let Some(sender) = self.irc_sender(account_id) {
            let _ = sender.send_quit("");
            return true;
        }
        let handle = self.task_handles.lock().unwrap().remove(account_id);
        match handle {
            Some(handle) => {
                handle.abort();
                // abort() drops the task at its next await point without
                // running any of its own cleanup, so the state transition
                // that would normally happen at the end of backend::irc::
                // spawn()'s task has to happen here instead.
                self.set_conn_state(state, account_id, ConnState::Disconnected, Some("cancelled"));
                true
            }
            None => false,
        }
    }

    pub fn irc_sender(&self, account_id: &str) -> Option<irc::client::Sender> {
        self.irc_handles.lock().unwrap().get(account_id).map(|h| h.sender.clone())
    }

    pub fn irc_current_nick(&self, account_id: &str) -> Option<String> {
        self.irc_handles.lock().unwrap().get(account_id).map(|h| h.nick.clone())
    }

    /// Records an inbound/outbound message: ensures the buffer exists,
    /// writes scrollback, computes highlight, emits "message" and (for
    /// highlights or DMs) "notification". Protocol-agnostic by design -
    /// every backend should funnel messages through this single path
    /// rather than duplicating highlight/store/emit logic (see project
    /// plan's "Architecture" section on why this lives in the daemon core,
    /// not per-backend).
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::too_many_arguments)]
    pub fn record_message(
        &self,
        state: &AppState,
        account_id: &str,
        buffer_name: &str,
        buffer_kind: &str,
        from: &str,
        body: &str,
        is_action: bool,
        kind: &str,
        reply_to: Option<ReplyPreview>,
        msg_id_override: Option<String>,
        force_highlight: bool,
        avatar_url: Option<String>,
        embeds: Vec<Embed>,
        attachments: Vec<Attachment>,
        sender_id: Option<String>,
    ) -> bool {
        self.record_message_at(
            state, account_id, buffer_name, buffer_kind, from, body, is_action, kind, reply_to,
            msg_id_override, force_highlight, avatar_url, embeds, attachments, sender_id, None, None,
        )
    }

    /// As `record_message`, for a protocol that knows when the message was
    /// actually sent.
    ///
    /// `sent_at` is unix seconds from the wire. Without one the clock at the
    /// moment of reading stands in, which is right for something just said
    /// and wrong for anything replayed - a burst arriving after a reconnect
    /// otherwise dates itself to the reconnect.
    #[allow(clippy::too_many_arguments)]
    pub fn record_message_at(
        &self,
        state: &AppState,
        account_id: &str,
        buffer_name: &str,
        buffer_kind: &str,
        from: &str,
        body: &str,
        is_action: bool,
        kind: &str,
        reply_to: Option<ReplyPreview>,
        msg_id_override: Option<String>,
        force_highlight: bool,
        avatar_url: Option<String>,
        embeds: Vec<Embed>,
        attachments: Vec<Attachment>,
        sender_id: Option<String>,
        sent_at: Option<i64>,
        html: Option<String>,
    ) -> bool {
        let buffer = self.ensure_buffer(state, account_id, buffer_name, buffer_kind);
        let own_nick = self
            .irc_current_nick(account_id)
            .or_else(|| self.own_identity(account_id))
            .unwrap_or_default();
        // `force_highlight` is Discord's own resolved-mentions answer (see
        // backend/discord.rs's mentions_own_user) - authoritative when
        // present, since the nick-substring fallback below can never match
        // a Discord mention (the body still has `<@id>` tokens, not the
        // account's nick, by the time this runs). IRC has no such
        // structured signal, so it always relies on the substring check.
        let is_highlight = force_highlight
            || (!own_nick.is_empty()
                && from != own_nick
                && body.to_lowercase().contains(&own_nick.to_lowercase()));
        let is_own = !own_nick.is_empty() && from == own_nick;
        let is_dm = buffer_kind == "dm";
        let ts = sent_at.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
        });
        // IRC has no native per-message id (a PRIVMSG carries none), so it
        // always falls back to nobilis's own generated one - but Discord's
        // MESSAGE_UPDATE/DELETE/REACTION_ADD/REMOVE dispatches reference
        // the *real* message id, and storing live messages under a fake
        // one would make those permanently unable to find their target
        // (confirmed live: only backfilled history, which does use real
        // ids, would ever match). Discord's own MESSAGE_CREATE handling
        // passes its real id through here for exactly that reason.
        let msg_id = msg_id_override.unwrap_or_else(model::next_message_id);

        match state.store.append_message(&buffer.id, &msg_id, from, body, ts, is_action, is_highlight, kind, reply_to.as_ref(), &[], is_own, avatar_url.as_deref(), &embeds, &attachments, sender_id.as_deref(), html.as_deref(), buffer_kind) {
            // Already had it, so there is nothing to announce. Sneedchat
            // replays a room's recent history whenever its connection comes
            // back, and every backend that carries real message ids can
            // repeat itself the same way after an outage. Without this, a
            // mention in that history is a fresh notification on every
            // reconnect - the same one, over and over, for a message that was
            // read hours ago.
            Ok(false) => return false,
            Ok(true) => {}
            // A message that could not be stored is still shown. Losing it
            // outright would be the worse failure, and a duplicate line is
            // recoverable by scrolling.
            Err(e) => tracing::warn!("failed to persist message: {e}"),
        }

        // Bump the buffer's own activity timestamp and re-broadcast it as a
        // bufferListChange - the frontend re-sorts its buffer list on that
        // event, so a channel/DM with a fresh message moves to the top of
        // its stack without needing a dedicated new event type.
        let updated_buffer = {
            let mut buffers = self.buffers.lock().unwrap();
            match buffers.get_mut(&buffer.id) {
                Some(b) => {
                    b.last_activity_ts = ts;
                    Some(b.clone())
                }
                None => None,
            }
        };
        if let Some(b) = updated_buffer {
            state.events.emit("bufferListChange", serde_json::to_value(&b).unwrap());
        }

        let message = Message {
            html: html.clone(),
            id: msg_id,
            buffer_id: buffer.id.clone(),
            from: from.to_string(),
            body: body.to_string(),
            ts,
            is_action,
            is_highlight,
            kind: kind.to_string(),
            reply_to,
            edited: false,
            reactions: Vec::new(),
            is_own,
            avatar_url: avatar_url.clone(),
            embeds,
            attachments,
            sender_id,
        };
        state.events.emit("message", serde_json::to_value(&message).unwrap());

        // Notify on every inbound DM regardless of content, or on a
        // highlighted channel message - two distinct rules (see
        // daemon/nobilis/uiops_conv.c's should_notify/is_highlight split).
        if from != own_nick && (is_dm || is_highlight) {
            state.events.emit(
                "notification",
                json!({
                    "accountId": account_id,
                    "bufferId": buffer.id,
                    "title": from,
                    "body": body,
                    "avatarUrl": avatar_url,
                }),
            );
        }
        true
    }

    /// A live edit (Discord's MESSAGE_UPDATE) - a no-op broadcast-wise if
    /// the message was never recorded locally (nothing visible to update).
    /// Returns whether a row was actually found and updated - backend/
    /// sockchat/mod.rs uses this to fall back to inserting a new message
    /// when an "edit" (a message_edit_date bump) arrives for a uuid it
    /// never actually stored, e.g. seeing a message for the first time
    /// that already carries prior edit history.
    pub fn update_message(&self, state: &AppState, buffer_id: &str, msg_id: &str, body: &str, embeds: &[Embed], attachments: &[Attachment]) -> bool {
        match state.store.update_message_body(buffer_id, msg_id, body, embeds, attachments) {
            Ok(true) => {
                state.events.emit("messageUpdated", json!({ "bufferId": buffer_id, "id": msg_id, "body": body, "edited": true, "embeds": embeds, "attachments": attachments }));
                true
            }
            Ok(false) => false,
            Err(e) => {
                tracing::warn!("failed to update message: {e}");
                false
            }
        }
    }

    /// Like update_message, but for a backend-internal body rewrite that
    /// isn't a real user edit (see backend/sockchat's attachment-link
    /// resolution) - doesn't set the `edited` flag either in storage or in
    /// the emitted event, so the frontend's "(edited)" label doesn't show
    /// up for something the user never touched.
    pub fn update_message_body_only(&self, state: &AppState, buffer_id: &str, msg_id: &str, body: &str) -> bool {
        match state.store.update_message_body_silent(buffer_id, msg_id, body) {
            Ok(true) => {
                state.events.emit("messageUpdated", json!({ "bufferId": buffer_id, "id": msg_id, "body": body, "edited": false, "embeds": [] }));
                true
            }
            Ok(false) => false,
            Err(e) => {
                tracing::warn!("failed to update message body: {e}");
                false
            }
        }
    }

    /// Discord's MESSAGE_DELETE - removes the row and tells clients to
    /// drop it from view, same as Discord's own clients (no tombstone).
    pub fn delete_message(&self, state: &AppState, buffer_id: &str, msg_id: &str) {
        match state.store.delete_message(buffer_id, msg_id) {
            Ok(true) => state.events.emit("messageDeleted", json!({ "bufferId": buffer_id, "id": msg_id })),
            Ok(false) => {}
            Err(e) => tracing::warn!("failed to delete message: {e}"),
        }
    }

    /// One reaction add/remove (Discord's REACTION_ADD/REMOVE are
    /// per-user-per-emoji events, not full snapshots) - broadcasts the
    /// message's updated full reaction list once applied.
    pub fn update_reaction(&self, state: &AppState, buffer_id: &str, msg_id: &str, emoji: &str, is_me: bool, add: bool) {
        match state.store.update_reaction(buffer_id, msg_id, emoji, is_me, add) {
            Ok(Some(reactions)) => state.events.emit("reactionsChanged", json!({ "bufferId": buffer_id, "id": msg_id, "reactions": reactions })),
            Ok(None) => {}
            Err(e) => tracing::warn!("failed to update reaction: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flag that keeps IRC's retry loop from undoing a deliberate
    /// disconnect. A session ending looks the same from inside the loop
    /// whether the link died or somebody switched the account off, and only
    /// this tells them apart.
    #[test]
    fn connection_intent_survives_until_it_is_cleared() {
        let rt = Runtime::new();
        assert!(!rt.wants_connected("irc:nick@example.org"), "nothing is wanted before a spawn");

        rt.set_wants_connected("irc:nick@example.org", true);
        assert!(rt.wants_connected("irc:nick@example.org"));
        // One account being switched off must not stand the others down.
        rt.set_wants_connected("irc:other@example.org", true);
        rt.set_wants_connected("irc:nick@example.org", false);
        assert!(!rt.wants_connected("irc:nick@example.org"));
        assert!(rt.wants_connected("irc:other@example.org"));
    }

    /// Shutdown stands every account down at once, or the daemon would
    /// spend its exit reconnecting the sessions quit_all just ended.
    #[test]
    fn quitting_everything_stands_every_account_down() {
        let rt = Runtime::new();
        rt.set_wants_connected("irc:a@example.org", true);
        rt.set_wants_connected("irc:b@example.org", true);
        rt.quit_all("bye");
        assert!(!rt.wants_connected("irc:a@example.org"));
        assert!(!rt.wants_connected("irc:b@example.org"));
    }

    /// A newer spawn owns the account; an older attempt still unwinding must
    /// stand down rather than reconnect alongside it.
    #[test]
    fn only_the_newest_attempt_owns_the_account() {
        let rt = Runtime::new();
        let first = rt.next_generation("irc:nick@example.org");
        assert!(rt.is_current_generation("irc:nick@example.org", first));
        let second = rt.next_generation("irc:nick@example.org");
        assert!(!rt.is_current_generation("irc:nick@example.org", first));
        assert!(rt.is_current_generation("irc:nick@example.org", second));
    }

    /// Discord repeats CALL_UPDATE for every change to a live call - somebody
    /// muting, somebody starting video - and each one carries the same ringing
    /// list. Announcing all of them would restart a frontend's ringtone on
    /// each, so only the change is news.
    #[test]
    fn a_call_is_announced_once_however_often_discord_repeats_itself() {
        let runtime = Runtime::new();
        assert!(runtime.set_ringing("acct|alex", "acct", "chan", true));
        assert!(!runtime.set_ringing("acct|alex", "acct", "chan", true));
        assert!(!runtime.set_ringing("acct|alex", "acct", "chan", true));
    }

    /// Stopping is news exactly once too - a caller hanging up produces both a
    /// CALL_UPDATE with an empty ringing list and a CALL_DELETE.
    #[test]
    fn a_call_stops_ringing_once() {
        let runtime = Runtime::new();
        runtime.set_ringing("acct|alex", "acct", "chan", true);
        assert!(runtime.set_ringing("acct|alex", "acct", "chan", false));
        assert!(!runtime.set_ringing("acct|alex", "acct", "chan", false));
    }

    /// Nothing ringing is not the same as never having asked, and a frontend
    /// opening mid-call reads this rather than the event it missed.
    #[test]
    fn what_is_ringing_can_be_asked_for_after_the_fact() {
        let runtime = Runtime::new();
        assert!(runtime.ringing_calls().is_empty());
        runtime.set_ringing("acct|alex", "acct", "chan", true);
        runtime.set_ringing("acct|sam", "acct", "chan2", true);
        let mut ringing: Vec<String> = runtime.ringing_calls().into_iter().map(|(b, _, _)| b).collect();
        ringing.sort();
        assert_eq!(ringing, vec!["acct|alex", "acct|sam"]);

        runtime.set_ringing("acct|alex", "acct", "chan", false);
        assert_eq!(runtime.ringing_calls().len(), 1);
    }

    /// Two people calling at once are two calls, not one that overwrote the
    /// other - the panel stacks them and each needs its own answer.
    #[test]
    fn two_callers_are_two_calls() {
        let runtime = Runtime::new();
        assert!(runtime.set_ringing("acct|alex", "acct", "chan", true));
        assert!(runtime.set_ringing("other|sam", "other", "chan2", true));
        assert_eq!(runtime.ringing_calls().len(), 2);
    }
}

#[cfg(test)]
mod voice_flag_tests {
    use super::*;

    #[test]
    fn a_shared_screen_is_read_off_the_voice_state() {
        let d = json!({ "self_stream": true, "self_video": false });
        let f = VoiceFlags::from_voice_state(&d);
        assert!(f.streaming);
        assert!(!f.video);
    }

    #[test]
    fn a_camera_is_not_a_screen_share() {
        // Independent, and they look nothing alike to whoever is watching.
        let d = json!({ "self_video": true });
        let f = VoiceFlags::from_voice_state(&d);
        assert!(f.video);
        assert!(!f.streaming);
    }

    #[test]
    fn a_server_mute_counts_as_muted() {
        // Who can undo it differs; whether anything is audible does not.
        let f = VoiceFlags::from_voice_state(&json!({ "mute": true }));
        assert!(f.muted);
        let f = VoiceFlags::from_voice_state(&json!({ "self_mute": true }));
        assert!(f.muted);
    }

    #[test]
    fn a_state_that_says_nothing_claims_nothing() {
        assert_eq!(VoiceFlags::from_voice_state(&json!({})), VoiceFlags::default());
    }
}
