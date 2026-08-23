use super::wire::*;
use crate::accounts::IrcAccountConfig;
use crate::backend;
use crate::state::AppState;
use serde_json::Value;
use std::collections::HashSet;

/// One handler per JSON-RPC method (daemon/nobilis/api.c's handle_request,
/// split out of the socket-framing code). Returns (result, error) - exactly
/// one is Some, matching the wire contract's response shape.
pub async fn dispatch(
    state: &AppState,
    method: &str,
    params: &Value,
    subscriptions: &mut HashSet<String>,
) -> (Option<Value>, Option<String>) {
    match method {
        "listAccounts" => (Some(serde_json::to_value(state.runtime.list_accounts(state)).unwrap()), None),

        "listBuffers" => (Some(serde_json::to_value(state.runtime.list_buffers()).unwrap()), None),

        // The rail: whatever the backends registered (Discord guilds today,
        // Matrix spaces later), plus an entry for every account whose protocol
        // has no grouping of its own. Synthesised here rather than registered
        // per-backend so IRC and Sneedchat need no code to appear at all.
        "listBufferGroups" => {
            let mut groups = state.runtime.list_buffer_groups();
            let registered: std::collections::HashSet<String> = groups.iter().map(|g| g.id.clone()).collect();
            for account in state.runtime.list_accounts(state) {
                let id = crate::model::account_group_id(&account.id);
                if registered.contains(&id) {
                    continue;
                }
                groups.push(crate::model::BufferGroup {
                    id,
                    account_id: account.id.clone(),
                    service: account.service.clone(),
                    kind: "account".to_string(),
                    name: account.display_name.clone(),
                    icon_url: account.avatar_url.clone(),
                    // After the guilds, which are the entries a user picks
                    // between most often.
                    position: 1000,
                });
            }
            groups.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.name.cmp(&b.name)));
            (Some(serde_json::to_value(groups).unwrap()), None)
        }

        "listBufferEmoji" => match p_str_opt(params, "bufferId") {
            None => (None, Some("listBufferEmoji requires \"bufferId\"".to_string())),
            Some(buffer_id) => (Some(serde_json::json!(state.runtime.get_discord_buffer_emojis(buffer_id))), None),
        },

        // Sneedchat's own site-wide smiley table (see backend/sockchat/
        // smilies.rs) - unlike Discord's per-guild emoji above, this is
        // fixed and global, so no bufferId is needed. `file` is a filename
        // within the client's own bundled sockchat-smilies/ resource dir,
        // not a URL - these images ship with the client rather than being
        // fetched from kiwifarms.st at runtime, so the client resolves the
        // actual path itself (it already knows its own install directory)
        // and this needs no I/O of any kind to answer.
        "listSockchatSmilies" => (
            Some(serde_json::json!(backend::sockchat::smilies::SMILIES.iter().map(|s| serde_json::json!({ "label": s.label, "aliases": s.aliases, "file": s.file })).collect::<Vec<_>>())),
            None,
        ),

        // Stop the daemon cleanly. A client that adopted an already-running
        // nobilis has no child process to signal, so this is the only way it
        // can stop one it did not spawn. Returns before the exit actually
        // happens - the daemon still has to send QUITs to every connected
        // network first, and the caller's socket closing is the real signal
        // that it is gone.
        "shutdown" => {
            state.shutdown.notify_one();
            (Some(ok_node()), None)
        }

        "listProtocols" => (
            Some(serde_json::json!([
                { "id": "irc", "name": "IRC" },
                { "id": "jabber", "name": "XMPP" },
                { "id": "matrix", "name": "Matrix" },
                { "id": "discord", "name": "Discord" },
                { "id": "slack", "name": "Slack" },
                { "id": "sockchat", "name": "Sneedchat" },
            ])),
            None,
        ),

        "getBacklog" => match p_str_opt(params, "bufferId") {
            None => (None, Some("getBacklog requires \"bufferId\"".to_string())),
            Some(buffer_id) => {
                let before = p_i64(params, "before", 0);
                let limit = p_i64(params, "limit", 200);
                // A nonzero `before` means the frontend is paginating
                // (scrolled to the top asking for older messages), not
                // doing the initial open-a-buffer fetch - only then is it
                // worth trying to pull more from Discord first, since the
                // initial fetch is already covered by the backfill that
                // runs when the buffer is first discovered (see backend/
                // discord.rs's backfill_channel_history).
                if before > 0 {
                    if let Some(buffer) = state.runtime.get_buffer(buffer_id) {
                        if let (Some(channel_id), Some(cfg)) =
                            (state.runtime.get_discord_channel(buffer_id), state.accounts.get_discord(&buffer.account_id))
                        {
                            backend::discord::extend_history(state, &cfg.token, &cfg.user_id, cfg.display_name.as_deref(), buffer_id, &channel_id).await;
                        }
                    }
                }
                match state.store.get_backlog(buffer_id, before, limit) {
                    Ok(messages) => {
                        // Discord's CDN links lapse about a day after they are
                        // issued, so scrollback this far back routinely carries
                        // dead ones. Re-sign them in the background: one history
                        // read re-signs every attachment in the page it returns,
                        // so a screenful costs one request rather than one per
                        // image. The page returns immediately either way - cached
                        // previews are already showing, and re-signed links
                        // arrive as messageUpdated.
                        backend::discord::resign_stale_attachments(state.clone(), buffer_id.to_string(), &messages);
                        (Some(serde_json::to_value(messages).unwrap()), None)
                    }
                    Err(e) => (None, Some(e.to_string())),
                }
            }
        },

        "subscribe" => match p_str_opt(params, "bufferId") {
            None => (None, Some("subscribe requires \"bufferId\"".to_string())),
            Some(buffer_id) => {
                subscriptions.insert(buffer_id.to_string());
                // Replay the last-known member list immediately - a fresh
                // subscribe otherwise only sees *future* presenceChange
                // pushes (join/part/etc.), leaving the userlist empty
                // until the next incremental change (see Runtime::
                // set_presence's doc comment).
                if let Some(members) = state.runtime.get_presence(buffer_id) {
                    state.events.emit("presenceChange", serde_json::json!({ "bufferId": buffer_id, "members": members }));
                }
                (Some(ok_node()), None)
            }
        },

        "unsubscribe" => {
            if let Some(buffer_id) = p_str_opt(params, "bufferId") {
                subscriptions.remove(buffer_id);
            }
            (Some(ok_node()), None)
        }

        "addAccount" => {
            let (nick, host) = match (p_str_opt(params, "nick"), p_str_opt(params, "host")) {
                (Some(n), Some(h)) => (n, h),
                _ => return (None, Some("addAccount requires \"nick\" and \"host\"".to_string())),
            };
            let config = IrcAccountConfig {
                nick: nick.to_string(),
                host: host.to_string(),
                port: params.get("port").and_then(|v| v.as_u64()).map(|v| v as u16),
                ssl: p_bool(params, "ssl", true),
                sasl: p_bool(params, "sasl", false),
                sasl_user: p_str_opt(params, "saslUser").map(String::from),
                password: p_str_opt(params, "password").map(String::from),
                realname: p_str_opt(params, "realname").map(String::from),
                username: p_str_opt(params, "username").map(String::from),
                quit_message: p_str_opt(params, "quitMessage").map(String::from),
                allow_plaintext_sasl: p_bool(params, "allowPlaintextSasl", false),
                autojoin: String::new(),
                nickserv_password: None,
                display_name: None,
                use_tor: false,
                tor_proxy: None,
            };
            match state.accounts.add_irc(config.clone()) {
                Ok(None) => (None, Some("account already exists".to_string())),
                Ok(Some(saved)) => {
                    backend::irc::spawn(state.clone(), saved.clone());
                    let account = crate::accounts::irc_account_to_json(&saved, "connecting");
                    (Some(serde_json::to_value(account).unwrap()), None)
                }
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "removeAccount" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => {
                // Stops a live connection OR cancels one still stuck
                // connecting - either way, don't leave an orphaned task
                // running against an account we're about to delete.
                state.runtime.disconnect(state, id);
                // Without this, its channels/DMs/server buffer would
                // linger in listBuffers indefinitely (nothing else clears
                // them - disconnecting alone doesn't).
                state.runtime.remove_buffers_for_account(state, id);
                // Its guilds would otherwise stay in the rail with nothing
                // under them.
                state.runtime.clear_buffer_groups_for_account(id);
                match state.accounts.remove(id) {
                    Ok(true) => (Some(ok_node()), None),
                    Ok(false) => (None, Some("no such account".to_string())),
                    Err(e) => (None, Some(e.to_string())),
                }
            }
        },

        "setAccountConnected" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => {
                let connect = p_bool(params, "connected", true);
                if connect {
                    if id.starts_with("discord:") {
                        match state.accounts.get_discord(id) {
                            Some(cfg) => {
                                backend::discord::spawn(state.clone(), cfg);
                                (Some(ok_node()), None)
                            }
                            None => (None, Some("no such account".to_string())),
                        }
                    } else if id.starts_with("sockchat:") {
                        match state.accounts.get_sockchat(id) {
                            Some(cfg) => {
                                backend::sockchat::spawn(state.clone(), cfg);
                                (Some(ok_node()), None)
                            }
                            None => (None, Some("no such account".to_string())),
                        }
                    } else if id.starts_with("matrix:") {
                        match state.accounts.get_matrix(id) {
                            Some(cfg) => {
                                backend::matrix::spawn(state.clone(), cfg);
                                (Some(ok_node()), None)
                            }
                            None => (None, Some("no such account".to_string())),
                        }
                    } else {
                        match state.accounts.get_irc(id) {
                            Some(cfg) => {
                                backend::irc::spawn(state.clone(), cfg);
                                (Some(ok_node()), None)
                            }
                            None => (None, Some("no such account".to_string())),
                        }
                    }
                } else if state.runtime.disconnect(state, id) {
                    (Some(ok_node()), None)
                } else {
                    (None, Some("no such account".to_string()))
                }
            }
        },

        "setAccountAutojoin" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => account_mutation_result(state.accounts.set_autojoin(id, p_str(params, "channels", ""))),
        },

        "setAccountNickservPassword" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => account_mutation_result(state.accounts.set_nickserv_password(id, p_str(params, "password", ""))),
        },

        "setAccountDisplayName" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => account_mutation_result(state.accounts.set_display_name(id, p_str(params, "name", ""))),
        },

        "setAccountSasl" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => account_mutation_result(state.accounts.set_sasl(
                id,
                p_bool(params, "enabled", false),
                p_str(params, "saslUser", ""),
                p_str(params, "password", ""),
                p_bool(params, "allowPlaintextSasl", false),
            )),
        },

        "joinBuffer" => {
            let (id, name) = match (p_str_opt(params, "accountId"), p_str_opt(params, "name")) {
                (Some(a), Some(n)) => (a, n),
                _ => return (None, Some("joinBuffer requires a known \"accountId\" and \"name\"".to_string())),
            };
            match state.runtime.irc_sender(id) {
                None => (None, Some("join failed (account not connected?)".to_string())),
                Some(sender) => match sender.send_join(name) {
                    Ok(()) => (Some(ok_node()), None),
                    Err(e) => (None, Some(e.to_string())),
                },
            }
        }

        "joinDiscordGuild" => {
            let (account_id, invite) = match (p_str_opt(params, "accountId"), p_str_opt(params, "invite")) {
                (Some(a), Some(i)) => (a, i),
                _ => return (None, Some("joinDiscordGuild requires \"accountId\" and \"invite\"".to_string())),
            };
            match backend::discord::join_guild(state, account_id, invite).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "openDiscordDm" => {
            let (account_id, user_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "userId")) {
                (Some(a), Some(u)) => (a, u),
                _ => return (None, Some("openDiscordDm requires \"accountId\" and \"userId\"".to_string())),
            };
            match backend::discord::open_dm(state, account_id, user_id).await {
                Ok(buffer_id) => (Some(serde_json::json!({ "bufferId": buffer_id })), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Synchronous read of the in-memory snapshot built at READY and kept
        // current by PRESENCE_UPDATE (see backend/discord.rs) - no network
        // round trip needed here, unlike listMatrixDevices.
        "listDiscordFriends" => match p_str_opt(params, "accountId") {
            None => (None, Some("listDiscordFriends requires \"accountId\"".to_string())),
            Some(account_id) => (Some(serde_json::json!(state.runtime.get_discord_friends(account_id))), None),
        },

        "addDiscordFriend" => {
            let (account_id, username) = match (p_str_opt(params, "accountId"), p_str_opt(params, "username")) {
                (Some(a), Some(u)) => (a, u),
                _ => return (None, Some("addDiscordFriend requires \"accountId\" and \"username\"".to_string())),
            };
            match backend::discord::add_friend(state, account_id, username).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "createDiscordGuild" => {
            let (account_id, name) = match (p_str_opt(params, "accountId"), p_str_opt(params, "name")) {
                (Some(a), Some(n)) => (a, n),
                _ => return (None, Some("createDiscordGuild requires \"accountId\" and \"name\"".to_string())),
            };
            match backend::discord::create_guild(state, account_id, name).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Fallback for a Discord attachment link whose signed `ex=`/`is=`/
        // `hm=` query string has expired (see backend/discord.rs's
        // message_link doc comment for why this backend can't just
        // silently re-sign one the way a real client does) - opens the
        // real message in an actual Discord client/web session instead,
        // which always has a valid signature of its own.
        "getDiscordMessageLink" => {
            let (buffer_id, message_id) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "messageId")) {
                (Some(b), Some(m)) => (b, m),
                _ => return (None, Some("getDiscordMessageLink requires \"bufferId\" and \"messageId\"".to_string())),
            };
            let Some(channel_id) = state.runtime.get_discord_channel(buffer_id) else {
                return (None, Some("no known Discord channel for this buffer".to_string()));
            };
            let guild_id = state.runtime.get_discord_guild(buffer_id);
            let url = backend::discord::message_link(&channel_id, guild_id.as_deref(), message_id);
            (Some(serde_json::json!({ "url": url })), None)
        }

        // Ask Discord for the message again so its attachment links come
        // back freshly signed - the links expire about a day after they are
        // issued, and the dedicated refresh endpoint refuses user tokens.
        "refreshDiscordAttachments" => {
            let (buffer_id, message_id) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "messageId")) {
                (Some(b), Some(m)) => (b, m),
                _ => return (None, Some("refreshDiscordAttachments requires \"bufferId\" and \"messageId\"".to_string())),
            };
            match backend::discord::refresh_attachments(state, buffer_id, message_id).await {
                Ok(attachments) => (Some(serde_json::json!({ "attachments": attachments })), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Repairs scrollback by re-reading recent history and storing only
        // what is missing. With no "bufferId" it sweeps every Discord buffer,
        // which is the useful shape after a storage bug: the messages that
        // went missing are by definition ones nobody knows to go looking for.
        "refillDiscordHistory" => {
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(50) as u32;
            let targets: Vec<String> = match p_str_opt(params, "bufferId") {
                Some(b) => vec![b.to_string()],
                None => state
                    .runtime
                    .list_buffers()
                    .into_iter()
                    .filter(|b| state.runtime.get_discord_channel(&b.id).is_some())
                    .map(|b| b.id)
                    .collect(),
            };

            let mut recovered = 0usize;
            let mut scanned = 0usize;
            let mut failed = 0usize;
            for (i, buffer_id) in targets.iter().enumerate() {
                // Space the requests out; a sweep is dozens of calls against
                // one token and there is no hurry about it.
                if i > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                }
                match backend::discord::refill_history(state, buffer_id, limit).await {
                    Ok(n) => {
                        scanned += 1;
                        recovered += n;
                    }
                    Err(e) => {
                        failed += 1;
                        tracing::debug!("discord: refilling {buffer_id}: {e}");
                    }
                }
            }
            (
                Some(serde_json::json!({ "scanned": scanned, "recovered": recovered, "failed": failed })),
                None,
            )
        }

        // Sets how this account presents itself: online or idle.
        //
        // Applied to whichever protocol the account belongs to, and recorded
        // either way so a reconnect carries it. Sneedchat has no presence
        // concept at all, so it reports that rather than silently accepting.
        "setAccountStatus" => {
            let (account_id, status) = match (p_str_opt(params, "accountId"), p_str_opt(params, "status")) {
                (Some(a), Some(s)) => (a, s),
                _ => return (None, Some("setAccountStatus requires \"accountId\" and \"status\"".to_string())),
            };
            if !matches!(status, "online" | "idle") {
                return (None, Some("status must be online or idle".to_string()));
            }
            // Recorded before it is applied: a status set while disconnected
            // still has to survive to the next connection.
            state.runtime.set_account_status(account_id, status);

            let applied = if state.accounts.get_discord(account_id).is_some() {
                backend::discord::apply_status(state, account_id, status).await
            } else if let Some(config) = state.accounts.get_matrix(account_id) {
                match backend::matrix::apply_status(state, &config, status).await {
                    Ok(()) => true,
                    Err(e) => return (None, Some(e.to_string())),
                }
            } else if let Some(sender) = state.runtime.irc_sender(account_id) {
                // IRC has only away and back.
                let result = match status {
                    "online" => sender.send(irc::proto::Command::AWAY(None)),
                    _ => sender.send(irc::proto::Command::AWAY(Some("Idle".to_string()))),
                };
                result.is_ok()
            } else {
                false
            };

            (Some(serde_json::json!({ "ok": true, "applied": applied })), None)
        }

        // Asks for a channel's member list.
        //
        // Separate from subscribe because a client subscribes to every buffer
        // it tracks unread counts for, while a member list belongs to the one
        // channel being looked at. Discord answers these per guild rather
        // than per channel, so asking for several at once makes the replies
        // ambiguous - and it is wasted traffic for channels nobody is reading.
        "requestMemberList" => match p_str_opt(params, "bufferId") {
            None => (None, Some("requestMemberList requires \"bufferId\"".to_string())),
            Some(buffer_id) => {
                let asked = backend::discord::request_member_list(state, buffer_id);
                (Some(serde_json::json!({ "requested": asked })), None)
            }
        },

        // A guild's voice channels, with how many people are in each.
        //
        // Occupancy is gateway-only - there is no endpoint that reports it -
        // so this reflects what has been seen since connecting.
        // The machine's sound devices.
        //
        // Reported from the daemon because that is where audio is handled -
        // the voice connection and its encoder live here, so the devices do
        // too.
        "listAudioDevices" => match backend::audio::list_devices() {
            Ok(devices) => (Some(serde_json::to_value(devices).unwrap()), None),
            Err(e) => (None, Some(e.to_string())),
        },

        // Searching one conversation's scrollback. Scoped to a buffer rather
        // than global because that is where the question is asked from - the
        // header of the conversation you are reading.
        "searchMessages" => {
            let (buffer_id, query) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "query")) {
                (Some(b), Some(q)) => (b, q),
                _ => return (None, Some("searchMessages requires \"bufferId\" and \"query\"".to_string())),
            };
            if query.trim().is_empty() {
                return (Some(serde_json::json!([])), None);
            }
            match state.store.search_messages(buffer_id, query.trim(), p_i64(params, "limit", 50)) {
                Ok(messages) => (Some(serde_json::to_value(messages).unwrap()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Calling someone directly. Opens the conversation if there is not
        // one yet, so calling from a member list works for somebody you have
        // never messaged.
        "callDiscordUser" => {
            let (account_id, user_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "userId")) {
                (Some(a), Some(u)) => (a, u),
                _ => return (None, Some("callDiscordUser requires \"accountId\" and \"userId\"".to_string())),
            };
            match backend::discord::call_user(state, account_id, user_id).await {
                Ok(buffer_id) => (Some(serde_json::json!({ "bufferId": buffer_id })), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Starting a call in a conversation that already exists - the button
        // at the head of a DM.
        "startDiscordCall" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("startDiscordCall requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such buffer".to_string()));
            };
            let Some(channel_id) = state.runtime.get_discord_channel(buffer_id) else {
                return (None, Some("that conversation has no Discord channel".to_string()));
            };
            match backend::discord::start_call(state, &buffer.account_id, &channel_id).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Where we are in voice, if anywhere. Reported per account because a
        // client showing a "connected" bar has to name the channel, and the
        // gateway is the only thing that knows.
        "getVoiceSession" => {
            let sessions: Vec<Value> = state
                .voice
                .connected_accounts()
                .iter()
                .filter_map(|id| {
                    let (guild_id, channel_id) = state.voice.current_channel(id)?;
                    // A DM call has no guild and so no voice channel list to
                    // look a name up in; the conversation's own buffer is what
                    // names it, which is also what a person would call it.
                    let name = guild_id
                        .as_deref()
                        .and_then(|g| {
                            state
                                .runtime
                                .discord_voice_channels(id, g)
                                .into_iter()
                                .find(|(c, _, _)| *c == channel_id)
                                .map(|(_, n, _)| n)
                        })
                        .or_else(|| {
                            state
                                .runtime
                                .discord_buffer_for_channel(&channel_id)
                                .and_then(|b| state.runtime.get_buffer(&b))
                                .map(|b| b.name)
                        })
                        .unwrap_or_else(|| channel_id.clone());
                    Some(serde_json::json!({
                        "accountId": id,
                        "guildId": guild_id,
                        "channelId": channel_id,
                        "channelName": name,
                        // Which conversation this call is in, so a client can
                        // mark the right row rather than guessing from the
                        // account - which cannot tell one DM from another.
                        "bufferId": state.runtime.discord_buffer_for_channel(&channel_id),
                        // A call with no guild is a one-to-one call, which a
                        // client shows differently.
                        "isDirect": guild_id.is_none(),
                    }))
                })
                .collect();
            (Some(serde_json::json!(sessions)), None)
        }

        // What is actually going in and out, for a level meter - and for
        // telling "nobody is talking" apart from "their audio never arrives",
        // which are otherwise the same silence.
        "getVoiceLevels" => {
            let accounts = state.voice.connected_accounts();
            let levels: Vec<Value> = accounts
                .iter()
                .map(|id| {
                    let (heard, received) = state.voice.output_level(id).unwrap_or((0.0, 0));
                    serde_json::json!({
                        "accountId": id,
                        "micPeak": state.voice.input_level(id).unwrap_or(0.0),
                        "heardPeak": heard,
                        "receivedSamples": received,
                    })
                })
                .collect();
            (Some(serde_json::json!(levels)), None)
        }

        // What voice is set to use, and whether it is silenced. Read back
        // rather than assumed by a client, since a mute survives a restart and
        // a second window has to agree with the first.
        "getVoicePrefs" => (Some(serde_json::to_value(state.voice_prefs.get()).unwrap()), None),

        // Choosing a device. An input choice takes effect on a call already in
        // progress: the stream is moved rather than reopened, which is what
        // the desktop's own mixer does.
        "setVoiceDevice" => {
            let Some(kind) = p_str_opt(params, "kind") else {
                return (None, Some("setVoiceDevice requires \"kind\"".to_string()));
            };
            let device_id = p_str_opt(params, "deviceId").unwrap_or(backend::audio::DEFAULT_ID).to_string();
            if kind != "input" && kind != "output" {
                return (None, Some(format!("unknown device kind {kind:?}")));
            }
            if kind == "input" && !device_id.is_empty() {
                if let Err(e) = backend::audio::route_input(&device_id) {
                    return (None, Some(e.to_string()));
                }
            }
            let prefs = state.voice_prefs.update(|p| {
                if kind == "input" {
                    p.input = Some(device_id.clone()).filter(|d| !d.is_empty());
                } else {
                    p.output = Some(device_id.clone()).filter(|d| !d.is_empty());
                }
            });
            state.events.emit("voicePrefsChanged", serde_json::to_value(&prefs).unwrap());
            (Some(serde_json::to_value(prefs).unwrap()), None)
        }

        // Muting the microphone, or the other people. Applied to every live
        // session rather than to one account: these are the machine's speakers
        // and microphone, and silencing them per-account would surprise anyone
        // in two calls at once.
        "setVoiceMuted" => {
            let mic = params.get("micMuted").and_then(|v| v.as_bool());
            let deaf = params.get("deafened").and_then(|v| v.as_bool());
            if mic.is_none() && deaf.is_none() {
                return (None, Some("setVoiceMuted requires \"micMuted\" or \"deafened\"".to_string()));
            }
            let prefs = state.voice_prefs.update(|p| {
                if let Some(m) = mic {
                    p.mic_muted = m;
                }
                if let Some(d) = deaf {
                    p.deafened = d;
                }
            });
            // Deafening implies not speaking either, which is what every other
            // client does and what people expect of the button.
            let mic_muted = prefs.mic_muted || prefs.deafened;
            for account_id in state.voice.connected_accounts() {
                state.voice.set_mic_muted(&account_id, mic_muted);
                backend::audio::set_playback_muted(prefs.deafened);
                backend::discord::announce_voice_flags(state, &account_id, mic_muted, prefs.deafened);
            }
            state.events.emit("voicePrefsChanged", serde_json::to_value(&prefs).unwrap());
            (Some(serde_json::to_value(prefs).unwrap()), None)
        }

        "listVoiceChannels" => {
            let (account_id, guild_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "guildId")) {
                (Some(a), Some(g)) => (a, g),
                _ => return (None, Some("listVoiceChannels requires \"accountId\" and \"guildId\"".to_string())),
            };
            let own = state.accounts.get_discord(account_id).map(|c| c.user_id).unwrap_or_default();
            let channels: Vec<Value> = state
                .runtime
                .discord_voice_channels(account_id, guild_id)
                .into_iter()
                .map(|(id, name, limit)| {
                    let others = state.runtime.discord_voice_occupants(account_id, &id, &own);
                    // Everyone including us, since a channel list has to show
                    // you your own presence; `empty` deliberately still means
                    // "nobody else", which is what the join rule tests.
                    let members: Vec<Value> = state
                        .runtime
                        .discord_voice_members(account_id, &id)
                        .into_iter()
                        .map(|(user_id, nick)| serde_json::json!({ "userId": user_id, "nick": nick, "isSelf": user_id == own }))
                        .collect();
                    serde_json::json!({
                        "id": id,
                        "name": name,
                        "userLimit": limit,
                        "occupants": others.len(),
                        "empty": others.is_empty(),
                        "members": members
                    })
                })
                .collect();
            (Some(serde_json::json!(channels)), None)
        }

        "joinVoiceChannel" => {
            // The guild is optional: a one-to-one call is a voice connection
            // to a DM channel, which has none.
            let (account_id, channel_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "channelId")) {
                (Some(a), Some(c)) => (a, c),
                _ => return (None, Some("joinVoiceChannel requires \"accountId\" and \"channelId\"".to_string())),
            };
            let guild_id = p_str_opt(params, "guildId");
            // Both default to the cautious setting, so a caller that says
            // nothing gets an empty channel and a closed microphone.
            let options = backend::discord_voice::VoiceOptions {
                solo: params.get("soloOnly").and_then(|v| v.as_bool()).unwrap_or(true),
                transmit: params.get("transmit").and_then(|v| v.as_bool()).unwrap_or(false),
            };
            match backend::discord::join_voice(state, account_id, guild_id, channel_id, options) {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "leaveVoiceChannel" => match p_str_opt(params, "accountId") {
            None => (None, Some("leaveVoiceChannel requires \"accountId\"".to_string())),
            Some(account_id) => (Some(serde_json::json!({ "left": backend::discord::leave_voice(state, account_id) })), None),
        },

        "partBuffer" => match p_str_opt(params, "bufferId") {
            None => (None, Some("no such buffer".to_string())),
            Some(buffer_id) => {
                if state.runtime.is_server_buffer(buffer_id) {
                    return (None, Some("this is the server buffer and can't be closed".to_string()));
                }
                match state.runtime.get_buffer(buffer_id) {
                    None => (None, Some("no such buffer".to_string())),
                    Some(buffer) => {
                        if let Some(sender) = state.runtime.irc_sender(&buffer.account_id) {
                            let _ = sender.send_part(&buffer.name);
                        }
                        state.runtime.remove_buffer(state, buffer_id);
                        (Some(ok_node()), None)
                    }
                }
            }
        },

        "sendMessage" => {
            let (buffer_id, body) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "body")) {
                (Some(b), Some(m)) => (b, m),
                _ => return (None, Some("sendMessage requires a known \"bufferId\" and \"body\"".to_string())),
            };
            // Only Discord has an attachment path or native replies today
            // (the "+" button and right-click Reply) - present but
            // ignored-vs-rejected depending on backend, same distinction
            // as everywhere else that's protocol-specific (e.g. autojoin/
            // NickServ being IRC-only fields on the account).
            let attachment_path = p_str_opt(params, "attachmentPath");
            let reply_to_id = p_str_opt(params, "replyToId");
            match state.runtime.get_buffer(buffer_id) {
                None => (None, Some("sendMessage requires a known \"bufferId\" and \"body\"".to_string())),
                Some(buffer) if buffer.account_id.starts_with("discord:") => match state.accounts.get_discord(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => {
                        let result = match attachment_path {
                            Some(path) => backend::discord::send_attachment(state, buffer_id, &cfg.token, body, path, reply_to_id).await,
                            None => backend::discord::send_message(state, buffer_id, &cfg.token, body, reply_to_id).await,
                        };
                        match result {
                            Ok(()) => (Some(ok_node()), None),
                            Err(e) => (None, Some(e.to_string())),
                        }
                    }
                },
                Some(buffer) if buffer.account_id.starts_with("sockchat:") => {
                    if reply_to_id.is_some() {
                        return (None, Some("replies aren't supported for this service".to_string()));
                    }
                    // Unlike Discord, Sneedchat's own chat protocol has no
                    // upload endpoint at all - see backend::sockchat::
                    // send_attachment's own doc comment for how this
                    // still ends up posting a real image.
                    let result = match attachment_path {
                        Some(path) => backend::sockchat::send_attachment(state, &buffer.account_id, &buffer.name, body, path).await,
                        None => backend::sockchat::send_message(state, &buffer.account_id, &buffer.name, body),
                    };
                    match result {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    }
                }
                Some(buffer) if buffer.account_id.starts_with("matrix:") => match state.accounts.get_matrix(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => match backend::matrix::send_message(state, &buffer.account_id, buffer_id, &cfg.access_token, body, reply_to_id, attachment_path).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                },
                Some(_) if attachment_path.is_some() => (None, Some("attachments aren't supported for this service".to_string())),
                Some(_) if reply_to_id.is_some() => (None, Some("replies aren't supported for this service".to_string())),
                Some(buffer) => match state.runtime.irc_sender(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(sender) => {
                        match backend::irc::send_message(state, &buffer.account_id, &sender, &buffer.name, body) {
                            Ok(()) => (Some(ok_node()), None),
                            Err(e) => (None, Some(e.to_string())),
                        }
                    }
                },
            }
        }

        "editMessage" => {
            let (buffer_id, msg_id, body) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "messageId"), p_str_opt(params, "body")) {
                (Some(b), Some(m), Some(body)) => (b, m, body),
                _ => return (None, Some("editMessage requires \"bufferId\", \"messageId\" and \"body\"".to_string())),
            };
            match state.runtime.get_buffer(buffer_id) {
                None => (None, Some("no such buffer".to_string())),
                Some(buffer) if buffer.account_id.starts_with("discord:") => match state.accounts.get_discord(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => match backend::discord::edit_message(state, buffer_id, &cfg.token, msg_id, body).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                },
                Some(buffer) if buffer.account_id.starts_with("sockchat:") => match backend::sockchat::edit_message(state, &buffer.account_id, &buffer.name, msg_id, body) {
                    Ok(()) => (Some(ok_node()), None),
                    Err(e) => (None, Some(e.to_string())),
                },
                Some(buffer) if buffer.account_id.starts_with("matrix:") => match state.accounts.get_matrix(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => match backend::matrix::edit_message(state, &buffer.account_id, buffer_id, &cfg.access_token, msg_id, body).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                },
                Some(_) => (None, Some("editing isn't supported for this service".to_string())),
            }
        }

        // Adds or removes *our own* reaction on a message - toggled from
        // the client side (its reaction pills / message hover toolbar).
        // Sneedchat has no reaction concept of its own.
        "toggleReaction" => {
            let (buffer_id, msg_id, emoji) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "messageId"), p_str_opt(params, "emoji")) {
                (Some(b), Some(m), Some(e)) => (b, m, e),
                _ => return (None, Some("toggleReaction requires \"bufferId\", \"messageId\" and \"emoji\"".to_string())),
            };
            let add = p_bool(params, "add", true);
            match state.runtime.get_buffer(buffer_id) {
                None => (None, Some("no such buffer".to_string())),
                Some(buffer) if buffer.account_id.starts_with("discord:") => match state.accounts.get_discord(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => match backend::discord::toggle_reaction(state, buffer_id, &cfg.token, msg_id, emoji, add).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                },
                Some(buffer) if buffer.account_id.starts_with("matrix:") => match state.accounts.get_matrix(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => match backend::matrix::toggle_reaction(state, &buffer.account_id, buffer_id, &cfg.access_token, msg_id, emoji, add).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                },
                Some(_) => (None, Some("reactions aren't supported for this service".to_string())),
            }
        }

        "deleteMessage" => {
            let (buffer_id, msg_id) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "messageId")) {
                (Some(b), Some(m)) => (b, m),
                _ => return (None, Some("deleteMessage requires \"bufferId\" and \"messageId\"".to_string())),
            };
            match state.runtime.get_buffer(buffer_id) {
                None => (None, Some("no such buffer".to_string())),
                Some(buffer) if buffer.account_id.starts_with("discord:") => match state.accounts.get_discord(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => match backend::discord::delete_message(state, buffer_id, &cfg.token, msg_id).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                },
                Some(buffer) if buffer.account_id.starts_with("sockchat:") => match backend::sockchat::delete_message(state, &buffer.account_id, &buffer.name, msg_id) {
                    Ok(()) => (Some(ok_node()), None),
                    Err(e) => (None, Some(e.to_string())),
                },
                Some(buffer) if buffer.account_id.starts_with("matrix:") => match state.accounts.get_matrix(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => match backend::matrix::delete_message(state, &buffer.account_id, buffer_id, &cfg.access_token, msg_id).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                },
                Some(_) => (None, Some("deleting isn't supported for this service".to_string())),
            }
        }

        // QR login is inherently async/multi-step - this just kicks it off
        // and returns immediately; progress and the eventual result arrive
        // via discordLoginQr/discordLoginScanned/discordLoginResult events
        // tagged with this loginId (see backend/discord.rs).
        // An optional "accountId" re-authenticates that existing account
        // instead of adding a new one - the usual reason to run this again is
        // a token Discord revoked, not a second account. Logging into a
        // different account than the one named is refused rather than
        // quietly adding it alongside.
        "addDiscordAccount" => {
            let login_id = format!("discord-login-{}", crate::model::next_message_id());
            let reauth = p_str_opt(params, "accountId").map(String::from);
            backend::discord::start_qr_login(state.clone(), login_id.clone(), reauth);
            (Some(serde_json::json!({ "loginId": login_id })), None)
        }

        // Username/password as an alternative to scanning a QR code. Same
        // async-kickoff shape: a two-factor challenge arrives as a
        // discordLoginMfa event (answer it with submitDiscordMfa), and
        // everything else ends in discordLoginResult.
        "addDiscordAccountPassword" => {
            let (login, password) = match (p_str_opt(params, "login"), p_str_opt(params, "password")) {
                (Some(l), Some(p)) => (l.to_string(), p.to_string()),
                _ => return (None, Some("addDiscordAccountPassword requires \"login\" and \"password\"".to_string())),
            };
            let login_id = format!("discord-login-{}", crate::model::next_message_id());
            let reauth = p_str_opt(params, "accountId").map(String::from);
            backend::discord::start_password_login(state.clone(), login_id.clone(), login, password, reauth);
            (Some(serde_json::json!({ "loginId": login_id })), None)
        }

        // The authenticator (or backup) code for a login that reported
        // discordLoginMfa. The ticket it needs is held against the loginId.
        "submitDiscordMfa" => {
            let (login_id, code) = match (p_str_opt(params, "loginId"), p_str_opt(params, "code")) {
                (Some(l), Some(c)) => (l.to_string(), c.to_string()),
                _ => return (None, Some("submitDiscordMfa requires \"loginId\" and \"code\"".to_string())),
            };
            backend::discord::submit_mfa_code(state.clone(), login_id, code);
            (Some(ok_node()), None)
        }

        // Tor bootstrap + login (+ a possible proof-of-work solve) can take
        // anywhere from instant to over a minute - same async-kickoff shape
        // as addDiscordAccount, with progress/result arriving via
        // sockChatLoginStatus/sockChatLoginResult events (see backend/
        // sockchat/mod.rs's start_login).
        "addSockChatAccount" => {
            let (username, password) = match (p_str_opt(params, "username"), p_str_opt(params, "password")) {
                (Some(u), Some(p)) => (u.to_string(), p.to_string()),
                _ => return (None, Some("addSockChatAccount requires \"username\" and \"password\"".to_string())),
            };
            let config = crate::accounts::SockChatAccountConfig {
                username,
                password,
                totp_secret: p_str_opt(params, "totpSecret").map(String::from),
                host: p_str_opt(params, "host").map(String::from).unwrap_or_else(|| backend::sockchat::DEFAULT_ONION.to_string()),
                tor_mode: p_str_opt(params, "torMode").map(String::from).unwrap_or_else(|| "embedded".to_string()),
                proxy: p_str_opt(params, "proxy").map(String::from),
                rooms: parse_sockchat_rooms(params).unwrap_or_default(),
                display_name: None,
                user_id: None,
            };
            let login_id = format!("sockchat-login-{}", crate::model::next_message_id());
            backend::sockchat::start_login(state.clone(), login_id.clone(), config);
            (Some(serde_json::json!({ "loginId": login_id })), None)
        }

        // Replaces the account's configured room list and reconnects it
        // immediately (rather than only on the next daemon restart) so a
        // freshly-added room shows up right away.
        "setSockChatRooms" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => {
                let Some(rooms) = parse_sockchat_rooms(params) else {
                    return (None, Some("setSockChatRooms requires \"rooms\": [{\"id\":.., \"name\":..}, ...]".to_string()));
                };
                match state.accounts.set_sockchat_rooms(id, rooms) {
                    Ok(true) => match state.accounts.get_sockchat(id) {
                        Some(cfg) => {
                            backend::sockchat::spawn(state.clone(), cfg);
                            (Some(ok_node()), None)
                        }
                        None => (None, Some("no such account".to_string())),
                    },
                    Ok(false) => (None, Some("no such account".to_string())),
                    Err(e) => (None, Some(e.to_string())),
                }
            }
        },

        // Tor is a daemon-global category in settings, not a per-account
        // one - there's only ever one embedded TorManager, so "embedded vs
        // external proxy" is applied uniformly to every configured
        // Sneedchat account rather than asked per-account. Reconnects each
        // affected account immediately, same as setSockChatRooms above.
        "setTorConfig" => {
            let tor_mode = p_str(params, "torMode", "embedded").to_string();
            let proxy = p_str_opt(params, "proxy").filter(|s| !s.is_empty()).map(String::from);
            for cfg in state.accounts.all_sockchat() {
                let id = cfg.account_id();
                if let Err(e) = state.accounts.set_sockchat_tor_config(&id, tor_mode.clone(), proxy.clone()) {
                    return (None, Some(e.to_string()));
                }
                if let Some(cfg) = state.accounts.get_sockchat(&id) {
                    backend::sockchat::spawn(state.clone(), cfg);
                }
            }
            (Some(ok_node()), None)
        }

        // Drops the shared embedded Tor client, forcing a fresh bootstrap
        // (and thus fresh circuits) on next use - keeps the on-disk
        // consensus/guard cache, so this is fast. Every Sneedchat account
        // is re-spawned afterward since a room connection already holds its
        // own clone of the old client and won't pick up the new one on its
        // own (see TorManager::restart's doc comment).
        "regenerateTorCircuit" => {
            state.tor.restart().await;
            for cfg in state.accounts.all_sockchat() {
                backend::sockchat::spawn(state.clone(), cfg);
            }
            (Some(ok_node()), None)
        }

        // Like regenerateTorCircuit, but also wipes the on-disk cache/state
        // dirs first, forcing a full cold bootstrap (can take up to a
        // minute) instead of reusing cached guards/consensus data.
        "restartTorFromScratch" => {
            let (cache_dir, state_dir) = state.tor.cache_dirs();
            for dir in [&cache_dir, &state_dir] {
                if dir.exists() {
                    if let Err(e) = std::fs::remove_dir_all(dir) {
                        return (None, Some(format!("failed to clear {}: {e}", dir.display())));
                    }
                }
            }
            state.tor.restart().await;
            for cfg in state.accounts.all_sockchat() {
                backend::sockchat::spawn(state.clone(), cfg);
            }
            (Some(ok_node()), None)
        }

        // Toggles routing an IRC account's connection through an external
        // SOCKS5 proxy (a system Tor daemon or Tor Browser - see
        // IrcAccountConfig::use_tor's doc comment for why this can't reuse
        // the embedded Arti client Sneedchat uses) and reconnects.
        "setAccountUseTor" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => {
                let use_tor = p_bool(params, "useTor", false);
                let proxy = p_str(params, "proxy", "");
                match state.accounts.set_irc_use_tor(id, use_tor, proxy) {
                    Ok(true) => match state.accounts.get_irc(id) {
                        Some(cfg) => {
                            backend::irc::spawn(state.clone(), cfg);
                            (Some(ok_node()), None)
                        }
                        None => (None, Some("no such account".to_string())),
                    },
                    Ok(false) => (None, Some("no such account".to_string())),
                    Err(e) => (None, Some(e.to_string())),
                }
            }
        },

        // Net-new protocols land in their own milestones (see project
        // plan) - not implemented yet.
        "addXmppAccount" => (None, Some(format!("{method}: not implemented yet"))),

        // Login (homeserver reachability + m.login.password) can take a
        // moment - same async-kickoff shape as addSockChatAccount, with
        // progress/result arriving via matrixLoginStatus/matrixLoginResult
        // events (see backend/matrix/mod.rs's start_login).
        "addMatrixAccount" => {
            let (homeserver_url, username, password) = match (p_str_opt(params, "homeserverUrl"), p_str_opt(params, "userId"), p_str_opt(params, "password")) {
                (Some(h), Some(u), Some(p)) => (h.to_string(), u.to_string(), p.to_string()),
                _ => return (None, Some("addMatrixAccount requires \"homeserverUrl\", \"userId\" and \"password\"".to_string())),
            };
            let login_id = format!("matrix-login-{}", crate::model::next_message_id());
            backend::matrix::start_login(state.clone(), login_id.clone(), homeserver_url, username, password);
            (Some(serde_json::json!({ "loginId": login_id })), None)
        }

        // Session verification (SAS - "compare emoji") + recovery key -
        // see backend/matrix/verification.rs's module doc for the flow.
        // Self-verification only: accountId always names *our own* Matrix
        // account, deviceId one of that same account's other logged-in
        // sessions.
        "listMatrixDevices" => match p_str_opt(params, "accountId") {
            None => (None, Some("listMatrixDevices requires \"accountId\"".to_string())),
            Some(account_id) => match backend::matrix::verification::list_own_devices(state, account_id).await {
                Ok(devices) => (Some(serde_json::json!(devices)), None),
                Err(e) => (None, Some(e.to_string())),
            },
        },

        // Deleting (logging out) another session - unlike everything else
        // here, this is UIA-gated by the Matrix spec, so it needs the
        // account password, not just the accountId/deviceId pair the
        // other verification methods take.
        "deleteMatrixDevice" => {
            let (account_id, device_id, password) =
                match (p_str_opt(params, "accountId"), p_str_opt(params, "deviceId"), p_str_opt(params, "password")) {
                    (Some(a), Some(d), Some(p)) => (a, d, p),
                    _ => return (None, Some("deleteMatrixDevice requires \"accountId\", \"deviceId\" and \"password\"".to_string())),
                };
            match backend::matrix::verification::delete_device(state, account_id, device_id, password).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "startMatrixVerification" => {
            let (account_id, device_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "deviceId")) {
                (Some(a), Some(d)) => (a, d),
                _ => return (None, Some("startMatrixVerification requires \"accountId\" and \"deviceId\"".to_string())),
            };
            match backend::matrix::verification::start_verification(state, account_id, device_id).await {
                Ok(verification_id) => (Some(serde_json::json!({ "verificationId": verification_id })), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "respondMatrixVerification" => {
            let (account_id, verification_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "verificationId")) {
                (Some(a), Some(v)) => (a, v),
                _ => return (None, Some("respondMatrixVerification requires \"accountId\" and \"verificationId\"".to_string())),
            };
            let accept = p_bool(params, "accept", false);
            match backend::matrix::verification::respond_to_request(state, account_id, verification_id, accept).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "confirmMatrixVerification" => {
            let (account_id, verification_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "verificationId")) {
                (Some(a), Some(v)) => (a, v),
                _ => return (None, Some("confirmMatrixVerification requires \"accountId\" and \"verificationId\"".to_string())),
            };
            let matches = p_bool(params, "matches", false);
            match backend::matrix::verification::confirm_sas(state, account_id, verification_id, matches).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "cancelMatrixVerification" => {
            let (account_id, verification_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "verificationId")) {
                (Some(a), Some(v)) => (a, v),
                _ => return (None, Some("cancelMatrixVerification requires \"accountId\" and \"verificationId\"".to_string())),
            };
            match backend::matrix::verification::cancel(state, account_id, verification_id).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Recovery key (server-side room key backup) - see
        // backend/matrix/backup.rs's module doc. Both are synchronous:
        // the handful of real HTTP round trips involved aren't
        // long-running, unlike login - the potentially-slow part (backing
        // up a large existing history) already happens in the background
        // via the sync loop's own per-cycle run_pending_backup call.
        "setupMatrixRecoveryKey" => match p_str_opt(params, "accountId") {
            None => (None, Some("setupMatrixRecoveryKey requires \"accountId\"".to_string())),
            Some(account_id) => match backend::matrix::backup::setup_recovery_key(state, account_id).await {
                Ok(recovery_key) => (Some(serde_json::json!({ "recoveryKey": recovery_key })), None),
                Err(e) => (None, Some(e.to_string())),
            },
        },

        "restoreMatrixRecoveryKey" => {
            let (account_id, recovery_key) = match (p_str_opt(params, "accountId"), p_str_opt(params, "recoveryKey")) {
                (Some(a), Some(r)) => (a, r),
                _ => return (None, Some("restoreMatrixRecoveryKey requires \"accountId\" and \"recoveryKey\"".to_string())),
            };
            match backend::matrix::backup::restore_from_recovery_key(state, account_id, recovery_key).await {
                Ok(summary) => (Some(serde_json::json!({ "restoredKeys": summary.imported_keys, "totalKeys": summary.total_keys })), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Room moderation - see backend/matrix/moderation.rs's module doc.
        // bufferId (not roomId) throughout, matching every other message-
        // scoped Matrix method - the buffer->room_id lookup happens inside
        // moderation.rs itself, same as sendMessage/editMessage/etc.
        "getMatrixRoomPermissions" => {
            let (account_id, buffer_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "bufferId")) {
                (Some(a), Some(b)) => (a, b),
                _ => return (None, Some("getMatrixRoomPermissions requires \"accountId\" and \"bufferId\"".to_string())),
            };
            match backend::matrix::moderation::permissions_for_buffer(state, account_id, buffer_id) {
                Ok(permissions) => (Some(permissions), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "joinMatrixRoom" => {
            let (account_id, room) = match (p_str_opt(params, "accountId"), p_str_opt(params, "roomIdOrAlias")) {
                (Some(a), Some(r)) => (a, r),
                _ => return (None, Some("joinMatrixRoom requires \"accountId\" and \"roomIdOrAlias\"".to_string())),
            };
            match backend::matrix::join_room(state, account_id, room).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "openMatrixDm" => {
            let (account_id, user_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "userId")) {
                (Some(a), Some(u)) => (a, u),
                _ => return (None, Some("openMatrixDm requires \"accountId\" and \"userId\"".to_string())),
            };
            let display_name = p_str_opt(params, "displayName").unwrap_or("");
            match backend::matrix::open_dm(state, account_id, user_id, display_name).await {
                Ok(buffer_id) => (Some(serde_json::json!({ "bufferId": buffer_id })), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "kickMatrixMember" => {
            let (account_id, buffer_id, user_id) =
                match (p_str_opt(params, "accountId"), p_str_opt(params, "bufferId"), p_str_opt(params, "userId")) {
                    (Some(a), Some(b), Some(u)) => (a, b, u),
                    _ => return (None, Some("kickMatrixMember requires \"accountId\", \"bufferId\" and \"userId\"".to_string())),
                };
            let reason = p_str_opt(params, "reason");
            match backend::matrix::moderation::kick_member(state, account_id, buffer_id, user_id, reason).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "banMatrixMember" => {
            let (account_id, buffer_id, user_id) =
                match (p_str_opt(params, "accountId"), p_str_opt(params, "bufferId"), p_str_opt(params, "userId")) {
                    (Some(a), Some(b), Some(u)) => (a, b, u),
                    _ => return (None, Some("banMatrixMember requires \"accountId\", \"bufferId\" and \"userId\"".to_string())),
                };
            let reason = p_str_opt(params, "reason");
            match backend::matrix::moderation::ban_member(state, account_id, buffer_id, user_id, reason).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "unbanMatrixMember" => {
            let (account_id, buffer_id, user_id) =
                match (p_str_opt(params, "accountId"), p_str_opt(params, "bufferId"), p_str_opt(params, "userId")) {
                    (Some(a), Some(b), Some(u)) => (a, b, u),
                    _ => return (None, Some("unbanMatrixMember requires \"accountId\", \"bufferId\" and \"userId\"".to_string())),
                };
            match backend::matrix::moderation::unban_member(state, account_id, buffer_id, user_id).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "muteMatrixMember" => {
            let (account_id, buffer_id, user_id) =
                match (p_str_opt(params, "accountId"), p_str_opt(params, "bufferId"), p_str_opt(params, "userId")) {
                    (Some(a), Some(b), Some(u)) => (a, b, u),
                    _ => return (None, Some("muteMatrixMember requires \"accountId\", \"bufferId\" and \"userId\"".to_string())),
                };
            match backend::matrix::moderation::mute_member(state, account_id, buffer_id, user_id).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        other => (None, Some(format!("unknown method \"{other}\""))),
    }
}

/// Reads `params.rooms` as `[{"id": .., "name": ..}, ...]`. `None` when the
/// field is absent entirely (so callers can tell "not provided" from "an
/// explicit empty list") vs. malformed entries, which are just skipped.
fn parse_sockchat_rooms(params: &Value) -> Option<Vec<crate::accounts::SockChatRoom>> {
    let arr = params.get("rooms")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|r| {
                let id = r.get("id")?.as_u64()? as u32;
                let name = r.get("name")?.as_str()?.to_string();
                Some(crate::accounts::SockChatRoom { id, name })
            })
            .collect(),
    )
}

fn account_mutation_result(result: anyhow::Result<bool>) -> (Option<Value>, Option<String>) {
    match result {
        Ok(true) => (Some(ok_node()), None),
        Ok(false) => (None, Some("no such account".to_string())),
        Err(e) => (None, Some(e.to_string())),
    }
}
