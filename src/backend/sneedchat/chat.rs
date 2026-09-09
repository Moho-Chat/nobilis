//! One websocket per room, which is what this protocol allows.
//!
//! A connection here joins exactly one room, so an account in three rooms
//! holds three sockets, each with its own reconnect. The frames they carry
//! are the same shape whichever room they came from.

use super::*;

pub(super) const MAX_WS_REDIRECTS: usize = 3;

/// Opens the chat websocket, following redirects manually (tungstenite
/// doesn't) and presenting the same identity headers a browser on the chat
/// page would - in particular `Origin`, without which the site's anti-bot
/// proxy silently drops the upgrade instead of completing or rejecting it
/// (confirmed live: omitting it just hangs forever with no error at all).
/// Ported from sneedchat-rs's `chat/socket.rs::connect`.
pub(super) async fn open_chat_websocket(transport: &Transport, host: &str, cookie_header: &str) -> Result<WebSocketStream<BoxStream>> {
    let mut url = format!("wss://{host}/chat.ws");
    let origin = format!("https://{host}");

    for _ in 0..=MAX_WS_REDIRECTS {
        let parsed = url::Url::parse(&url).with_context(|| format!("parsing {url}"))?;
        let host = parsed.host_str().with_context(|| format!("{url} has no host"))?.to_string();
        let tls = matches!(parsed.scheme(), "wss" | "https");
        let port = parsed.port().unwrap_or(if tls { 443 } else { 80 });

        let stream = transport.connect(&host, port, tls).await.context("opening chat websocket transport")?;
        let mut request = url.as_str().into_client_request()?;
        let headers = request.headers_mut();
        headers.insert("User-Agent", HeaderValue::from_str(DEFAULT_USER_AGENT)?);
        // Same-origin request, as a browser on the chat page would send -
        // without this the anti-bot proxy in front of the site never
        // completes (or rejects) the upgrade at all.
        headers.insert("Origin", HeaderValue::from_str(&origin)?);
        if !cookie_header.is_empty() {
            headers.insert("Cookie", HeaderValue::from_str(cookie_header).context("cookie header")?);
        }

        match tokio_tungstenite::client_async(request, stream).await {
            Ok((ws, _resp)) => return Ok(ws),
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
                let status = resp.status();
                if status.as_u16() == pow::GATE_STATUS {
                    bail!("blocked by the anti-bot gate during the chat handshake");
                }
                let location = resp.headers().get(hyper::header::LOCATION).and_then(|v| v.to_str().ok()).map(str::to_string);
                match (status.is_redirection(), location) {
                    (true, Some(loc)) => {
                        url = resolve_ws_redirect(&url, &loc)?;
                        tracing::info!("sneedchat: websocket redirected to {url}");
                    }
                    _ => bail!("chat handshake rejected with HTTP {status}"),
                }
            }
            Err(e) => return Err(anyhow::Error::new(e).context("chat websocket handshake")),
        }
    }
    bail!("chat handshake redirected more than {MAX_WS_REDIRECTS} times (last: {url})")
}

/// Resolves a redirect target against the current URL, normalising an
/// `http(s)` location back to `ws(s)` so the handshake stays a websocket one.
pub(super) fn resolve_ws_redirect(base: &str, location: &str) -> Result<String> {
    let joined = url::Url::parse(base)?.join(location)?;
    let mut out = joined.clone();
    let scheme = match joined.scheme() {
        "http" => Some("ws"),
        "https" => Some("wss"),
        _ => None,
    };
    if let Some(s) = scheme {
        out.set_scheme(s).map_err(|_| anyhow!("could not rewrite scheme of {joined}"))?;
    }
    Ok(out.to_string())
}

pub(super) const ROOM_RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);

pub(super) const ROOM_RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// One room's own indefinite reconnect loop, reusing the account-wide
/// shared `session` (cloning it is cheap - its cookie jar is Arc-shared,
/// so a refresh done here is immediately visible to every other room's
/// next attempt too).
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_room(
    state: &AppState,
    transport: &Transport,
    session: &Session,
    creds: &Credentials,
    two_factor: &TwoFactor,
    account_id: &str,
    host: &str,
    room: &SneedChatRoom,
    is_primary: bool,
) {
    let mut delay = ROOM_RECONNECT_INITIAL_DELAY;
    loop {
        let result = std::panic::AssertUnwindSafe(run_room_once(state, transport, session, account_id, host, room, is_primary)).catch_unwind().await;
        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("sneedchat[{account_id}] room {} ({}): {e:#}", room.id, room.name);
                // A stale session is the most likely reason one specific
                // room starts failing while the others keep working -
                // refresh before retrying rather than hammering the same
                // rejected cookies every backoff cycle.
                if let Err(re) = session.refresh(creds, two_factor).await {
                    tracing::warn!("sneedchat[{account_id}] room {}: session refresh failed: {re:#}", room.id);
                }
            }
            Err(_) => tracing::error!("sneedchat[{account_id}] room {} ({}): task panicked", room.id, room.name),
        }
        // No cleanup call here - run_room_once's own RemoveSenderOnDrop
        // guard already ran (on every return path, including a panic) by
        // the time this line is reached, and it's the one that actually
        // knows which specific sender to remove (see its own doc comment
        // on why an unconditional remove-by-key here would be unsafe).
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(ROOM_RECONNECT_MAX_DELAY);
    }
}

pub(super) async fn run_room_once(state: &AppState, transport: &Transport, session: &Session, account_id: &str, host: &str, room: &SneedChatRoom, is_primary: bool) -> Result<()> {
    let cookie_header = session.cookie_header().unwrap_or_default();
    let ws = open_chat_websocket(transport, host, &cookie_header).await?;
    let (sink, mut incoming) = ws.split();

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

    state.runtime.insert_sneedchat_sender(account_id, room.id, out_tx.clone());
    struct RemoveSenderOnDrop<'a> {
        state: &'a AppState,
        account_id: &'a str,
        room: u32,
        sender: tokio::sync::mpsc::UnboundedSender<String>,
    }
    impl Drop for RemoveSenderOnDrop<'_> {
        fn drop(&mut self) {
            self.state.runtime.remove_sneedchat_sender(self.account_id, self.room, &self.sender);
        }
    }
    let _sender_guard = RemoveSenderOnDrop { state, account_id, room: room.id, sender: out_tx.clone() };

    out_tx.send(format!("/join {}", room.id)).map_err(|_| anyhow!("chat socket closed before joining"))?;
    let buffer_name = room_buffer_name(&room.name);
    // Created eagerly rather than waiting for the first live message - a
    // quiet room would otherwise show no buffer/tab at all despite being
    // connected and joined.
    state.runtime.ensure_buffer(state, account_id, &buffer_name, "channel");
    // The server follows a join with the room's whole roster, so anything left
    // from a previous connection would only be stale.
    state.runtime.set_presence(&crate::model::buffer_id(account_id, &buffer_name), serde_json::json!([]));
    tracing::info!("sneedchat[{account_id}]: joined room {} ({})", room.id, room.name);

    loop {
        // Bounded, not a bare `.await` - a Tor circuit that's gone quietly
        // bad (no clean Close frame, no error, just nothing ever arriving
        // again) would otherwise leave this loop parked forever with no way
        // to notice the outage at all, let alone reconnect from it. Any
        // inbound traffic at all (a real message, or even a Ping/Pong the
        // `Some(Ok(_)) => continue` arm below swallows) resets this, so a
        // genuinely quiet-but-alive room doesn't get penalized - this only
        // fires when literally nothing has come through the socket.
        let frame = match tokio::time::timeout(IDLE_TIMEOUT, incoming.next()).await {
            Ok(Some(Ok(WsMessage::Text(text)))) => text.to_string(),
            Ok(Some(Ok(WsMessage::Close(frame)))) => bail!("connection closed: {frame:?}"),
            Ok(Some(Ok(_))) => continue,
            Ok(Some(Err(e))) => bail!("websocket error: {e}"),
            Ok(None) => bail!("connection ended unexpectedly"),
            Err(_) => bail!("no activity for {}s, assuming the connection is dead", IDLE_TIMEOUT.as_secs()),
        };
        handle_frame(state, &session.http, host, account_id, &buffer_name, is_primary, &frame).await?;
    }
}

/// How long to go without any inbound websocket activity (a message, or
/// even just a Ping/Pong) before giving up on this connection and forcing a
/// reconnect - see the read loop above. Comfortably above any plausible
/// server-side heartbeat interval, so this only ever fires for a genuinely
/// stalled connection, not a quiet room.
pub(super) const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) async fn handle_frame(state: &AppState, http: &http::HttpClient, host: &str, account_id: &str, buffer_name: &str, is_primary: bool, frame: &str) -> Result<()> {
    let resp = protocol::ServerResponse::parse(frame);

    if !resp.users_joined.is_empty() || !resp.users_left.is_empty() {
        update_roster(state, account_id, buffer_name, &resp.users_joined, &resp.users_left);
    }

    if let Some(text) = &resp.plaintext {
        tracing::debug!("sneedchat[{account_id}]: {text}");
        let lower = text.to_lowercase();
        if lower.contains("cannot join") || lower.contains("session") {
            bail!("server rejected the connection: {text}");
        }
        return Ok(());
    }

    // The site pushes our live permission set for the room on join (and,
    // it seems, whenever it changes) - this is the one on-wire signal that
    // distinguishes "authenticated" from "silently downgraded to a guest",
    // which a reconnect using a since-expired session cookie produces:
    // the websocket handshake itself succeeds either way (the anti-bot
    // proxy in front of it doesn't check the cookie, only the chat
    // application layer does), so there's no error to bail out on there.
    // Bailing here instead - this account always has real credentials, so
    // can_send=false can only mean the session needs a fresh login, and
    // routes through the exact same reconnect-with-refresh path a real
    // connection error already takes (see run_room's catch, which calls
    // session.refresh() before the next attempt).
    if let Some(perms) = &resp.perms {
        if !perms.can_send {
            bail!("server reports we can only view, not send (likely viewing as a guest) - session needs to be refreshed");
        }
    }

    let buffer_id = crate::model::buffer_id(account_id, buffer_name);

    // A batch delete: uuids removed elsewhere (e.g. by a moderator),
    // arriving separately from any message object.
    for uuid in &resp.deleted_uuids {
        state.runtime.delete_message(state, &buffer_id, uuid);
    }

    // A replayed batch is stored and shown like anything else, but it is not
    // news: the site sends the room's recent history on every join, so without
    // this a reconnect turns an hours-old mention into a fresh alert. The
    // already-seen check catches the second reconnect onwards; this catches
    // the first, which is the one that actually fires.
    if resp.history {
        state.runtime.set_replaying(&buffer_id, true);
    }

    for m in &resp.messages {
        if m.is_deleted() {
            state.runtime.delete_message(state, &buffer_id, &m.message_uuid);
            continue;
        }
        let body = protocol::unescape_html(&m.message_raw);
        if body.is_empty() {
            continue;
        }
        let avatar_url = match &m.author.avatar_url {
            Some(raw) => cached_avatar_path(http, host, &m.author.id, raw).await,
            None => None,
        };

        // There's no separate "edit" event on this wire - the server just
        // re-sends the same message_uuid with a bumped edit date. Try to
        // update an already-stored row first; only if that uuid was never
        // actually stored (e.g. this is the first time we've ever seen it,
        // and it just happens to already carry prior edit history) does
        // it fall through to being recorded as an ordinary new message.
        if m.message_edit_date > 0 && !m.message_uuid.is_empty() && state.runtime.update_message(state, &buffer_id, &m.message_uuid, &body, &[], &[]) {
            continue;
        }
        // Always attributed to the room this specific connection is
        // joined to, rather than trusting the wire's own room_id - this
        // connection should only ever see traffic for its own room anyway.
        let msg_id = (!m.message_uuid.is_empty()).then(|| m.message_uuid.clone());
        let is_new = state.runtime.record_message(state, account_id, buffer_name, "channel", &m.author.username, &body, false, "chat", None, msg_id, false, avatar_url, Vec::new(), Vec::new(), None);
        // Only for messages with a real, stable id - nothing to re-target
        // a later body-patch at otherwise (see spawn_attachment_resolve) -
        // and only for ones not seen before. A reconnect replays the room's
        // recent history, and looking every attachment in it up again is a
        // request per message, over Tor, for answers already on disk.
        if is_new && !m.message_uuid.is_empty() {
            spawn_attachment_resolve(state.clone(), http.clone(), buffer_id.clone(), m.message_uuid.clone(), body);
        }
    }
    // Cleared unconditionally rather than only when it was set: nothing above
    // returns early, and leaving it set would silence the room permanently.
    state.runtime.set_replaying(&buffer_id, false);

    // What the site says on entering the room. Recorded as a line in the room
    // rather than shown as a banner, because it is the same kind of thing as
    // an IRC topic arriving and this client already has somewhere to put that
    // - and because a banner nobody dismissed would still be there tomorrow.
    //
    // Once per text rather than once per connect: every reconnect carries it
    // again, and a room whose connection drops twice an hour would otherwise
    // fill with copies of the same announcement.
    if let Some(motd) = &resp.motd {
        if state.runtime.take_new_sneedchat_motd(account_id, buffer_name, motd) {
            state.runtime.record_message(
                // "system" rather than "topic": a topic line is filtered by the
                // setting that hides IRC topic changes, and a room's own
                // announcement is not an IRC topic change.
                state, account_id, buffer_name, "channel", "", motd, false, "system", None, None, false, None,
                Vec::new(), Vec::new(), None,
            );
        }
    }

    if is_primary {
        if let Some(w) = &resp.whisper {
            let body = protocol::unescape_html(&w.message_raw);
            // The site echoes our own whisper back to us, which is worth
            // knowing rather than working around: it means a whisper we sent
            // arrives twice, once from `send_whisper` recording it locally and
            // once from here.
            //
            // The local one is kept and this one dropped, because the echo has
            // lost the only thing that made the line useful - who it went to.
            // Its author is us, so it reads as us saying something to nobody
            // in particular, while the one we wrote says "@name, ...".
            //
            // Not deduplicated by id, because they have none in common: ours
            // is recorded as it is sent, before the site has given it one.
            if !body.is_empty() && !is_own_name(state, account_id, &w.author.username) {
                let avatar_url = match &w.author.avatar_url {
                    Some(raw) => cached_avatar_path(http, host, &w.author.id, raw).await,
                    None => None,
                };
                let msg_id = (!w.message_uuid.is_empty()).then(|| w.message_uuid.clone());
                for buffer in record_whisper(state, account_id, &w.author.username, &body, msg_id, avatar_url, true, None) {
                    if !w.message_uuid.is_empty() {
                        spawn_attachment_resolve(state.clone(), http.clone(), buffer, w.message_uuid.clone(), body.clone());
                    }
                }
            }
        }
    }

    Ok(())
}

/// Whether a name on the wire is this account's own.
///
/// Both the display name and the login name, because a whisper's author is
/// whichever the site is showing and an account can have set either. Compared
/// loosely - trimmed and case-insensitive - since the cost of being wrong is
/// asymmetric: missing a match shows a duplicate line, and a false match
/// would silently drop a real whisper from somebody whose name happened to
/// differ only in case, which cannot happen because the site's names are
/// unique without it.
pub(super) fn is_own_name(state: &AppState, account_id: &str, name: &str) -> bool {
    let Some(cfg) = state.accounts.get_sneedchat(account_id) else { return false };
    name_matches(cfg.display_name.as_deref().unwrap_or_default(), &cfg.username, name)
}

pub(super) fn name_matches(display_name: &str, username: &str, candidate: &str) -> bool {
    let candidate = candidate.trim();
    if candidate.is_empty() {
        return false;
    }
    [display_name, username]
        .iter()
        .any(|mine| !mine.trim().is_empty() && mine.trim().eq_ignore_ascii_case(candidate))
}

/// Puts a whisper in front of whoever is reading, whichever room that is.
///
/// A whisper belongs to no room, and the site shows it wherever you happen to
/// be looking. Nothing here can know which room a frontend has open, so it is
/// recorded in every room this account is in rather than one being picked and
/// hoped for.
///
/// That replaces a buffer of its own, which is what the report called "a DM
/// window". The separate buffer was the same mistake in a worse form: a
/// private message you only see by going somewhere else to look, and a
/// conversation that pulled you out of the room you were in to have it.
///
/// Storing the same message under several buffers is fine and not a
/// duplicate - the store keys on (buffer, id), so each room holds it once and
/// none of them holds it twice however many sockets saw it.
#[allow(clippy::too_many_arguments)]
pub(super) fn record_whisper(
    state: &AppState,
    account_id: &str,
    from: &str,
    body: &str,
    msg_id: Option<String>,
    avatar_url: Option<String>,
    // `notify`: whether this one should raise an alert. True for a whisper
    // somebody sent us; false for one we sent, which needs no telling.
    notify: bool,
    // `only`: the one buffer this belongs in, for a whisper we sent - it
    // belongs in the conversation it was sent from, not in every room at
    // once. Absent for an inbound one, which arrives from nowhere in
    // particular and has to be visible wherever the reader is.
    only: Option<&str>,
) -> Vec<String> {
    let Some(cfg) = state.accounts.get_sneedchat(account_id) else { return Vec::new() };
    let mut fresh = Vec::new();
    let rooms: Vec<SneedChatRoom> = match only {
        Some(buffer_name) => {
            let wanted = room_name_of(buffer_name);
            effective_rooms(&cfg).into_iter().filter(|r| r.name == wanted).collect()
        }
        None => effective_rooms(&cfg),
    };
    for (index, room) in rooms.into_iter().enumerate() {
        // Highlighted in the first room only, and highlighting is what raises
        // an alert - so one whisper is one notification however many rooms it
        // is written into. The copies are still whispers and are still drawn
        // as whispers, because the client colours them by kind rather than by
        // highlight; what they are not is a second time somebody's desktop
        // says the same thing.
        let alert = notify && index == 0;
        let name = room_buffer_name(&room.name);
        let is_new = state.runtime.record_message(
            state,
            account_id,
            &name,
            "channel",
            from,
            body,
            false,
            "whisper",
            None,
            msg_id.clone(),
            alert,
            avatar_url.clone(),
            Vec::new(),
            Vec::new(),
            None,
        );
        if is_new {
            fresh.push(crate::model::buffer_id(account_id, &name));
        }
    }
    fresh
}
