use crate::accounts::IrcAccountConfig;
use crate::model::MemberRank;
use crate::nickserv::NickservWait;
use crate::runtime::{ConnState, IrcHandle};
use crate::state::AppState;
use anyhow::{anyhow, bail, Result};
use base64::Engine;
use futures::prelude::*;
use irc::client::prelude::*;
use irc::client::ClientStream;
use irc::proto::CapSubCommand;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
// A real IRC network's anti-flood/throttling can silently drop new
// connection attempts for a while after too many in quick succession from
// the same source - confirmed live: disconnecting and immediately
// reconnecting a couple of times in a row was enough to trigger it against
// both Libera and Rizon simultaneously (their connection attempts just sat
// hung mid-TCP-handshake). No amount of timeout tuning on our end fixes
// that - the actual fix is not re-triggering it, by pacing out our own
// attempts per account.
const MIN_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

/// Spawns a background task that connects this account and runs its event
/// loop for as long as the connection lasts. No auto-reconnect in this
/// milestone (see project plan) - a failure just reports connectionState
/// "disconnected" with an error and stops, same as killing/restarting the
/// whole daemon would already require for other recovery paths.
///
/// The whole attempt is wrapped in catch_unwind - a bug anywhere in run()
/// must still leave the account in a resolved (not stuck-"connecting")
/// state, since nothing else would otherwise clear it (a panic inside a
/// bare tokio::spawn'd task, if left uncaught here, unwinds straight past
/// the state-setting code below it and the task just vanishes).
pub fn spawn(state: AppState, config: IrcAccountConfig) {
    let account_id = config.account_id();
    // Guarantee at most one live connection attempt per account. Without
    // this, an old task still mid-graceful-QUIT (or still stuck connecting)
    // stayed alive at the same time this new one starts, racing it as a
    // second, independent connection under the same nick - see
    // reset_connection()'s doc comment for why that's the actual root cause
    // of accounts getting permanently stuck at "connecting" after a fast
    // disconnect-then-reconnect.
    state.runtime.reset_connection(&account_id);
    // Claimed first, before anything else - see connect_generation's doc
    // comment. Every cleanup path below is gated on this still being the
    // current generation when the task finishes, so a slow-to-unwind old
    // task (e.g. one still waiting on its own graceful QUIT to complete)
    // can never blow away a newer, already-successful connection's state
    // just because they happen to share an account_id.
    let generation = state.runtime.next_generation(&account_id);
    state.runtime.set_conn_state(&state, &account_id, ConnState::Connecting, None);
    let delay = state.runtime.throttle_connect_attempt(&account_id, MIN_RECONNECT_INTERVAL);
    let join_handle = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            if !delay.is_zero() {
                state.runtime.report_progress(&state, &account_id, &format!("Waiting {}s before connecting (reconnected too recently)...", delay.as_secs().max(1)));
                tokio::time::sleep(delay).await;
            }
            let result = std::panic::AssertUnwindSafe(run(&state, &config))
                .catch_unwind()
                .await;
            match result {
                Ok(Ok(())) => state.runtime.finish_connection(&state, &account_id, generation, ConnState::Disconnected, None),
                Ok(Err(e)) => {
                    tracing::warn!("irc[{account_id}]: {e}");
                    state.runtime.finish_connection(&state, &account_id, generation, ConnState::Disconnected, Some(&e.to_string()));
                }
                Err(_) => {
                    tracing::error!("irc[{account_id}]: connection task panicked");
                    state.runtime.finish_connection(&state, &account_id, generation, ConnState::Disconnected, Some("internal error (see nobilis logs)"));
                }
            }
        }
    });
    // Registered immediately, before the connection attempt even starts -
    // this is what lets setAccountConnected(false) cancel a connection
    // that's still stuck in DNS/TCP/TLS/registration, which has no Sender
    // yet for a graceful QUIT (see Runtime::disconnect()).
    state.runtime.insert_task_handle(&account_id, join_handle.abort_handle());
}

pub fn send_autojoin(sender: &Sender, autojoin_csv: &str) {
    if !autojoin_csv.trim().is_empty() {
        if let Err(e) = sender.send_join(autojoin_csv) {
            tracing::warn!("autojoin failed: {e}");
        }
    }
}

// Confirmed live: a plain `tokio::time::timeout(CONNECT_TIMEOUT,
// Client::from_config(..))` is not a reliable enough bound in practice -
// reproduced an account sitting at "Resolving and connecting..." for
// several minutes (no error, no state change) despite that inner 20s
// timeout, twice, across two different networks, right after a
// disconnect/reconnect cycle. The exact mechanism wasn't pinned down (the
// irc crate's DNS resolution goes through tokio's own spawn_blocking,
// which should be cancellation-safe), but Runtime::disconnect's abort()
// reliably broke out of it both times. Rather than trust the inner,
// per-step timeouts to always fire, this wraps the *entire* connect+
// register phase in one independent outer timeout, sized generously above
// the sum of every inner one, so establish() is guaranteed to resolve one
// way or another within this bound regardless of what's misbehaving
// underneath - a real fix for "how do we get unstuck", even without a
// fully confirmed root cause for "why".
const OVERALL_ESTABLISH_TIMEOUT: Duration = Duration::from_secs(75);

async fn run(state: &AppState, config: &IrcAccountConfig) -> Result<()> {
    let account_id = config.account_id();

    // establish() is deliberately run as its own *separate* tokio task
    // (tokio::spawn), not just an inline `.await`ed async fn call, and the
    // timeout below wraps *that task's JoinHandle*, not establish() itself.
    // This isn't a cosmetic difference: the previous version (`tokio::time
    // ::timeout(TIMEOUT, establish(...)).await` in the *same* task) still
    // hung indefinitely in practice, past even a generous 75s outer bound.
    // The reason: if something inside establish()'s call chain genuinely
    // blocks a thread without ever yielding (returning Poll::Pending) -
    // some native-tls/OpenSSL handshake paths do exactly this - then the
    // *timer check* for a timeout wrapping it in the same task can also
    // never run, because polling the timeout future polls the blocked
    // future first, in the same call stack, on the same thread. A
    // `tokio::time::timeout` and the future it wraps only race properly
    // when the wrapped future actually cooperates by yielding sometimes.
    // Spawning establish() as an independent task and awaiting its
    // JoinHandle instead means the *waiting* side runs in a genuinely
    // separate poll context (and, under tokio's multi-threaded scheduler,
    // often a separate OS thread) - so this outer timeout can fire on
    // schedule regardless of whether the inner task is truly wedged.
    let establish_task = tokio::spawn({
        let state = state.clone();
        let config = config.clone();
        async move { establish(&state, &config).await }
    });
    let establish_abort = establish_task.abort_handle();

    let (sender, mut stream, nickserv_wait) = match tokio::time::timeout(OVERALL_ESTABLISH_TIMEOUT, establish_task).await {
        Err(_) => {
            // Best-effort: if establish() actually is cooperating (just
            // slow, not truly wedged), this frees it immediately instead
            // of leaving it to run out the clock on its own. If it really
            // is stuck on a non-yielding blocking call, this can't do
            // anything about that call itself, but it does stop this
            // task from waiting on it any further - the account is
            // reported disconnected right now either way, not eventually.
            establish_abort.abort();
            bail!(
                "connection setup did not complete within {}s (safety-net timeout - a step almost certainly blocked without ever yielding, which no async timeout inside it could have caught)",
                OVERALL_ESTABLISH_TIMEOUT.as_secs()
            );
        }
        Ok(Err(join_err)) => bail!("connection setup task failed: {join_err}"),
        Ok(Ok(inner)) => inner?,
    };

    // channel -> nick -> rank, populated from RPL_NAMREPLY and kept
    // approximately in sync via JOIN/PART/QUIT/NICK (mode-driven rank
    // changes after the initial NAMREPLY aren't tracked live in this
    // milestone - a rejoin/NAMES refresh corrects it; see project plan's
    // note on NickList polish being pre-existing follow-up work).
    let mut channels: HashMap<String, HashMap<String, MemberRank>> = HashMap::new();

    while let Some(msg) = stream.next().await.transpose()? {
        handle_message(state, &account_id, &config.nick, &sender, msg, &nickserv_wait, &mut channels).await;
    }

    Ok(())
}

/// Standard system Tor daemon port - the sensible zero-config default when
/// `use_tor` is on but no explicit proxy address was given (e.g. Tor
/// Browser's 9150 instead). Split from `tor_proxy` by `:` if present.
fn proxy_host(config: &IrcAccountConfig) -> &str {
    config.tor_proxy.as_deref().and_then(|p| p.split(':').next()).filter(|s| !s.is_empty()).unwrap_or("127.0.0.1")
}

fn proxy_port(config: &IrcAccountConfig) -> u16 {
    config
        .tor_proxy
        .as_deref()
        .and_then(|p| p.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .unwrap_or(9050)
}

/// Everything from DNS resolution through NickServ IDENTIFY being sent -
/// i.e. the whole "connecting" phase, up to (and including, for the
/// non-SASL+NickServ case) the point where ConnState flips to Connected.
/// Split out from run() specifically so it can be wrapped in one single
/// outer timeout independent of its own inner per-step ones - see
/// OVERALL_ESTABLISH_TIMEOUT's doc comment above.
async fn establish(state: &AppState, config: &IrcAccountConfig) -> Result<(Sender, ClientStream, Option<NickservWait>)> {
    let account_id = config.account_id();
    let port = config.port.unwrap_or(if config.ssl { 6697 } else { 6667 });

    let irc_config = Config {
        nickname: Some(config.nick.clone()),
        // A prior ungraceful disconnect (crash, killed process, dropped
        // network) can leave our own old session as a "ghost" still
        // holding the real nick server-side for a while - confirmed live
        // against Libera, where a session can persist well past any
        // ping-timeout that ought to have reclaimed it. Without an alt
        // nick, that alone makes registration fail outright with "none of
        // the specified nicknames were usable" and the account never
        // recovers on its own. Registering under this fallback instead
        // lets the connection succeed immediately; the GHOST-and-reclaim
        // step right after wait_for_welcome() below then takes the real
        // nick back if we have NickServ credentials to do it with.
        alt_nicks: vec![format!("{}_", config.nick)],
        server: Some(config.host.clone()),
        port: Some(port),
        use_tls: Some(config.ssl),
        username: Some(config.username.clone().unwrap_or_else(|| config.nick.clone())),
        realname: Some(config.realname.clone().unwrap_or_else(|| config.nick.clone())),
        // SASL uses AUTHENTICATE, not the server PASS field.
        password: if config.sasl { None } else { config.password.clone() },
        proxy_type: config.use_tor.then_some(ProxyType::Socks5),
        proxy_server: config.use_tor.then(|| proxy_host(config).to_string()),
        proxy_port: config.use_tor.then(|| proxy_port(config)),
        ..Default::default()
    };

    state.runtime.report_progress(state, &account_id, &format!("Resolving and connecting to {}:{port}...", config.host));

    // from_config() does DNS + TCP + TLS handshake with no timeout of its
    // own - an unreachable address (e.g. a AAAA record with no real IPv6
    // route) can otherwise hang here indefinitely with nothing to catch it,
    // leaving the account stuck at "connecting" forever. (This inner bound
    // turned out not to be sufficient on its own in practice - see
    // OVERALL_ESTABLISH_TIMEOUT above - but it's kept as-is since it still
    // gives a more specific error message on the common/expected failure
    // path, e.g. an actually-refused connection.)
    let mut client = tokio::time::timeout(CONNECT_TIMEOUT, Client::from_config(irc_config))
        .await
        .map_err(|_| anyhow!("timed out connecting to {}:{port}", config.host))??;
    let mut stream = client.stream()?;
    let sender = client.sender();

    if config.sasl {
        register_with_sasl(state, &account_id, &sender, &mut stream, config).await?;
    } else {
        // No CAP negotiation needed - straight NICK/USER (+ PASS if a
        // plain server password is set), same tail as the crate's own
        // identify().
        state.runtime.report_progress(state, &account_id, "Registering (NICK/USER)...");
        client.identify()?;
    }

    state.runtime.report_progress(state, &account_id, "Waiting for server welcome...");
    wait_for_welcome(state, &account_id, &mut stream).await?;

    // Landed on the alt_nicks fallback above means our real nick's old
    // session is still alive server-side. With NickServ credentials we can
    // actually do something about it instead of just running under the
    // fallback forever: GHOST-kill that stale session and reclaim the real
    // nick. Sent back-to-back with no artificial delay between them -
    // relying on a single IRC connection's commands being processed by the
    // server in the order received, same as real clients (irssi, HexChat)
    // do this - so by the time NICK is processed, GHOST's kill already
    // has been.
    if client.current_nickname() != config.nick {
        if let Some(pw) = &config.nickserv_password {
            state.runtime.report_progress(state, &account_id, "Reclaiming nickname from a stale session...");
            sender.send_privmsg("NickServ", format!("GHOST {} {pw}", config.nick))?;
            sender.send(Command::NICK(config.nick.clone()))?;
        }
    }

    state.runtime.insert_irc_handle(&account_id, IrcHandle { sender: sender.clone(), nick: config.nick.clone() });
    state.runtime.set_conn_state(state, &account_id, ConnState::Connected, None);
    state.runtime.ensure_buffer(state, &account_id, &config.host, "server");

    let nickserv_wait = if !config.sasl {
        if let Some(pw) = &config.nickserv_password {
            // No report_progress here (unlike every earlier stage) -
            // ConnState is already Connected by this point (see just
            // above), and report_progress unconditionally stamps its
            // event with state:"connecting". Emitting one here would
            // incorrectly flip an already-connected account's displayed
            // state back to "connecting" with nothing to ever flip it
            // back afterward (NickServ identify has no success event of
            // its own to hang a correction on).
            sender.send_privmsg("NickServ", format!("IDENTIFY {pw}"))?;
            Some(NickservWait::arm(sender.clone(), config.autojoin.clone()))
        } else {
            send_autojoin(&sender, &config.autojoin);
            None
        }
    } else {
        send_autojoin(&sender, &config.autojoin);
        None
    };

    Ok((sender, stream, nickserv_wait))
}

async fn register_with_sasl(state: &AppState, account_id: &str, sender: &Sender, stream: &mut ClientStream, config: &IrcAccountConfig) -> Result<()> {
    state.runtime.report_progress(state, account_id, "Requesting SASL capability...");
    sender.send_cap_req(&[Capability::Sasl])?;
    wait_for(stream, |m| matches!(&m.command, Command::CAP(_, CapSubCommand::ACK, _, _))).await
        .map_err(|_| anyhow!("server did not acknowledge SASL capability"))?;

    state.runtime.report_progress(state, account_id, "Starting SASL PLAIN...");
    sender.send_sasl_plain()?;
    wait_for(stream, |m| matches!(&m.command, Command::AUTHENTICATE(s) if s == "+")).await
        .map_err(|_| anyhow!("server did not respond to AUTHENTICATE PLAIN"))?;

    state.runtime.report_progress(state, account_id, "Sending SASL credentials...");
    let user = config.sasl_user.clone().unwrap_or_else(|| config.nick.clone());
    let pass = config.password.clone().unwrap_or_default();
    let payload = base64::engine::general_purpose::STANDARD.encode(format!("\0{user}\0{pass}"));
    sender.send_sasl(payload)?;

    let result = wait_for(stream, |m| {
        matches!(
            &m.command,
            Command::Response(Response::RPL_SASLSUCCESS, _)
                | Command::Response(Response::ERR_SASLFAIL, _)
                | Command::Response(Response::ERR_SASLTOOLONG, _)
                | Command::Response(Response::ERR_SASLABORT, _)
        )
    })
    .await
    .map_err(|_| anyhow!("timed out waiting for SASL result"))?;

    if !matches!(result.command, Command::Response(Response::RPL_SASLSUCCESS, _)) {
        bail!("SASL authentication failed");
    }

    sender.send(Command::CAP(None, CapSubCommand::END, None, None))?;
    sender.send(Command::NICK(config.nick.clone()))?;
    sender.send(Command::USER(
        config.username.clone().unwrap_or_else(|| config.nick.clone()),
        "0".to_string(),
        config.realname.clone().unwrap_or_else(|| config.nick.clone()),
    ))?;
    Ok(())
}

async fn wait_for_welcome(state: &AppState, account_id: &str, stream: &mut ClientStream) -> Result<()> {
    let result = tokio::time::timeout(REGISTRATION_TIMEOUT, async {
        loop {
            match stream.next().await {
                Some(Ok(m)) => match &m.command {
                    // 001 itself (and anything else seen while scanning for
                    // it, e.g. 002-004) would otherwise just be discarded
                    // by this loop's `_ => continue` - record banner
                    // numerics on the way past instead of dropping them,
                    // same handling as the main loop's is_connection_banner
                    // arm gives everything that arrives *after* this
                    // point.
                    Command::Response(code, args) if is_connection_banner(*code) => {
                        if let Some(text) = banner_text(*code, args) {
                            let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                            state.runtime.record_message(state, account_id, host, "server", "*", &text, false, "system", None, None, false, None, Vec::new(), None);
                        }
                        if matches!(code, Response::RPL_WELCOME) {
                            return Ok(());
                        }
                    }
                    Command::Response(Response::ERR_NICKNAMEINUSE, args) => {
                        return Err(anyhow!("nickname already in use{}", args.first().map(|n| format!(" ({n})")).unwrap_or_default()))
                    }
                    Command::Response(Response::ERR_NICKCOLLISION, _) => return Err(anyhow!("nickname collision")),
                    Command::Response(Response::ERR_ERRONEOUSNICKNAME, _) => return Err(anyhow!("erroneous nickname")),
                    Command::Response(Response::ERR_PASSWDMISMATCH, _) => return Err(anyhow!("password mismatch")),
                    _ => continue,
                },
                Some(Err(e)) => return Err(anyhow!(e)),
                None => return Err(anyhow!("connection closed during registration")),
            }
        }
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow!("timed out waiting for server welcome (001) - check credentials/host")),
    }
}

async fn wait_for(stream: &mut ClientStream, pred: impl Fn(&Message) -> bool) -> Result<Message> {
    tokio::time::timeout(REGISTRATION_TIMEOUT, async {
        loop {
            match stream.next().await {
                Some(Ok(m)) if pred(&m) => return Ok(m),
                Some(Ok(_)) => continue,
                Some(Err(e)) => return Err(anyhow!(e)),
                None => return Err(anyhow!("connection closed during registration")),
            }
        }
    })
    .await
    .map_err(|_| anyhow!("timed out"))?
}

fn is_channel(target: &str) -> bool {
    target.starts_with(['#', '&', '+', '!'])
}

fn strip_action(body: &str) -> Option<&str> {
    body.strip_prefix('\u{1}')
        .and_then(|s| s.strip_prefix("ACTION "))
        .and_then(|s| s.strip_suffix('\u{1}'))
}

async fn handle_message(
    state: &AppState,
    account_id: &str,
    own_nick: &str,
    sender: &Sender,
    msg: Message,
    nickserv_wait: &Option<NickservWait>,
    channels: &mut HashMap<String, HashMap<String, MemberRank>>,
) {
    let from = msg.source_nickname().unwrap_or("").to_string();

    match msg.command {
        Command::PRIVMSG(target, body) => {
            let (buffer_name, kind) = if is_channel(&target) {
                (target.clone(), "channel")
            } else {
                (from.clone(), "dm")
            };
            if let Some(action_body) = strip_action(&body) {
                state.runtime.record_message(state, account_id, &buffer_name, kind, &from, action_body, true, "chat", None, None, false, None, Vec::new(), None);
            } else {
                state.runtime.record_message(state, account_id, &buffer_name, kind, &from, &body, false, "chat", None, None, false, None, Vec::new(), None);
            }
        }

        // A NOTICE addressed directly to us (not a channel) is, per
        // RFC1459's own intent for the command, a service/status
        // announcement rather than a real conversation - this is exactly
        // how NickServ sends registration reminders, identify
        // confirmations, and "invalid password" errors. HexChat (and most
        // other clients) surface these in the server tab rather than
        // opening a query window with "NickServ" - matching that here.
        // A NOTICE actually sent *to a channel* (e.g. from network staff)
        // still belongs in that channel.
        Command::NOTICE(target, body) => {
            if let Some(wait) = nickserv_wait {
                // Still resolves the identify-wait/autojoin-timer side
                // effect - just no longer suppresses showing the message
                // (previously `return`ed here, hiding NickServ's own
                // "you are now identified" confirmation entirely).
                wait.notice(&from, &body);
            }
            if is_channel(&target) {
                state.runtime.record_message(state, account_id, &target, "channel", &from, &body, false, "chat", None, None, false, None, Vec::new(), None);
            } else {
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                state.runtime.record_message(state, account_id, host, "server", &from, &body, false, "system", None, None, false, None, Vec::new(), None);
            }
        }

        Command::JOIN(channel, _, _) => {
            let members = channels.entry(channel.clone()).or_default();
            members.insert(from.clone(), MemberRank::None);
            if from == own_nick {
                state.runtime.ensure_buffer(state, account_id, &channel, "channel");
            }
            emit_presence(state, account_id, &channel, members);
        }

        Command::PART(channel, _) => {
            if from == own_nick {
                let buffer_id = crate::model::buffer_id(account_id, &channel);
                state.runtime.remove_buffer(state, &buffer_id);
                channels.remove(&channel);
            } else if let Some(members) = channels.get_mut(&channel) {
                members.remove(&from);
                emit_presence(state, account_id, &channel, members);
            }
        }

        Command::QUIT(_) => {
            for (channel, members) in channels.iter_mut() {
                if members.remove(&from).is_some() {
                    emit_presence(state, account_id, channel, members);
                }
            }
        }

        Command::NICK(new_nick) => {
            for (channel, members) in channels.iter_mut() {
                if let Some(rank) = members.remove(&from) {
                    members.insert(new_nick.clone(), rank);
                    emit_presence(state, account_id, channel, members);
                }
            }
        }

        Command::Response(Response::RPL_NAMREPLY, args) => {
            // args: [nick, symbol, channel, "name1 @name2 +name3 ..."]
            if let (Some(channel), Some(names)) = (args.get(2), args.get(3)) {
                let members = channels.entry(channel.clone()).or_default();
                for raw in names.split_whitespace() {
                    let (rank, nick) = parse_prefixed_nick(raw);
                    members.insert(nick.to_string(), rank);
                }
                emit_presence(state, account_id, channel, members);
            }
        }

        // Sent once on join if the channel has a topic set (args: [nick,
        // channel, topic]) - previously silently dropped, meaning topics
        // never showed up at all under this backend.
        Command::Response(Response::RPL_TOPIC, args) => {
            if let (Some(channel), Some(topic)) = (args.get(1), args.get(2)) {
                let body = format!("Topic for {channel}: {topic}");
                state.runtime.record_message(state, account_id, channel, "channel", "*", &body, false, "topic", None, None, false, None, Vec::new(), None);
            }
        }

        // A live topic change while we're in the channel.
        Command::TOPIC(channel, Some(topic)) => {
            let body = format!("{from} changed the topic to: {topic}");
            state.runtime.record_message(state, account_id, &channel, "channel", "*", &body, false, "topic", None, None, false, None, Vec::new(), None);
        }

        // Connection banner (001-005), LUSERS (251-255, 265-266), and MOTD
        // (372/375/376, 422) - previously silently dropped entirely, same
        // gap as topics before the earlier fix. This is exactly the
        // "server-wide, not addressed to any channel or person" content
        // the unclosable server buffer exists for (see model.c's
        // nobilis_buffer_kind) - libpurple's own irc_msg_default() fallback
        // numeric handler wrote to this same buffer for the same reason.
        Command::Response(code, args) if is_connection_banner(code) => {
            if let Some(text) = banner_text(code, &args) {
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                state.runtime.record_message(state, account_id, host, "server", "*", &text, false, "system", None, None, false, None, Vec::new(), None);
            }
        }

        // Channel-operation failures (can't join, banned, full, wrong key,
        // invite-only, etc.) - previously silently dropped, meaning a
        // failed /join gave no feedback at all. These happen precisely
        // when there's no channel buffer to show them in (the join never
        // succeeded), so the always-present server buffer is the only
        // reliable place - same as HexChat's server tab.
        Command::Response(code, args) if is_channel_error(code) => {
            if let Some(text) = channel_error_text(&args) {
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                state.runtime.record_message(state, account_id, host, "server", "*", &text, false, "system", None, None, false, None, Vec::new(), None);
            }
        }

        _ => {}
    }

    let _ = sender; // reserved for future PING/PONG or raw passthrough handling
}

fn is_connection_banner(code: Response) -> bool {
    matches!(
        code,
        Response::RPL_WELCOME
            | Response::RPL_YOURHOST
            | Response::RPL_CREATED
            | Response::RPL_MYINFO
            | Response::RPL_ISUPPORT
            | Response::RPL_LUSERCLIENT
            | Response::RPL_LUSEROP
            | Response::RPL_LUSERUNKNOWN
            | Response::RPL_LUSERCHANNELS
            | Response::RPL_LUSERME
            | Response::RPL_LOCALUSERS
            | Response::RPL_GLOBALUSERS
            | Response::RPL_MOTDSTART
            | Response::RPL_MOTD
            | Response::RPL_ENDOFMOTD
            | Response::ERR_NOMOTD
    )
}

/// Extracts the human-readable text from a connection-banner numeric.
/// Most of these have their real content as the single trailing
/// (":"-prefixed) parameter, so the last arg is exactly right - but
/// RPL_ISUPPORT (005) is the odd one out: `<nick> TOKEN1 TOKEN2 ... :are
/// supported by this server`, where the *trailing* text is just a fixed
/// caption and the actually useful content is every token before it.
/// Taking only args.last() for 005 would show "are supported by this
/// server" three times over and silently drop every real ISUPPORT token.
fn banner_text(code: Response, args: &[String]) -> Option<String> {
    if code == Response::RPL_ISUPPORT {
        if args.len() > 2 {
            return Some(args[1..args.len() - 1].join(" "));
        }
        return None;
    }
    args.last().cloned()
}

fn is_channel_error(code: Response) -> bool {
    matches!(
        code,
        Response::ERR_NOSUCHNICK
            | Response::ERR_NOSUCHCHANNEL
            | Response::ERR_CANNOTSENDTOCHAN
            | Response::ERR_TOOMANYCHANNELS
            | Response::ERR_UNAVAILRESOURCE
            | Response::ERR_NOTONCHANNEL
            | Response::ERR_CHANNELISFULL
            | Response::ERR_INVITEONLYCHAN
            | Response::ERR_BANNEDFROMCHAN
            | Response::ERR_BADCHANNELKEY
    )
}

/// `<nick> <channel-or-nick> :<reason>` for all of these - combine into
/// one readable line, e.g. "#foo: Cannot join channel (+i)".
fn channel_error_text(args: &[String]) -> Option<String> {
    let target = args.get(1)?;
    let reason = args.last()?;
    Some(format!("{target}: {reason}"))
}

fn parse_prefixed_nick(raw: &str) -> (MemberRank, &str) {
    match raw.as_bytes().first() {
        Some(b'~') => (MemberRank::Founder, &raw[1..]),
        Some(b'@') => (MemberRank::Op, &raw[1..]),
        Some(b'%') => (MemberRank::HalfOp, &raw[1..]),
        Some(b'+') => (MemberRank::Voice, &raw[1..]),
        _ => (MemberRank::None, raw),
    }
}

fn emit_presence(state: &AppState, account_id: &str, channel: &str, members: &HashMap<String, MemberRank>) {
    let buffer_id = crate::model::buffer_id(account_id, channel);
    let member_list: Vec<_> = members
        .iter()
        .map(|(nick, rank)| json!({ "nick": nick, "prefix": rank.prefix(), "away": false }))
        .collect();
    let member_list = json!(member_list);
    // Persisted so a client subscribing after this point (reopening the
    // buffer, or a fresh UI session) can get the current roster immediately
    // via subscribe's replay instead of waiting for the next incremental
    // change - see Runtime::get_presence and rpc/methods.rs's subscribe.
    state.runtime.set_presence(&buffer_id, member_list.clone());
    state.events.emit("presenceChange", json!({ "bufferId": buffer_id, "members": member_list }));
}

/// Sends `body` to `target_buffer` (an already-open buffer's channel/nick
/// name) - slash command (/join /part /me /nick /topic /msg) if it starts
/// with "/" ("//" escapes a literal leading slash), otherwise a plain
/// PRIVMSG. Mirrors daemon/nobilis/actions.c's nobilis_send_message, minus
/// libpurple's purple_cmd_do_command registry (hand-rolled here instead -
/// see project plan). Returns Err with a user-facing message on failure.
pub fn send_message(state: &AppState, account_id: &str, sender: &Sender, target_buffer: &str, body: &str) -> Result<()> {
    if let Some(rest) = body.strip_prefix('/') {
        if let Some(literal) = rest.strip_prefix('/') {
            return send_plain(state, account_id, sender, target_buffer, &format!("/{literal}"));
        }
        let mut parts = rest.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("").to_lowercase();
        let arg = parts.next().unwrap_or("").trim();
        return match cmd.as_str() {
            "join" => sender.send_join(arg).map_err(|e| anyhow!(e)),
            "part" => sender.send_part(target_buffer).map_err(|e| anyhow!(e)),
            "me" => {
                sender.send_action(target_buffer, arg)?;
                let own_nick = state.runtime.irc_current_nick(account_id).unwrap_or_default();
                state.runtime.record_message(state, account_id, target_buffer, buffer_kind_hint(target_buffer), &own_nick, arg, true, "chat", None, None, false, None, Vec::new(), None);
                Ok(())
            }
            "nick" => sender.send(Command::NICK(arg.to_string())).map_err(|e| anyhow!(e)),
            "topic" => sender.send_topic(target_buffer, arg).map_err(|e| anyhow!(e)),
            // Moderator actions - the frontend only offers these in the
            // userlist context menu when the presence data it already has
            // shows the local user holding founder/op/halfop in this
            // channel (see its member list), so there's no separate
            // permission check needed here: the server itself is the
            // actual authority and will simply reject these if we don't
            // really have the rank, same as any other IRC client.
            "kick" => {
                let mut kick_parts = arg.splitn(2, ' ');
                let nick = kick_parts.next().unwrap_or("");
                let reason = kick_parts.next();
                if nick.is_empty() {
                    bail!("/kick requires a nick");
                }
                sender.send(Command::KICK(target_buffer.to_string(), nick.to_string(), reason.map(String::from))).map_err(|e| anyhow!(e))
            }
            "ban" => {
                if arg.is_empty() {
                    bail!("/ban requires a nick");
                }
                let mask = format!("{arg}!*@*");
                sender.send(Command::ChannelMODE(target_buffer.to_string(), vec![Mode::Plus(ChannelMode::Ban, Some(mask))])).map_err(|e| anyhow!(e))
            }
            "op" => set_channel_mode(sender, target_buffer, ChannelMode::Oper, true, arg),
            "deop" => set_channel_mode(sender, target_buffer, ChannelMode::Oper, false, arg),
            "voice" => set_channel_mode(sender, target_buffer, ChannelMode::Voice, true, arg),
            "devoice" => set_channel_mode(sender, target_buffer, ChannelMode::Voice, false, arg),
            "msg" => {
                let mut msg_parts = arg.splitn(2, ' ');
                let to = msg_parts.next().unwrap_or("");
                let text = msg_parts.next().unwrap_or("");
                if to.is_empty() {
                    bail!("/msg requires a target");
                }
                send_plain(state, account_id, sender, to, text)
            }
            other => bail!("unknown command \"/{other}\""),
        };
    }
    send_plain(state, account_id, sender, target_buffer, body)
}

fn set_channel_mode(sender: &Sender, channel: &str, mode: ChannelMode, add: bool, nick: &str) -> Result<()> {
    if nick.is_empty() {
        bail!("this command requires a nick");
    }
    let m = if add { Mode::Plus(mode, Some(nick.to_string())) } else { Mode::Minus(mode, Some(nick.to_string())) };
    sender.send(Command::ChannelMODE(channel.to_string(), vec![m])).map_err(|e| anyhow!(e))
}

fn send_plain(state: &AppState, account_id: &str, sender: &Sender, target: &str, body: &str) -> Result<()> {
    sender.send_privmsg(target, body)?;
    // No echo-message capability requested, so the server won't send this
    // back to us - record it locally, same as libpurple's write_im/
    // write_chat firing for locally-sent messages too.
    let own_nick = state.runtime.irc_current_nick(account_id).unwrap_or_default();
    state.runtime.record_message(state, account_id, target, buffer_kind_hint(target), &own_nick, body, false, "chat", None, None, false, None, Vec::new(), None);
    Ok(())
}

fn buffer_kind_hint(target: &str) -> &'static str {
    if is_channel(target) { "channel" } else { "dm" }
}
