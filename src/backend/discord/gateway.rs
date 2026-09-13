//! The connection everything else hangs off.
//!
//! One websocket per account, carrying every event Discord has for it: the
//! initial READY with the whole account's shape, then dispatches for as long
//! as it stays up. This module owns the socket, the heartbeat, the resume,
//! and the dispatch table that decides what each event means.

use super::*;

pub(super) const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Discord's gateway close code for "the token in your IDENTIFY payload is
/// invalid/revoked" - documented at discord.com/developers/docs/topics/
/// opcodes-and-status-codes#gateway-close-event-codes, and confirmed live
/// against this project's own account going through exactly this (see
/// GatewayAuthFailed's doc comment). Distinct from op 9 "invalid session"
/// (a resumable-session issue mid-connection, already handled by the plain
/// reconnect-with-backoff path below) - this one means the *credential*
/// itself is dead, not just this particular session.
pub(super) const CLOSE_CODE_AUTH_FAILED: u16 = 4004;

/// Marker error so run_gateway_with_retry can tell "the token is
/// permanently dead" apart from every other (transient, worth retrying)
/// failure via a plain downcast, without run_gateway itself needing to
/// know anything about retry policy. Surfaced after this backend's own
/// account got deauthed mid-session (2026-08-20) with no clear signal
/// beyond the raw close frame - previously indistinguishable from any
/// other dropped connection, so the account just sat retrying the same
/// dead token forever, showing a generic "connecting".
#[derive(Debug)]
pub(super) struct GatewayAuthFailed;

impl std::fmt::Display for GatewayAuthFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Discord rejected this login (session revoked) - reconnect from the Accounts pane")
    }
}

impl std::error::Error for GatewayAuthFailed {}

pub(super) async fn next_json<S>(stream: &mut S) -> Result<Value>
where
    S: futures::Stream<Item = std::result::Result<WsMessage, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        match stream.next().await {
            Some(Ok(WsMessage::Text(text))) => return serde_json::from_str(&text).context("parsing gateway JSON"),
            Some(Ok(WsMessage::Close(frame))) => {
                if frame.as_ref().is_some_and(|f| u16::from(f.code) == CLOSE_CODE_AUTH_FAILED) {
                    return Err(GatewayAuthFailed.into());
                }
                bail!("connection closed: {frame:?}")
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => bail!("websocket error: {e}"),
            None => bail!("connection ended unexpectedly"),
        }
    }
}

/// Spawns the background task that keeps a Discord account's gateway
/// connection alive - the real-time equivalent of backend::irc::spawn.
/// Called both right after a fresh QR login and, from main.rs, for every
/// saved Discord account on daemon startup.
pub fn spawn(state: AppState, config: DiscordAccountConfig) {
    let account_id = config.account_id();
    // Guarantee at most one live gateway session per account - the same
    // guard backend::irc::spawn() needed (see Runtime::reset_connection's
    // doc comment): without this, a stale/zombie session left over from an
    // earlier connect (one whose socket silently died without the read
    // loop ever erroring - confirmed live: an account that still showed
    // "connected" kept a channel_map from hours earlier and simply stopped
    // receiving *any* live gateway dispatches for it) could sit alongside
    // a fresh one, or an old task's eventual cleanup could stomp a newer
    // connection's state. A no-op for the common case (nothing to reset).
    // Also what makes disconnect()/removeAccount reliably stop the retry
    // loop below - it works by aborting this same task_handle, regardless
    // of whether that loop is mid-connection or mid-backoff-sleep.
    state.runtime.reset_connection(&account_id);
    let join_handle = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run_gateway_with_retry(&state, &config, &account_id).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, join_handle.abort_handle());
}

pub(super) const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);

pub(super) const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Keeps re-establishing the gateway connection for as long as this task
/// lives - a fresh attempt (after a backoff wait, past the first) on every
/// disconnect: a network blip, Discord requesting a reconnect (op 7), an
/// invalidated session (op 9), or a detected-zombie connection (missed
/// heartbeat ack - see run_gateway). No "give up permanently" case for an
/// ordinary transient failure - it just keeps failing visibly (the account
/// shows "connecting" with the last error as detail) rather than settling
/// into a silent, permanently-stuck state, matching what a real Discord
/// client does.
///
/// The one deliberate exception is GatewayAuthFailed (gateway close code
/// 4004 - the token itself is dead, not just this session): retrying with
/// the exact same credential can never succeed, so this loop stops and
/// leaves the account in ConnState::AuthFailed instead of retrying forever
/// against a token that will keep getting rejected. The only way out is a
/// fresh login (spawn() gets called again from there, replacing this task)
/// - see accounts.rs's add_discord doc comment for why that upserts the
/// same account id (same buffers/scrollback) rather than creating a new one.
///
/// Otherwise only stops when this task itself is aborted from outside: an
/// explicit disconnect, account removal, or a newer spawn() superseding it
/// via reset_connection().
/// What survives a dropped gateway connection.
///
/// Two things live here for the same reason. Discord can *resume* a session
/// rather than starting a new one, which replays what was missed instead of
/// re-sending the whole account - but a resumed connection sends no READY
/// and no GUILD_CREATE, so the channel and guild maps those normally build
/// have to come across too or every message would arrive for a channel this
/// connection has never heard of.
#[derive(Default)]
pub(super) struct GatewaySession {
    resume: Option<Resume>,
    /// channel_id -> (buffer name, buffer kind).
    channel_map: HashMap<String, (String, String)>,
    /// Each guild's payload as GUILD_CREATE delivered it.
    guild_context: HashMap<String, Value>,
}

/// Enough to pick a session back up: what it was called, where to reconnect,
/// and how far through its event stream we got.
pub(super) struct Resume {
    session_id: String,
    url: String,
    seq: u64,
}

pub(super) async fn run_gateway_with_retry(state: &AppState, config: &DiscordAccountConfig, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    let mut session = GatewaySession::default();
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        let result = std::panic::AssertUnwindSafe(run_gateway(state, config, &mut session)).catch_unwind().await;
        let detail = match result {
            Ok(Ok(())) => "gateway session ended".to_string(),
            Ok(Err(e)) => {
                if e.downcast_ref::<GatewayAuthFailed>().is_some() {
                    tracing::warn!("discord[{account_id}]: {e} - giving up, needs a fresh login");
                    state.runtime.set_conn_state(state, account_id, ConnState::AuthFailed, Some(&e.to_string()));
                    return;
                }
                tracing::warn!("discord[{account_id}]: {e}");
                e.to_string()
            }
            Err(_) => {
                tracing::error!("discord[{account_id}]: connection task panicked");
                "internal error (see nobilis logs)".to_string()
            }
        };
        // set_conn_state() first (in case run_gateway got as far as
        // Connected before dying, which report_progress() alone can't
        // correct - it only ever re-stamps an *already*-"connecting"
        // state), then report_progress() for the live countdown text.
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        state.runtime.report_progress(state, account_id, &format!("{detail} - reconnecting in {}s...", delay.as_secs()));
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
    }
}

pub(super) async fn run_gateway(state: &AppState, config: &DiscordAccountConfig, session: &mut GatewaySession) -> Result<()> {
    // Disjoint borrows, so the resume state can be updated while the maps
    // are being passed around mutably.
    let GatewaySession { resume, channel_map, guild_context } = session;
    let account_id = config.account_id();
    // Discord asks that a resume go to the url it handed out with the
    // session rather than to the front door.
    let connect_url = resume.as_ref().map(|r| r.url.clone()).unwrap_or_else(|| GATEWAY_URL.to_string());
    let (ws, _resp) = tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(connect_url.as_str()))
        .await
        .map_err(|_| anyhow!("timed out connecting to Discord's gateway"))?
        .context("connecting to Discord's gateway")?;
    let (sink, mut stream) = ws.split();

    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let forwarder = tokio::spawn(async move {
        let mut sink = sink;
        while let Some(text) = out_rx.recv().await {
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    struct AbortOnDrop(tokio::task::JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _forwarder_guard = AbortOnDrop(forwarder);

    let hello = next_json(&mut stream).await.context("waiting for gateway hello")?;
    let heartbeat_interval = hello["d"]["heartbeat_interval"].as_u64().unwrap_or(41_250).max(1);

    // Discord's own documented client contract: if an ack (op 11) hasn't
    // arrived by the time the next heartbeat is due, the connection is
    // "zombied" and must be dropped and re-established, not just
    // heartbeat-retried forever - the read loop's own next_json().await
    // would otherwise wait on a socket that looks alive (never errors,
    // never closes) but has simply stopped receiving anything at all, the
    // exact "shows connected, never gets another message" failure this
    // whole retry mechanism exists to catch. ack_pending starts true so
    // the very first tick (before any heartbeat has been sent) can't
    // false-positive as a missed ack.
    let ack_pending = Arc::new(AtomicBool::new(false));
    let stale_notify = Arc::new(Notify::new());
    let hb_tx = out_tx.clone();
    let hb_ack_pending = ack_pending.clone();
    let hb_stale_notify = stale_notify.clone();
    let heartbeat_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(rand::random::<u64>() % heartbeat_interval)).await;
        let mut ticker = tokio::time::interval(Duration::from_millis(heartbeat_interval));
        loop {
            ticker.tick().await;
            if hb_ack_pending.swap(true, Ordering::SeqCst) {
                hb_stale_notify.notify_one();
                break;
            }
            if hb_tx.send(json!({ "op": 1, "d": Value::Null }).to_string()).is_err() {
                break;
            }
        }
    });
    let _heartbeat_guard = AbortOnDrop(heartbeat_task);

    // Deliberately minimal - no "intents" field (that's a bot-gateway-only
    // concept; a user token gets everything its own account can see
    // regardless), matching what working self-bot clients send.
    // Exposed so setAccountStatus can push a presence update onto this
    // connection rather than waiting for a reconnect to carry it.
    let account_id = config.account_id();
    state.runtime.set_discord_gateway_sender(&account_id, out_tx.clone());
    struct ClearSenderOnDrop<'a>(&'a AppState, String);
    impl Drop for ClearSenderOnDrop<'_> {
        fn drop(&mut self) {
            self.0.runtime.clear_discord_gateway_sender(&self.1);
        }
    }
    let _sender_guard = ClearSenderOnDrop(state, account_id.clone());

    // A resume replays what was missed instead of re-sending the whole
    // account; an identify starts fresh. Either way the server decides -
    // op 9 below says a resume was refused.
    if let Some(r) = resume.as_ref() {
        out_tx.send(json!({ "op": 6, "d": { "token": config.token, "session_id": r.session_id, "seq": r.seq } }).to_string())?;
    } else {
    out_tx.send(
        json!({
            "op": 2,
            "d": {
                "token": config.token,
                "properties": { "os": "linux", "browser": "nobilis", "device": "nobilis" },
                "compress": false,
                "large_threshold": 50,
                // Carried in IDENTIFY as well as pushed live, so a status set
                // before a reconnect survives it.
                "presence": presence_payload(&state.runtime.account_status(&account_id)),
            }
        })
        .to_string(),
    )?;
    }

    // channel_id -> (buffer name, buffer kind), rebuilt fresh from READY/
    // GUILD_CREATE each connection - see runtime.rs's discord_channels for
    // the reverse (persistent) mapping sendMessage needs.
    // channel_map and guild_context are carried in from GatewaySession above:
    // a resumed connection is sent neither READY nor GUILD_CREATE, so
    // rebuilding them here would leave every incoming message addressed to a
    // channel this connection had never heard of.

    loop {
        let msg = tokio::select! {
            result = next_json(&mut stream) => result?,
            _ = stale_notify.notified() => bail!("no heartbeat ack received - connection is zombied"),
        };
        let op = msg.get("op").and_then(|v| v.as_i64()).unwrap_or(-1);
        // Every dispatch is numbered, and a resume says how far it got.
        // Tracked before the match so it counts events this client chose
        // not to handle - the server's count includes them either way, and
        // resuming from a lower number would replay what was already seen.
        if let (Some(seq), Some(r)) = (msg.get("s").and_then(|v| v.as_u64()), resume.as_mut()) {
            r.seq = seq;
        }
        match op {
            0 => {
                let t = msg.get("t").and_then(|v| v.as_str()).unwrap_or("");
                let d = &msg["d"];
                match t {
                    "READY" => {
                        let username = d["user"]["global_name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or_else(|| d["user"]["username"].as_str())
                            .unwrap_or("me")
                            .to_string();
                        state.runtime.set_own_identity(&account_id, &username);
                        // Nitro, which is what lets this account's emoji be
                        // used away from the guild they belong to. Read here
                        // because READY is the only place the payload carries
                        // it - `premium_type` 1 is Classic, 2 is Nitro, 3 is
                        // Basic, and all three buy the cross-guild use this
                        // cares about; 0 or absent is none.
                        let nitro = d["user"]["premium_type"].as_u64().unwrap_or(0) > 0;
                        state.runtime.set_emoji_unrestricted(&account_id, nitro);
                        // What a later reconnect needs to pick this session
                        // back up rather than starting over.
                        if let Some(session_id) = d["session_id"].as_str() {
                            // Also what an interaction has to name - see
                            // Runtime::set_discord_gateway_session.
                            state.runtime.set_discord_gateway_session(&account_id, session_id);
                            *resume = Some(Resume {
                                session_id: session_id.to_string(),
                                url: d["resume_gateway_url"]
                                    .as_str()
                                    .map(|u| format!("{u}/?v=10&encoding=json"))
                                    .unwrap_or_else(|| GATEWAY_URL.to_string()),
                                seq: msg.get("s").and_then(|v| v.as_u64()).unwrap_or(0),
                            });
                        }
                        if let Some(hash) = d["user"]["avatar"].as_str() {
                            let url = format!("https://cdn.discordapp.com/avatars/{}/{hash}.png", config.user_id);
                            if state.accounts.set_discord_avatar_url(&account_id, &url).unwrap_or(false) {
                                state.events.emit("accountAvatarChanged", json!({ "accountId": account_id, "avatarUrl": url }));
                            }
                        }

                        // What this account has silenced on Discord itself.
                        // Read before the guilds arrive, and applied again as
                        // each one does - see `mutes::apply`.
                        if !d["user_guild_settings"].is_null() {
                            mutes::note_settings(state, &account_id, &d["user_guild_settings"]);
                        }

                        // Friends list: READY's own `relationships` array
                        // (type 1 = friend - 2/3/4 are blocked/incoming-
                        // request/outgoing-request, not shown here) plus
                        // whatever initial status `presences` already
                        // carries for each. Not every friend necessarily has
                        // a presences entry yet at this point (Discord only
                        // guarantees one once a PRESENCE_UPDATE has actually
                        // been seen for them this session) - those default
                        // to "offline" until their first PRESENCE_UPDATE
                        // arrives, same as a real client briefly shows before
                        // its own presence subscription catches up.
                        // Whatever READY already knows about who is around.
                        // Used for the friends list and for direct message
                        // rosters alike: without it every conversation opens
                        // showing the other person offline until they happen
                        // to change status, which for somebody idle all day
                        // is never.
                        let presences: HashMap<&str, &str> = d["presences"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|p| Some((p["user"]["id"].as_str()?, p["status"].as_str().unwrap_or("offline"))))
                            .collect();

                        if let Some(relationships) = d["relationships"].as_array() {
                            let friends: Vec<Value> = relationships
                                .iter()
                                // 1 is a friend, 3 a request somebody sent
                                // this account, 4 one it sent. All three
                                // belong in the list: an incoming request
                                // that is not shown is one that cannot be
                                // answered, which is how it used to be.
                                // 2 is blocked, which is not a friend and is
                                // not waiting for anything.
                                .filter(|r| matches!(r["type"].as_i64(), Some(1) | Some(3) | Some(4)))
                                .filter_map(|r| Some(friend_json(r, &presences)?))
                                .collect();
                            state.runtime.set_discord_friends(&account_id, friends);
                        }

                        let mut new_dm_buffers: Vec<(String, String)> = Vec::new();
                        if let Some(dms) = d["private_channels"].as_array() {
                            for ch in dms {
                                if let Some(pair) = register_dm_channel(state, &account_id, ch, channel_map, &presences) {
                                    new_dm_buffers.push(pair);
                                }
                            }
                        }
                        // Conversations are the ones people notice a gap in,
                        // and there are few enough of them to ask about
                        // directly. Channels wait until opened - a few hundred
                        // requests on every connect would be rate-limited and
                        // mostly wasted.
                        spawn_dm_catch_up(state.clone(), config.clone(), channel_map.clone());

                        state.runtime.set_conn_state(state, &account_id, ConnState::Connected, None);
                        tracing::info!("discord[{account_id}]: ready as {username}");

                        // For an account in relatively few guilds, READY's own
                        // `guilds` array already carries full guild objects
                        // (name + channels) directly - confirmed live, zero
                        // GUILD_CREATE dispatches ever arrived for a 4-guild
                        // account despite ready.guilds having all 4 with
                        // channels embedded. GUILD_CREATE (below) still
                        // exists as a fallback for accounts where Discord
                        // *does* send guilds separately (a documented
                        // behavior for large accounts) or a guild joined
                        // mid-session - register_guild_channels no-ops
                        // harmlessly on a guild it's already seen via either
                        // path (channel_map dedup).
                        if let Some(guilds) = d["guilds"].as_array() {
                            for g in guilds {
                                register_guild_channels(state, config, g, channel_map).await;
                            }
                        }

                        // READY's own private_channels array isn't reliably
                        // complete for user accounts (confirmed live: two
                        // active DM conversations kept getting MESSAGE_UPDATE
                        // traffic for channel ids that never appeared there) -
                        // fetch the full list over REST as a follow-up rather
                        // than trusting the gateway snapshot alone.
                        match http_client().get(format!("{API_BASE}/users/@me/channels")).header("Authorization", &config.token).send().await {
                            Ok(resp) => {
                                let body = resp.text().await.unwrap_or_default();
                                match serde_json::from_str::<Vec<Value>>(&body) {
                                    Ok(channels) => {
                                        for ch in &channels {
                                            if let Some(pair) = register_dm_channel(state, &account_id, ch, channel_map, &presences) {
                                                new_dm_buffers.push(pair);
                                            }
                                        }
                                    }
                                    Err(e) => tracing::warn!("discord[{account_id}]: parsing /users/@me/channels: {e}"),
                                }
                            }
                            Err(e) => tracing::warn!("discord[{account_id}]: fetching /users/@me/channels: {e}"),
                        }

                        // The direct messages now exist, so the settings
                        // read at the top of READY have somewhere to land.
                        mutes::apply(state, &account_id);

                        spawn_backfill(state.clone(), config.token.clone(), config.user_id.clone(), config.display_name.clone(), new_dm_buffers);
                    }
                    "GUILD_CREATE" => {
                        register_guild_channels(state, config, d, channel_map).await;
                        // The channels this guild's settings were about now
                        // exist. READY carried the mutes before any of them
                        // did, so this is where most of them actually land.
                        mutes::apply(state, &account_id);
                        // Kept so a channel created later can be placed
                        // without re-fetching the guild: naming it and
                        // deciding whether this account may see it both
                        // need the guild's roles and our own membership,
                        // which arrive only here.
                        if let Some(guild_id) = d["id"].as_str() {
                            // Kept in the runtime as well as here: the
                            // gateway task owns this map, and a profile
                            // asked for from an RPC needs the same roles to
                            // say what somebody is in a guild.
                            state.runtime.set_discord_guild_roles(guild_id, d["roles"].as_array().cloned().unwrap_or_default());
                            guild_context.insert(guild_id.to_string(), d.clone());
                        }
                    }

                    // A channel appearing, being renamed, or going away
                    // while connected. Without these the channel list was
                    // whatever it had been at connect: a new channel never
                    // showed, a deleted one stayed, and a rename never
                    // landed - all of it only fixed by restarting.
                    // A thread started, renamed, or gone. Unhandled until
                    // now, which is why a thread only ever appeared if this
                    // account was already in it when the client connected.
                    "THREAD_CREATE" | "THREAD_UPDATE" | "THREAD_LIST_SYNC" => {
                        let Some(guild_id) = d["guild_id"].as_str() else { continue };
                        let Some(guild) = guild_context.get(guild_id).cloned() else { continue };
                        // Registered by re-running the guild with these
                        // threads on it, so the naming, the heading and the
                        // permission check are the same code that ran at
                        // connect rather than a second copy of it.
                        let threads = match t {
                            "THREAD_LIST_SYNC" => d["threads"].as_array().cloned().unwrap_or_default(),
                            _ => vec![d.clone()],
                        };
                        // A rename is a new name, and a buffer's identity is
                        // its name - so the old one goes first or both would
                        // sit in the list.
                        if t == "THREAD_UPDATE" {
                            if let Some(id) = d["id"].as_str() {
                                if let Some((old_name, _)) = channel_map.get(id).cloned() {
                                    let renamed = d["name"].as_str().map(|n| format!("{}/#{n}", guild["name"].as_str().unwrap_or("guild")));
                                    if renamed.as_deref() != Some(old_name.as_str()) {
                                        state.runtime.remove_buffer(state, &crate::model::buffer_id(&account_id, &old_name));
                                        channel_map.remove(id);
                                    }
                                }
                            }
                        }
                        let mut one = guild.clone();
                        one["threads"] = serde_json::Value::Array(threads);
                        register_guild_channels(state, config, &one, channel_map).await;
                    }

                    // Deleted, or archived out of reach. Either way it is no
                    // longer somewhere to talk, and a buffer left behind
                    // would be one whose sends all fail.
                    "THREAD_DELETE" => {
                        let Some(id) = d["id"].as_str() else { continue };
                        if let Some((name, _)) = channel_map.remove(id) {
                            state.runtime.remove_buffer(state, &crate::model::buffer_id(&account_id, &name));
                        }
                    }

                    "CHANNEL_CREATE" | "CHANNEL_UPDATE" => {
                        let Some(channel_id) = d["channel_id"].as_str().or_else(|| d["id"].as_str()) else { continue };
                        match d["guild_id"].as_str() {
                            // A guild channel is placed by re-running the
                            // guild's own registration with this one channel
                            // in it, so naming, category ordering and the
                            // permission check are the same code that ran at
                            // connect rather than a second copy of it.
                            Some(guild_id) => {
                                let Some(guild) = guild_context.get(guild_id).cloned() else { continue };
                                // A rename has to drop the old buffer first:
                                // the buffer's identity is its name, so the
                                // new one would otherwise appear alongside
                                // the old rather than replace it.
                                if let Some((old_name, _)) = channel_map.get(channel_id).cloned() {
                                    let new_name = d["name"].as_str().map(|n| format!("{}/#{n}", guild["name"].as_str().unwrap_or("guild")));
                                    if new_name.as_deref() == Some(old_name.as_str()) {
                                        continue;
                                    }
                                    state.runtime.remove_buffer(state, &crate::model::buffer_id(&account_id, &old_name));
                                    channel_map.remove(channel_id);
                                }
                                let mut one = guild.clone();
                                one["channels"] = serde_json::Value::Array(vec![d.clone()]);
                                register_guild_channels(state, config, &one, channel_map).await;
                                // A channel made while connected inherits its
                                // guild's mute, which is already known - so
                                // it arrives quiet rather than becoming quiet
                                // after the first thing said in it.
                                mutes::apply(state, &account_id);
                                // An update can also be a permission change,
                                // which can take access away as easily as
                                // give it - and taking it away means removing
                                // a channel, which the single-channel path
                                // above cannot do.
                                if t == "CHANNEL_UPDATE" {
                                    resync_guild(state, config, guild_id, channel_map).await;
                                }
                            }
                            // A DM or group DM opened from another client.
                            // No presence snapshot to seed it with - that
                            // only exists in READY - and none is needed:
                            // PRESENCE_UPDATE fills it in from here on.
                            None => {
                                let presences: HashMap<&str, &str> = HashMap::new();
                                register_dm_channel(state, &account_id, d, channel_map, &presences);
                                mutes::apply(state, &account_id);
                            }
                        }
                    }

                    // Somebody started typing. Discord sends no matching
                    // "stopped" event - a client is expected to forget after
                    // about ten seconds, or when a message from that person
                    // arrives - so the expiry is carried rather than left for
                    // each frontend to invent its own.
                    "TYPING_START" => {
                        let Some(channel_id) = d["channel_id"].as_str() else { continue };
                        let Some(user_id) = d["user_id"].as_str() else { continue };
                        if user_id == config.user_id {
                            continue;
                        }
                        let Some((name, _)) = channel_map.get(channel_id) else { continue };
                        // A guild event carries the member; a DM carries no
                        // member object at all, which is why the remembered
                        // name matters rather than being a nicety. Empty
                        // strings are skipped so a blank nickname does not
                        // win over a name we actually know.
                        let nick = d["member"]["nick"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or_else(|| d["member"]["user"]["global_name"].as_str().filter(|s| !s.is_empty()))
                            .or_else(|| d["member"]["user"]["username"].as_str().filter(|s| !s.is_empty()))
                            .map(str::to_string)
                            .or_else(|| state.runtime.discord_known_name(&account_id, user_id))
                            .unwrap_or_else(|| "Someone".to_string());
                        state.events.emit(
                            "typing",
                            json!({
                                "accountId": account_id,
                                "bufferId": crate::model::buffer_id(&account_id, name),
                                "nick": nick,
                                "expiresInMs": TYPING_TTL_MS,
                            }),
                        );
                    }

                    // Read somewhere else. Discord sends this to every one
                    // of an account's sessions, including the one that did
                    // the acking, so a client acting on it also settles its
                    // own state rather than needing to guess.
                    "MESSAGE_ACK" => {
                        let Some(channel_id) = d["channel_id"].as_str() else { continue };
                        let Some((name, _)) = channel_map.get(channel_id) else { continue };
                        state.events.emit(
                            "bufferRead",
                            json!({
                                "accountId": account_id,
                                "bufferId": crate::model::buffer_id(&account_id, name),
                            }),
                        );
                    }

                    // Somebody asked to be friends, was accepted, or is
                    // gone. Unhandled until now, so a request that arrived
                    // while the client was running stayed invisible until the
                    // next restart - and one answered elsewhere stayed in the
                    // list until then too.
                    "RELATIONSHIP_ADD" => {
                        let mut friends = state.runtime.get_discord_friends(&account_id);
                        if let Some(user_id) = d["user"]["id"].as_str() {
                            friends.retain(|f| f["userId"].as_str() != Some(user_id));
                        }
                        if let Some(row) = friend_json(d, &HashMap::new()) {
                            friends.push(row);
                        }
                        state.runtime.set_discord_friends(&account_id, friends.clone());
                        state.events.emit("discordFriends", json!({ "accountId": account_id, "friends": friends }));
                    }

                    "RELATIONSHIP_REMOVE" => {
                        let Some(user_id) = d["id"].as_str() else { continue };
                        let mut friends = state.runtime.get_discord_friends(&account_id);
                        friends.retain(|f| f["userId"].as_str() != Some(user_id));
                        state.runtime.set_discord_friends(&account_id, friends.clone());
                        state.events.emit("discordFriends", json!({ "accountId": account_id, "friends": friends }));
                    }

                    // Somebody joined or left a group message. Not
                    // necessarily by anything this client did: the whole
                    // point is the change made from a phone, or by another
                    // person in the group, which this had no way to hear.
                    "CHANNEL_RECIPIENT_ADD" | "CHANNEL_RECIPIENT_REMOVE" => {
                        let Some(channel_id) = d["channel_id"].as_str() else { continue };
                        people::recipient_changed(
                            state,
                            &account_id,
                            channel_id,
                            &d["user"],
                            t == "CHANNEL_RECIPIENT_ADD",
                        );
                    }

                    "CHANNEL_DELETE" => {
                        let Some(channel_id) = d["id"].as_str() else { continue };
                        if let Some((name, _)) = channel_map.remove(channel_id) {
                            state.runtime.remove_buffer(state, &crate::model::buffer_id(&account_id, &name));
                        }
                    }

                    // Left from another client, kicked, or the guild itself
                    // deleted. The "unavailable" form is an outage rather
                    // than a departure, and taking the channels away for
                    // one would look identical to being removed.
                    // A role's permissions changed, or a role appeared or
                    // went away. Any of those can change which channels this
                    // account may read, and the list is otherwise whatever
                    // it was at connect.
                    "GUILD_ROLE_CREATE" | "GUILD_ROLE_UPDATE" | "GUILD_ROLE_DELETE" => {
                        let Some(guild_id) = d["guild_id"].as_str() else { continue };
                        if guild_context.contains_key(guild_id) {
                            resync_guild(state, config, guild_id, channel_map).await;
                        }
                    }

                    // What this account may see inside a guild has changed.
                    // Agreeing to a server's rules is the case that matters:
                    // every channel becomes visible at once, and the list
                    // here was built at connect and never revisited.
                    //
                    // Handled as an event rather than after our own accept
                    // call, so it also covers the rules being agreed to in
                    // Discord's own client while this one is running.
                    "GUILD_MEMBER_UPDATE" => {
                        let Some(guild_id) = d["guild_id"].as_str() else { continue };
                        if d["user"]["id"].as_str() != Some(config.user_id.as_str()) {
                            continue;
                        }
                        if d["pending"].as_bool().unwrap_or(false) {
                            continue;
                        }
                        state.runtime.clear_discord_guild_pending(state, &account_id, guild_id);
                        // The gate lifting is one reason to re-read; our own
                        // roles changing is the other, since that is what
                        // decides which channels are permitted. A nickname
                        // change arrives here too and is no reason at all,
                        // so the roles are compared rather than assumed.
                        let was_gated = state.runtime.take_discord_gated(&guild_group_id(&account_id, guild_id));
                        let roles_now: Vec<String> = d["roles"].as_array().into_iter().flatten().filter_map(|r| r.as_str().map(String::from)).collect();
                        let roles_changed = state.runtime.set_discord_own_roles(&guild_group_id(&account_id, guild_id), roles_now);
                        if was_gated || roles_changed {
                            resync_guild(state, config, guild_id, channel_map).await;
                        }
                    }

                    "GUILD_DELETE" => {
                        let Some(guild_id) = d["id"].as_str() else { continue };
                        if d["unavailable"].as_bool() == Some(true) {
                            continue;
                        }
                        guild_context.remove(guild_id);
                        for buffer_id in state.runtime.discord_buffers_in_guild(&account_id, guild_id) {
                            if let Some(channel_id) = state.runtime.get_discord_channel(&buffer_id) {
                                channel_map.remove(&channel_id);
                            }
                            state.runtime.remove_buffer(state, &buffer_id);
                        }
                        state.runtime.remove_buffer_group(state, &guild_group_id(&account_id, guild_id));
                    }
                    "MESSAGE_CREATE" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, kind)) = channel_map.get(channel_id).cloned() else { continue };
                        let author = &d["author"];
                        let from = author["global_name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or_else(|| author["username"].as_str())
                            .unwrap_or("unknown")
                            .to_string();
                        let embeds = extract_embeds(d);
                        let mut attachments = extract_attachments(d);
                        attachments.extend(extract_stickers(d));
                        // A forwarded picture is a picture, and belongs in the message the
                        // way any other attachment does.
                        attachments.extend(forwarded_attachments(d));
                        let body = extract_body(d).unwrap_or_default();
                        if is_empty_message(&body, &embeds, &attachments) {
                            continue;
                        }
                        let body = resolve_mentions(&body, d, &config.user_id, config.display_name.as_deref());
                        let is_mention = mentions_own_user(d, &config.user_id);
                        let reply_to = extract_reply(d);
                        let real_msg_id = d["id"].as_str().map(|s| s.to_string());
                        let avatar_url = author_avatar_url(author);
                        // Kept against their id as well as put on the message.
                        // A direct call's voice states carry no member object,
                        // so a face seen here is the only one its call view
                        // will ever have to show.
                        if let (Some(id), Some(url)) = (author["id"].as_str(), avatar_url.as_deref()) {
                            state.runtime.remember_discord_avatar(&account_id, id, url);
                        }
                        // Discord's gateway echoes a user's own sent messages
                        // back through this same dispatch (that's how its
                        // official multi-device sync works) - unlike IRC,
                        // sendMessage below deliberately does NOT also
                        // record locally, so this is the only place a sent
                        // message gets appended, exactly once. Passing
                        // Discord's own id through (rather than letting
                        // record_message generate one) is what lets a later
                        // edit/delete/reaction on this exact message find it.
                        // Cache previews before the links expire, so this
                        // message still shows its pictures when it is read
                        // back out of scrollback tomorrow.
                        let thumb_target = (
                            model::buffer_id(&account_id, &buffer_name),
                            real_msg_id.clone().unwrap_or_default(),
                            attachments.clone(),
                        );
                        // The author's id travels with the message. It is what a profile
                        // lookup asks Discord about - a display name is not something
                        // the API accepts - and what a moderation action would need.
                        state.runtime.record_message(state, &account_id, &buffer_name, &kind, &from, &body, false, "chat", reply_to, real_msg_id.clone(), is_mention, avatar_url, embeds, attachments, author["id"].as_str().map(str::to_string));
                        // Who wrote a forwarded message, which the snapshot
                        // does not say. A read of the original, so it happens
                        // after the message is already on screen rather than
                        // holding it up.
                        if d["message_snapshots"].is_array() {
                            if let Some(msg_id) = real_msg_id {
                                let state = state.clone();
                                let account_id = account_id.clone();
                                let buffer_id = model::buffer_id(&account_id, &buffer_name);
                                let reference = d["message_reference"].clone();
                                tokio::spawn(async move {
                                    name_forward(&state, &account_id, &buffer_id, &msg_id, &reference).await;
                                });
                            }
                        }
                        // The buttons under it, if it has any. After the
                        // message rather than with it: they are written onto
                        // the row that was just made, and a great many bot
                        // messages are nothing but their buttons.
                        if let Some(id) = d["id"].as_str() {
                            note_components(state, &thumb_target.0, id, d);
                            // A poll is drawn on a card as well as written
                            // into the log: the log records what was asked,
                            // the card is the thing that can be answered.
                            if polls::has_poll(d) {
                                polls::announce(state, &thumb_target.0, id, d);
                            }
                        }
                        if !thumb_target.1.is_empty() {
                            cache_thumbnails(state.clone(), thumb_target.0, thumb_target.1, thumb_target.2);
                        }
                    }
                    "MESSAGE_UPDATE" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, _)) = channel_map.get(channel_id).cloned() else { continue };
                        let Some(msg_id) = d["id"].as_str() else { continue };
                        let buffer_id = model::buffer_id(&account_id, &buffer_name);
                        // A vote - anybody's - comes back as an update to the
                        // message carrying the poll, with a recounted
                        // `results` in it. Before everything below, because
                        // that update has no edit stamp and no embeds, and
                        // would otherwise be dropped as nothing having
                        // happened.
                        if polls::has_poll(d) {
                            polls::announce(state, &buffer_id, msg_id, d);
                        }
                        // No edited_timestamp means nobody edited anything:
                        // this is Discord re-sending the message once it has
                        // finished unfurling a link. That used to be dropped,
                        // so a preview that took a second to resolve never
                        // appeared at all - and most of them take a second.
                        //
                        // Only the embeds are taken from it. The update is a
                        // partial object: it carries no content, and reading
                        // a body out of it would replace what somebody wrote
                        // with nothing.
                        if d["edited_timestamp"].is_null() {
                            let embeds = extract_embeds(d);
                            if embeds.is_empty() {
                                continue;
                            }
                            let Ok(Some(stored)) = state.store.get_message(&buffer_id, msg_id) else { continue };
                            state.runtime.update_message(state, &buffer_id, msg_id, &stored.body, &embeds, &stored.attachments);
                            continue;
                        }
                        let embeds = extract_embeds(d);
                        let mut attachments = extract_attachments(d);
                        attachments.extend(extract_stickers(d));
                        // A forwarded picture is a picture, and belongs in the message the
                        // way any other attachment does.
                        attachments.extend(forwarded_attachments(d));
                        let body = extract_body(d).unwrap_or_default();
                        let body = resolve_mentions(&body, d, &config.user_id, config.display_name.as_deref());
                        state.runtime.update_message(state, &buffer_id, msg_id, &body, &embeds, &attachments);
                        // A bot editing its own message routinely swaps the
                        // buttons - a poll that closes, a page that turns.
                        note_components(state, &buffer_id, msg_id, d);
                    }
                    "MESSAGE_DELETE" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, _)) = channel_map.get(channel_id).cloned() else { continue };
                        let Some(msg_id) = d["id"].as_str() else { continue };
                        let buffer_id = model::buffer_id(&account_id, &buffer_name);
                        state.runtime.delete_message(state, &buffer_id, msg_id);
                    }
                    "MESSAGE_REACTION_ADD" | "MESSAGE_REACTION_REMOVE" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, _)) = channel_map.get(channel_id).cloned() else { continue };
                        let Some(msg_id) = d["message_id"].as_str() else { continue };
                        // Discord's own `<:name:id>` shorthand for custom
                        // emoji - unambiguous, and lets the frontend later
                        // regex-detect it to render the actual image via
                        // the emoji CDN. Plain unicode emoji (no id) pass
                        // through as-is and render natively in any font.
                        let emoji_name = d["emoji"]["name"].as_str().unwrap_or("?");
                        let emoji_key = match d["emoji"]["id"].as_str() {
                            Some(id) => format!("<:{emoji_name}:{id}>"),
                            None => emoji_name.to_string(),
                        };
                        let is_me = d["user_id"].as_str() == Some(config.user_id.as_str());
                        let buffer_id = model::buffer_id(&account_id, &buffer_name);
                        state.runtime.update_reaction(state, &buffer_id, msg_id, &emoji_key, is_me, t == "MESSAGE_REACTION_ADD");
                    }
                    // A server renamed, re-iconed, or changed its emoji. All
                    // three live in the guild object this client registered
                    // at connect, so the cheapest correct answer is to
                    // register it again - which is what the role events
                    // already do for the same reason.
                    "GUILD_UPDATE" | "GUILD_EMOJIS_UPDATE" => {
                        let Some(guild_id) = d["guild_id"].as_str().or_else(|| d["id"].as_str()) else { continue };
                        if guild_context.contains_key(guild_id) {
                            resync_guild(state, config, guild_id, channel_map).await;
                        }
                    }

                    // Our own name or picture changed - which is on every
                    // message we send, and was invisible here until a
                    // restart.
                    "USER_UPDATE" => {
                        if d["id"].as_str() != Some(config.user_id.as_str()) {
                            continue;
                        }
                        let name = d["global_name"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .or_else(|| d["username"].as_str())
                            .unwrap_or_default();
                        if !name.is_empty() {
                            let _ = state.accounts.set_display_name(&account_id, name);
                        }
                        if let (Some(avatar), Some(id)) = (d["avatar"].as_str(), d["id"].as_str()) {
                            let url = format!("https://cdn.discordapp.com/avatars/{id}/{avatar}.png?size=128");
                            if state.accounts.set_discord_avatar_url(&account_id, &url).unwrap_or(false) {
                                state.events.emit("accountAvatarChanged", json!({ "accountId": account_id, "avatarUrl": url }));
                            }
                        }
                    }

                    // Somebody joined or left the server. The member window is
                    // a lazy view Discord only sends when asked, so asking
                    // again is the whole of keeping it right - and only for a
                    // guild whose list is actually on screen.
                    // A bot answering a command with a form rather than a
                    // message. Passed straight up: the window draws it, and
                    // whatever comes back goes out as an interaction of its
                    // own - see submit_modal.
                    "INTERACTION_MODAL_CREATE" => {
                        let modal = if d["modal"].is_object() { &d["modal"] } else { d };
                        let channel = modal["channel_id"].as_str().or_else(|| d["channel_id"].as_str());
                        // Where it belongs. A modal names its channel; where
                        // it does not, it belongs to whatever this account
                        // last asked for - which is the command that opened
                        // it, moments ago.
                        let buffer_id = channel
                            .and_then(|c| state.runtime.discord_buffer_for_channel(&account_id, c))
                            .or_else(|| state.runtime.discord_last_interaction(&account_id));
                        if let Some(buffer_id) = buffer_id {
                            state.events.emit(
                                "discordModal",
                                json!({
                                    "accountId": account_id,
                                    "bufferId": buffer_id,
                                    "id": modal["id"],
                                    "customId": modal["custom_id"],
                                    "applicationId": modal["application_id"].as_str().or_else(|| d["application_id"].as_str()),
                                    "title": modal["title"].as_str().unwrap_or("Fill this in"),
                                    "fields": modal_fields(modal),
                                }),
                            );
                        }
                    }

                    // Somebody pinned or unpinned something. Discord says
                    // only that the list changed, never what it changed to -
                    // so the list is asked for again, which also tells every
                    // window watching the conversation.
                    //
                    // Only for a channel this client is actually showing: a
                    // pin in a guild's four hundredth channel is not news
                    // worth a request, and the list is fetched on opening a
                    // conversation anyway.
                    "CHANNEL_PINS_UPDATE" => {
                        if let Some(channel_id) = d["channel_id"].as_str() {
                            if let Some(buffer_id) = state.runtime.discord_buffer_for_channel(&account_id, channel_id) {
                                let state = state.clone();
                                let account_id = account_id.clone();
                                // In its own task: the gateway loop must keep
                                // reading, and this is an HTTP round trip.
                                tokio::spawn(async move {
                                    if let Err(e) = list_pinned(&state, &account_id, &buffer_id).await {
                                        tracing::debug!("discord[{account_id}]: re-reading pins for {buffer_id}: {e:#}");
                                    }
                                });
                            }
                        }
                    }

                    // A server or a channel muted - or unmuted - in another
                    // client. One entry, carrying that guild's whole setting.
                    "USER_GUILD_SETTINGS_UPDATE" => {
                        mutes::note_settings(state, &account_id, d);
                    }

                    "GUILD_MEMBER_ADD" | "GUILD_MEMBER_REMOVE" => {
                        let Some(guild_id) = d["guild_id"].as_str() else { continue };
                        if let Some(buffer_id) = state.runtime.discord_member_list_target(&account_id, guild_id) {
                            request_member_list(state, &buffer_id);
                        }
                    }

                    // A purge. Without this every message in it stayed on
                    // screen, since the per-message deletes are not sent.
                    "MESSAGE_DELETE_BULK" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, _)) = channel_map.get(channel_id).cloned() else { continue };
                        let buffer_id = model::buffer_id(&account_id, &buffer_name);
                        for id in d["ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                            state.runtime.delete_message(state, &buffer_id, id);
                        }
                    }

                    // Reactions cleared wholesale - all of them, or every one
                    // of a single emoji. Neither sends the per-user removals
                    // that would otherwise take them off screen.
                    "MESSAGE_REACTION_REMOVE_ALL" | "MESSAGE_REACTION_REMOVE_EMOJI" => {
                        let channel_id = d["channel_id"].as_str().unwrap_or_default();
                        let Some((buffer_name, _)) = channel_map.get(channel_id).cloned() else { continue };
                        let Some(msg_id) = d["message_id"].as_str() else { continue };
                        let buffer_id = model::buffer_id(&account_id, &buffer_name);
                        match d["emoji"]["name"].as_str().filter(|_| t == "MESSAGE_REACTION_REMOVE_EMOJI") {
                            Some(name) => {
                                let key = match d["emoji"]["id"].as_str() {
                                    Some(id) => format!("<:{name}:{id}>"),
                                    None => name.to_string(),
                                };
                                state.runtime.remove_reaction_entirely(state, &buffer_id, msg_id, &key);
                            }
                            None => state.runtime.clear_reactions(state, &buffer_id, msg_id),
                        }
                    }

                    "GUILD_MEMBER_LIST_UPDATE" => {
                        let Some(guild_id) = d["guild_id"].as_str() else { continue };
                        let Some(buffer_id) = state.runtime.discord_member_list_target(&account_id, guild_id) else { continue };
                        update_member_list(state, &buffer_id, d);
                    }

                    "VOICE_STATE_UPDATE" => {
                        let Some(user_id) = d["user_id"].as_str() else { continue };
                        let channel_id = d["channel_id"].as_str();
                        // The flags ride on the voice state - there is no
                        // separate dispatch for going live or turning a
                        // camera on, so dropping them here would mean nobody
                        // could ever be told a screen was being shared.
                        state.runtime.set_discord_voice_presence(
                            &account_id,
                            user_id,
                            channel_id,
                            voice_member_name(d),
                            voice_member_avatar(d).as_deref(),
                            crate::runtime::VoiceFlags::from_voice_state(d),
                        );
                        announce_voice_membership(state, &account_id, d["guild_id"].as_str(), channel_id);

                        if user_id == config.user_id {
                            // Our own move. The session id here is half of what
                            // a voice connection needs; VOICE_SERVER_UPDATE
                            // carries the other half.
                            state.runtime.set_discord_voice_self(&account_id, channel_id);
                            state.events.emit(
                                "discordVoiceState",
                                json!({
                                    "accountId": account_id,
                                    "channelId": channel_id,
                                    "sessionId": d["session_id"].as_str()
                                }),
                            );
                            voice::note_voice_state(
                                state,
                                &account_id,
                                d["guild_id"].as_str(),
                                channel_id,
                                d["session_id"].as_str(),
                            )
                            .await;
                        } else if let (Some(ours), Some(theirs)) = (state.runtime.discord_voice_self(&account_id), channel_id) {
                            // Somebody else arrived where we are. Leaving is
                            // the daemon's job rather than the caller's: by the
                            // time a client could react it would already have
                            // been in a channel with a stranger.
                            if ours == theirs && state.voice.options(&account_id).solo {
                                tracing::info!("discord[{account_id}]: leaving voice - another user joined");
                                leave_voice(state, &account_id);
                                state.events.emit(
                                    "discordVoiceLeft",
                                    json!({ "accountId": account_id, "reason": "someone else joined", "userId": user_id }),
                                );
                            }
                        }
                    }

                    // Somebody is calling this account, or has stopped.
                    //
                    // A call in a DM is announced with the set of people whose
                    // clients should be ringing. Our own id being in it is the
                    // whole signal: CALL_CREATE for a call we placed lists the
                    // other person, not us. The set shrinks as people answer
                    // or decline, so the same check on CALL_UPDATE is what
                    // says the ringing has stopped - a caller who hangs up
                    // before being answered produces exactly that and no
                    // CALL_DELETE at all.
                    "CALL_CREATE" | "CALL_UPDATE" => {
                        let Some(channel_id) = d["channel_id"].as_str() else { continue };
                        let ringing = d["ringing"]
                            .as_array()
                            .map(|r| r.iter().any(|u| u.as_str() == Some(config.user_id.as_str())))
                            .unwrap_or(false);
                        announce_call(state, &account_id, channel_id, ringing);
                    }

                    "CALL_DELETE" => {
                        let Some(channel_id) = d["channel_id"].as_str() else { continue };
                        announce_call(state, &account_id, channel_id, false);
                    }

                    "VOICE_SERVER_UPDATE" => {
                        // The endpoint and token the audio connection is
                        // opened against. Announced to the client as well as
                        // used, because a frontend showing a call wants to
                        // know the session moved even though it is this
                        // daemon that carries the audio.
                        state.events.emit(
                            "discordVoiceServer",
                            json!({
                                "accountId": account_id,
                                "guildId": d["guild_id"].as_str(),
                                "endpoint": d["endpoint"].as_str(),
                                "hasToken": d["token"].as_str().is_some()
                            }),
                        );
                        voice::note_voice_server(
                            state,
                            &account_id,
                            d["guild_id"].as_str(),
                            d["endpoint"].as_str(),
                            d["token"].as_str(),
                        )
                        .await;
                    }

                    // A stream coming into being, changing, or ending -
                    // ours or anybody else's. Two of these carry the two
                    // halves of the handshake a stream connection needs, in
                    // either order, exactly as the voice ones above do.
                    "STREAM_CREATE" | "STREAM_SERVER_UPDATE" | "STREAM_UPDATE" => {
                        golive::note_stream(state, &account_id, t, d).await;
                    }

                    "STREAM_DELETE" => {
                        golive::note_stream_gone(state, &account_id, d);
                    }

                    "PRESENCE_UPDATE" => {
                        // Fires for guild members too, not just friends -
                        // update_discord_presence itself is the friends-only
                        // gate (a no-op, no event emitted, for any user_id
                        // not already in this account's friends list).
                        let Some(user_id) = d["user"]["id"].as_str() else { continue };
                        let status = d["status"].as_str().unwrap_or("offline");
                        if state.runtime.update_discord_presence(&account_id, user_id, status) {
                            state.events.emit("discordPresenceUpdate", json!({ "accountId": account_id, "userId": user_id, "status": status }));
                        }
                        // Also refresh any roster this person appears in, so
                        // an open channel's list follows them going online or
                        // away without waiting for a fresh subscription.
                        update_presence_in_rosters(state, user_id, status);
                    }
                    _ => {}
                }
            }
            7 => bail!("gateway requested a reconnect"),
            // The resume was refused, or the session is gone. Everything
            // carried across goes with it: the next connection gets a fresh
            // READY and GUILD_CREATE, and keeping stale maps would mean
            // holding buffers for channels this account may no longer be in.
            9 => {
                *resume = None;
                channel_map.clear();
                guild_context.clear();
                bail!("session invalidated by gateway")
            }
            // Heartbeat ack - clears the flag heartbeat_task checks before
            // sending the *next* one, so a real ack landing between ticks
            // is exactly what keeps this connection from ever being
            // declared zombied in the first place.
            11 => ack_pending.store(false, Ordering::SeqCst),
            _ => {}
        }
    }
}
