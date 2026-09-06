use super::wire::*;
use crate::accounts::IrcAccountConfig;
use crate::backend;
use crate::state::AppState;
use serde_json::Value;

/// One handler per JSON-RPC method (daemon/nobilis/api.c's handle_request,
/// split out of the socket-framing code). Returns (result, error) - exactly
/// one is Some, matching the wire contract's response shape.
/// An HTTP client and a token for a signed-in Kick account.
///
/// One place, because every moderation call needs both and the signed-out
/// case needs saying rather than reporting as "not connected" - the account
/// is connected and reading fine, it simply cannot act.
/// Which account a conversation belongs to, or an empty string - which every
/// lookup that takes one already treats as "no such account".
fn channel_account(state: &AppState, buffer_id: &str) -> String {
    state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default()
}

fn kick_credential(state: &AppState, account_id: &str) -> std::result::Result<(reqwest::Client, String), String> {
    let cfg = state.accounts.get_kick(account_id).ok_or_else(|| "no such account".to_string())?;
    let token = cfg
        .token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "sign in to Kick from Accounts to moderate".to_string())?;
    let http = backend::kick::api::client().map_err(|e| e.to_string())?;
    Ok((http, token))
}

/// Stops somebody's messages arriving, on whichever service they are on.
///
/// One place rather than two, because the menu entry and the typed command
/// are the same request. Answers with where the ignore holds: "account" when
/// the service itself was told and it follows the account everywhere, "window"
/// when there was nobody to tell and this is moho declining to show what still
/// arrives.
async fn apply_ignore(state: &AppState, account_id: &str, target: &str, ignored: bool) -> anyhow::Result<&'static str> {
    match crate::model::service_of(account_id) {
        // The homeserver keeps the list and stops sending the messages, so
        // there is nothing to keep here.
        "matrix" => {
            backend::matrix::set_ignored_user(state, account_id, target, ignored).await?;
            Ok("account")
        }
        // Discord has a real block. It is told as well as recorded, because a
        // block is account-wide and people expect it to hold in the phone app
        // too - and recorded as well as told, because the gateway keeps
        // delivering what a blocked person says and it is this client that
        // has to stop showing it.
        "discord" => {
            backend::discord::set_blocked(state, account_id, target, ignored).await?;
            state.ignores.set(account_id, target, ignored);
            Ok("account")
        }
        _ => {
            state.ignores.set(account_id, target, ignored);
            Ok("window")
        }
    }
}

pub async fn dispatch(
    state: &AppState,
    method: &str,
    params: &Value,
    subscriptions: &super::Subscriptions,
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
                if registered.contains(&crate::model::account_group_id(&account.id)) {
                    continue;
                }
                groups.push(account_group(&account));
            }
            groups.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.name.cmp(&b.name)));
            (Some(serde_json::to_value(groups).unwrap()), None)
        }

        // Whatever this conversation lets you put in a message that is not
        // text. Two services answer it and they answer differently - Discord's
        // emoji carry their picture in the token, while Kick's have to be
        // named alongside a URL and a verdict on whether this account may send
        // them - but the question the client is asking is the same one, so it
        // stays one method rather than becoming one per protocol.
        "listBufferEmoji" => match p_str_opt(params, "bufferId") {
            None => (None, Some("listBufferEmoji requires \"bufferId\"".to_string())),
            Some(buffer_id) if buffer_id.starts_with("kick:") => match state.runtime.kick_channel(buffer_id) {
                None => (Some(serde_json::json!([])), None),
                Some(channel) => (
                    Some(serde_json::json!(backend::kick::emotes::offer(&channel.emotes, channel.subscribed))),
                    None,
                ),
            },
            Some(buffer_id) => (Some(serde_json::json!(state.runtime.get_discord_buffer_emojis(buffer_id))), None),
        },

        // Sneedchat's own site-wide smiley table (see backend/sneedchat/
        // smilies.rs) - unlike Discord's per-guild emoji above, this is
        // fixed and global, so no bufferId is needed. `file` is a filename
        // within the client's own bundled sneedchat-smilies/ resource dir,
        // not a URL - these images ship with the client rather than being
        // fetched from kiwifarms.st at runtime, so the client resolves the
        // actual path itself (it already knows its own install directory)
        // and this needs no I/O of any kind to answer.
        "listSneedchatSmilies" => (
            Some(serde_json::json!(backend::sneedchat::smilies::SMILIES.iter().map(|s| serde_json::json!({ "label": s.label, "aliases": s.aliases, "file": s.file })).collect::<Vec<_>>())),
            None,
        ),

        // Stop the daemon cleanly. A client that adopted an already-running
        // nobilis has no child process to signal, so this is the only way it
        // can stop one it did not spawn. Returns before the exit actually
        // happens - the daemon still has to send QUITs to every connected
        // network first, and the caller's socket closing is the real signal
        // that it is gone.
        "shutdown" => {
            // After a moment, not now. Requests are answered in their own
            // tasks, so the "ok" to this one is still on its way to the
            // connection loop - and a daemon that exits first answers the
            // request to stop by dropping the socket, which a client cannot
            // tell apart from a crash.
            let state = state.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                state.shutdown.notify_one();
            });
            (Some(ok_node()), None)
        }

        // Every protocol says whether an account can actually be added for
        // it. Listing the planned ones without that flag was a promise the
        // daemon could not keep: a client drawing this list offered Slack,
        // and the add call came back "unknown method" because no such arm
        // exists. Dropping them instead would lose the fact that they are
        // planned, which is worth telling a client that wants to grey them
        // out - the settings pane already does exactly that.
        // What this daemon actually is. Asked because it is not otherwise
        // answerable from inside a running client: a frontend and the daemon
        // it adopted are built and deployed separately, and either can be
        // older than the other without anything looking wrong.
        "version" => (
            Some(serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "commit": env!("NOBILIS_BUILD_COMMIT"),
            })),
            None,
        ),

        "listProtocols" => (
            Some(serde_json::json!([
                { "id": "irc", "name": "IRC", "available": true },
                { "id": "matrix", "name": "Matrix", "available": true },
                { "id": "discord", "name": "Discord", "available": true },
                { "id": "sneedchat", "name": "Sneedchat", "available": true },
                { "id": "kick", "name": "Kick", "available": true },
                { "id": "jabber", "name": "XMPP", "available": false },
                { "id": "slack", "name": "Slack", "available": false },
            ])),
            None,
        ),

        "getBacklog" => match p_str_opt(params, "bufferId") {
            None => (None, Some("getBacklog requires \"bufferId\"".to_string())),
            Some(buffer_id) => {
                // Reading forward instead, for a reader who arrived in the
                // middle of a conversation and is coming back towards the
                // present. Answered on its own rather than falling through the
                // rest of this arm, none of which - the Discord catch-up, the
                // Kick cursor - is about that direction.
                let after = p_i64(params, "after", 0);
                if after > 0 {
                    let limit = p_i64(params, "limit", 200);
                    return match state.store.messages_after(buffer_id, after, limit) {
                        Ok(rows) => (Some(serde_json::to_value(rows).unwrap_or_default()), None),
                        Err(e) => (None, Some(e.to_string())),
                    };
                }
                let before = p_i64(params, "before", 0);
                let limit = p_i64(params, "limit", 200);
                // A nonzero `before` means the frontend is paginating
                // (scrolled to the top asking for older messages), not
                // doing the initial open-a-buffer fetch - only then is it
                // worth trying to pull more from Discord first, since the
                // initial fetch is already covered by the backfill that
                // runs when the buffer is first discovered (see backend/
                // discord.rs's backfill_channel_history).
                if let Some(buffer) = state.runtime.get_buffer(buffer_id) {
                    if let (Some(channel_id), Some(cfg)) =
                        (state.runtime.get_discord_channel(buffer_id), state.accounts.get_discord(&buffer.account_id))
                    {
                        if before > 0 {
                            backend::discord::extend_history(state, &cfg.token, &cfg.user_id, cfg.display_name.as_deref(), buffer_id, &channel_id).await;
                        } else {
                            // Opening a conversation is the moment to find out
                            // what was said while the client was closed. Done
                            // here rather than for every channel at once on
                            // connect: a few hundred channels would mean a few
                            // hundred requests, most for conversations nobody
                            // is about to read.
                            backend::discord::catch_up_channel(state, &cfg.token, &cfg.user_id, cfg.display_name.as_deref(), buffer_id, &channel_id).await;
                        }
                    }
                    // Kick pages back through its own history endpoint,
                    // continuing from where the last page stopped rather than
                    // by timestamp - the endpoint is cursor-based, and asking
                    // by time would re-fetch the same page forever.
                    if before > 0 && buffer.account_id.starts_with("kick:") {
                        if let Some(channel) = state.runtime.kick_channel(buffer_id) {
                            // No cursor means the start has been reached; the
                            // store already holds everything there is.
                            if let Some(cursor) = channel.history_cursor {
                                if let Ok(http) = backend::kick::api::client() {
                                    let next = backend::kick::backfill(
                                        state,
                                        &http,
                                        &buffer.account_id,
                                        &channel.slug,
                                        channel.channel_id,
                                        Some(&cursor),
                                    )
                                    .await;
                                    state.runtime.set_kick_history_cursor(buffer_id, next);
                                }
                            }
                        }
                    }

                    // IRC pages back through CHATHISTORY, where the server
                    // has it. Unlike the other two this is not a request with
                    // a reply - the history arrives as ordinary messages on
                    // the same connection - so there is nothing to await, and
                    // the store is watched instead until the page lands or it
                    // becomes clear nothing is coming.
                    //
                    // Crude, and the honest shape of the problem: a batch
                    // ending is not something this call can be handed. What it
                    // buys is that the page returned below already contains
                    // the history, rather than the history arriving afterwards
                    // as live messages and being appended to the bottom of a
                    // conversation it belongs at the top of.
                    if before > 0
                        && buffer.kind == "channel"
                        && state.runtime.irc_has_chathistory(&buffer.account_id)
                    {
                        if let Some(sender) = state.runtime.irc_sender(&buffer.account_id) {
                            if let Some(request) = backend::irc::chathistory_before(&buffer.name, before) {
                                let had = state.store.get_backlog(buffer_id, before, limit).map(|m| m.len()).unwrap_or(0);
                                if sender.send(request).is_ok() {
                                    backend::irc::await_history(state, buffer_id, before, limit, had).await;
                                }
                            }
                        }
                    }

                    // Matrix pages back through /messages the same way, and
                    // only when paginating: the initial open is served from
                    // what sync already delivered, and fetching history for
                    // every buffer somebody merely clicks on would be a
                    // request per glance.
                    if before > 0 && buffer.account_id.starts_with("matrix:") {
                        if let Err(e) = backend::matrix::backfill(state, &buffer.account_id, buffer_id, 50).await {
                            tracing::debug!("matrix backfill: {e}");
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
                subscriptions.lock().unwrap().insert(buffer_id.to_string());
                // Replay the last-known member list immediately - a fresh
                // subscribe otherwise only sees *future* presenceChange
                // pushes (join/part/etc.), leaving the userlist empty
                // until the next incremental change (see Runtime::
                // set_presence's doc comment).
                if let Some(members) = state.runtime.get_presence(buffer_id) {
                    state.events.emit("presenceChange", serde_json::json!({ "bufferId": buffer_id, "members": members }));
                }
                // Read markers are replayed for the same reason: they arrive
                // on sync and are not repeated once they stop moving, so a
                // window opened afterwards would show none until somebody
                // read something new.
                backend::matrix::replay_read_receipts(state, buffer_id);

                // And what a Kick channel is broadcasting, which changes while
                // nobody is looking: a viewer count from an hour ago is worse
                // than none, so opening the channel asks again.
                if let Some(stream) = state.runtime.kick_stream(buffer_id) {
                    state.events.emit("kickStream", stream);
                }
                // A poll or prediction already running when you opened the
                // channel: the events only carry the ones that change while
                // you are watching.
                for card in state.runtime.live_cards_for(buffer_id) {
                    // Only while it still means something. A card kept from
                    // an hour ago is not news to a window opening now, and
                    // one whose clock ran out before you arrived would open
                    // the conversation with somebody else's finished poll
                    // across the top of it.
                    if !backend::kick::card_is_current(&card) {
                        continue;
                    }
                    state.events.emit("pollCard", serde_json::json!({
                        "bufferId": buffer_id,
                        "kind": card["kind"].clone(),
                        "poll": card,
                    }));
                }
                // Asked of the account rather than of the channel: a window
                // restores the conversation it had open before the account
                // has finished connecting, and a gate on the channel's own
                // ids meant the poll and the prediction running right now
                // were never asked about at all. The refreshers wait for the
                // channel themselves.
                if state.runtime.get_buffer(buffer_id).is_some_and(|b| b.account_id.starts_with("kick:")) {
                    let (state, buffer_id) = (state.clone(), buffer_id.to_string());
                    tokio::spawn(async move {
                        backend::kick::refresh_stream(&state, &buffer_id).await;
                        backend::kick::refresh_poll(&state, &buffer_id).await;
                        // And the prediction, which carries two things no
                        // broadcast does: what this account has riding on it
                        // and what it has left to bet.
                        backend::kick::refresh_prediction(&state, &buffer_id).await;
                    });
                }
                (Some(ok_node()), None)
            }
        },

        "unsubscribe" => {
            if let Some(buffer_id) = p_str_opt(params, "bufferId") {
                subscriptions.lock().unwrap().remove(buffer_id);
            }
            (Some(ok_node()), None)
        }

        "addAccount" => {
            let (nick, host) = match (p_str_opt(params, "nick"), p_str_opt(params, "host")) {
                (Some(n), Some(h)) => (n, h),
                _ => return (None, Some("addAccount requires \"nick\" and \"host\"".to_string())),
            };
            let config = IrcAccountConfig {
                notify: String::new(),
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
                sasl_mechanism: p_str_opt(params, "saslMechanism").map(String::from),
                sasl_cert_path: p_str_opt(params, "saslCertPath").map(String::from),
                sasl_cert_pass: p_str_opt(params, "saslCertPass").map(String::from),
                // A link can name channels, so a new account can arrive with
                // somewhere to be rather than connecting to nothing.
                autojoin: p_str_opt(params, "autojoin").unwrap_or("").to_string(),
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
                // Its highlight words live in their own file rather than on
                // the account, so removing the account has to say so - or
                // they would come back with an account added under the same
                // name later.
                state.highlights.forget(id);
                state.ignores.forget(id);
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
                    } else if id.starts_with("sneedchat:") {
                        match state.accounts.get_sneedchat(id) {
                            Some(cfg) => {
                                backend::sneedchat::spawn(state.clone(), cfg);
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
                    } else if id.starts_with("kick:") {
                        match state.accounts.get_kick(id) {
                            Some(cfg) => {
                                backend::kick::spawn(state.clone(), cfg);
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

        // The words that count besides your name.
        //
        // Global ones and one account's own are the same call with and
        // without an account: they are the same kind of thing, and a message
        // only has to match one of them.
        "getHighlightKeywords" => (
            Some(serde_json::json!({ "global": state.highlights.global() })),
            None,
        ),

        "setHighlightKeywords" => {
            let Some(words) = params.get("keywords").and_then(|v| v.as_array()) else {
                return (None, Some("setHighlightKeywords requires \"keywords\"".to_string()));
            };
            let words: Vec<String> = words.iter().filter_map(|w| w.as_str().map(str::to_string)).collect();
            match p_str_opt(params, "accountId").filter(|id| !id.is_empty()) {
                Some(account_id) => {
                    if state.runtime.list_accounts(state).iter().all(|a| a.id != account_id) {
                        return (None, Some("no such account".to_string()));
                    }
                    // No event: the account carries its own words, and the
                    // client re-reads the accounts after a change it made -
                    // which is how every other per-account setting here
                    // finds its way back onto the screen.
                    state.highlights.set_account(account_id, words);
                }
                None => state.highlights.set_global(words),
            }
            (Some(serde_json::json!({ "global": state.highlights.global() })), None)
        }

        "setAccountDisplayName" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => {
                let renamed = account_mutation_result(state.accounts.set_display_name(id, p_str(params, "name", "")));
                // The rail entry carries the same name, and it is built from
                // the account rather than stored - so nothing would have told
                // anybody it changed, and the heading above the channel list
                // kept the old name until the client was restarted.
                if renamed.1.is_none() {
                    if let Some(account) = state.runtime.list_accounts(state).into_iter().find(|a| a.id == id) {
                        state.events.emit(
                            "bufferGroupChange",
                            serde_json::to_value(account_group(&account)).unwrap(),
                        );
                    }
                }
                renamed
            }
        },

        "setAccountSasl" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => account_mutation_result(state.accounts.set_sasl(
                id,
                p_bool(params, "enabled", false),
                p_str(params, "saslUser", ""),
                p_str(params, "password", ""),
                p_bool(params, "allowPlaintextSasl", false),
                p_str_opt(params, "saslMechanism"),
                p_str_opt(params, "saslCertPath"),
                p_str_opt(params, "saslCertPass"),
            )),
        },

        "joinBuffer" => {
            let (id, name) = match (p_str_opt(params, "accountId"), p_str_opt(params, "name")) {
                (Some(a), Some(n)) => (a, n),
                _ => return (None, Some("joinBuffer requires a known \"accountId\" and \"name\"".to_string())),
            };
            // Kick has no join to perform: a channel is a streamer, and naming
            // one is the whole of it. So this is "start watching", which the
            // connection does by subscribing its existing socket - and the
            // handle is remembered, so it comes back tomorrow.
            if id.starts_with("kick:") {
                let slug = backend::kick::api::normalise_slug(name);
                if slug.is_empty() {
                    return (None, Some(format!("\"{name}\" is not a Kick handle")));
                }
                let Some(sender) = state.runtime.kick_sender(id) else {
                    return (None, Some("that Kick account is not connected".to_string()));
                };
                if sender.send(backend::kick::Command::Join(slug.clone())).is_err() {
                    return (None, Some("that Kick account is not connected".to_string()));
                }
                if let Some(mut cfg) = state.accounts.get_kick(id) {
                    if !cfg.channels.contains(&slug) {
                        cfg.channels.push(slug);
                        let _ = state.accounts.set_kick_channels(id, cfg.channels);
                    }
                }
                return (Some(ok_node()), None);
            }
            match state.runtime.irc_sender(id) {
                None => (None, Some("join failed (account not connected?)".to_string())),
                Some(sender) => match sender.send_join(name) {
                    Ok(()) => (Some(ok_node()), None),
                    Err(e) => (None, Some(e.to_string())),
                },
            }
        }

        // "I am writing something." Discord and Matrix both carry it; IRC
        // and Sneedchat have no such notion, so this is quietly a no-op
        // there for the same reason markBufferRead is.
        //
        // Failure is swallowed rather than reported. A typing indicator that
        // did not arrive is not worth interrupting somebody mid-sentence
        // over, and this is called repeatedly while they write.
        "sendTyping" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("sendTyping requires \"bufferId\"".to_string()));
            };
            // Matrix can say "stopped", Discord cannot - it only expires.
            let typing = params.get("typing").and_then(|v| v.as_bool()).unwrap_or(true);
            match state.runtime.get_buffer(buffer_id) {
                Some(buffer) if buffer.account_id.starts_with("discord:") => {
                    if typing {
                        if let Some(cfg) = state.accounts.get_discord(&buffer.account_id) {
                            if let Err(e) = backend::discord::send_typing(state, buffer_id, &cfg.token).await {
                                tracing::debug!("sendTyping: {e}");
                            }
                        }
                    }
                    (Some(ok_node()), None)
                }
                Some(buffer) if buffer.account_id.starts_with("matrix:") => {
                    if let Some(room_id) = state.runtime.get_matrix_room(&buffer.id) {
                        if let Err(e) = backend::matrix::send_typing(state, &buffer.account_id, &room_id, typing).await {
                            tracing::debug!("sendTyping: {e}");
                        }
                    }
                    (Some(ok_node()), None)
                }
                // IRC says it with a client-only tag on an empty TAGMSG,
                // and says nothing at all where the server has no
                // message-tags to carry one - see send_typing.
                Some(buffer) if crate::model::service_of(&buffer.account_id) == "irc" => {
                    backend::irc::send_typing(state, &buffer.account_id, &buffer.name, typing);
                    (Some(ok_node()), None)
                }
                _ => (Some(ok_node()), None),
            }
        }

        // "I have read this." Only Discord has anywhere to put it - IRC and
        // Sneedchat have no read state at all, and Matrix's receipts are
        // their own piece of work - so this is quietly a no-op elsewhere
        // rather than an error, letting a client call it on every buffer it
        // opens without first asking what protocol it is.
        // A thread, read whole. Matrix is the only protocol here with the
        // idea; anything else answers with whatever is already stored, which
        // for a plain reply chain is just the message being pointed at.
        // The public room directory - Element's room explorer. Optionally
        // somebody else's directory, which is how a room on a server this
        // account has never touched is found at all.
        "searchMatrixRooms" => {
            let Some(account_id) = p_str_opt(params, "accountId") else {
                return (None, Some("searchMatrixRooms requires \"accountId\"".to_string()));
            };
            let query = p_str_opt(params, "query").unwrap_or("");
            // Extra homeservers to ask beyond the ones the daemon works out
            // for itself. A list, because the whole point is that the answers
            // are merged rather than looked at one server at a time.
            let servers: Vec<String> = params
                .get("servers")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            // Paging is per server: each has its own place in its own
            // directory, so one token could not describe where the merged
            // list had got to.
            let since = params.get("since").and_then(|v| v.as_object()).cloned().unwrap_or_default();
            let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(30) as u32;
            match backend::matrix::search_public_rooms(state, account_id, query, &servers, &since, limit).await {
                Ok(result) => (Some(result), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Who somebody is. Answered as a `profile` event rather than as this
        // call's own reply: IRC's answer is several numerics ending in one
        // that says the reply is over, and everything else needs a request
        // over the network that this daemon must not sit on (see rpc/mod.rs).
        //
        // What comes back immediately is the little that is known without
        // asking anyone - the name, and that a question is outstanding - so
        // the card opens filled in rather than blank.
        // Who and what can be tagged here, beyond the member list the client
        // already has: Discord's mentionable roles. Empty for every service
        // without the idea, so a client can ask unconditionally.
        // Muting a room for the account rather than for this window. Matrix
        // keeps it server-side, so a room quietened here is quiet on a phone
        // too - which is what somebody means by muting a room.
        "setMatrixRoomMuted" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("setMatrixRoomMuted requires \"bufferId\"".to_string()));
            };
            let muted = params.get("muted").and_then(|v| v.as_bool()).unwrap_or(true);
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            if !buffer.account_id.starts_with("matrix:") {
                return (Some(ok_node()), None);
            }
            match backend::matrix::set_room_muted(state, &buffer.account_id, buffer_id, muted).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "listMentionRoles" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("listMentionRoles requires \"bufferId\"".to_string()));
            };
            let roles = state
                .runtime
                .get_buffer(buffer_id)
                .filter(|b| b.account_id.starts_with("discord:"))
                .and_then(|b| b.group_id)
                .and_then(|g| g.rsplit_once("guild:").map(|(_, id)| id.to_string()))
                .map(|guild| state.runtime.discord_mentionable_roles(&guild))
                .unwrap_or_default();
            (Some(serde_json::json!({ "roles": roles })), None)
        }

        "requestProfile" => {
            let (Some(buffer_id), Some(who)) = (p_str_opt(params, "bufferId"), p_str_opt(params, "nick")) else {
                return (None, Some("requestProfile requires \"bufferId\" and \"nick\"".to_string()));
            };
            let user_id = p_str_opt(params, "userId").unwrap_or(who).to_string();
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let account_id = buffer.account_id.clone();
            let service = crate::model::service_of(&account_id).to_string();
            let (who, buffer_id) = (who.to_string(), buffer_id.to_string());
            // Built before the lookup is handed to a task, since that task
            // takes the name with it.
            let opening = crate::profile::pending(crate::model::service_of(&account_id), &account_id, &who);

            // IRC asks by sending a command; its answer arrives through the
            // same numerics /whois has always used.
            if service == "irc" {
                match state.runtime.irc_sender(&account_id) {
                    Some(sender) => {
                        state.runtime.begin_irc_whois(&account_id, &who);
                        let line = format!("WHOIS {who}");
                        if let Ok(message) = line.parse::<irc::proto::Message>() {
                            let _ = sender.send(message);
                        }
                    }
                    None => return (None, Some("not connected".to_string())),
                }
            } else {
                let state = state.clone();
                tokio::spawn(async move {
                    let profile = match service.as_str() {
                        "matrix" => backend::matrix::profile(&state, &account_id, &buffer_id, &user_id).await,
                        "discord" => backend::discord::profile(&state, &account_id, &buffer_id, &user_id, &who).await,
                        "kick" => backend::kick::profile(&state, &account_id, &buffer_id, &who).await,
                        "sneedchat" => backend::sneedchat::profile(&state, &account_id, &buffer_id, &who),
                        // A service with nothing to add still answers, so the
                        // card stops waiting and shows the name.
                        other => {
                            let mut bare = crate::profile::pending(other, &account_id, &who);
                            bare["pending"] = serde_json::json!(false);
                            bare
                        }
                    };
                    crate::profile::emit(&state, profile);
                });
            }

            (Some(opening), None)
        }

        "fetchThread" => {
            let (Some(buffer_id), Some(root_id)) = (p_str_opt(params, "bufferId"), p_str_opt(params, "rootId")) else {
                return (None, Some("fetchThread requires \"bufferId\" and \"rootId\"".to_string()));
            };
            let account_id = state.runtime.get_buffer(buffer_id).map(|b| b.account_id).unwrap_or_default();
            let messages = if account_id.starts_with("matrix:") {
                match backend::matrix::fetch_thread(state, &account_id, buffer_id, root_id).await {
                    Ok(messages) => messages,
                    Err(e) => return (None, Some(format!("{e:#}"))),
                }
            } else {
                state.store.thread_messages(buffer_id, root_id).unwrap_or_default()
            };
            (Some(serde_json::json!({ "messages": messages })), None)
        }

        "markBufferRead" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("markBufferRead requires \"bufferId\"".to_string()));
            };
            match state.runtime.get_buffer(buffer_id) {
                Some(buffer) if buffer.account_id.starts_with("discord:") => match state.accounts.get_discord(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => match backend::discord::ack_read(state, buffer_id, &cfg.token).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                },
                // Matrix has read markers and they were never sent, so a room
                // read here stayed bold everywhere else. Best-effort: failing
                // to say a room has been read must not make reading it look
                // like an error.
                Some(buffer) if buffer.account_id.starts_with("matrix:") => {
                    // Absent means yes: a client that has not been taught
                    // about the privacy toggle should behave as it did
                    // before there was one.
                    let publicly = params.get("public").and_then(|v| v.as_bool()).unwrap_or(true);
                    if let Err(e) = backend::matrix::mark_read(state, &buffer.account_id, buffer_id, publicly).await {
                        tracing::debug!("matrix read markers: {e:#}");
                    }
                    (Some(ok_node()), None)
                }
                _ => (Some(ok_node()), None),
            }
        }

        // What a server wants agreed to before it will let this account
        // speak, and agreeing to it. Two calls rather than one so the rules
        // can be read before they are accepted - agreeing to something
        // unseen is not agreement.
        "getDiscordMemberVerification" | "acceptDiscordMemberVerification" => {
            let (account_id, guild_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "guildId")) {
                (Some(a), Some(g)) => (a, g),
                _ => return (None, Some(format!("{method} requires \"accountId\" and \"guildId\""))),
            };
            if method == "getDiscordMemberVerification" {
                match backend::discord::member_verification(state, account_id, guild_id).await {
                    Ok(form) => (Some(form), None),
                    Err(e) => (None, Some(e.to_string())),
                }
            } else {
                match backend::discord::accept_member_verification(state, account_id, guild_id).await {
                    Ok(()) => (Some(ok_node()), None),
                    Err(e) => (None, Some(e.to_string())),
                }
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

        // A conversation with several people at once. Separate from
        // openDiscordDm rather than folded into it, because the two mean
        // different things to Discord: one id reuses the DM you already have
        // with that person, while a list always makes a new group. A caller
        // that could not tell them apart would quietly create a duplicate
        // conversation every time it opened a one-to-one.
        "openDiscordGroupDm" => {
            let account_id = match p_str_opt(params, "accountId") {
                Some(a) => a,
                None => return (None, Some("openDiscordGroupDm requires \"accountId\"".to_string())),
            };
            let user_ids: Vec<String> = params
                .get("userIds")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            match backend::discord::open_dm_with(state, account_id, &user_ids).await {
                Ok(buffer_id) => (Some(serde_json::json!({ "bufferId": buffer_id })), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "addToDiscordGroupDm" | "removeFromDiscordGroupDm" => {
            let (account_id, buffer_id, user_id) = match (
                p_str_opt(params, "accountId"),
                p_str_opt(params, "bufferId"),
                p_str_opt(params, "userId"),
            ) {
                (Some(a), Some(b), Some(u)) => (a, b, u),
                _ => {
                    return (
                        None,
                        Some(format!("{method} requires \"accountId\", \"bufferId\" and \"userId\"")),
                    )
                }
            };
            let channel_id = match state.runtime.get_discord_channel(buffer_id) {
                Some(c) => c,
                None => return (None, Some("that conversation is not a Discord channel".to_string())),
            };
            let result = if method == "addToDiscordGroupDm" {
                backend::discord::add_to_group_dm(state, account_id, &channel_id, user_id).await
            } else {
                backend::discord::remove_from_group_dm(state, account_id, &channel_id, user_id).await
            };
            match result {
                Ok(()) => (Some(ok_node()), None),
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

        // Answering a request somebody sent, or taking back one this account
        // sent. Both are the same pair of calls to Discord; which of the
        // three states you were in decides what it means.
        "answerDiscordFriendRequest" => {
            let (account_id, user_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "userId")) {
                (Some(a), Some(u)) => (a, u),
                _ => return (None, Some("answerDiscordFriendRequest requires \"accountId\" and \"userId\"".to_string())),
            };
            let accept = params.get("accept").and_then(|v| v.as_bool()).unwrap_or(false);
            match backend::discord::answer_friend_request(state, account_id, user_id, accept).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
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
            // Four rather than two. Do-not-disturb and invisible are what
            // people actually want from a client that is not the service's
            // own - invisible in particular is how you read a server without
            // being counted as present - and both are ordinary statuses to
            // Discord rather than a disconnection.
            //
            // Every service is told whatever it can make of them: Matrix has
            // no such distinction and maps them itself.
            if !matches!(status, "online" | "idle" | "dnd" | "invisible") {
                return (None, Some("status must be online, idle, dnd or invisible".to_string()));
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

        // A copy of this session's room keys, in Element's own encrypted
        // format - the one that does not depend on the homeserver being up.
        "exportMatrixKeys" => {
            let (account_id, path) = match (p_str_opt(params, "accountId"), p_str_opt(params, "path")) {
                (Some(a), Some(p)) => (a, p),
                _ => return (None, Some("exportMatrixKeys requires \"accountId\" and \"path\"".to_string())),
            };
            let Some(session) = state.runtime.get_matrix_machine(account_id) else {
                return (None, Some("account is not connected".to_string()));
            };
            match session.export_room_keys(p_str(params, "passphrase", "")).await {
                Ok((text, count)) => match tokio::fs::write(path, text).await {
                    Ok(()) => (Some(serde_json::json!({ "keys": count, "path": path })), None),
                    Err(e) => (None, Some(format!("could not write the file: {e}"))),
                },
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "importMatrixKeys" => {
            let (account_id, path) = match (p_str_opt(params, "accountId"), p_str_opt(params, "path")) {
                (Some(a), Some(p)) => (a, p),
                _ => return (None, Some("importMatrixKeys requires \"accountId\" and \"path\"".to_string())),
            };
            let Some(session) = state.runtime.get_matrix_machine(account_id) else {
                return (None, Some("account is not connected".to_string()));
            };
            let text = match tokio::fs::read_to_string(path).await {
                Ok(text) => text,
                Err(e) => return (None, Some(format!("could not read the file: {e}"))),
            };
            match session.import_room_keys(&text, p_str(params, "passphrase", "")).await {
                Ok((imported, total)) => (Some(serde_json::json!({ "imported": imported, "total": total })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // What the homeserver says this account is called and looks like -
        // which is a different question from the local rename beside it in
        // the account panel, and the only one anybody else can see.
        "matrixOwnProfile" => match p_str_opt(params, "accountId") {
            None => (None, Some("matrixOwnProfile requires \"accountId\"".to_string())),
            Some(account_id) => match backend::matrix::own_profile(state, account_id).await {
                Ok(profile) => (Some(profile), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            },
        },

        "setMatrixProfileName" => {
            let Some(account_id) = p_str_opt(params, "accountId") else {
                return (None, Some("setMatrixProfileName requires \"accountId\"".to_string()));
            };
            match backend::matrix::set_own_display_name(state, account_id, p_str(params, "name", "")).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "setMatrixProfileAvatar" => {
            let (account_id, path) = match (p_str_opt(params, "accountId"), p_str_opt(params, "path")) {
                (Some(a), Some(p)) => (a, p),
                _ => return (None, Some("setMatrixProfileAvatar requires \"accountId\" and \"path\"".to_string())),
            };
            match backend::matrix::set_own_avatar(state, account_id, path).await {
                Ok(url) => (Some(serde_json::json!({ "avatarUrl": url })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Who this account has asked never to hear from. The account's own
        // list, so it agrees with Element and travels to every client.
        "listMatrixIgnored" => match p_str_opt(params, "accountId") {
            None => (None, Some("listMatrixIgnored requires \"accountId\"".to_string())),
            Some(account_id) => {
                let mut users: Vec<String> = state.runtime.matrix_ignored(account_id).into_iter().collect();
                users.sort();
                (Some(serde_json::json!({ "accountId": account_id, "users": users })), None)
            }
        },

        "setMatrixIgnored" => {
            let (account_id, user_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "userId")) {
                (Some(a), Some(u)) => (a, u),
                _ => return (None, Some("setMatrixIgnored requires \"accountId\" and \"userId\"".to_string())),
            };
            let ignored = params.get("ignored").and_then(|v| v.as_bool()).unwrap_or(true);
            match backend::matrix::set_ignored_user(state, account_id, user_id, ignored).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Which conversations a room has going on beside the main one.
        //
        // From the server rather than from scrollback: a thread whose root
        // has scrolled past is unreachable otherwise, which is exactly the
        // thread somebody is looking for.
        "listMatrixThreads" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("listMatrixThreads requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let limit = p_i64(params, "limit", 25).clamp(1, 100) as u32;
            match backend::matrix::list_threads(state, &buffer.account_id, buffer_id, limit).await {
                Ok(answer) => (Some(answer), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // What a room has told everyone to read first.
        // What this account may do to other people here, and which roles it
        // could give them. Advisory - Discord re-checks every action - so a
        // failure to answer means no moderation entries rather than an error.
        "getDiscordPowers" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("getDiscordPowers requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let mut answer = backend::discord::guild_powers(state, &buffer.account_id, buffer_id);
            answer["roles"] = backend::discord::assignable_roles(state, buffer_id);

            (Some(answer), None)
        }

        // Removing somebody, barring them, or putting them in timeout.
        "moderateDiscordMember" => {
            let (buffer_id, user_id, action) = match (
                p_str_opt(params, "bufferId"),
                p_str_opt(params, "userId"),
                p_str_opt(params, "action"),
            ) {
                (Some(b), Some(u), Some(a)) => (b, u, a),
                _ => return (None, Some("moderateDiscordMember requires \"bufferId\", \"userId\" and \"action\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let minutes = params.get("minutes").and_then(|v| v.as_i64());
            let reason = p_str_opt(params, "reason");
            match backend::discord::moderate_member(state, &buffer.account_id, buffer_id, user_id, action, minutes, reason).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "setDiscordMemberRole" => {
            let (buffer_id, user_id, role_id) = match (
                p_str_opt(params, "bufferId"),
                p_str_opt(params, "userId"),
                p_str_opt(params, "roleId"),
            ) {
                (Some(b), Some(u), Some(r)) => (b, u, r),
                _ => return (None, Some("setDiscordMemberRole requires \"bufferId\", \"userId\" and \"roleId\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let give = params.get("give").and_then(|v| v.as_bool()).unwrap_or(true);
            match backend::discord::set_member_role(state, &buffer.account_id, buffer_id, user_id, role_id, give).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // One end of a Matrix call, signalled through the room.
        //
        // The media never comes near this daemon: the client has a WebRTC
        // stack and this process does not, so what crosses here is the offer,
        // the answer, the network candidates and the hangup - see
        // backend/matrix/calls.rs.
        "sendMatrixCallEvent" => {
            let (buffer_id, event_type) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "type")) {
                (Some(b), Some(t)) => (b, t),
                _ => return (None, Some("sendMatrixCallEvent requires \"bufferId\" and \"type\"".to_string())),
            };
            if !event_type.starts_with("m.call.") {
                return (None, Some("that is not a call event".to_string()));
            }
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let Some(content) = params.get("content").filter(|c| c.is_object()).cloned() else {
                return (None, Some("sendMatrixCallEvent requires \"content\"".to_string()));
            };
            match backend::matrix::calls::send(state, &buffer.account_id, buffer_id, event_type, content).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Where to reach this homeserver's relay, for a call between two
        // networks that will not talk to each other directly.
        "matrixTurnServers" => {
            let Some(account_id) = p_str_opt(params, "accountId") else {
                return (None, Some("matrixTurnServers requires \"accountId\"".to_string()));
            };
            match backend::matrix::calls::turn_servers(state, account_id).await {
                Ok(answer) => (Some(answer), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // The threads a Discord channel or forum is holding.
        //
        // Asked for when it is opened rather than at connect: a guild can be
        // holding hundreds, and the ones this account is already in arrive
        // with the guild anyway.
        "listDiscordThreads" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("listDiscordThreads requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            if !buffer.account_id.starts_with("discord:") {
                return (None, Some("that is not a Discord conversation".to_string()));
            }
            match backend::discord::list_threads(state, &buffer.account_id, buffer_id).await {
                Ok(answer) => (Some(answer), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Opens one of them as a conversation of its own.
        "openDiscordThread" => {
            let (buffer_id, thread_id) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "threadId")) {
                (Some(b), Some(t)) => (b, t),
                _ => return (None, Some("openDiscordThread requires \"bufferId\" and \"threadId\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            match backend::discord::open_thread(state, &buffer.account_id, buffer_id, thread_id).await {
                Ok(answer) => (Some(answer), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Every command that can be typed here, whoever implements it.
        //
        // One list rather than one per source: somebody typing a slash wants
        // to know what happens next, and whether the answer is this daemon's
        // own parser or a bot a server installed is not a distinction they
        // are asking about. The menu says which anyway, on the right, the way
        // Discord's own does.
        //
        // Built-ins are matched here rather than in the client for the reason
        // the table is here at all: the commands belong to the daemon, and a
        // second matcher in a window would be a second thing to keep in step.
        "listCommands" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("listCommands requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let query = p_str(params, "query", "");
            let service = crate::model::service_of(&buffer.account_id);
            let mut out: Vec<serde_json::Value> = crate::commands::matching(service, query)
                .into_iter()
                .map(|c| {
                    serde_json::json!({
                        "name": c.name,
                        "usage": c.usage,
                        "description": c.description,
                        "source": "Built-in",
                        "kind": "builtin",
                    })
                })
                .collect();

            // And whatever this channel's bots offer, which only Discord has.
            // Its own search does the matching, so a query no built-in
            // answers can still be answered here.
            if service == "discord" {
                match backend::discord::list_commands(state, &buffer.account_id, buffer_id, query).await {
                    Ok(answer) => {
                        if let Some(rows) = answer["commands"].as_array() {
                            for row in rows {
                                out.push(serde_json::json!({
                                    "name": row["name"],
                                    "usage": backend::discord::command_usage(&row["command"]),
                                    "description": row["description"],
                                    "source": row["application"].as_str().unwrap_or("Application"),
                                    "kind": "application",
                                    "command": row["command"],
                                }));
                            }
                        }
                    }
                    // Not an error: a channel whose bots cannot be asked
                    // about still has the built-ins, and a menu that refused
                    // to open because one half of it failed would be worse
                    // than a menu missing that half.
                    Err(e) => tracing::debug!("discord: listing commands: {e:#}"),
                }
            }
            (Some(serde_json::json!({ "commands": out })), None)
        }

        // The slash commands a channel's own bots offer, on their own.
        //
        // Listed per channel because that is the question with the right
        // answer: a bot installed across a guild can still be unusable in the
        // channel somebody is typing in.
        "listDiscordCommands" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("listDiscordCommands requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let query = p_str(params, "query", "");
            match backend::discord::list_commands(state, &buffer.account_id, buffer_id, query).await {
                Ok(answer) => (Some(answer), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "runDiscordCommand" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("runDiscordCommand requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            // The command as Discord described it, handed straight back:
            // Discord checks it against its own copy and refuses one that has
            // been rebuilt from the parts a client happened to show.
            let Some(command) = params.get("command").filter(|c| c.is_object()) else {
                return (None, Some("runDiscordCommand requires the command it was given".to_string()));
            };
            let options = params.get("options").cloned().unwrap_or_else(|| serde_json::json!([]));
            match backend::discord::run_command(state, &buffer.account_id, buffer_id, command, options).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Pressing a button on a message, or answering one of its menus.
        "useDiscordComponent" => {
            let (buffer_id, message_id, custom_id) = match (
                p_str_opt(params, "bufferId"),
                p_str_opt(params, "messageId"),
                p_str_opt(params, "customId"),
            ) {
                (Some(b), Some(m), Some(c)) => (b, m, c),
                _ => return (None, Some("useDiscordComponent requires \"bufferId\", \"messageId\" and \"customId\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let is_select = params.get("select").and_then(|v| v.as_bool()).unwrap_or(false);
            let values: Vec<String> = params
                .get("values")
                .and_then(|v| v.as_array())
                .map(|vs| vs.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            match backend::discord::use_component(state, &buffer.account_id, buffer_id, message_id, custom_id, is_select, values).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Discord's own search, over a whole server rather than this window's
        // copy of one channel.
        //
        // The filters are the ones Discord's own box takes, and they are sent
        // as names because that is what somebody types - resolving them to ids
        // happens in the backend, where the conversations are.
        "searchDiscordMessages" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("searchDiscordMessages requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            if !buffer.account_id.starts_with("discord:") {
                return (None, Some("that is not a Discord conversation".to_string()));
            }
            let text = |key: &str| p_str_opt(params, key).map(str::trim).filter(|v| !v.is_empty()).map(String::from);
            let filters = backend::discord::SearchFilters {
                content: text("query"),
                from: text("from"),
                mentions: text("mentions"),
                in_channel: text("in"),
                has: text("has"),
                before: params.get("before").and_then(|v| v.as_i64()),
                after: params.get("after").and_then(|v| v.as_i64()),
                offset: params.get("offset").and_then(|v| v.as_i64()).unwrap_or(0).clamp(0, 5000),
            };
            match backend::discord::search_messages(state, &buffer.account_id, buffer_id, &filters).await {
                Ok(answer) => (Some(answer), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Goes and gets the part of the conversation one message is in.
        //
        // What makes a pinned message or a search result reachable rather than
        // merely listed: both name a message that may be older than anything
        // stored here, and a client that could only page backwards would have
        // to read the whole conversation in between to reach it - which for a
        // pin from two years ago is not a wait, it is a refusal.
        //
        // Answers with when the message was sent, which is what a client needs
        // to read it back out of the store.
        "loadMessageContext" => {
            let (buffer_id, message_id) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "messageId")) {
                (Some(b), Some(m)) => (b, m),
                _ => return (None, Some("loadMessageContext requires \"bufferId\" and \"messageId\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let found = if buffer.account_id.starts_with("discord:") {
                backend::discord::load_context(state, &buffer.account_id, buffer_id, message_id).await
            } else if buffer.account_id.starts_with("matrix:") {
                backend::matrix::load_context(state, &buffer.account_id, buffer_id, message_id).await
            } else {
                Err(anyhow::anyhow!("this service cannot be asked for one message"))
            };
            match found {
                // The window itself, not just where to find it: the client
                // would otherwise have to page backwards from what it has to
                // reach these rows, which is the walk through everything in
                // between that this call exists to avoid.
                Ok(ts) => {
                    let around = state.store.messages_around(buffer_id, ts, 50).unwrap_or_default();
                    (
                        Some(serde_json::json!({
                            "ts": ts,
                            "messages": serde_json::to_value(around).unwrap_or_default(),
                        })),
                        None,
                    )
                }
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Reads forward from a message, towards the present.
        //
        // The other half of arriving in the middle of a conversation: what was
        // fetched around a pinned message ends somewhere, and the reader who
        // carries on downwards past it should be given the rest rather than
        // silently stepping over a hole into last week.
        "loadNewerMessages" => {
            let (buffer_id, message_id) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "messageId")) {
                (Some(b), Some(m)) => (b, m),
                _ => return (None, Some("loadNewerMessages requires \"bufferId\" and \"messageId\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let added = if buffer.account_id.starts_with("discord:") {
                backend::discord::load_newer(state, &buffer.account_id, buffer_id, message_id).await
            } else if buffer.account_id.starts_with("matrix:") {
                backend::matrix::load_newer(state, &buffer.account_id, buffer_id, message_id).await
            } else {
                Err(anyhow::anyhow!("this service cannot be read forward"))
            };
            match added {
                Ok(added) => (Some(serde_json::json!({ "added": added })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // What a conversation has told everyone to read first.
        //
        // One method for every service that has pins rather than one each:
        // the panel that shows them is the same panel, and the difference
        // between a Matrix pin and a Discord one is entirely on this side.
        "listPinned" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("listPinned requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let answer = if buffer.account_id.starts_with("discord:") {
                backend::discord::list_pinned(state, &buffer.account_id, buffer_id).await
            } else if buffer.account_id.starts_with("matrix:") {
                backend::matrix::list_pinned(state, &buffer.account_id, buffer_id).await
            } else {
                Err(anyhow::anyhow!("this service has no pinned messages"))
            };
            match answer {
                Ok(answer) => (Some(answer), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Pinning one, or taking the pin off. Whether this account may is the
        // server's decision, and its refusal is passed through in its words.
        "setPinned" => {
            let (buffer_id, message_id) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "messageId").or_else(|| p_str_opt(params, "eventId"))) {
                (Some(b), Some(e)) => (b, e),
                _ => return (None, Some("setPinned requires \"bufferId\" and \"messageId\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let pinned = params.get("pinned").and_then(|v| v.as_bool()).unwrap_or(true);
            let done = if buffer.account_id.starts_with("discord:") {
                backend::discord::set_pinned(state, &buffer.account_id, buffer_id, message_id, pinned).await
            } else if buffer.account_id.starts_with("matrix:") {
                backend::matrix::set_pinned(state, &buffer.account_id, buffer_id, message_id, pinned).await
            } else {
                Err(anyhow::anyhow!("this service has no pinned messages"))
            };
            match done {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // The same question asked of the homeserver rather than of this
        // window's own copy.
        //
        // Separate from `searchMessages` rather than folded into it: local
        // search is instant and offline, server search is a round trip that
        // can fail, and a client should be able to show the first while it
        // waits for the second. Every other protocol keeps the old method
        // untouched.
        "searchMatrixMessages" => {
            let (buffer_id, query) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "query")) {
                (Some(b), Some(q)) => (b, q),
                _ => return (None, Some("searchMatrixMessages requires \"bufferId\" and \"query\"".to_string())),
            };
            if query.trim().is_empty() {
                return (Some(serde_json::json!({ "results": [], "roomNames": {}, "count": 0 })), None);
            }
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            if !buffer.account_id.starts_with("matrix:") {
                return (None, Some("that conversation is not on Matrix".to_string()));
            }
            // "This room" or "everywhere" - the room is often exactly what
            // the person searching has forgotten.
            let room = match p_str(params, "scope", "room") {
                "account" => None,
                _ => state.runtime.get_matrix_room(buffer_id),
            };
            let limit = p_i64(params, "limit", 25).clamp(1, 100) as u32;
            match backend::matrix::search_messages(state, &buffer.account_id, room.as_deref(), query.trim(), limit).await {
                Ok(mut answer) => {
                    // Why an encrypted room comes back empty, said once here
                    // rather than guessed at by every frontend: the server
                    // holds ciphertext and cannot read it.
                    answer["encrypted"] = serde_json::json!(state.runtime.is_matrix_room_encrypted(buffer_id));
                    (Some(answer), None)
                }
                Err(e) => (None, Some(format!("{e:#}"))),
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

        // Answering a call that is ringing, and turning one down. Both take
        // the conversation rather than the channel, because that is what a
        // frontend has in its hand when somebody presses the button.
        "acceptCall" | "declineCall" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some(format!("{method} requires \"bufferId\"")));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such buffer".to_string()));
            };
            let Some(channel_id) = state.runtime.get_discord_channel(buffer_id) else {
                return (None, Some("that conversation has no Discord channel".to_string()));
            };
            let result = if method == "acceptCall" {
                backend::discord::accept_call(state, &buffer.account_id, &channel_id)
            } else {
                backend::discord::decline_call(state, &buffer.account_id, &channel_id).await
            };
            match result {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // What is ringing right now. The daemon outlives any one window, so a
        // client that opens mid-call has missed the event that announced it
        // and would otherwise show nothing while the phone is still going.
        "getIncomingCalls" => {
            let calls: Vec<Value> = state
                .runtime
                .ringing_calls()
                .into_iter()
                .map(|(buffer_id, account_id, channel_id)| {
                    serde_json::json!({
                        "accountId": account_id,
                        "bufferId": buffer_id,
                        "channelId": channel_id,
                        "ringing": true
                    })
                })
                .collect();
            (Some(Value::Array(calls)), None)
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
                                .discord_buffer_for_channel(id, &channel_id)
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
                        "bufferId": state.runtime.discord_buffer_for_channel(id, &channel_id),
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
                    // Everyone else in the call. Discord names a stream only
                    // when its owner starts speaking, so where that has not
                    // arrived the roster is what says who the audio can
                    // possibly be from.
                    let own = state.accounts.get_discord(id).map(|c| c.user_id).unwrap_or_default();
                    let others: Vec<String> = state
                        .runtime
                        .discord_voice_self(id)
                        .map(|channel| {
                            state
                                .runtime
                                .discord_voice_roster(id, &channel)
                                .into_iter()
                                .map(|m| m.user_id)
                                .filter(|u| *u != own)
                                .collect()
                        })
                        .unwrap_or_default();
                    serde_json::json!({
                        "accountId": id,
                        "micPeak": state.voice.input_level(id).unwrap_or(0.0),
                        "heardPeak": heard,
                        "receivedSamples": received,
                        // Who is audible right now, so a call view can ring
                        // the person talking rather than only showing that
                        // somebody is. Loudest first.
                        "speakers": state
                            .voice
                            .speakers(id, &others)
                            .into_iter()
                            .map(|(user_id, peak)| serde_json::json!({ "userId": user_id, "peak": peak }))
                            .collect::<Vec<_>>(),
                    })
                })
                .collect();
            (Some(serde_json::json!(levels)), None)
        }

        // What voice is set to use, and whether it is silenced. Read back
        // rather than assumed by a client, since a mute survives a restart and
        // a second window has to agree with the first.
        "getVoicePrefs" => (Some(serde_json::to_value(state.voice_prefs.get()).unwrap()), None),

        // Files offered over IRC. Offers and transfers are one list: what a
        // person is watching is a file arriving, and being offered is its
        // first state rather than a different kind of thing.
        "listTransfers" => {
            let rows: Vec<Value> =
                state.runtime.dcc_transfers().iter().map(backend::irc_dcc::transfer_json).collect();
            (Some(serde_json::json!(rows)), None)
        }

        // Offering somebody a file. The listening half of DCC, so it is also
        // the half with conditions on it - see offer_file.
        "sendFile" => {
            let (account_id, nick, path) =
                match (p_str_opt(params, "accountId"), p_str_opt(params, "nick"), p_str_opt(params, "path")) {
                    (Some(a), Some(n), Some(p)) => (a, n, p),
                    _ => return (None, Some("sendFile requires \"accountId\", \"nick\" and \"path\"".to_string())),
                };
            match backend::irc_dcc::offer_file(state, account_id, nick, path).await {
                Ok(id) => (Some(serde_json::json!({ "id": id })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "acceptTransfer" => {
            let Some(id) = p_str_opt(params, "id") else {
                return (None, Some("acceptTransfer requires \"id\"".to_string()));
            };
            backend::irc_dcc::accept(state, id);
            (Some(Value::Bool(true)), None)
        }

        // Turning down an offer and stopping one already running are the same
        // request from where the person is sitting - they want it to stop.
        "cancelTransfer" => {
            let Some(id) = p_str_opt(params, "id") else {
                return (None, Some("cancelTransfer requires \"id\"".to_string()));
            };
            backend::irc_dcc::cancel(state, id, "declined");
            (Some(Value::Bool(true)), None)
        }

        // The resolved directory goes out alongside the setting, so a window
        // can show where files will actually land rather than an empty box
        // that means "wherever the platform puts them".
        "getDccPrefs" => {
            let prefs = state.dcc_prefs.get();
            let mut out = serde_json::to_value(&prefs).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = out.as_object_mut() {
                obj.insert("resolvedDirectory".into(), Value::String(prefs.download_dir().display().to_string()));
            }
            (Some(out), None)
        }

        "setDccPrefs" => {
            let prefs = state.dcc_prefs.update(|p| {
                if let Some(dir) = p_str_opt(params, "directory") {
                    p.directory = (!dir.is_empty()).then(|| dir.to_string());
                }
                if let Some(n) = params.get("maxBytes").and_then(|v| v.as_u64()) {
                    p.max_bytes = n;
                }
                if let Some(n) = params.get("maxTransfers").and_then(|v| v.as_u64()) {
                    // One is the smallest useful answer; zero would mean
                    // nothing could ever be accepted, which the auto-accept
                    // toggle already expresses better.
                    p.max_transfers = (n as usize).max(1);
                }
                if let Some(n) = params.get("maxRate").and_then(|v| v.as_u64()) {
                    p.max_rate = n;
                }
                if let Some(b) = params.get("autoAccept").and_then(|v| v.as_bool()) {
                    p.auto_accept = b;
                }
                if let Some(ip) = p_str_opt(params, "advertisedIp") {
                    p.advertised_ip = (!ip.is_empty()).then(|| ip.to_string());
                }
            });
            state.events.emit("dccPrefsChanged", serde_json::to_value(&prefs).unwrap_or_default());
            (Some(serde_json::to_value(&prefs).unwrap_or_default()), None)
        }

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

        // Everyone in one voice channel, named.
        //
        // Separate from listVoiceChannels, which answers per guild and so has
        // nothing to say about a one-to-one call: a DM call happens in a
        // channel that belongs to no guild, and it is exactly the call a
        // person is most likely to be looking at.
        "listVoiceMembers" => {
            let (account_id, channel_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "channelId")) {
                (Some(a), Some(c)) => (a, c),
                _ => return (None, Some("listVoiceMembers requires \"accountId\" and \"channelId\"".to_string())),
            };
            let own = state.accounts.get_discord(account_id).map(|c| c.user_id).unwrap_or_default();
            let members: Vec<Value> = state
                .runtime
                .discord_voice_roster(account_id, channel_id)
                .into_iter()
                .map(|m| voice_member_json(&m, &own))
                .collect();
            (Some(serde_json::json!(members)), None)
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
                        .discord_voice_roster(account_id, &id)
                        .into_iter()
                        .map(|m| voice_member_json(&m, &own))
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
                        // Closing a conversation should leave it, not merely
                        // stop drawing it. Removing the buffer and nothing
                        // else meant the account was still in the room as far
                        // as everyone else in it was concerned, and the buffer
                        // came back on the next sync.
                        //
                        // What "leaving" means is the protocol's business.
                        // IRC parts a channel. Matrix leaves and forgets a
                        // room. A Discord direct message is closed, but one of
                        // a guild's channels cannot be left on its own - you
                        // are in it because you are in the guild - so that
                        // stays a local matter, as does Sneedchat, whose rooms
                        // are a fixed list rather than something joined.
                        if let Some(sender) = state.runtime.irc_sender(&buffer.account_id) {
                            let _ = sender.send_part(&buffer.name);
                        } else if buffer.account_id.starts_with("kick:") {
                            // Nothing to leave - watching a streamer is not a
                            // membership - so this unsubscribes the socket and
                            // forgets the handle, which is all "closing" can
                            // mean here.
                            if let Some(sender) = state.runtime.kick_sender(&buffer.account_id) {
                                let _ = sender.send(backend::kick::Command::Part(buffer.name.clone()));
                            }
                            if let Some(cfg) = state.accounts.get_kick(&buffer.account_id) {
                                let left: Vec<String> = cfg.channels.into_iter().filter(|c| *c != buffer.name).collect();
                                let _ = state.accounts.set_kick_channels(&buffer.account_id, left);
                            }
                        } else if buffer.account_id.starts_with("matrix:") {
                            if let Some(room_id) = state.runtime.get_matrix_room(buffer_id) {
                                if let Err(e) = backend::matrix::leave_room(state, &buffer.account_id, &room_id).await {
                                    return (None, Some(e.to_string()));
                                }
                            }
                        } else if buffer.kind == "dm" && buffer.account_id.starts_with("discord:") {
                            if let Some(channel_id) = state.runtime.get_discord_channel(buffer_id) {
                                if let Err(e) = backend::discord::close_dm(state, &buffer.account_id, &channel_id).await {
                                    return (None, Some(e.to_string()));
                                }
                            }
                        }
                        state.runtime.remove_buffer(state, buffer_id);
                        (Some(ok_node()), None)
                    }
                }
            }
        },

        // Everything that mentioned this account, across every conversation.
        //
        // Asked of the daemon rather than assembled by a client from what it
        // happens to have loaded: a mention worth an inbox is usually in a
        // channel nobody has opened, so the client has never seen it.
        "getMentions" => {
            // Channels only, which is the same rule Discord's own mentions
            // list follows. A direct message is addressed to you in its
            // entirety, so "mentioned" adds nothing there - and it already has
            // both its own page and its own tile in the column. Leaving them
            // in also dragged along every service robot that talks in a query:
            // on IRC, NickServ says your nickname in every line it sends, so
            // the page filled with "You are now identified for" and buried
            // the mentions it exists to collect.
            let buffers: Vec<String> = state
                .runtime
                .list_buffers()
                .into_iter()
                .filter(|b| b.kind == "channel")
                .map(|b| b.id)
                .collect();
            let limit = p_i64(params, "limit", 100);
            match state.store.mentions(&buffers, limit) {
                Ok(messages) => (Some(serde_json::to_value(messages).unwrap()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Where a file can be put so it can be linked to, and doing it.
        //
        // Listed rather than hard-coded into each client: the choice of where
        // somebody's files go is theirs, and a frontend drawing that menu
        // should not have to know the answer separately from the daemon that
        // performs it.
        "listUploadHosts" => (Some(serde_json::Value::Array(crate::upload::hosts())), None),

        "uploadFile" => {
            let Some(path) = p_str_opt(params, "path") else {
                return (None, Some("uploadFile requires \"path\"".to_string()));
            };
            let host = p_str_opt(params, "host")
                .and_then(crate::upload::Host::parse)
                .unwrap_or(crate::upload::Host::Catbox);
            match crate::upload::upload(host, path, p_str_opt(params, "retention")).await {
                Ok(url) => (Some(serde_json::json!({ "url": url })), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Leaving a whole guild or space, from its tile in the rail.
        //
        // The rail entry's own id carries everything needed - it is built as
        // "<accountId>|guild:<id>" or "<accountId>|space:<roomId>" - so there
        // is no separate lookup to do. An id of neither shape is an account's
        // own entry, which is not a thing that can be left.
        "leaveGroup" => {
            let Some(group_id) = p_str_opt(params, "groupId") else {
                return (None, Some("leaveGroup requires \"groupId\"".to_string()));
            };
            let Some((account_id, rest)) = group_id.split_once('|') else {
                return (None, Some("that is not something you can leave".to_string()));
            };
            let result = if let Some(guild_id) = rest.strip_prefix("guild:") {
                backend::discord::leave_guild(state, account_id, guild_id).await
            } else if let Some(room_id) = rest.strip_prefix("space:") {
                backend::matrix::leave_room(state, account_id, room_id).await
            } else {
                return (None, Some("that is not something you can leave".to_string()));
            };
            match result {
                Ok(()) => {
                    state.runtime.remove_buffer_group(state, group_id);
                    (Some(ok_node()), None)
                }
                Err(e) => (None, Some(e.to_string())),
            }
        }

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
            // The commands that are only a way of writing something, applied
            // before anybody decides how to send it: they mean the same on
            // every service because they are just text, and doing it here is
            // what keeps them meaning the same.
            let rewritten = state
                .runtime
                .get_buffer(buffer_id)
                .and_then(|b| crate::commands::rewrite(crate::model::service_of(&b.account_id), body));
            let body: &str = rewritten.as_deref().unwrap_or(body);
            // Not hearing from somebody, typed rather than clicked. Handled
            // here rather than in each backend because the answer is the same
            // on all of them and only one of them - IRC - has ever had the
            // command: it is the same list the menu writes to.
            if let Some(buffer) = state.runtime.get_buffer(buffer_id) {
                if let Some(rest) = body.strip_prefix("/ignore").or_else(|| body.strip_prefix("/unignore")) {
                    let ignoring = body.starts_with("/ignore");
                    let target = rest.trim();
                    let line = if target.is_empty() {
                        // Asking rather than setting, which is what a bare
                        // /ignore means in the clients that have one.
                        let list = state.ignores.for_account(&buffer.account_id);
                        if list.is_empty() {
                            "ignoring nobody - /ignore <name> adds somebody".to_string()
                        } else {
                            format!("ignoring: {}", list.join(", "))
                        }
                    } else {
                        // The same call the menu entry makes, so a typed
                        // ignore and a clicked one cannot mean different
                        // things - including telling Matrix and Discord,
                        // which have lists of their own.
                        match apply_ignore(state, &buffer.account_id, target, ignoring).await {
                            Err(e) => return (None, Some(format!("{e:#}"))),
                            Ok("account") if ignoring => format!("ignoring {target} - on this account everywhere"),
                            Ok(_) if ignoring => format!("ignoring {target} - in moho, since this service has no list of its own"),
                            Ok(_) => format!("no longer ignoring {target}"),
                        }
                    };
                    state.runtime.record_message(
                        state, &buffer.account_id, &buffer.name, &buffer.kind, "*", &line, false, "system",
                        None, None, false, None, Vec::new(), Vec::new(), None,
                    );
                    return (Some(ok_node()), None);
                }
            }
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
                Some(buffer) if buffer.account_id.starts_with("kick:") => {
                    let Some(cfg) = state.accounts.get_kick(&buffer.account_id) else {
                        return (None, Some("account not connected".to_string()));
                    };
                    // The one thing a signed-out Kick account cannot do. Said
                    // plainly, because "not connected" would be a lie - it is
                    // connected, and reading fine.
                    let Some(token) = cfg.token.as_deref().filter(|t| !t.is_empty()) else {
                        return (None, Some("sign in to Kick from Accounts to talk in chat".to_string()));
                    };
                    let Some(channel) = state.runtime.kick_channel(buffer_id) else {
                        return (None, Some("that channel is not being watched".to_string()));
                    };
                    // A reply needs the whole of what it answers, not just an
                    // id: Kick carries the original's text and author in the
                    // message so it quotes in everybody's window. All of it is
                    // in scrollback already, so this is a lookup rather than a
                    // round trip - and a reply to something that has scrolled
                    // out of the cap still sends, as an ordinary message,
                    // because losing the message would be the worse trade.
                    let reply_to = reply_to_id
                        .and_then(|id| state.store.get_message(buffer_id, id).ok().flatten())
                        .map(|m| backend::kick::api::ReplyTo {
                            message_id: m.id,
                            body: m.body,
                            sender_id: m.sender_id.as_deref().and_then(|s| s.parse().ok()),
                            sender_name: m.from,
                        });
                    // Kick chat has no attachments, so one is ignored rather
                    // than refused - the same way every other protocol-specific
                    // field is where it does not apply.
                    let http = match backend::kick::api::client() {
                        Ok(c) => c,
                        Err(e) => return (None, Some(e.to_string())),
                    };
                    match backend::kick::api::send_message(&http, token, channel.chatroom_id, body, reply_to.as_ref()).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(format!("{e:#}"))),
                    }
                }
                Some(buffer) if buffer.account_id.starts_with("sneedchat:") => {
                    // Sneedchat answers somebody by name rather than by
                    // message id, so a reply needs whoever wrote the message
                    // being replied to. A reply to something no longer in
                    // scrollback still sends, just without the mention -
                    // losing the message would be the worse trade.
                    let reply_to_nick = reply_to_id
                        .and_then(|id| state.store.get_message(buffer_id, id).ok().flatten())
                        .map(|m| m.from);
                    // Unlike Discord, Sneedchat's own chat protocol has no
                    // upload endpoint at all - see backend::sneedchat::
                    // send_attachment's own doc comment for how this
                    // still ends up posting a real image. The host choice
                    // reaches it the same way it reaches IRC below; without
                    // that it always went to postimg.cc, which takes images
                    // and nothing else.
                    let result = match attachment_path {
                        Some(path) => {
                            let host = p_str_opt(params, "uploadHost").and_then(crate::upload::Host::parse);
                            backend::sneedchat::send_attachment(state, &buffer.account_id, &buffer.name, body, path, host).await
                        }
                        None => backend::sneedchat::send_message(state, &buffer.account_id, &buffer.name, body, reply_to_nick.as_deref()),
                    };
                    match result {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    }
                }
                Some(buffer) if buffer.account_id.starts_with("matrix:") => match state.accounts.get_matrix(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(cfg) => match backend::matrix::send_message(state, &buffer.account_id, buffer_id, &cfg.access_token, body, reply_to_id, params.get("thread").and_then(|v| v.as_bool()).unwrap_or(false), attachment_path).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(e.to_string())),
                    },
                },
                Some(_) if reply_to_id.is_some() => (None, Some("replies aren't supported for this service".to_string())),
                Some(buffer) => match state.runtime.irc_sender(&buffer.account_id) {
                    None => (None, Some("account not connected".to_string())),
                    Some(sender) => {
                        // IRC carries text and nothing else, so an attachment
                        // is uploaded and the link sent - which is what a
                        // person does by hand on IRC anyway. The caption goes
                        // with it on the same line rather than as a second
                        // message, so the two cannot arrive out of order or
                        // be split by somebody else talking.
                        let body = match attachment_path {
                            None => body.to_string(),
                            Some(path) => {
                                let host = p_str_opt(params, "uploadHost")
                                    .and_then(crate::upload::Host::parse)
                                    .unwrap_or(crate::upload::Host::Catbox);
                                match crate::upload::upload(host, path, p_str_opt(params, "uploadRetention")).await {
                                    Ok(link) if body.is_empty() => link,
                                    Ok(link) => format!("{body} {link}"),
                                    Err(e) => return (None, Some(e.to_string())),
                                }
                            }
                        };
                        match backend::irc::send_message(state, &buffer.account_id, &sender, &buffer.name, &body) {
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
                Some(buffer) if buffer.account_id.starts_with("sneedchat:") => match backend::sneedchat::edit_message(state, &buffer.account_id, &buffer.name, msg_id, body) {
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
                Some(buffer) if buffer.account_id.starts_with("kick:") => {
                    match (kick_credential(state, &buffer.account_id), state.runtime.kick_channel(buffer_id)) {
                        (Err(e), _) => (None, Some(e)),
                        (_, None) => (None, Some("that channel is not being watched".to_string())),
                        (Ok((http, token)), Some(channel)) => {
                            match backend::kick::api::delete_message(&http, &token, channel.chatroom_id, msg_id).await {
                                Ok(()) => (Some(ok_node()), None),
                                Err(e) => (None, Some(format!("{e:#}"))),
                            }
                        }
                    }
                }
                Some(buffer) if buffer.account_id.starts_with("sneedchat:") => match backend::sneedchat::delete_message(state, &buffer.account_id, &buffer.name, msg_id) {
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

        // A token a frontend already obtained, by signing in on Discord's own
        // login page in a real browser window.
        //
        // Synchronous where the other two are not, and that is the point of
        // it: captcha, two-factor and device verification all happened in
        // that window, on Discord's own page, so there is no challenge left
        // to relay back and forth. All that remains is to check the token
        // works and save the account.
        "addDiscordAccountToken" => {
            let Some(token) = p_str_opt(params, "token") else {
                return (None, Some("addDiscordAccountToken requires \"token\"".to_string()));
            };
            let login_id = format!("discord-login-{}", crate::model::next_message_id());
            let reauth = p_str_opt(params, "accountId").map(String::from);
            match backend::discord::finish_token_login(state, &login_id, token.to_string(), reauth).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
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
        // sneedChatLoginStatus/sneedChatLoginResult events (see backend/
        // sneedchat/mod.rs's start_login).
        "addSneedChatAccount" => {
            let (username, password) = match (p_str_opt(params, "username"), p_str_opt(params, "password")) {
                (Some(u), Some(p)) => (u.to_string(), p.to_string()),
                _ => return (None, Some("addSneedChatAccount requires \"username\" and \"password\"".to_string())),
            };
            let username_for_cookies = username.clone();
            let config = crate::accounts::SneedChatAccountConfig {
                username,
                password,
                totp_secret: p_str_opt(params, "totpSecret").map(String::from),
                host: p_str_opt(params, "host").map(String::from).unwrap_or_else(|| backend::sneedchat::DEFAULT_ONION.to_string()),
                tor_mode: p_str_opt(params, "torMode").map(String::from).unwrap_or_else(|| "embedded".to_string()),
                proxy: p_str_opt(params, "proxy").map(String::from),
                rooms: parse_sneedchat_rooms(params).unwrap_or_default(),
                display_name: None,
                user_id: None,
                // Kept from a previous browser sign-in when this is the same
                // account being re-added with a corrected password: a live
                // session is the one thing here that still gets anybody in.
                cookies: state
                    .accounts
                    .get_sneedchat(&format!("sneedchat:{username_for_cookies}"))
                    .map(|old| old.cookies)
                    .unwrap_or_default(),
            };
            let login_id = format!("sneedchat-login-{}", crate::model::next_message_id());
            backend::sneedchat::start_login(state.clone(), login_id.clone(), config);
            (Some(serde_json::json!({ "loginId": login_id })), None)
        }

        // A session obtained by signing in somewhere with a screen.
        //
        // The forum's login form now carries a CAPTCHA, so a stored password
        // is no longer a way in on its own. The client opens the site's own
        // login page in a browser window, a person answers the verification
        // there, and what comes back is the session cookies - which is all
        // the daemon ever wanted from a password anyway.
        //
        // Takes the cookies as the header a browser would send: one string,
        // the shape the value already has wherever it came from.
        "setSneedChatCookies" => {
            let Some(raw) = p_str_opt(params, "cookies").filter(|c| !c.trim().is_empty()) else {
                return (None, Some("setSneedChatCookies requires \"cookies\"".to_string()));
            };
            let cookies: std::collections::BTreeMap<String, String> = raw
                .split(';')
                .filter_map(|pair| pair.split_once('='))
                .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                .collect();
            if cookies.is_empty() {
                return (None, Some("those do not look like cookies".to_string()));
            }

            // Which account they belong to: the one named, or the only one
            // there is. A person with a single Sneedchat account should not
            // have to say which - and a person with several must.
            let existing = state.accounts.all_sneedchat();
            let named = p_str_opt(params, "accountId").and_then(|id| state.accounts.get_sneedchat(id));
            // Who the browser says just signed in. It is also how an account
            // is found when the window is the only thing that has ever known
            // this person's name - which, with the login form gone, it is.
            let signed_in_as = p_str_opt(params, "username").map(str::trim).filter(|u| !u.is_empty());
            let config = named
                .or_else(|| signed_in_as.and_then(|u| state.accounts.get_sneedchat(&format!("sneedchat:{u}"))))
                .or_else(|| match existing.len() {
                    1 => existing.first().cloned(),
                    _ => None,
                });
            let mut config = match (config, signed_in_as) {
                (Some(found), _) => found,
                // Nobody to attach this to, but the site said who it is - so
                // the account is made here rather than demanded first. This
                // is the whole of adding a Sneedchat account now: the form
                // that used to take a password cannot get anybody in.
                (None, Some(username)) => crate::accounts::SneedChatAccountConfig {
                    username: username.to_string(),
                    password: String::new(),
                    totp_secret: None,
                    host: backend::sneedchat::DEFAULT_ONION.to_string(),
                    tor_mode: "embedded".to_string(),
                    proxy: None,
                    rooms: Vec::new(),
                    display_name: None,
                    user_id: None,
                    cookies: Default::default(),
                },
                (None, None) => {
                    return (
                        None,
                        Some("could not tell who signed in - add the account first, then sign in again".to_string()),
                    )
                }
            };
            // Deliberately not renamed to match whoever the browser signed in
            // as: an existing account's id is derived from its name, so a
            // rename here would leave the old account behind and quietly
            // start a second one beside it.
            config.cookies = cookies;
            let account_id = config.account_id();
            if let Err(e) = state.accounts.add_sneedchat(config.clone()) {
                return (None, Some(e.to_string()));
            }
            // Straight into a connection: the session is live now, and the
            // point of having just signed in is not to wait for a retry.
            backend::sneedchat::spawn(state.clone(), config);
            (Some(serde_json::json!({ "ok": true, "accountId": account_id })), None)
        }

        // What rooms the site has, read from the site rather than from a
        // list written into the client. Separate from setSneedChatRooms
        // because knowing which rooms exist and choosing which to join are
        // different acts - and a person may well want to see the catalogue
        // without changing anything.
        "listSneedChatRooms" => match p_str_opt(params, "accountId") {
            None => (None, Some("listSneedChatRooms requires \"accountId\"".to_string())),
            Some(id) => {
                // Whatever was read last, at once - and a fresh read started
                // behind it, which arrives as a sneedchatRooms event. Waiting
                // here would block this client's whole socket for the fifteen
                // seconds the site takes to answer through Tor and its gate.
                let cached = state.runtime.sneedchat_room_catalogue(id).unwrap_or_default();
                backend::sneedchat::refresh_rooms(state.clone(), id.to_string());
                (
                    Some(serde_json::json!({
                        "rooms": cached.iter().map(|r| serde_json::json!({ "id": r.id, "name": r.name })).collect::<Vec<_>>()
                    })),
                    None,
                )
            }
        },

        // Replaces the account's configured room list and reconnects it
        // immediately (rather than only on the next daemon restart) so a
        // freshly-added room shows up right away.
        "setSneedChatRooms" => match p_str_opt(params, "accountId") {
            None => (None, Some("no such account".to_string())),
            Some(id) => {
                let Some(rooms) = parse_sneedchat_rooms(params) else {
                    return (None, Some("setSneedChatRooms requires \"rooms\": [{\"id\":.., \"name\":..}, ...]".to_string()));
                };
                match state.accounts.set_sneedchat_rooms(id, rooms) {
                    Ok(true) => match state.accounts.get_sneedchat(id) {
                        Some(cfg) => {
                            backend::sneedchat::spawn(state.clone(), cfg);
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
        // affected account immediately, same as setSneedChatRooms above.
        "setTorConfig" => {
            let tor_mode = p_str(params, "torMode", "embedded").to_string();
            let proxy = p_str_opt(params, "proxy").filter(|s| !s.is_empty()).map(String::from);
            for cfg in state.accounts.all_sneedchat() {
                let id = cfg.account_id();
                if let Err(e) = state.accounts.set_sneedchat_tor_config(&id, tor_mode.clone(), proxy.clone()) {
                    return (None, Some(e.to_string()));
                }
                if let Some(cfg) = state.accounts.get_sneedchat(&id) {
                    backend::sneedchat::spawn(state.clone(), cfg);
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
            for cfg in state.accounts.all_sneedchat() {
                backend::sneedchat::spawn(state.clone(), cfg);
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
            for cfg in state.accounts.all_sneedchat() {
                backend::sneedchat::spawn(state.clone(), cfg);
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
        //
        // Slack answers here too. It was advertised by listProtocols with no
        // arm of its own, so asking for it fell through to "unknown method",
        // which reads as a client bug rather than as work not yet done.
        "addXmppAccount" | "addSlackAccount" => (None, Some(format!("{method}: not implemented yet"))),

        // Login (homeserver reachability + m.login.password) can take a
        // moment - same async-kickoff shape as addSneedChatAccount, with
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

        // How a homeserver lets people sign in, asked before anything is
        // typed: a server offering only SSO has no password to take, and a
        // form demanding one there is a form nobody can complete.
        "matrixLoginFlows" => {
            let Some(homeserver_url) = p_str_opt(params, "homeserverUrl") else {
                return (None, Some("matrixLoginFlows requires \"homeserverUrl\"".to_string()));
            };
            match backend::matrix::login_flows(homeserver_url).await {
                Ok(flows) => (Some(serde_json::json!({ "flows": flows })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Signing in through the homeserver's own web login. Answers with the
        // login id straight away; the URL to open arrives as a status event a
        // moment later, once the port it has to point at exists.
        "addMatrixAccountSso" => {
            let Some(homeserver_url) = p_str_opt(params, "homeserverUrl") else {
                return (None, Some("addMatrixAccountSso requires \"homeserverUrl\"".to_string()));
            };
            let login_id = format!("matrix-login-{}", crate::model::next_message_id());
            backend::matrix::start_sso_login(
                state.clone(),
                login_id.clone(),
                homeserver_url.to_string(),
                p_str_opt(params, "provider").map(|s| s.to_string()),
            );
            (Some(serde_json::json!({ "loginId": login_id })), None)
        }

        // Adds a Kick account, signed in or not.
        //
        // The token is optional and that is the interesting part: Kick's chat
        // is public, so an account with no credential reads every channel it
        // is pointed at. Somebody who only wants to watch never has to sign in
        // at all, and one who does gets sending and their subscriber emotes.
        //
        // Synchronous, unlike Discord's and Matrix's logins: there is nothing
        // to wait for. The browser window already did the signing in (see
        // moho's browser-login), so all this does is check who the token
        // belongs to and start the connection.
        "addKickAccount" => {
            let token = p_str_opt(params, "token")
                .map(backend::kick::api::bare_token)
                .filter(|t| !t.is_empty());
            let http = match backend::kick::api::client() {
                Ok(c) => c,
                Err(e) => return (None, Some(e.to_string())),
            };
            let username = match &token {
                Some(token) => match backend::kick::api::identity(&http, token).await {
                    Ok(who) => who.username.unwrap_or_default(),
                    Err(e) => return (None, Some(format!("{e:#}"))),
                },
                // A name rather than an empty one, because it becomes half the
                // account id and shows in the client as who this account is.
                None => "viewer".to_string(),
            };
            let existing = state.accounts.get_kick(&format!("kick:{username}"));
            let config = crate::accounts::KickAccountConfig {
                username,
                // Signing in again is not a reason to forget what this
                // account is called here.
                display_name: existing.as_ref().and_then(|e| e.display_name.clone()),
                token,
                // Signing in again keeps what was being watched, and keeps
                // knowing that the follows have already been read in. Losing
                // either to a re-login would be a poor trade for a refreshed
                // token - the second one would put back every channel the
                // person had closed.
                followed_synced: existing.as_ref().is_some_and(|e| e.followed_synced),
                channels: existing.map(|e| e.channels).unwrap_or_default(),
            };
            match state.accounts.add_kick(config) {
                Ok(saved) => {
                    let id = saved.account_id();
                    backend::kick::spawn(state.clone(), saved);
                    (Some(serde_json::json!({ "accountId": id })), None)
                }
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Moderation, for a Kick account that is a moderator of the channel.
        //
        // Kick is the authority and re-checks every one of these; what it
        // refuses comes back in its own words, which say far more than a
        // status code - not a moderator, no longer signed in, no such person.
        "moderateKickUser" => {
            let (account_id, buffer_id, username, action) = match (
                p_str_opt(params, "accountId"),
                p_str_opt(params, "bufferId"),
                p_str_opt(params, "username"),
                p_str_opt(params, "action"),
            ) {
                (Some(a), Some(b), Some(u), Some(action)) => (a, b, u, action),
                _ => {
                    return (
                        None,
                        Some("moderateKickUser requires \"accountId\", \"bufferId\", \"username\" and \"action\"".to_string()),
                    )
                }
            };
            let (http, token) = match kick_credential(state, account_id) {
                Ok(pair) => pair,
                Err(e) => return (None, Some(e)),
            };
            let Some(channel) = state.runtime.kick_channel(buffer_id) else {
                return (None, Some("that channel is not being watched".to_string()));
            };
            // A timeout is a ban with an end on it, which is Kick's own model
            // and the distinction a moderator actually means.
            let minutes = params.get("minutes").and_then(|v| v.as_u64()).map(|m| m as u32);
            let result = match action {
                "ban" => backend::kick::api::ban(&http, &token, &channel.slug, username, None).await,
                "timeout" => {
                    backend::kick::api::ban(&http, &token, &channel.slug, username, Some(minutes.unwrap_or(10))).await
                }
                "unban" => backend::kick::api::unban(&http, &token, &channel.slug, username).await,
                other => return (None, Some(format!("moderateKickUser does not know \"{other}\""))),
            };
            match result {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Who may talk in a channel: followers only, subscribers only, and
        // how long between messages.
        //
        // Sent whole because that is what the endpoint takes, so the values
        // not being changed are read back from what the channel already is
        // rather than guessed - a caller turning on slow mode must not
        // silently turn off followers-only on the way.
        "setKickChatMode" => {
            let (account_id, buffer_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "bufferId")) {
                (Some(a), Some(b)) => (a, b),
                _ => return (None, Some("setKickChatMode requires \"accountId\" and \"bufferId\"".to_string())),
            };
            let (http, token) = match kick_credential(state, account_id) {
                Ok(pair) => pair,
                Err(e) => return (None, Some(e)),
            };
            let Some(channel) = state.runtime.kick_channel(buffer_id) else {
                return (None, Some("that channel is not being watched".to_string()));
            };
            let followers = p_bool(params, "followersOnly", channel.followers_only);
            let subscribers = p_bool(params, "subscribersOnly", channel.subscribers_only);
            let slow = match params.get("slowSeconds") {
                Some(serde_json::Value::Null) => None,
                Some(v) => v.as_u64().map(|s| s as u32).filter(|s| *s > 0),
                None => channel.slow_seconds,
            };
            match backend::kick::api::set_chat_mode(&http, &token, &channel.slug, followers, subscribers, slow).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Reads this account's Kick follows again, and starts watching any
        // that are new.
        //
        // Needed because the automatic one deliberately happens once. That
        // flag exists so closing a channel sticks, and the cost of it is that
        // a streamer followed later never appears - so this is the way to ask
        // for them, rather than the sync being made to run on every connect
        // and quietly undoing every channel somebody had closed.
        //
        // Adds only. Un-following on Kick does not close the buffer here: by
        // this point the list is the person's own, and a sync that removed
        // things would be the same overreach as one that put them back.
        // Following a channel, or stopping. Deliberately only following:
        // subscribing is money and raiding is a broadcaster's own gesture,
        // and neither belongs behind a chat client's button.
        //
        // Kick's follow list is also what this account's channel list syncs
        // from, so doing it here keeps that list editable from the place it
        // is read.
        // The viewer count for the channel somebody is looking at.
        //
        // Kick pushes a stream starting and stopping but never the count, and
        // the client is the only side that knows which channel is on screen -
        // so it asks, on its own clock, for that one channel.
        "refreshKickStream" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("refreshKickStream requires \"bufferId\"".to_string()));
            };
            if state.runtime.kick_channel(buffer_id).is_none() {
                return (None, Some("that is not a Kick channel".to_string()));
            }
            let (state, buffer_id) = (state.clone(), buffer_id.to_string());
            // Throttled rather than forced. Every window showing a channel
            // asks on its own minute, so popping one out doubled the rate on
            // an endpoint Kick rate-limits readily - and being rate-limited is
            // indistinguishable, from here, from being signed out. The
            // throttle makes the cost of a second window nothing.
            tokio::spawn(async move { backend::kick::refresh_stream(&state, &buffer_id).await });
            (Some(ok_node()), None)
        }

        // A vote in the poll on screen. The answer Kick sends back is the
        // poll with the vote in it, so the card updates from the reply rather
        // than waiting for the broadcast that follows it.
        "votePoll" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("votePoll requires \"bufferId\"".to_string()));
            };
            // Kick numbers its options and Matrix gives them ids of their
            // own, so this is read as whichever it is and handed back to
            // whichever service the conversation belongs to.
            let option_id = params.get("optionId").and_then(|v| v.as_i64()).unwrap_or(-1);
            // Matrix polls answer the same question through a different
            // door: a vote is an event in the room, not a call to an API.
            if let Some(buffer) = state.runtime.get_buffer(buffer_id) {
                if buffer.account_id.starts_with("matrix:") {
                    let Some(poll_id) = state.runtime.live_card(buffer_id, "poll").and_then(|c| c["id"].as_str().map(|s| s.to_string()))
                    else {
                        return (None, Some("there is no poll open here".to_string()));
                    };
                    let answer = p_str_opt(params, "answerId")
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| option_id.to_string());
                    return match backend::matrix::vote_in_poll(state, &buffer.account_id, buffer_id, &poll_id, &answer).await {
                        Ok(()) => (Some(ok_node()), None),
                        Err(e) => (None, Some(format!("{e:#}"))),
                    };
                }
            }
            let Some(channel) = state.runtime.kick_channel(buffer_id) else {
                return (None, Some("that is not a Kick channel".to_string()));
            };
            let (http, token) = match kick_credential(state, &channel_account(state, buffer_id)) {
                Ok(pair) => pair,
                Err(e) => return (None, Some(e)),
            };
            match backend::kick::api::vote_poll(&http, &token, &channel.slug, option_id).await {
                Ok(poll) => {
                    backend::kick::announce_poll(state, buffer_id, Some(&poll));
                    (Some(ok_node()), None)
                }
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Points on an outcome. The answer carries the prediction with this
        // bet in it, so the card redraws from the reply rather than waiting
        // for the broadcast that follows.
        "betPrediction" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("betPrediction requires \"bufferId\"".to_string()));
            };
            let Some(option_id) = p_str_opt(params, "optionId") else {
                return (None, Some("betPrediction requires \"optionId\"".to_string()));
            };
            let amount = params.get("amount").and_then(|v| v.as_i64()).unwrap_or(0);
            let Some(channel) = state.runtime.kick_channel(buffer_id) else {
                return (None, Some("that is not a Kick channel".to_string()));
            };
            let (http, token) = match kick_credential(state, &channel_account(state, buffer_id)) {
                Ok(pair) => pair,
                Err(e) => return (None, Some(e)),
            };
            match backend::kick::api::vote_prediction(&http, &token, &channel.slug, option_id, amount).await {
                Ok(answer) => {
                    let points = backend::kick::api::points(&http, &token, &channel.slug).await.ok();
                    match answer {
                        Some((prediction, vote)) => {
                            backend::kick::announce_prediction_card(state, buffer_id, &prediction, vote.as_ref(), points)
                        }
                        // Took the bet and said nothing useful about it: ask.
                        None => backend::kick::refresh_prediction(state, buffer_id).await,
                    }
                    (Some(ok_node()), None)
                }
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Taking the poll down, which Kick allows the streamer and their
        // moderators and refuses to everybody else.
        "endPoll" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("endPoll requires \"bufferId\"".to_string()));
            };
            let Some(channel) = state.runtime.kick_channel(buffer_id) else {
                return (None, Some("that is not a Kick channel".to_string()));
            };
            let (http, token) = match kick_credential(state, &channel_account(state, buffer_id)) {
                Ok(pair) => pair,
                Err(e) => return (None, Some(e)),
            };
            match backend::kick::api::delete_poll(&http, &token, &channel.slug).await {
                Ok(()) => {
                    backend::kick::announce_poll(state, buffer_id, None);
                    (Some(ok_node()), None)
                }
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // What this conversation has voted on before now.
        //
        // Read from storage rather than from memory: a poll outlives the
        // minute it was open, and "what did we vote on last night" is a
        // question asked after a restart more often than before one.
        "listPolls" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("listPolls requires \"bufferId\"".to_string()));
            };
            let kind = p_str(params, "kind", "poll");
            let limit = params.get("limit").and_then(|v| v.as_i64()).unwrap_or(25).clamp(1, 200);
            match state.store.live_cards(buffer_id, kind, limit) {
                Ok(rows) => {
                    let cards: Vec<serde_json::Value> = rows
                        .into_iter()
                        .filter_map(|(body, ts)| {
                            let mut card: serde_json::Value = serde_json::from_str(&body).ok()?;
                            // When it was, which the card itself does not
                            // carry - it only ever knew how long it had left.
                            card["ts"] = serde_json::json!(ts);
                            Some(card)
                        })
                        .collect();
                    (Some(serde_json::json!(cards)), None)
                }
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "setKickFollowing" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("setKickFollowing requires \"bufferId\"".to_string()));
            };
            let follow = params.get("follow").and_then(|v| v.as_bool()).unwrap_or(true);
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such conversation".to_string()));
            };
            let Some(channel) = state.runtime.kick_channel(buffer_id) else {
                return (None, Some("that is not a Kick channel".to_string()));
            };
            let Some(token) = state.accounts.get_kick(&buffer.account_id).and_then(|c| c.token).filter(|t| !t.is_empty())
            else {
                return (None, Some("sign in to Kick from Accounts to follow channels".to_string()));
            };
            let http = match backend::kick::api::client() {
                Ok(c) => c,
                Err(e) => return (None, Some(e.to_string())),
            };
            match backend::kick::api::set_following(&http, &token, &channel.slug, follow).await {
                Ok(()) => {
                    // The header reads this, and it has just changed.
                    backend::kick::refresh_standing(state, buffer_id).await;
                    (Some(ok_node()), None)
                }
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "syncKickFollows" => {
            let Some(account_id) = p_str_opt(params, "accountId") else {
                return (None, Some("syncKickFollows requires \"accountId\"".to_string()));
            };
            let Some(cfg) = state.accounts.get_kick(account_id) else {
                return (None, Some("no such account".to_string()));
            };
            let Some(token) = cfg.token.as_deref().filter(|t| !t.is_empty()) else {
                return (None, Some("sign in to Kick from Accounts to read your follows".to_string()));
            };
            let http = match backend::kick::api::client() {
                Ok(c) => c,
                Err(e) => return (None, Some(e.to_string())),
            };
            let followed = match backend::kick::api::followed(&http, token).await {
                Ok(f) => f,
                Err(e) => return (None, Some(format!("{e:#}"))),
            };

            let mut channels = cfg.channels.clone();
            let mut added: Vec<String> = Vec::new();
            for slug in followed {
                if added.len() >= backend::kick::MAX_FOLLOWED {
                    break;
                }
                if !channels.contains(&slug) {
                    channels.push(slug.clone());
                    added.push(slug);
                }
            }

            if !added.is_empty() {
                if let Err(e) = state.accounts.set_kick_channels(account_id, channels) {
                    return (None, Some(e.to_string()));
                }
            }
            // Told to the live connection so they open now rather than at the
            // next restart. A disconnected account keeps them anyway - they
            // are persisted above - which is why this is not an error.
            let connected = match state.runtime.kick_sender(account_id) {
                Some(sender) => {
                    for slug in &added {
                        let _ = sender.send(backend::kick::Command::Join(slug.clone()));
                    }
                    true
                }
                None => false,
            };
            (Some(serde_json::json!({ "added": added.len(), "channels": added, "connected": connected })), None)
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

        // Where this account stands on cross-signing, and how to fix it.
        "matrixCrossSigningStatus" => match p_str_opt(params, "accountId") {
            None => (None, Some("matrixCrossSigningStatus requires \"accountId\"".to_string())),
            Some(account_id) => match backend::matrix::verification::cross_signing_status(state, account_id).await {
                Ok(status) => (Some(status), None),
                Err(e) => (None, Some(e.to_string())),
            },
        },

        // Creating the identity where there is none. Never a reset: replacing
        // one un-verifies every device you have, everywhere, for everyone -
        // not something a client should offer behind the same button.
        "matrixBootstrapCrossSigning" => match p_str_opt(params, "accountId") {
            None => (None, Some("matrixBootstrapCrossSigning requires \"accountId\"".to_string())),
            Some(account_id) => {
                match backend::matrix::verification::bootstrap_cross_signing(state, account_id, p_str(params, "password", "")).await {
                    Ok(()) => (Some(ok_node()), None),
                    Err(e) => (None, Some(format!("{e:#}"))),
                }
            }
        },

        // Verifying another person, which is a different gesture from
        // verifying one of your own sessions: it travels through a direct
        // message with them, made if there is not one.
        "startMatrixUserVerification" => {
            let (account_id, user_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "userId")) {
                (Some(a), Some(u)) => (a, u),
                _ => return (None, Some("startMatrixUserVerification requires \"accountId\" and \"userId\"".to_string())),
            };
            match backend::matrix::verification::start_user_verification(state, account_id, user_id).await {
                Ok(verification_id) => (Some(serde_json::json!({ "verificationId": verification_id })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "startMatrixVerification" => {
            let (account_id, device_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "deviceId")) {
                (Some(a), Some(d)) => (a, d),
                _ => return (None, Some("startMatrixVerification requires \"accountId\" and \"deviceId\"".to_string())),
            };
            match backend::matrix::verification::start_verification(state, account_id, p_str_opt(params, "userId"), device_id).await {
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

        // Invitations. Listed rather than turned into buffers: an invite is
        // not a room you are in, and answering it is a decision somebody has
        // to make. The set also arrives unprompted as a "matrixInvites"
        // event on every change, so this is for a client that has just
        // opened and missed the last one.
        "listMatrixInvites" => match p_str_opt(params, "accountId") {
            None => (None, Some("listMatrixInvites requires \"accountId\"".to_string())),
            Some(account_id) => (Some(serde_json::Value::Array(state.runtime.matrix_invites(account_id))), None),
        },

        // Accepting is an ordinary join, and declining is an ordinary leave;
        // the server drops the invitation either way and stops listing it,
        // which is how it disappears from the pending set on the next sync.
        "acceptMatrixInvite" | "declineMatrixInvite" => {
            let (account_id, room_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "roomId")) {
                (Some(a), Some(r)) => (a, r),
                _ => return (None, Some(format!("{method} requires \"accountId\" and \"roomId\""))),
            };
            let result = if method == "acceptMatrixInvite" {
                backend::matrix::join_room(state, account_id, room_id, &[]).await
            } else {
                backend::matrix::leave_room(state, account_id, room_id).await
            };
            match result {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "joinMatrixRoom" => {
            let (account_id, room) = match (p_str_opt(params, "accountId"), p_str_opt(params, "roomIdOrAlias")) {
                (Some(a), Some(r)) => (a, r),
                _ => return (None, Some("joinMatrixRoom requires \"accountId\" and \"roomIdOrAlias\"".to_string())),
            };
            let via: Vec<String> = params
                .get("via")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            match backend::matrix::join_room(state, account_id, room, &via).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // Starting an IRC conversation. Unlike every other protocol here
        // there is nothing to ask the server for - a query exists because a
        // client says it does - so this only creates the buffer and checks
        // whether the person is currently connected.
        // One way in for every service that has one-to-one conversations,
        // rather than a caller that has to know which method each of them
        // wants. What identifies somebody differs - IRC has only a nick,
        // Discord and Matrix have real ids - so both are taken and each
        // backend uses the one it can.
        //
        // The per-service methods below remain: they are what this dispatches
        // to, and they are still the right call where the caller already knows
        // which service it is talking about.
        "openDirectMessage" => {
            let Some(account_id) = p_str_opt(params, "accountId") else {
                return (None, Some("openDirectMessage requires \"accountId\"".to_string()));
            };
            let user_id = p_str_opt(params, "userId").unwrap_or("");
            let nick = p_str_opt(params, "nick").unwrap_or("");
            let Some(account) = state.runtime.list_accounts(state).into_iter().find(|a| a.id == account_id) else {
                return (None, Some(format!("no account {account_id}")));
            };

            let opened = match account.service.as_str() {
                // A query is local: IRC has no server-side notion of opening
                // one, so the nick is all there is and all that is needed.
                "irc" => {
                    let who = if nick.is_empty() { user_id } else { nick };
                    if who.is_empty() {
                        Err(anyhow::anyhow!("no nick to open a conversation with"))
                    } else {
                        backend::irc::open_query(state, account_id, who)
                    }
                }
                "discord" => {
                    if user_id.is_empty() {
                        Err(anyhow::anyhow!("Discord needs the person's id, not their name"))
                    } else {
                        backend::discord::open_dm(state, account_id, user_id).await
                    }
                }
                "matrix" => {
                    if user_id.is_empty() {
                        Err(anyhow::anyhow!("Matrix needs the person's id, not their name"))
                    } else {
                        backend::matrix::open_dm(state, account_id, user_id, nick).await
                    }
                }
                other => Err(anyhow::anyhow!("{other} has no direct messages")),
            };
            match opened {
                Ok(buffer_id) => (Some(serde_json::json!({ "bufferId": buffer_id })), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        // A private message on Sneedchat, which has no direct-message
        // conversations of its own: a whisper is addressed by name and shown
        // in the room, rather than opening anything.
        //
        // `bufferId` is where it was sent from, so it appears in that
        // conversation rather than in every room the account is in. Optional,
        // because a caller with no room in mind is answered rather than
        // refused - it simply goes everywhere instead.
        "sendWhisper" => {
            let (account_id, target, body) =
                match (p_str_opt(params, "accountId"), p_str_opt(params, "target"), p_str_opt(params, "body")) {
                    (Some(a), Some(t), Some(b)) => (a, t, b),
                    _ => return (None, Some("sendWhisper requires \"accountId\", \"target\" and \"body\"".to_string())),
                };
            let from_buffer = p_str_opt(params, "bufferId")
                .and_then(|id| state.runtime.get_buffer(id))
                .filter(|b| b.account_id == account_id)
                .map(|b| b.name);
            match backend::sneedchat::send_whisper(state, account_id, target, body, from_buffer.as_deref()) {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(e.to_string())),
            }
        }

        "openIrcQuery" => {
            let (account_id, nick) = match (p_str_opt(params, "accountId"), p_str_opt(params, "nick")) {
                (Some(a), Some(n)) => (a, n),
                _ => return (None, Some("openIrcQuery requires \"accountId\" and \"nick\"".to_string())),
            };
            match backend::irc::open_query(state, account_id, nick) {
                Ok(buffer_id) => (Some(serde_json::json!({ "bufferId": buffer_id })), None),
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

        // Making a room, or a space - which is a room with a different
        // creation type and nothing else different, so it is one call.
        "createMatrixRoom" => {
            let Some(account_id) = p_str_opt(params, "accountId") else {
                return (None, Some("createMatrixRoom requires \"accountId\"".to_string()));
            };
            let name = p_str(params, "name", "").trim().to_string();
            if name.is_empty() {
                return (None, Some("a room needs a name".to_string()));
            }
            match backend::matrix::create_room(
                state,
                account_id,
                &name,
                p_str(params, "topic", ""),
                p_bool(params, "isSpace", false),
                p_bool(params, "isPublic", false),
                p_bool(params, "encrypted", false),
            )
            .await
            {
                Ok(room_id) => (Some(serde_json::json!({ "roomId": room_id })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // A room's own name and topic. Two state events with one shape, which
        // is why they share a method rather than having one each.
        // The account's own stickers, from the packs it carries and the ones
        // its rooms share.
        "listMatrixStickers" => {
            let Some(buffer_id) = p_str_opt(params, "bufferId") else {
                return (None, Some("listMatrixStickers requires \"bufferId\"".to_string()));
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such buffer".to_string()));
            };
            let account = match state.accounts.get_matrix(&buffer.account_id) {
                Some(a) => a,
                None => return (Some(serde_json::json!([])), None),
            };
            let mut out = Vec::new();
            for sticker in state.runtime.matrix_stickers(&buffer.account_id) {
                // Resolved here, where the token is: an mxc URI names media on
                // a homeserver and is not a URL anything can load.
                let path = backend::matrix::cached_media_path(&account.homeserver_url, &account.access_token, &sticker.mxc, "").await;
                out.push(serde_json::json!({
                    "name": sticker.name,
                    "pack": sticker.pack,
                    "mxc": sticker.mxc,
                    "body": sticker.body,
                    "url": path,
                }));
            }
            (Some(serde_json::json!(out)), None)
        }

        "sendMatrixSticker" => {
            let (buffer_id, mxc) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "mxc")) {
                (Some(b), Some(m)) => (b, m),
                _ => return (None, Some("sendMatrixSticker requires \"bufferId\" and \"mxc\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such buffer".to_string()));
            };
            match backend::matrix::send_sticker(state, &buffer.account_id, buffer_id, mxc, p_str(params, "body", "sticker")).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Somewhere, as a pin. Live location sharing is a different feature
        // with different promises and is deliberately not this.
        "sendMatrixLocation" => {
            let (buffer_id, place) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "place")) {
                (Some(b), Some(p)) => (b, p),
                _ => return (None, Some("sendMatrixLocation requires \"bufferId\" and \"place\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such buffer".to_string()));
            };
            match backend::matrix::send_location(state, &buffer.account_id, buffer_id, place, p_str(params, "label", "")).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Not hearing from somebody, on whichever service they are on.
        //
        // One method rather than one per backend, because the person doing it
        // is answering the same question everywhere - and the difference in
        // what it costs them is worth saying out loud, which is what the
        // answer carries back.
        "setIgnored" => {
            let (account_id, target) = match (p_str_opt(params, "accountId"), p_str_opt(params, "target")) {
                (Some(a), Some(t)) => (a, t),
                _ => return (None, Some("setIgnored requires \"accountId\" and \"target\"".to_string())),
            };
            let ignored = params.get("ignored").and_then(|v| v.as_bool()).unwrap_or(true);
            match apply_ignore(state, account_id, target, ignored).await {
                Ok(scope) => (Some(serde_json::json!({ "scope": scope })), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "listIgnored" => {
            let Some(account_id) = p_str_opt(params, "accountId") else {
                return (None, Some("listIgnored requires \"accountId\"".to_string()));
            };
            if crate::model::service_of(account_id) == "matrix" {
                // The homeserver's list, which the sync already keeps here.
                let mut users: Vec<String> = state.runtime.matrix_ignored(account_id).into_iter().collect();
                users.sort();
                return (Some(serde_json::json!(users)), None);
            }
            (Some(serde_json::json!(state.ignores.for_account(account_id))), None)
        }

        // Asking to be let into a room that is asked rather than entered.
        "knockMatrixRoom" => {
            let (account_id, room) = match (p_str_opt(params, "accountId"), p_str_opt(params, "roomIdOrAlias")) {
                (Some(a), Some(r)) => (a, r),
                _ => return (None, Some("knockMatrixRoom requires \"accountId\" and \"roomIdOrAlias\"".to_string())),
            };
            let via: Vec<String> = params
                .get("via")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            match backend::matrix::knock_room(state, account_id, room, &via, p_str(params, "reason", "")).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Telling whoever runs the server about a message. Needs no power in
        // the room, which is the whole point of it.
        "reportMatrixMessage" => {
            let (buffer_id, message_id) = match (p_str_opt(params, "bufferId"), p_str_opt(params, "messageId")) {
                (Some(b), Some(m)) => (b, m),
                _ => return (None, Some("reportMatrixMessage requires \"bufferId\" and \"messageId\"".to_string())),
            };
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such buffer".to_string()));
            };
            match backend::matrix::report_message(state, &buffer.account_id, buffer_id, message_id, p_str(params, "reason", "")).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Putting a room in a space, or taking it out of one.
        "setMatrixSpaceChild" => {
            let (space_group_id, buffer_id) = match (p_str_opt(params, "spaceId"), p_str_opt(params, "bufferId")) {
                (Some(s), Some(b)) => (s, b),
                _ => return (None, Some("setMatrixSpaceChild requires \"spaceId\" and \"bufferId\"".to_string())),
            };
            let child = params.get("child").and_then(|v| v.as_bool()).unwrap_or(true);
            let Some(buffer) = state.runtime.get_buffer(buffer_id) else {
                return (None, Some("no such buffer".to_string()));
            };
            let Some(child_room) = state.runtime.get_matrix_room(buffer_id) else {
                return (None, Some("that conversation is not a Matrix room".to_string()));
            };
            // The client names the space by its group id, which is what the
            // rail and the room list both hold; the room id is inside it.
            let Some(space_room) = crate::backend::matrix::space_room_id(space_group_id) else {
                return (None, Some("that is not a Matrix space".to_string()));
            };
            match backend::matrix::set_space_child(state, &buffer.account_id, &space_room, &child_room, child).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        "setMatrixRoomState" => {
            let (account_id, buffer_id) = match (p_str_opt(params, "accountId"), p_str_opt(params, "bufferId")) {
                (Some(a), Some(b)) => (a, b),
                _ => return (None, Some("setMatrixRoomState requires \"accountId\" and \"bufferId\"".to_string())),
            };
            let mut done = false;
            if let Some(name) = p_str_opt(params, "name") {
                if let Err(e) = backend::matrix::set_room_state(state, account_id, buffer_id, "m.room.name", serde_json::json!({ "name": name })).await {
                    return (None, Some(format!("{e:#}")));
                }
                done = true;
            }
            if let Some(topic) = p_str_opt(params, "topic") {
                if let Err(e) = backend::matrix::set_room_state(state, account_id, buffer_id, "m.room.topic", serde_json::json!({ "topic": topic })).await {
                    return (None, Some(format!("{e:#}")));
                }
                done = true;
            }
            if done {
                (Some(ok_node()), None)
            } else {
                (None, Some("setMatrixRoomState needs a \"name\" or a \"topic\"".to_string()))
            }
        }

        // Promoting or demoting somebody. Power levels were readable and
        // movable only downwards, through mute - so nobody could be made an
        // operator of a room from here.
        "setMatrixPowerLevel" => {
            let (account_id, buffer_id, user_id) =
                match (p_str_opt(params, "accountId"), p_str_opt(params, "bufferId"), p_str_opt(params, "userId")) {
                    (Some(a), Some(b), Some(u)) => (a, b, u),
                    _ => {
                        return (
                            None,
                            Some("setMatrixPowerLevel requires \"accountId\", \"bufferId\" and \"userId\"".to_string()),
                        )
                    }
                };
            let Some(level) = params.get("level").and_then(|v| v.as_i64()) else {
                return (None, Some("setMatrixPowerLevel requires a numeric \"level\"".to_string()));
            };
            match backend::matrix::moderation::set_power_level(state, account_id, buffer_id, user_id, level).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
            }
        }

        // Asking somebody into a room. Its absence was the odd one out: this
        // client could accept an invitation and decline one, and never send
        // one - so a room created here could never gain a second member here.
        "inviteMatrixMember" => {
            let (account_id, buffer_id, user_id) =
                match (p_str_opt(params, "accountId"), p_str_opt(params, "bufferId"), p_str_opt(params, "userId")) {
                    (Some(a), Some(b), Some(u)) => (a, b, u),
                    _ => {
                        return (
                            None,
                            Some("inviteMatrixMember requires \"accountId\", \"bufferId\" and \"userId\"".to_string()),
                        )
                    }
                };
            // A Matrix id or nothing. The server would refuse anything else
            // anyway, but its error names an endpoint rather than the thing
            // that was typed.
            if !user_id.starts_with('@') || !user_id.contains(':') {
                return (None, Some(format!("\"{user_id}\" is not a Matrix address - they look like @someone:server")));
            }
            match backend::matrix::moderation::invite_member(state, account_id, buffer_id, user_id).await {
                Ok(()) => (Some(ok_node()), None),
                Err(e) => (None, Some(format!("{e:#}"))),
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
fn parse_sneedchat_rooms(params: &Value) -> Option<Vec<crate::accounts::SneedChatRoom>> {
    let arr = params.get("rooms")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|r| {
                let id = r.get("id")?.as_u64()? as u32;
                let name = r.get("name")?.as_str()?.to_string();
                Some(crate::accounts::SneedChatRoom { id, name })
            })
            .collect(),
    )
}

/// The rail entry an account has by being an account.
///
/// Not stored anywhere: guilds and spaces are registered by the backends that
/// discover them, but an account's own entry is whatever the account says it
/// is right now - which is why renaming one has to say so itself.
fn account_group(account: &crate::model::Account) -> crate::model::BufferGroup {
    crate::model::BufferGroup {
        id: crate::model::account_group_id(&account.id),
        account_id: account.id.clone(),
        service: account.service.clone(),
        kind: "account".to_string(),
        name: account.display_name.clone(),
        icon_url: account.avatar_url.clone(),
        // After the guilds, which are the entries a user picks between most
        // often.
        position: 1000,
        pending: false,
    }
}

fn account_mutation_result(result: anyhow::Result<bool>) -> (Option<Value>, Option<String>) {
    match result {
        Ok(true) => (Some(ok_node()), None),
        Ok(false) => (None, Some("no such account".to_string())),
        Err(e) => (None, Some(e.to_string())),
    }
}

/// One person in a voice channel, as a frontend sees them.
///
/// The flags are what make a call view more than a list of names: whether
/// somebody is sharing a screen, has a camera on, or is silent. Discord sends
/// all four on the voice state and nowhere else, so this is the only place a
/// client can learn them.
fn voice_member_json(m: &crate::runtime::VoiceRosterEntry, own: &str) -> Value {
    serde_json::json!({
        "userId": m.user_id,
        "nick": m.name,
        "isSelf": m.user_id == own,
        // Absent rather than null when we have never seen their face, which
        // is what the frontend already falls back on for everyone else.
        "avatarUrl": m.avatar_url,
        "streaming": m.flags.streaming,
        "video": m.flags.video,
        "muted": m.flags.muted,
        "deafened": m.flags.deafened,
    })
}
