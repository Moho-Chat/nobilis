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
// Same shape the other three backends already use: start small so a blip
// costs a couple of seconds, and give up ground quickly when a server is
// genuinely down rather than hammering it.
const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Spawns a background task that keeps this account connected: it runs the
/// event loop, and when that ends it waits and connects again.
///
/// It used to stop instead, which made a momentary drop permanent - the one
/// backend where that was true, since sockchat, Discord and Matrix have all
/// had retry loops for a while. Recovering meant noticing and reconnecting
/// by hand, and on a client left running all day the noticing is the part
/// that does not happen.
///
/// Two things keep the loop from being a nuisance. Attempts back off 3s to
/// 60s, on top of the existing per-account pacing, because a real network's
/// anti-flood will silently drop connections from a source that retries too
/// eagerly. And it stops when the account is switched off: disconnect()
/// clears the intent flag before sending its QUIT, so an ending that was
/// asked for is distinguishable from a link that died.
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
    state.runtime.set_wants_connected(&account_id, true);
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
            let mut backoff = RECONNECT_INITIAL_DELAY;
            loop {
                let result = std::panic::AssertUnwindSafe(run(&state, &config))
                    .catch_unwind()
                    .await;
                let detail = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => {
                        tracing::warn!("irc[{account_id}]: {e}");
                        Some(e.to_string())
                    }
                    Err(_) => {
                        tracing::error!("irc[{account_id}]: connection task panicked");
                        Some("internal error (see nobilis logs)".to_string())
                    }
                };

                // Switched off while that session was running, or switched
                // off *by* ending it - disconnect() sends QUIT, which is
                // what brought us back here.
                if !state.runtime.wants_connected(&account_id) {
                    state.runtime.finish_connection(&state, &account_id, generation, ConnState::Disconnected, detail.as_deref());
                    return;
                }

                // A newer spawn for this account has taken over; that one
                // owns the connection now and this loop must not race it.
                if !state.runtime.is_current_generation(&account_id, generation) {
                    return;
                }

                // The session is over, so the Sender that went with it is
                // dead. Drop it before the wait, or a send during the gap
                // goes into a socket nobody is reading.
                state.runtime.remove_irc_handle(&account_id);
                state.runtime.set_conn_state(&state, &account_id, ConnState::Connecting, None);
                let because = detail.unwrap_or_else(|| "Connection closed".to_string());
                state.runtime.report_progress(&state, &account_id, &format!("{because} - reconnecting in {}s...", backoff.as_secs()));
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RECONNECT_MAX_DELAY);

                let pace = state.runtime.throttle_connect_attempt(&account_id, MIN_RECONNECT_INTERVAL);
                if !pace.is_zero() {
                    tokio::time::sleep(pace).await;
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

    // Whether the people we hold conversations with are actually connected.
    //
    // IRC has no presence: a message to someone who is offline is accepted by
    // the server and silently discarded, so a client that says nothing leaves
    // you talking to a wall. ISON is the universal way to ask - MONITOR is
    // better where it exists, but not every server has it and this needs no
    // capability negotiation to work anywhere.
    let ison = tokio::spawn(poll_query_presence(state.clone(), account_id.clone(), sender.clone()));

    // Published for as long as this connection is up, so a file offered over
    // it is fetched back the same way. Registered rather than rebuilt when a
    // transfer starts: turning Tor off in settings does not move a connection
    // that is already established, and a transfer must follow the connection
    // rather than the setting.
    state.runtime.set_irc_transport(&account_id, Some(super::irc_dcc::transport_for(config)));

    let result = async {
        while let Some(msg) = stream.next().await.transpose()? {
            handle_message(state, &account_id, &config.nick, &sender, msg, &nickserv_wait, &mut channels).await;
        }
        Ok::<(), anyhow::Error>(())
    }
    .await;
    ison.abort();
    // The route goes with the connection, and so does anything running over
    // it: a transfer that outlived its connection would be a socket nobody
    // is watching, and an unanswered offer would be a prompt for a file that
    // can no longer arrive. `result?` below returns on error, so this has to
    // come first.
    state.runtime.set_irc_transport(&account_id, None);
    state.runtime.cancel_dcc_for_account(&account_id);
    result?;

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

    // Whether the account is authenticated by the time registration finishes.
    // Ticking the SASL box is a request, not a guarantee: a server with no
    // SASL registers us normally, and then NickServ is still the way in.
    let mut authenticated = false;
    if config.sasl {
        authenticated = register_with_sasl(state, &account_id, &sender, &mut stream, config).await?;
    } else {
        // Same tail as the crate's own identify(), by way of the shared
        // helper: there is no SASL to negotiate here, but the capabilities
        // above are still worth asking for, and they have to be requested
        // before CAP END like any other.
        state.runtime.report_progress(state, &account_id, "Registering (NICK/USER)...");
        end_cap_and_register(&sender, config)?;
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

    // Keyed on whether SASL actually authenticated rather than on whether it
    // was asked for. A server that turned out not to offer it leaves the
    // account unidentified, and skipping NickServ there would silently drop
    // the one credential that still works.
    let nickserv_wait = if !authenticated {
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

/// Registers with SASL, saying whether the account actually authenticated.
///
/// `false` means the server does not offer SASL and registration finished the
/// ordinary way instead. It is not a failure: plenty of small networks have
/// never implemented it, and refusing to connect to one because a checkbox was
/// ticked would be worse than connecting the way every other client does. The
/// caller uses the answer to decide whether NickServ still has a job to do.
///
/// A server that *does* offer SASL and then rejects the credentials is a real
/// error and stays one. That is a wrong password, and quietly carrying on
/// unauthenticated is how somebody ends up sitting in a channel under an
/// unregistered nick believing they are identified.
async fn register_with_sasl(state: &AppState, account_id: &str, sender: &Sender, stream: &mut ClientStream, config: &IrcAccountConfig) -> Result<bool> {
    // SASL PLAIN is the password with base64 wrapped round it - an encoding,
    // not a cipher. On a cleartext link it is the password in the clear to
    // anything on the path, which is worse than NickServ only in that the
    // person doing it believes "SASL" means it is protected.
    if !sasl_transport_ok(config.ssl, config.allow_plaintext_sasl) {
        bail!(
            "refusing to send SASL credentials over an unencrypted connection to {} - \
             turn on TLS, or allow plaintext SASL for this account if the network really has no TLS port",
            config.host
        );
    }

    state.runtime.report_progress(state, account_id, "Requesting SASL capability...");
    sender.send_cap_req(&[Capability::Sasl])?;
    // NAK and the timeout mean the same thing here - this server has no SASL -
    // and both are answered by registering normally. Watching for NAK as well
    // as ACK is what turns the common case from a 20-second stall into an
    // immediate answer.
    let offered = wait_for(stream, |m| {
        matches!(
            &m.command,
            Command::CAP(_, CapSubCommand::ACK, _, _) | Command::CAP(_, CapSubCommand::NAK, _, _)
        )
    })
    .await
    .ok()
    .is_some_and(|m| matches!(&m.command, Command::CAP(_, CapSubCommand::ACK, _, _)));

    if !offered {
        state.runtime.report_progress(state, account_id, "Server has no SASL; registering normally...");
        end_cap_and_register(sender, config)?;
        return Ok(false);
    }

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
        bail!("SASL authentication failed - check the SASL username and password for this account");
    }

    end_cap_and_register(sender, config)?;
    Ok(true)
}

/// Capabilities asked for on every connection, whatever else is going on.
///
/// `server-time` is the one that matters: without it a message is stamped
/// with the local clock at the moment it is read, so anything the server
/// replays - and anything that arrives in a burst after a reconnect - dates
/// itself to now rather than to when it was said.
///
/// `multi-prefix` makes RPL_NAMREPLY carry every rank a person holds ("@+nick")
/// instead of only the highest, which is what lets a rank being taken away
/// leave the one underneath it intact.
const WANTED_CAPS: &[&str] = &["server-time", "multi-prefix"];

/// Closes capability negotiation and sends the ordinary NICK/USER pair.
///
/// Shared by the authenticated path and the fell-back-to-nothing path because
/// both owe the server a CAP END: having asked for capabilities, registration
/// does not proceed until we say we are finished asking, and a server left
/// waiting for that just sits there until the establish timeout fires.
fn end_cap_and_register(sender: &Sender, config: &IrcAccountConfig) -> Result<()> {
    // One REQ per capability, and no waiting on the replies.
    //
    // Separate lines because a CAP REQ is atomic: a server that does not
    // know one name in the list refuses the whole line, so bundling these
    // with sasl would mean an old server dropping SASL over multi-prefix.
    // And no waiting because there is nothing to decide - a granted
    // capability changes what arrives, which is visible in what arrives.
    // Sent here rather than earlier so they cannot be confused with the
    // ACK/NAK the SASL exchange above is watching for.
    for cap in WANTED_CAPS {
        sender.send(Command::CAP(None, CapSubCommand::REQ, None, Some((*cap).to_string())))?;
    }
    sender.send(Command::CAP(None, CapSubCommand::END, None, None))?;
    // A server password is not a SASL credential: it goes in PASS, before
    // NICK, and only where SASL is not the thing authenticating us. This
    // matches what the crate's own identify() sends, which is what the
    // non-SASL path used before it came through here.
    if !config.sasl {
        if let Some(pw) = config.password.as_deref().filter(|p| !p.is_empty()) {
            sender.send(Command::PASS(pw.to_string()))?;
        }
    }
    sender.send(Command::NICK(config.nick.clone()))?;
    sender.send(Command::USER(
        config.username.clone().unwrap_or_else(|| config.nick.clone()),
        "0".to_string(),
        config.realname.clone().unwrap_or_else(|| config.nick.clone()),
    ))?;
    Ok(())
}

/// Starts a conversation with somebody.
///
/// IRC has no concept of opening one: a query is a client-side idea, and the
/// server only learns of it when a message is actually sent. So this creates
/// the buffer and asks once whether they are there, which is what a person
/// wants to know before typing.
pub fn open_query(state: &AppState, account_id: &str, nick: &str) -> Result<String> {
    if nick.trim().is_empty() || is_channel(nick) {
        bail!("{nick:?} is not a nickname");
    }
    let buffer = state.runtime.ensure_buffer(state, account_id, nick, "dm");
    if let Some(sender) = state.runtime.irc_sender(account_id) {
        let _ = sender.send(Command::Raw("ISON".to_string(), vec![nick.to_string()]));
    }
    Ok(buffer.id)
}

/// How often to ask the server who among our conversation partners is on.
///
/// Slow enough to be invisible traffic on any network, quick enough that the
/// warning shown before sending is rarely stale. ERR_NOSUCHNICK covers the
/// gap: it is the server's own answer at the moment of sending.
const ISON_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// Asks, repeatedly, which of the people we have conversations with are online.
async fn poll_query_presence(state: AppState, account_id: String, sender: irc::client::Sender) {
    loop {
        tokio::time::sleep(ISON_INTERVAL).await;

        let nicks: Vec<String> = state
            .runtime
            .list_buffers()
            .into_iter()
            .filter(|b| b.account_id == account_id && b.kind == "dm")
            .map(|b| b.name)
            .collect();
        if nicks.is_empty() {
            continue;
        }
        // One request for everyone rather than one each: ISON takes a list,
        // and a server will answer a line of them in a single reply.
        for chunk in nicks.chunks(20) {
            if sender.send(Command::Raw("ISON".to_string(), chunk.to_vec())).is_err() {
                return;
            }
        }
    }
}

/// Records which conversation partners the server just said are online.
///
/// ISON answers with only the nicks that *are* on, so anyone asked about and
/// missing from the reply is offline - which is the answer this exists to get.
fn apply_ison(state: &AppState, account_id: &str, online: &str) {
    let online: Vec<String> = online.split_whitespace().map(|n| n.to_lowercase()).collect();
    for buffer in state.runtime.list_buffers() {
        if buffer.account_id != account_id || buffer.kind != "dm" {
            continue;
        }
        let here = online.contains(&buffer.name.to_lowercase());
        let members = json!([{
            "nick": buffer.name,
            "userId": buffer.name,
            "prefix": "",
            "away": !here,
            "status": if here { "online" } else { "offline" },
        }]);
        state.runtime.set_presence(&buffer.id, members.clone());
        state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": members }));
    }
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
                            state.runtime.record_message(state, account_id, host, "server", "*", &text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
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
    // Only for what somebody actually said. The rest of what arrives here
    // is this client's own narration - join lines, topic notices, error
    // text - which belongs at the moment it is shown rather than at
    // whatever the server stamped the triggering event with.
    let sent_at = server_time(&msg);

    match msg.command {
        Command::PRIVMSG(target, body) => {
            let (buffer_name, kind) = if is_channel(&target) {
                (target.clone(), "channel")
            } else {
                (from.clone(), "dm")
            };
            // Before it is treated as something somebody said. A file offer
            // is CTCP, and recording it as a message put a line of control
            // characters in the log where the offer should have been.
            if let Some(dcc) = super::irc_dcc::parse_dcc(&body) {
                super::irc_dcc::incoming(state, account_id, &from, &buffer_name, kind, dcc).await;
                return;
            }
            if let Some(action_body) = strip_action(&body) {
                state.runtime.record_message_at(state, account_id, &buffer_name, kind, &from, action_body, true, "chat", None, None, false, None, Vec::new(), Vec::new(), None, sent_at, None);
            } else {
                state.runtime.record_message_at(state, account_id, &buffer_name, kind, &from, &body, false, "chat", None, None, false, None, Vec::new(), Vec::new(), None, sent_at, None);
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
                state.runtime.record_message(state, account_id, &target, "channel", &from, &body, false, "chat", None, None, false, None, Vec::new(), Vec::new(), None);
            } else {
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                state.runtime.record_message(state, account_id, host, "server", &from, &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
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

        // A rank given or taken after the join.
        //
        // Without this the member list is only ever as accurate as the
        // moment RPL_NAMREPLY arrived: an op handed out afterwards never
        // showed, and one taken away never went, so the list quietly drifted
        // and stayed wrong until the channel was rejoined.
        //
        // Losing a rank sets the member back to none rather than to whatever
        // they held underneath it. IRC sends one prefix per user unless
        // multi-prefix is negotiated, so there is genuinely no way to know
        // from here that somebody who just lost +o still holds +v - the
        // capability that would say so is its own piece of work.
        Command::ChannelMODE(channel, modes) => {
            if let Some(members) = channels.get_mut(&channel) {
                let mut changed = false;
                for m in &modes {
                    let (mode, target, granting) = match m {
                        Mode::Plus(mode, Some(target)) => (mode, target, true),
                        Mode::Minus(mode, Some(target)) => (mode, target, false),
                        // Channel modes proper (+m, +t, bans) carry no nick
                        // to re-rank, and belong to the channel rather than
                        // to anybody in it.
                        _ => continue,
                    };
                    let Some(rank) = rank_from_mode(mode) else { continue };
                    let Some(slot) = members.get_mut(target.as_str()) else { continue };
                    *slot = if granting { rank } else { MemberRank::None };
                    changed = true;
                    let body = format!(
                        "{from} {} {} {} {target}",
                        if granting { "gives" } else { "takes" },
                        rank_word(rank),
                        if granting { "to" } else { "from" },
                    );
                    state.runtime.record_message(state, account_id, &channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                }
                if changed {
                    emit_presence(state, account_id, &channel, members);
                }
            }
        }

        // Being removed from a channel by somebody else.
        //
        // Previously silent: no message, and the buffer sat there looking
        // joined until the next attempt to speak into it failed.
        Command::KICK(channel, target, reason) => {
            let reason = reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
            if target == own_nick {
                // Said in the server buffer rather than the channel: the
                // channel buffer is about to be removed, so anything written
                // there goes to something nobody can open.
                let host = account_id.split_once('@').map(|(_, h)| h).unwrap_or(account_id);
                let body = match reason {
                    Some(r) => format!("Kicked from {channel} by {from} ({r})"),
                    None => format!("Kicked from {channel} by {from}"),
                };
                state.runtime.record_message(state, account_id, host, "server", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
                let buffer_id = crate::model::buffer_id(account_id, &channel);
                state.runtime.remove_buffer(state, &buffer_id);
                channels.remove(&channel);
            } else if let Some(members) = channels.get_mut(&channel) {
                members.remove(&target);
                let body = match reason {
                    Some(r) => format!("{target} was kicked by {from} ({r})"),
                    None => format!("{target} was kicked by {from}"),
                };
                state.runtime.record_message(state, account_id, &channel, "channel", "*", &body, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
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

        // args: [nick, "nick1 nick2 ..."] - only those who are on.
        Command::Response(Response::RPL_ISON, args) => {
            apply_ison(state, account_id, args.last().map(String::as_str).unwrap_or(""));
        }

        // The server's own answer that a message went nowhere. Authoritative
        // where the poll above is merely recent: this arrives because of
        // something just sent, so it also corrects the stored presence.
        Command::Response(Response::ERR_NOSUCHNICK, args) => {
            if let Some(nick) = args.get(1) {
                let existing = state
                    .runtime
                    .list_buffers()
                    .into_iter()
                    .find(|b| b.account_id == account_id && b.kind == "dm" && b.name.eq_ignore_ascii_case(nick));
                if let Some(buffer) = existing {
                    let members = json!([{
                        "nick": nick,
                        "userId": nick,
                        "prefix": "",
                        "away": true,
                        "status": "offline",
                    }]);
                    state.runtime.set_presence(&buffer.id, members.clone());
                    state.events.emit("presenceChange", json!({ "bufferId": buffer.id, "members": members }));
                    state.events.emit(
                        "deliveryFailed",
                        json!({
                            "bufferId": buffer.id,
                            "reason": "offline",
                            "text": format!("{nick} is not online. They will not receive this message."),
                        }),
                    );
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
                state.runtime.record_message(state, account_id, channel, "channel", "*", &body, false, "topic", None, None, false, None, Vec::new(), Vec::new(), None);
            }
        }

        // A live topic change while we're in the channel.
        Command::TOPIC(channel, Some(topic)) => {
            let body = format!("{from} changed the topic to: {topic}");
            state.runtime.record_message(state, account_id, &channel, "channel", "*", &body, false, "topic", None, None, false, None, Vec::new(), Vec::new(), None);
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
                state.runtime.record_message(state, account_id, host, "server", "*", &text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
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
                state.runtime.record_message(state, account_id, host, "server", "*", &text, false, "system", None, None, false, None, Vec::new(), Vec::new(), None);
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

/// The rank a channel mode letter grants, or None for a mode that is about
/// the channel rather than about a person in it.
///
/// Admin (+a) folds into Founder: it outranks op, and the member model has
/// no separate step for it.
fn rank_from_mode(mode: &ChannelMode) -> Option<MemberRank> {
    match mode {
        ChannelMode::Founder | ChannelMode::Admin => Some(MemberRank::Founder),
        ChannelMode::Oper => Some(MemberRank::Op),
        ChannelMode::Halfop => Some(MemberRank::HalfOp),
        ChannelMode::Voice => Some(MemberRank::Voice),
        _ => None,
    }
}

/// What to call a rank in a sentence.
fn rank_word(rank: MemberRank) -> &'static str {
    match rank {
        MemberRank::Founder => "founder status",
        MemberRank::Op => "operator status",
        MemberRank::HalfOp => "half-operator status",
        MemberRank::Voice => "voice",
        MemberRank::None => "nothing",
    }
}

/// When the server says this message was sent, if it says at all.
///
/// The server-time tag is RFC3339 in UTC to millisecond precision
/// ("2026-08-29T12:34:56.789Z"). Absent unless the capability was granted,
/// which is the ordinary case on an older server - hence an Option rather
/// than a default, so the caller can fall back to the clock rather than to
/// the epoch.
fn server_time(msg: &Message) -> Option<i64> {
    let tags = msg.tags.as_ref()?;
    // Tag is a tuple struct of (name, value); matched by field rather than
    // by pattern to save importing it for one line.
    let raw = tags.iter().find(|tag| tag.0 == "time")?.1.as_deref()?;
    Some(chrono::DateTime::parse_from_rfc3339(raw).ok()?.timestamp())
}

/// Splits "@+nick" into the rank it carries and the nick itself.
///
/// Every prefix is consumed, not just the first. With multi-prefix granted a
/// server lists all of them, highest first - so the first is the rank, and
/// leaving the rest attached would make "+nick" the person's name.
fn parse_prefixed_nick(raw: &str) -> (MemberRank, &str) {
    let mut rest = raw;
    let mut rank = MemberRank::None;
    while let Some(byte) = rest.as_bytes().first() {
        let this = match byte {
            b'~' => MemberRank::Founder,
            b'@' => MemberRank::Op,
            b'%' => MemberRank::HalfOp,
            b'+' => MemberRank::Voice,
            _ => break,
        };
        // The highest is listed first, so only the first one read counts.
        if rank == MemberRank::None {
            rank = this;
        }
        rest = &rest[1..];
    }
    (rank, rest)
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
                state.runtime.record_message(state, account_id, target_buffer, buffer_kind_hint(target_buffer), &own_nick, arg, true, "chat", None, None, false, None, Vec::new(), Vec::new(), None);
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
    state.runtime.record_message(state, account_id, target, buffer_kind_hint(target), &own_nick, body, false, "chat", None, None, false, None, Vec::new(), Vec::new(), None);
    Ok(())
}

fn buffer_kind_hint(target: &str) -> &'static str {
    if is_channel(target) { "channel" } else { "dm" }
}

/// Whether SASL credentials may be put on the wire for this connection.
///
/// TLS, or an explicit decision to do it anyway. Deliberately not satisfied by
/// `use_tor`: a Tor circuit protects the traffic as far as the exit node, and
/// the exit node is precisely where it turns back into cleartext IRC bound for
/// a plaintext port. Treating that as encrypted would hand the password to a
/// stranger's relay rather than to a stranger's ISP, which is not an
/// improvement worth making silently.
fn sasl_transport_ok(ssl: bool, allow_plaintext: bool) -> bool {
    ssl || allow_plaintext
}

#[cfg(test)]
mod tests {
    use super::*;

    /// multi-prefix lists every rank a person holds, highest first. Reading
    /// only the first byte left the rest stuck to the front of the nick.
    #[test]
    fn every_prefix_is_stripped_and_the_highest_wins() {
        assert_eq!(parse_prefixed_nick("@+bob"), (MemberRank::Op, "bob"));
        assert_eq!(parse_prefixed_nick("~@%+bob"), (MemberRank::Founder, "bob"));
        assert_eq!(parse_prefixed_nick("+bob"), (MemberRank::Voice, "bob"));
        assert_eq!(parse_prefixed_nick("bob"), (MemberRank::None, "bob"));
        // A nick is never only prefixes, but it must not panic if it is.
        assert_eq!(parse_prefixed_nick("@"), (MemberRank::Op, ""));
        assert_eq!(parse_prefixed_nick(""), (MemberRank::None, ""));
    }

    /// server-time is what stops replayed history dating itself to the
    /// moment it was read.
    #[test]
    fn a_server_timestamp_is_read_when_the_server_sends_one() {
        let msg = "@time=2026-08-29T12:34:56.789Z :bob!b@h PRIVMSG #chan :hi"
            .parse::<irc::proto::Message>()
            .unwrap();
        assert_eq!(server_time(&msg), Some(1788006896));

        // No tag at all is the ordinary case on a server without the
        // capability, and must fall back rather than resolve to the epoch.
        let bare = ":bob!b@h PRIVMSG #chan :hi".parse::<irc::proto::Message>().unwrap();
        assert_eq!(server_time(&bare), None);
    }

    /// Only the modes that rank a person move the member list; the ones
    /// that configure the channel must not be mistaken for one.
    #[test]
    fn only_person_modes_carry_a_rank() {
        assert_eq!(rank_from_mode(&ChannelMode::Oper), Some(MemberRank::Op));
        assert_eq!(rank_from_mode(&ChannelMode::Voice), Some(MemberRank::Voice));
        assert_eq!(rank_from_mode(&ChannelMode::Halfop), Some(MemberRank::HalfOp));
        assert_eq!(rank_from_mode(&ChannelMode::Founder), Some(MemberRank::Founder));
        // +a outranks op and the member model has no separate step for it.
        assert_eq!(rank_from_mode(&ChannelMode::Admin), Some(MemberRank::Founder));

        for mode in [ChannelMode::Ban, ChannelMode::Moderated, ChannelMode::InviteOnly, ChannelMode::Key, ChannelMode::Limit, ChannelMode::Secret] {
            assert_eq!(rank_from_mode(&mode), None, "{mode:?} is about the channel, not a person in it");
        }
    }

    /// The sentence has to read correctly in both directions, since the same
    /// words are used for granting and for taking away.
    #[test]
    fn a_rank_change_reads_as_a_sentence() {
        assert_eq!(
            format!("bob {} {} {} carol", "gives", rank_word(MemberRank::Op), "to"),
            "bob gives operator status to carol"
        );
        assert_eq!(
            format!("bob {} {} {} carol", "takes", rank_word(MemberRank::Voice), "from"),
            "bob takes voice from carol"
        );
    }

    #[test]
    fn tls_carries_sasl_without_asking() {
        assert!(sasl_transport_ok(true, false));
    }

    #[test]
    fn cleartext_refuses_sasl_by_default() {
        assert!(!sasl_transport_ok(false, false));
    }

    #[test]
    fn cleartext_carries_sasl_when_allowed_on_purpose() {
        assert!(sasl_transport_ok(false, true));
    }

    #[test]
    fn allowing_plaintext_does_not_change_the_tls_case() {
        // The allowance is a floor, not a mode: it only ever answers for the
        // connection that had no encryption to begin with.
        assert!(sasl_transport_ok(true, true));
    }
}
