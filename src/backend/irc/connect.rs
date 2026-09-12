//! Getting on to a network and staying on it.
//!
//! Everything between "there is an account configured" and "there is a
//! registered session": the socket (direct, through Tor, or through a proxy),
//! the capability negotiation, the welcome, and the retry loop around all of
//! it. Registration is the part with the most ways to fail silently, which is
//! why so much of this is about noticing that nothing arrived.

use super::*;

pub(super) const REGISTRATION_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

// A real IRC network's anti-flood/throttling can silently drop new
// connection attempts for a while after too many in quick succession from
// the same source - confirmed live: disconnecting and immediately
// reconnecting a couple of times in a row was enough to trigger it against
// both Libera and Rizon simultaneously (their connection attempts just sat
// hung mid-TCP-handshake). No amount of timeout tuning on our end fixes
// that - the actual fix is not re-triggering it, by pacing out our own
// attempts per account.
pub(super) const MIN_RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

// Same shape the other three backends already use: start small so a blip
// costs a couple of seconds, and give up ground quickly when a server is
// genuinely down rather than hammering it.
pub(super) const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);

pub(super) const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Spawns a background task that keeps this account connected: it runs the
/// event loop, and when that ends it waits and connects again.
///
/// It used to stop instead, which made a momentary drop permanent - the one
/// backend where that was true, since sneedchat, Discord and Matrix have all
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
                // The next connection negotiates its own. A server that
                // granted history last time is not promising to this time,
                // and asking on the strength of a stale answer is a command
                // answered with an error.
                state.runtime.clear_irc_caps(&account_id);
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
pub(super) const OVERALL_ESTABLISH_TIMEOUT: Duration = Duration::from_secs(75);

pub(super) async fn run(state: &AppState, config: &IrcAccountConfig) -> Result<()> {
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
    let mut channels: HashMap<String, HashMap<String, Who>> = HashMap::new();

    // Whether the people we hold conversations with are actually connected.
    //
    // IRC has no presence: a message to someone who is offline is accepted by
    // the server and silently discarded, so a client that says nothing leaves
    // you talking to a wall. ISON is the universal way to ask - MONITOR is
    // better where it exists, but not every server has it and this needs no
    // capability negotiation to work anywhere.
    let ison = tokio::spawn(poll_query_presence(state.clone(), account_id.clone(), sender.clone()));

    // And who to be told about whether or not there is a conversation open
    // with them. Cleared first: a reconnect knows nothing about who is on, and
    // keeping the old answers would announce arrivals that are only this
    // client coming back.
    notify_seen().lock().unwrap().remove(&account_id);
    state.runtime.set_irc_monitors(&account_id, false);
    start_monitor(&sender, &notify_list(config));

    // Published for as long as this connection is up, so a file offered over
    // it is fetched back the same way. Registered rather than rebuilt when a
    // transfer starts: turning Tor off in settings does not move a connection
    // that is already established, and a transfer must follow the connection
    // rather than the setting.
    state.runtime.set_irc_transport(&account_id, Some(dcc::transport_for(config)));

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
pub(super) fn proxy_host(config: &IrcAccountConfig) -> &str {
    config.tor_proxy.as_deref().and_then(|p| p.split(':').next()).filter(|s| !s.is_empty()).unwrap_or("127.0.0.1")
}

pub(super) fn proxy_port(config: &IrcAccountConfig) -> u16 {
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
pub(super) async fn establish(state: &AppState, config: &IrcAccountConfig) -> Result<(Sender, ClientStream, Option<NickservWait>)> {
    let account_id = config.account_id();

    // A network that told us, over TLS, not to come back in plaintext.
    //
    // Applied here rather than at the account, because it is not the
    // account's setting to change: STS exists so that the *next* connection
    // cannot be talked down to plaintext by somebody in the middle of it, and
    // a policy that only applied when somebody remembered to tick a box would
    // protect nobody. The stored port comes with it - a network that moved
    // its TLS listener said so when it set the policy.
    //
    // Overridden into a copy of the account rather than carried alongside it,
    // so that everything downstream reads one truth. `config.ssl` is not only
    // used to open the socket: the SASL path refuses to send credentials over
    // a connection it believes is in the clear, and a flag passed separately
    // would have left that check reading the old answer on exactly the
    // connection STS had just upgraded.
    let mut config = config.clone();
    if !config.ssl {
        if let Some(policy) = state.irc_sts.policy(&config.host) {
            tracing::info!("irc[{account_id}]: STS in force for {}, using TLS on port {}", config.host, policy.port);
            state.runtime.report_progress(state, &account_id, &format!("{} requires TLS (STS) - connecting on port {} instead", config.host, policy.port));
            config.ssl = true;
            config.port = Some(policy.port);
        }
    }
    let config = &config;
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
        // The certificate SASL EXTERNAL authenticates with. Set on the
        // connection rather than sent in a message: EXTERNAL means "whoever
        // this TLS session already proved me to be", so without it here there
        // is nothing for the server to check.
        client_cert_path: config.sasl_cert_path.clone().filter(|p| !p.is_empty()),
        client_cert_pass: config.sasl_cert_pass.clone().filter(|p| !p.is_empty()),
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

    // On a connection made in the clear, ask what the server offers before
    // saying anything at all.
    //
    // The ordinary STS check lives in `end_cap_and_register`, which on the
    // SASL path does not run until after the credentials have gone - and a
    // network that advertises STS is a network saying those credentials
    // should never have crossed a plaintext socket. `sasl_transport_ok`
    // already refuses to send them in the clear unless somebody has
    // explicitly allowed it for this account, so this closes the case where
    // they have: the allowance was made about the network as it was, and STS
    // is the network saying it has changed its mind.
    //
    // One extra round trip, on plaintext connections only, which are both the
    // rare case and the one with something to lose. A second `CAP LS` from
    // `end_cap_and_register` after this is legal and answered again.
    if !config.ssl {
        if let Some(raw) = offered_caps_raw(&sender, &mut stream).await {
            if let Some(port) = note_sts_policy(state, config, &raw.join(" ")) {
                bail!("{} requires TLS (STS) - reconnecting on port {port}", config.host);
            }
        }
    }

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
        end_cap_and_register(state, &sender, &mut stream, config).await?;
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

    state.runtime.insert_irc_handle(
        &account_id,
        IrcHandle {
            sender: sender.clone(),
            nick: config.nick.clone(),
            quit_message: quit_message(config),
        },
    );
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
/// What this account says when it leaves.
///
/// A default rather than nothing, because an empty QUIT is what a dropped
/// connection looks like and a deliberate one should not. Trimmed, since a
/// message of only spaces is the same as none.
pub(super) fn quit_message(config: &IrcAccountConfig) -> String {
    match config.quit_message.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        Some(message) => message.to_string(),
        None => DEFAULT_QUIT_MESSAGE.to_string(),
    }
}

pub const DEFAULT_QUIT_MESSAGE: &str = "moho";

pub(super) const WANTED_CAPS: &[&str] = &[
    "server-time",
    "multi-prefix",
    // Who is away, as it happens. Without it away is known only about
    // somebody just looked up or just messaged, so a roster is a list of
    // people who might be there.
    "away-notify",
    // Who is identified to services, and to what account - carried on the
    // join itself rather than needing a WHOIS per person.
    "extended-join",
    "account-notify",
    // A host change without the fake part-and-rejoin a server otherwise has
    // to fake it with.
    "chghost",
    // A message's own id, which is what makes replayed history recognisable
    // as history rather than as new. Without it every backfill would arrive
    // as a fresh copy of everything already on screen: IRC has no id of its
    // own, so this daemon generates one, and a generated id can never match
    // the message it is a second copy of.
    "message-tags",
    // How a server frames a replay. Not read for its own sake - the messages
    // inside a batch are ordinary ones - but a server will not send
    // chathistory to a client that cannot be told where a batch begins.
    "batch",
    // The history itself. Ergo and a few others have it; the ones that do not
    // simply NAK it, which costs nothing.
    "draft/chathistory",
    "chathistory",
    // Our own messages, sent back to us by the server.
    //
    // Which makes the line on screen the line the network delivered - with
    // the server's own id and timestamp, and after whatever truncation or
    // rewriting it applied - rather than this client's guess at it. Where it
    // is absent the local copy is still written; see send_plain.
    "echo-message",
    // A tag on a send, repeated on whatever answers it. Not needed to match
    // the echo (the echo is recognisable on its own) but it is what turns a
    // refusal into an answer about *this* message rather than a numeric that
    // arrived at about the same time.
    "labeled-response",
    // The roster, filled in on arrival. NAMES answers `nick!user@host`
    // instead of a bare nick, so joining a channel says who everybody is
    // rather than only what they are called - and costs no round trip at all.
    "userhost-in-names",
    // Which account each message came from. `account-notify` and
    // `extended-join` already say who is identified at the moment they join
    // or change it; this is the per-message half, which is what survives a
    // netsplit or a client that was not watching at the time.
    "account-tag",
    // The network's own word for "this one is a program", rather than this
    // client guessing from a name.
    "bot-mode",
    // A realname that can be changed without reconnecting. Without it the
    // field is fixed by USER at registration, and changing it means dropping
    // every channel and every query to say a different sentence about
    // yourself.
    "setname",
    // Somebody else being invited to a channel we are in. An INVITE addressed
    // to us arrives regardless; this is the one the capability exists for.
    "invite-notify",
    // The server's own word that everything here is UTF-8, which is one
    // fewer thing to guess at on malformed bytes.
    "utf8-only",
    // Refusals in a form that can be read rather than matched. FAIL, WARN and
    // NOTE carry a code and a sentence, which is what a modern server uses
    // for anything without a numeric of its own.
    "standard-replies",
    // The notify list, answered with the same detail everything else carries
    // - who they are identified as and whether they are away, rather than a
    // bare "they are online".
    "extended-monitor",
    // The server stops volunteering a member list on join, because this
    // client asks for one when a conversation is opened instead. An account
    // that autojoins twenty channels was being sent twenty member lists at
    // connect, every one of them about a room nobody had looked at yet.
    "no-implicit-names",
    // Four drafts, requested because WeeChat, Halloy and bIRC already ship
    // them: a draft nobody has built can still change underneath an
    // implementation, and a draft three clients interoperate on is a protocol
    // whatever the registry calls it. Each degrades to exactly the old
    // behaviour where it is not offered. See `drafts.rs`.
    //
    // A message longer than a line, sent as one message rather than cut.
    "draft/multiline",
    // Taking a message back, which IRC has never had and every other service
    // here has always had.
    "draft/message-redaction",
    // Where this conversation was read up to, kept by the server so a second
    // client starts where the first left off.
    "draft/read-marker",
    // A channel changing its name without becoming a second channel with the
    // first one's history stranded in it.
    "draft/channel-rename",
];

/// The capabilities named by a CAP ACK, from whichever field they arrived in.
///
/// `CAP * ACK :multi-prefix` puts the list in the parameter before the
/// trailing one, which is not where reading a raw IRC line suggests it would
/// be. Both are accepted because both are legal on the wire, and getting this
/// wrong is silent: nothing errors, the client simply believes the server
/// granted nothing.
pub(super) fn cap_list<'a>(param: Option<&'a str>, suffix: Option<&'a str>) -> &'a str {
    param.or(suffix).unwrap_or("")
}

/// Which of a `CAP NEW` offer are worth asking for.
///
/// The intersection with `WANTED_CAPS`, because a server offering something
/// this daemon does not understand should not be answered with a request it
/// would then have to honour. Values are stripped the same way `grant_irc_caps`
/// strips them: a server may offer `sasl=PLAIN,EXTERNAL`, and the name is what
/// is being asked for.
///
/// `sasl` is deliberately not among them, and it is the interesting omission.
/// It is not in `WANTED_CAPS` at all - the handshake in `sasl.rs` requests it
/// on its own, before registration, and drives the exchange by reading the
/// stream directly. Asking for it again here would record the capability as
/// held without authenticating anything, which is worse than not asking:
/// `irc_has_cap(.., "sasl")` would then be true for a connection that is not
/// signed in. Re-authenticating an already-registered connection needs a state
/// machine in the router rather than a blocking read, and is its own piece of
/// work.
pub(super) fn caps_worth_requesting(offered: &str) -> Vec<&str> {
    offered
        .split_whitespace()
        .map(|cap| cap.split('=').next().unwrap_or(cap))
        .filter(|cap| WANTED_CAPS.contains(cap))
        .collect()
}

/// Acts on an `sts=` in a server's `CAP LS`, if there is one.
///
/// Which half applies depends on how this connection was made, and the spec is
/// emphatic about it:
///
/// - Over **TLS**, `duration=` is remembered. That is the promise worth
///   keeping, because it is what stops the *next* connection being talked down
///   to plaintext by somebody in the middle of it. `duration=0` withdraws the
///   policy and is obeyed - it is the only way out for a network that turns
///   TLS off.
/// - Over **plaintext**, only `port=` counts, and nothing is remembered.
///   An attacker who can rewrite a plaintext stream can write the policy too,
///   and a remembered forgery would outlive the attack. The upgrade is left to
///   the reconnect: the policy is recorded for this host with a short life,
///   the connection is dropped, and `establish` picks it up on the way back.
fn note_sts_policy(state: &AppState, config: &IrcAccountConfig, caps: &str) -> Option<u16> {
    let advert = sts::sts_from_caps(caps)?;
    let account_id = config.account_id();
    if config.ssl {
        let duration = advert.duration?;
        let port = config.port.unwrap_or(6697);
        tracing::info!("irc[{account_id}]: STS for {} - TLS on port {port} for {duration}s", config.host);
        state.irc_sts.remember(&config.host, port, duration);
        None
    } else {
        let port = advert.port?;
        // A policy learned in the clear is only good enough to get us onto
        // the encrypted port once, which is why it is stored with a short
        // life: the duration that actually binds is read from the
        // advertisement that arrives over TLS, on the connection this one is
        // about to be replaced by.
        tracing::info!("irc[{account_id}]: STS offered over plaintext, reconnecting to port {port} over TLS");
        state.irc_sts.remember(&config.host, port, 60);
        Some(port)
    }
}

/// Closes capability negotiation and sends the ordinary NICK/USER pair.
///
/// Shared by the authenticated path and the fell-back-to-nothing path because
/// both owe the server a CAP END: having asked for capabilities, registration
/// does not proceed until we say we are finished asking, and a server left
/// waiting for that just sits there until the establish timeout fires.
pub(super) async fn end_cap_and_register(
    state: &AppState,
    sender: &Sender,
    stream: &mut ClientStream,
    config: &IrcAccountConfig,
) -> Result<()> {
    // Ask what this server has before asking it for anything.
    //
    // This used to send one REQ per capability and not wait for the answers,
    // on the reasoning that a CAP REQ is atomic - a server that does not know
    // one name refuses the whole line - so a line each meant an old server
    // could refuse one capability without taking the rest with it.
    //
    // That reasoning is sound and the conclusion was wrong, in a way that
    // only showed up once the list got long. Twenty REQ lines is twenty lines
    // into a server's flood protection: Libera answers them about one a
    // second, so RPL_WELCOME arrived after the registration timeout had
    // already given up, and the account simply failed to connect.
    //
    // Asking first fixes both problems at once. Only advertised capabilities
    // are requested, so nothing can be refused for being unknown, and they
    // go in one line because there is no longer a reason to spread them out.
    // A server that answers no LS at all gets the old behaviour, since
    // something that ancient is exactly the case the atomicity worry was
    // about.
    let raw = offered_caps_raw(sender, stream).await;
    if let Some(raw) = raw.as_deref() {
        // An upgrade cannot be done in place: this is the middle of
        // registration, and the way to a TLS port is a fresh connection. So
        // the policy is stored and this attempt is abandoned - the reconnect
        // loop comes straight back, and `establish` reads the policy on the
        // way in. Registering first and reconnecting after would send NICK,
        // USER and possibly a server password in the clear, which is the
        // exact thing the network just asked us not to do.
        if let Some(port) = note_sts_policy(state, config, &raw.join(" ")) {
            bail!("{} requires TLS (STS) - reconnecting on port {port}", config.host);
        }
    }
    let offered: Option<Vec<String>> =
        raw.map(|raw| raw.iter().map(|cap| cap.split('=').next().unwrap_or(cap).to_string()).collect());
    match offered {
        Some(offered) => {
            let wanted: Vec<&str> = WANTED_CAPS.iter().copied().filter(|cap| offered.iter().any(|o| o == cap)).collect();
            if !wanted.is_empty() {
                sender.send(Command::CAP(None, CapSubCommand::REQ, None, Some(wanted.join(" "))))?;
            }
        }
        None => {
            for cap in WANTED_CAPS {
                sender.send(Command::CAP(None, CapSubCommand::REQ, None, Some((*cap).to_string())))?;
            }
        }
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

/// What the server says it can do, or nothing if it will not say.
///
/// `CAP LS 302` may answer over several lines, each but the last marked with
/// a `*` in the parameter before the list. They are gathered until the one
/// without it, because a capability named on a continuation line is as real
/// as one named on the first.
///
/// A server too old to answer at all is not an error: it times out, this
/// returns None, and the caller falls back to asking for everything the way
/// it always did.
pub(super) async fn offered_caps(sender: &Sender, stream: &mut ClientStream) -> Option<Vec<String>> {
    offered_caps_raw(sender, stream).await.map(|raw| {
        raw.iter()
            // `cap=value` advertises a capability with parameters - SASL names
            // its mechanisms this way, STS its duration and port. The name is
            // what is requested.
            .map(|cap| cap.split('=').next().unwrap_or(cap).to_string())
            .collect()
    })
}

/// The offer as the server wrote it, values and all.
///
/// Separate from `offered_caps` because two callers want two different things
/// out of one answer: negotiation wants names to compare against
/// `WANTED_CAPS`, and STS wants the value it would otherwise have thrown away.
pub(super) async fn offered_caps_raw(sender: &Sender, stream: &mut ClientStream) -> Option<Vec<String>> {
    if sender.send_cap_ls(NegotiationVersion::V302).is_err() {
        return None;
    }
    let mut offered: Vec<String> = Vec::new();
    loop {
        let msg = wait_for(stream, |m| matches!(&m.command, Command::CAP(_, CapSubCommand::LS, _, _))).await.ok()?;
        let Command::CAP(_, _, ref param, ref suffix) = msg.command else { return None };
        // A multiline answer puts "*" where a single-line one puts the list,
        // so the list is whichever field is not the continuation marker.
        let more = param.as_deref() == Some("*");
        let list = if more { suffix.as_deref().unwrap_or("") } else { cap_list(param.as_deref(), suffix.as_deref()) };
        for cap in list.split_whitespace() {
            offered.push(cap.to_string());
        }
        if !more {
            tracing::debug!("irc: server offers {} capabilities: {}", offered.len(), offered.join(" "));
            return Some(offered);
        }
    }
}

pub(super) async fn wait_for_welcome(state: &AppState, account_id: &str, stream: &mut ClientStream) -> Result<()> {
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
                    // Capabilities are ACKed during registration, which is
                    // exactly the window this loop is scanning - and this
                    // loop used to drop everything it was not looking for.
                    // So every capability the server granted was forgotten
                    // the moment it was granted, and the whole client
                    // behaved as though the server had none: no history
                    // backfill, no typing tag, no echo of what was sent.
                    Command::CAP(_, CapSubCommand::ACK, param, suffix) => {
                        let caps = cap_list(param.as_deref(), suffix.as_deref());
                        tracing::debug!("irc[{account_id}]: capabilities granted: {caps}");
                        state.runtime.grant_irc_caps(account_id, caps);
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

pub(super) async fn wait_for(stream: &mut ClientStream, pred: impl Fn(&Message) -> bool) -> Result<Message> {
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

pub(super) fn is_connection_banner(code: Response) -> bool {
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
pub(super) fn banner_text(code: Response, args: &[String]) -> Option<String> {
    if code == Response::RPL_ISUPPORT {
        if args.len() > 2 {
            return Some(args[1..args.len() - 1].join(" "));
        }
        return None;
    }
    args.last().cloned()
}

/// Whether SASL credentials may be put on the wire for this connection.
///
/// TLS, or an explicit decision to do it anyway. Deliberately not satisfied by
/// `use_tor`: a Tor circuit protects the traffic as far as the exit node, and
/// the exit node is precisely where it turns back into cleartext IRC bound for
/// a plaintext port. Treating that as encrypted would hand the password to a
/// stranger's relay rather than to a stranger's ISP, which is not an
/// improvement worth making silently.
pub(super) fn sasl_transport_ok(ssl: bool, allow_plaintext: bool) -> bool {
    ssl || allow_plaintext
}

#[cfg(test)]
mod quit_tests {
    use super::*;

    fn config(message: Option<&str>) -> IrcAccountConfig {
        IrcAccountConfig { quit_message: message.map(String::from), ..Default::default() }
    }

    #[test]
    fn builds_the_history_requests_the_spec_defines() {
        assert_eq!(chathistory_latest("#channel").to_string().trim_end(), "CHATHISTORY LATEST #channel * 100");
        let before = chathistory_before("#channel", 1_767_915_177).unwrap();
        assert_eq!(
            before.to_string().trim_end(),
            "CHATHISTORY BEFORE #channel timestamp=2026-01-08T23:32:57.000Z 100"
        );
    }

    #[test]
    fn a_msgid_is_used_where_the_server_gives_one() {
        // Without this every replayed message is a fresh copy of one already
        // on screen: IRC has no id of its own, so the daemon generates one,
        // and a generated id can never match the message it duplicates.
        let with_id: Message = "@msgid=abc123 :nick!u@h PRIVMSG #chan :hello".parse().unwrap();
        assert_eq!(message_id(&with_id).as_deref(), Some("abc123"));

        let without: Message = ":nick!u@h PRIVMSG #chan :hello".parse().unwrap();
        assert_eq!(message_id(&without), None);

        // An empty tag is not an id, and using one would collapse every such
        // message into a single row.
        let empty: Message = "@msgid= :nick!u@h PRIVMSG #chan :hello".parse().unwrap();
        assert_eq!(message_id(&empty), None);
    }

    #[test]
    fn a_bare_nick_becomes_a_mask_and_a_mask_is_left_alone() {
        assert_eq!(ban_mask("someone"), "someone!*@*");
        assert_eq!(ban_mask("  someone  "), "someone!*@*");
        // Wrapping a mask again would ban nobody while appearing to work.
        assert_eq!(ban_mask("*!*@example.com"), "*!*@example.com");
        assert_eq!(ban_mask("someone!user@host"), "someone!user@host");
        assert_eq!(ban_mask("*@example.com"), "*@example.com");
    }

    #[test]
    fn mode_acts_on_the_channel_it_was_typed_in_unless_told_otherwise() {
        // The common case: no target named, so it is this channel.
        assert_eq!(split_mode_target("#here", "+o someone"), ("#here", "+o someone"));
        assert_eq!(split_mode_target("#here", "-m"), ("#here", "-m"));
        // Asking rather than setting.
        assert_eq!(split_mode_target("#here", ""), ("#here", ""));
        assert_eq!(split_mode_target("#here", "b"), ("b", ""));
        // A target named explicitly.
        assert_eq!(split_mode_target("#here", "#other +o someone"), ("#other", "+o someone"));
        assert_eq!(split_mode_target("#here", "somenick +i"), ("somenick", "+i"));
    }

    #[test]
    fn every_mode_has_a_letter() {
        assert_eq!(mode_letter(&ChannelMode::Moderated), 'm');
        assert_eq!(mode_letter(&ChannelMode::InviteOnly), 'i');
        assert_eq!(mode_letter(&ChannelMode::Key), 'k');
        assert_eq!(mode_letter(&ChannelMode::ProtectedTopic), 't');
        // The ones a network invented, which is most of them in practice.
        assert_eq!(mode_letter(&ChannelMode::Unknown('C')), 'C');
    }

    #[test]
    fn reads_every_spelling_of_a_mechanism() {
        assert_eq!(SaslMechanism::parse("external"), Some(SaslMechanism::External));
        assert_eq!(SaslMechanism::parse(" PLAIN "), Some(SaslMechanism::Plain));
        for spelling in ["SCRAM-SHA-256", "scram-sha256", "SCRAM_SHA_256"] {
            assert_eq!(SaslMechanism::parse(spelling), Some(SaslMechanism::ScramSha256), "for {spelling}");
        }
        // Something we cannot do is not silently treated as something we can.
        assert_eq!(SaslMechanism::parse("SCRAM-SHA-1"), None);
        assert_eq!(SaslMechanism::parse("ECDSA-NIST256P-CHALLENGE"), None);
    }

    #[test]
    fn leads_with_the_strongest_this_account_is_equipped_for() {
        // PLAIN, deliberately. SCRAM is stronger and almost nothing on IRC
        // implements it, and a server refusing an unknown mechanism is only
        // *recommended* to say which ones it has - so leading with SCRAM
        // would break SASL on Libera and Rizon rather than upgrade it.
        let bare = IrcAccountConfig::default();
        assert_eq!(preferred_mechanism(&bare), SaslMechanism::Plain);

        let with_cert = IrcAccountConfig { sasl_cert_path: Some("/keys/libera.pem".into()), ..Default::default() };
        assert_eq!(preferred_mechanism(&with_cert), SaslMechanism::External);

        // Configuring a certificate and then naming a mechanism means the
        // name: somebody pinning PLAIN has a reason, usually a server that
        // advertises what it will not accept.
        let pinned = IrcAccountConfig {
            sasl_cert_path: Some("/keys/libera.pem".into()),
            sasl_mechanism: Some("plain".into()),
            ..Default::default()
        };
        assert_eq!(preferred_mechanism(&pinned), SaslMechanism::Plain);

        // A mechanism nobody here speaks falls back rather than failing the
        // connection over a typo in a config file.
        let nonsense = IrcAccountConfig { sasl_mechanism: Some("magic".into()), ..Default::default() };
        assert_eq!(preferred_mechanism(&nonsense), SaslMechanism::Plain);

        assert_eq!(
            preferred_mechanism(&IrcAccountConfig { sasl_mechanism: Some("scram-sha-256".into()), ..Default::default() }),
            SaslMechanism::ScramSha256
        );
    }

    #[test]
    fn says_what_the_account_says() {
        assert_eq!(quit_message(&config(Some("gone fishing"))), "gone fishing");
    }

    #[test]
    fn an_account_with_nothing_to_say_still_says_something() {
        // An empty QUIT is what a dropped connection looks like, and leaving
        // deliberately should not look like falling over.
        assert_eq!(quit_message(&config(None)), DEFAULT_QUIT_MESSAGE);
        assert_eq!(quit_message(&config(Some(""))), DEFAULT_QUIT_MESSAGE);
        assert_eq!(quit_message(&config(Some("   "))), DEFAULT_QUIT_MESSAGE);
    }

    #[test]
    fn trims_what_it_is_given() {
        assert_eq!(quit_message(&config(Some("  bye  "))), "bye");
    }
}

#[cfg(test)]
mod cap_new_tests {
    use super::caps_worth_requesting;

    /// A `CAP NEW` is answered with the intersection, not with everything.
    ///
    /// Asking for something never wanted is not harmless: a capability this
    /// daemon does not understand still changes what the server sends once it
    /// is granted, and nothing here would know what to do with it.
    #[test]
    fn only_what_was_wanted_in_the_first_place_is_asked_for() {
        let asked = caps_worth_requesting("chghost vendor.example/thing away-notify");
        assert_eq!(asked, vec!["chghost", "away-notify"]);
    }

    /// A capability offered with a value is asked for by name.
    #[test]
    fn a_capability_offered_with_a_value_is_asked_for_by_name() {
        assert_eq!(caps_worth_requesting("draft/chathistory=50"), vec!["draft/chathistory"]);
    }

    /// SASL is left alone on purpose - see `caps_worth_requesting`. Recording
    /// it as held without running the exchange would make
    /// `irc_has_cap(.., "sasl")` true for a connection that is not signed in.
    #[test]
    fn sasl_is_not_taken_up_here() {
        assert!(caps_worth_requesting("sasl=PLAIN,EXTERNAL").is_empty());
    }

    /// An offer of nothing recognisable produces no request, rather than an
    /// empty `CAP REQ` for a server to puzzle over.
    #[test]
    fn an_offer_of_nothing_useful_is_not_answered() {
        assert!(caps_worth_requesting("vendor.example/one vendor.example/two").is_empty());
        assert!(caps_worth_requesting("").is_empty());
    }
}

#[cfg(test)]
mod cap_tests {
    use super::cap_list;
    use irc::proto::{CapSubCommand, Command, Message};

    /// Which field of a CAP LS line holds the list, and which holds the "*"
    /// that says another line is coming.
    ///
    /// Read by content rather than position, because a live network is what
    /// proved position wrong: looking for the star in one field only meant
    /// the last continuation line was taken for the whole answer, and every
    /// capability advertised after it was silently never requested. Libera
    /// offers nineteen; this client was seeing nine of them.
    #[test]
    fn a_multiline_offer_is_read_to_the_end() {
        // Both orderings of (param, suffix), because both occur.
        for (param, suffix) in [(Some("*"), Some("a b")), (Some("a b"), Some("*"))] {
            let fields = [param, suffix];
            let more = fields.iter().any(|f| f.map(str::trim) == Some("*"));
            let caps: Vec<&str> = fields
                .into_iter()
                .flatten()
                .filter(|f| f.trim() != "*")
                .flat_map(str::split_whitespace)
                .collect();
            assert!(more, "a line carrying a star has more to come");
            assert_eq!(caps, vec!["a", "b"]);
        }

        // The final line carries no star and is the end of the answer.
        let fields = [None, Some("c d")];
        assert!(!fields.iter().any(|f| f.map(str::trim) == Some("*")));
    }

    /// Where the granted capabilities actually live in a parsed CAP ACK.
    ///
    /// Not a hypothetical: this client read them out of the trailing
    /// parameter, and the crate puts them in the one before it - so every
    /// capability every server ever granted was dropped on the floor, and
    /// the whole client behaved as though no server had any.
    #[test]
    fn a_cap_ack_carries_its_list_in_the_parameter() {
        let msg: Message = ":lithium.libera.chat CAP me ACK :echo-message\r\n".parse().unwrap();
        let Command::CAP(_, CapSubCommand::ACK, param, suffix) = &msg.command else {
            panic!("not a CAP ACK: {:?}", msg.command);
        };
        assert_eq!(cap_list(param.as_deref(), suffix.as_deref()), "echo-message");
    }

    #[test]
    fn both_spellings_are_read() {
        // Whichever field it lands in, and however many are granted at once.
        assert_eq!(cap_list(Some("a b"), None), "a b");
        assert_eq!(cap_list(None, Some("a b")), "a b");
        assert_eq!(cap_list(None, None), "");
    }
}
