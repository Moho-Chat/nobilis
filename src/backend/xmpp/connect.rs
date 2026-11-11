//! An account's connection: made, kept up, and stopped for the right reasons.
//!
//! The library's `StanzaStream` does the hard part of staying connected - resource binding, stream management (acks and
//! resumption), and a liveness probe - but its own way of getting a connection retries every failure forever, a wrong
//! password included, and says nothing about why. So the stream is given a *reconnector* of ours instead
//! ([`reconnector`]): it makes a connection through [`Connector`] (our routing and trust), authenticates, and
//! sorts what went wrong:
//!
//! - the server **refused the credentials** (`not-authorized`, `credentials-expired`, `account-disabled`, ...): stop, say
//!   so, and wait to be told a new password - retrying with the same one can only get the account locked or banned;
//! - the **certificate was refused**: stop, and keep what the server showed so the window can offer it to be pinned (or
//!   the CA to be added); the name or the chain is wrong, and trying again will not change that;
//! - the account itself is **not usable** (a malformed JID, a certificate file that is not PEM): stop, in words;
//! - anything else (the network, a timeout, a stream error, a server restarting): try again after a wait that grows
//!   while attempts fail at once and starts over after a connection that held (`Runtime::next_retry_delay`).
//!
//! Once connected, stanzas arrive as [`Event`]s. Signing in is the whole of this file for now: what the stanzas mean
//! (roster, presence, messages) comes in the milestones after it (Gitea issue #23).

use super::session::{Handle, Session};
use super::transport::{Connector, Seen, SeenSlot, Settings};
use crate::accounts::XmppAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use futures::StreamExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::parsers::sasl::DefinedCondition;
use tokio_xmpp::stanzastream::{Connection, Event, StanzaStream, StreamEvent};
use tokio_xmpp::xmlstream::Timeouts;
use tokio_xmpp::Error;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(5);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);
/// Silence tolerated before the library probes the connection (an XEP-0198 `<r/>`, or an XEP-0199 ping without stream
/// management), and then before it gives the connection up: a connection that went half-open is noticed within 45 seconds,
/// and the probe every 30 quiet seconds keeps NAT bindings alive the way a whitespace ping would.
const TIMEOUTS: Timeouts = Timeouts { read_timeout: Duration::from_secs(30), response_timeout: Duration::from_secs(15) };
/// How often the clocks are compared to notice that the computer slept, and how far apart they must drift to say it did.
const WAKE_TICK: Duration = Duration::from_secs(5);
const WAKE_DRIFT: Duration = Duration::from_secs(10);
/// How long a connection has, after a wake, to answer a ping before it is taken for dead.
const WAKE_PING: Duration = Duration::from_secs(10);

/// Why an account stopped trying.
#[derive(Debug, PartialEq, Clone)]
pub enum Stop {
    /// The server will not accept this password.
    Credentials(String),
    /// The server's certificate is not trusted. Carries what it showed.
    Certificate(String),
    /// The account cannot be used as it is set up.
    Setup(String),
}

impl Stop {
    pub fn message(&self) -> &str {
        match self {
            Stop::Credentials(m) | Stop::Certificate(m) | Stop::Setup(m) => m,
        }
    }
}

/// Why each stopped account stopped, in one word for its card (see `model::Account::stop_reason`).
fn stop_reasons() -> &'static Mutex<std::collections::HashMap<String, String>> {
    static REASONS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, String>>> = std::sync::OnceLock::new();
    REASONS.get_or_init(Default::default)
}

pub fn stop_reason(account_id: &str) -> String {
    stop_reasons().lock().unwrap_or_else(|e| e.into_inner()).get(account_id).cloned().unwrap_or_default()
}

fn set_stop_reason(account_id: &str, reason: &str) {
    let mut r = stop_reasons().lock().unwrap_or_else(|e| e.into_inner());
    if reason.is_empty() {
        r.remove(account_id);
    } else {
        r.insert(account_id.to_string(), reason.to_string());
    }
}

/// A resource to bind when the account has none yet: `moho.` and eight characters, kept from then on.
pub fn new_resource() -> String {
    use rand::Rng;
    const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::thread_rng();
    let tail: String = (0..8).map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char).collect();
    format!("moho.{tail}")
}

/// A JID as typed, as the daemon keeps it: the bare form, prepared (case, Unicode normalisation, IDNA).
pub fn normalise_jid(input: &str) -> Result<BareJid, String> {
    let jid = Jid::new(input.trim()).map_err(|e| format!("{input:?} is not an XMPP address: {e}"))?;
    let Some(node) = jid.node() else {
        return Err(format!("{input:?} has no user part: an account is written user@server"));
    };
    // RFC 7622: an internationalised domain is compared in its Unicode form (A-labels become U-labels), and a trailing dot
    // is not part of it - so an address typed as `x@xn--bcher-kva.example` or `x@example.org.` is the account it names.
    let domain = jid.domain().as_str().trim_end_matches('.');
    let (unicode, checked) = idna::domain_to_unicode(domain);
    let domain = if checked.is_ok() { unicode } else { domain.to_string() };
    BareJid::new(&format!("{}@{domain}", node.as_str())).map_err(|e| format!("{input:?} is not an XMPP address: {e}"))
}

/// What a failed attempt means. `None` is "try again".
pub fn classify(error: &Error, seen: &Option<Seen>) -> Option<Stop> {
    match error {
        Error::Auth(tokio_xmpp::error::AuthError::Fail(cond)) => match cond {
            DefinedCondition::NotAuthorized | DefinedCondition::CredentialsExpired | DefinedCondition::AccountDisabled | DefinedCondition::InvalidAuthzid => {
                Some(Stop::Credentials(match cond {
                    DefinedCondition::NotAuthorized => "The server did not accept this password".to_string(),
                    DefinedCondition::CredentialsExpired => "The password has expired: set a new one with the server".to_string(),
                    DefinedCondition::AccountDisabled => "The server says this account is disabled".to_string(),
                    _ => "The server did not accept this account name".to_string(),
                }))
            }
            DefinedCondition::MechanismTooWeak | DefinedCondition::EncryptionRequired => Some(Stop::Setup("The server will not use any sign-in method moho will offer it over this connection".to_string())),
            _ => None,
        },
        Error::Auth(tokio_xmpp::error::AuthError::NoMechanism) => Some(Stop::Setup("The server offers no sign-in method moho can use".to_string())),
        Error::JidParse(e) => Some(Stop::Setup(format!("The address is not usable: {e}"))),
        // A refused certificate arrives as an I/O error wrapping the TLS library's. What the server showed is the
        // authority on whether it was the certificate: the slot is written by the checker, refused or not.
        Error::Io(_) => match seen {
            Some(seen) if !seen.refused.is_empty() => Some(Stop::Certificate(format!("The server's certificate was not trusted ({}). Its fingerprint is {}.", seen.refused, seen.sha256))),
            _ => None,
        },
        _ => None,
    }
}

/// What a failed sign-in means. `None` is "try again".
pub fn classify_login(error: &super::login::LoginError, seen: &Option<Seen>) -> Option<Stop> {
    use super::login::LoginError;
    match error {
        LoginError::Credentials(m) => Some(Stop::Credentials(m.clone())),
        LoginError::Setup(m) => Some(Stop::Setup(m.clone())),
        LoginError::Retry(_) => None,
        LoginError::Stream(e) => {
            let (words, retry) = super::login::stream_error_words(e);
            (!retry).then_some(Stop::Setup(words))
        }
        LoginError::Transport(e) => classify(e, seen),
    }
}

fn login_words(error: &super::login::LoginError) -> String {
    use super::login::LoginError;
    match error {
        LoginError::Credentials(m) | LoginError::Setup(m) | LoginError::Retry(m) => m.clone(),
        LoginError::Stream(e) => super::login::stream_error_words(e).0,
        LoginError::Transport(e) => format!("{e}"),
    }
}

/// One attempt: reach the server, sign in, and hand the library its connection. What the account learned (a token, how
/// strongly it signed in, when) is written to it.
async fn attempt(state: &AppState, account_id: &str, connector: &Connector, identity: &Jid, observed: &super::wire::ObservedSlot) -> Result<Connection, super::login::LoginError> {
    use super::login::{Creds, LoginError};
    let config = state.accounts.get_xmpp(account_id).ok_or_else(|| LoginError::Setup("the account was removed".to_string()))?;
    let creds = Creds {
        password: config.password.clone(),
        floor: super::sasl::Floor::from_config(&config.sasl_floor),
        fast: (!config.fast_token.is_empty()).then(|| (config.fast_token.clone(), config.fast_mechanism.clone())),
        user_agent_id: config.user_agent_id.clone(),
    };
    let (raw, bindings) = connector.open(identity).await.map_err(LoginError::Transport)?;
    let wire = super::wire::Wire::new(raw, observed.clone());
    let resumable = observed.lock().unwrap_or_else(|e| e.into_inner()).resumable;
    let tap = connector.tap.clone();
    let outcome = super::login::login(wire, bindings, identity, &creds, resumable, move |w| Box::new(super::tap::Tapped::new(w, tap)) as Box<dyn tokio_xmpp::connect::AsyncReadAndWrite>, TIMEOUTS).await?;
    observed.lock().unwrap_or_else(|e| e.into_inner()).stream_error = None;
    tracing::info!("xmpp[{account_id}]: signed in with {}{}{}", outcome.mechanism, if outcome.sasl2 { " over SASL2" } else { "" }, if outcome.bound_inline { ", bound inline (Bind2)" } else { "" });
    {
        let floor = outcome.floor;
        let how = format!("{}{}{}", outcome.mechanism, if outcome.sasl2 { " over SASL2" } else { "" }, if outcome.bound_inline { " with Bind2" } else { "" });
        let (token, drop_token) = (outcome.token.clone(), outcome.drop_token);
        let _ = state.accounts.mutate_xmpp(account_id, move |a| {
            if floor > super::sasl::Floor::from_config(&a.sasl_floor) {
                a.sasl_floor = floor.as_config().to_string();
            }
            if drop_token {
                a.fast_token.clear();
                a.fast_mechanism.clear();
                a.fast_expiry.clear();
            }
            if let Some(t) = token {
                (a.fast_token, a.fast_mechanism, a.fast_expiry) = (t.token, t.mechanism, t.expiry);
            }
            a.last_connected = chrono::Utc::now().to_rfc3339();
            a.last_mechanism = how;
        });
    }
    Ok(outcome.connection)
}

/// What the library calls to get a connection, now and after every drop it cannot resume.
fn reconnector(state: AppState, account_id: String, connector: Connector, identity: Arc<Mutex<Jid>>, seen: SeenSlot, observed: super::wire::ObservedSlot, stop: mpsc::UnboundedSender<Stop>) -> Box<dyn FnMut(Option<String>, oneshot::Sender<Connection>) + Send + 'static> {
    // When the last connection was handed over, and the wait before the next one if it fell over at once: a server (or a bug)
    // that takes the sign-in and then drops the stream every time must not be signed in to as fast as the network allows.
    let handed: Arc<Mutex<(Option<std::time::Instant>, Duration)>> = Arc::new(Mutex::new((None, RECONNECT_INITIAL_DELAY)));
    Box::new(move |_preferred_location, mut slot| {
        let (state, account_id, connector, identity, seen, observed, stop, handed) = (state.clone(), account_id.clone(), connector.clone(), identity.clone(), seen.clone(), observed.clone(), stop.clone(), handed.clone());
        tokio::spawn(async move {
            let quick = {
                let mut h = handed.lock().unwrap_or_else(|e| e.into_inner());
                match h.0 {
                    Some(at) if at.elapsed() < Duration::from_secs(10) => {
                        let wait = h.1;
                        h.1 = (h.1 * 2).min(RECONNECT_MAX_DELAY);
                        Some(wait)
                    }
                    _ => {
                        h.1 = RECONNECT_INITIAL_DELAY;
                        None
                    }
                }
            };
            if let Some(wait) = quick {
                tracing::warn!("xmpp[{account_id}]: the last connection ended as soon as it was made; waiting {} s", wait.as_secs());
                state.runtime.report_retry(&state, &account_id, "The server ended the connection straight after signing in", wait);
                tokio::time::sleep(wait).await;
            }
            let mut delay = RECONNECT_INITIAL_DELAY;
            // Where `see-other-host` sent us, once per run of attempts.
            let mut redirect: Option<(String, u16)> = None;
            let mut redirected = false;
            // How the last connection ended, if the server said.
            // Read, not taken: the account's task says it too, and may get to it after this. It is cleared once signed in again.
            let ended = observed.lock().unwrap_or_else(|e| e.into_inner()).stream_error.clone();
            if let Some(e) = &ended {
                state.runtime.set_conn_state(&state, &account_id, ConnState::Connecting, Some(&super::login::stream_error_words(e).0));
                match e.condition.as_str() {
                    "conflict" => {
                        // Another device took this resource. Binding the same one again would take it back, and round it
                        // would go: this device gets a new one, and keeps it.
                        let fresh = new_resource();
                        tracing::info!("xmpp[{account_id}]: another device signed in as this one's resource; binding {fresh}");
                        let kept = fresh.clone();
                        let _ = state.accounts.mutate_xmpp(&account_id, move |a| a.resource = kept);
                        let mut id = identity.lock().unwrap_or_else(|e| e.into_inner());
                        if let Ok(full) = id.to_bare().with_resource_str(&fresh) {
                            *id = Jid::from(full);
                        }
                    }
                    "see-other-host" => {
                        redirect = super::login::other_host(&e.other_host);
                        redirected = true;
                    }
                    _ => {}
                }
            }
            loop {
                if slot.is_closed() {
                    return;
                }
                *seen.lock().unwrap() = None;
                let mut c = connector.clone();
                if let Some((host, port)) = &redirect {
                    c.settings.host = host.clone();
                    c.settings.port = *port;
                }
                let me = identity.lock().unwrap_or_else(|e| e.into_inner()).clone();
                match attempt(&state, &account_id, &c, &me, &observed).await {
                    Ok(connection) => {
                        handed.lock().unwrap_or_else(|e| e.into_inner()).0 = Some(std::time::Instant::now());
                        if slot.send(connection).is_err() {
                            tracing::debug!("xmpp[{account_id}]: the stream was dropped while connecting");
                        }
                        return;
                    }
                    Err(e) => {
                        if let super::login::LoginError::Stream(se) = &e {
                            if se.condition == "see-other-host" && !redirected {
                                redirected = true;
                                redirect = super::login::other_host(&se.other_host);
                                tracing::info!("xmpp[{account_id}]: the server sent this account to {:?}", redirect);
                                continue;
                            }
                        }
                        let shown = seen.lock().unwrap().clone();
                        if let Some(why) = classify_login(&e, &shown) {
                            tracing::info!("xmpp[{account_id}]: stopped: {}", why.message());
                            match (&why, shown) {
                                (Stop::Certificate(_), Some(seen)) => super::transport::remember_refused(&account_id, seen),
                                _ => super::transport::forget_refused(&account_id),
                            }
                            let _ = stop.send(why);
                            // Hold the slot: dropping it would make the library treat the stream as failed. The account
                            // task drops the stream, which closes it.
                            slot.closed().await;
                            return;
                        }
                        let words = login_words(&e);
                        tracing::warn!("xmpp[{account_id}]: {words}");
                        state.runtime.set_conn_state(&state, &account_id, ConnState::Connecting, None);
                        let wait = state.runtime.next_retry_delay(&account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY);
                        state.runtime.report_retry(&state, &account_id, &words, wait);
                        tokio::time::sleep(wait).await;
                    }
                }
            }
        });
    })
}

/// Starts (or restarts) an account's connection.
pub fn spawn(state: AppState, config: XmppAccountConfig) {
    let account_id = config.account_id();
    crate::net::route::router().mark(&account_id, config.use_tor);
    state.runtime.reset_connection(&account_id);
    let task = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run(&state, &account_id).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, task.abort_handle());
}

/// The account's connection for as long as it is wanted. Returns when it has stopped for good.
async fn run(state: &AppState, account_id: &str) {
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    super::transport::forget_refused(account_id);
    let Some(config) = state.accounts.get_xmpp(account_id) else { return };
    set_stop_reason(account_id, "");
    let pinned = !config.pin_sha256.is_empty();
    let stopped = |stop: &Stop| {
        let (conn, advice, reason) = match stop {
            Stop::Credentials(_) => (ConnState::AuthFailed, " Enter the password again from Accounts.", "password"),
            Stop::Certificate(_) if pinned => (ConnState::AuthFailed, " The certificate is not the one that was trusted: look at what changed in Accounts.", "certificate-changed"),
            Stop::Certificate(_) => (ConnState::AuthFailed, " Trust the certificate or add its authority from Accounts.", "certificate"),
            Stop::Setup(_) => (ConnState::AuthFailed, " Check the account's settings in Accounts.", "setup"),
        };
        set_stop_reason(account_id, reason);
        state.runtime.set_conn_state(state, account_id, conn, Some(&format!("{}.{advice}", stop.message().trim_end_matches('.'))));
    };
    let identity = match normalise_jid(&config.jid) {
        Ok(bare) => {
            let resource = if config.resource.is_empty() { new_resource() } else { config.resource.clone() };
            if resource != config.resource {
                let kept = resource.clone();
                let _ = state.accounts.mutate_xmpp(account_id, move |a| a.resource = kept);
            }
            match bare.with_resource_str(&resource) {
                Ok(full) => Jid::from(full),
                Err(e) => return stopped(&Stop::Setup(format!("The resource {resource:?} is not usable: {e}"))),
            }
        }
        Err(e) => return stopped(&Stop::Setup(e)),
    };
    if config.user_agent_id.is_empty() {
        let id = super::login::new_uuid();
        let _ = state.accounts.mutate_xmpp(account_id, move |a| a.user_agent_id = id);
    }
    let seen: SeenSlot = Default::default();
    let observed: super::wire::ObservedSlot = Default::default();
    let settings = Settings { account_id: account_id.to_string(), host: config.host.clone(), port: config.port, direct: config.security == "direct", websocket: config.security == "websocket", ca_pem: config.ca_pem.clone(), pin_sha256: config.pin_sha256.clone() };
    let connector = match Connector::new(settings, &seen) {
        Ok(c) => c,
        Err(e) => return stopped(&Stop::Setup(format!("{e:#}"))),
    };
    let tap = connector.tap.clone();
    let (stop_tx, mut stop_rx) = mpsc::unbounded_channel();
    let stream = StanzaStream::new(reconnector(state.clone(), account_id.to_string(), connector, Arc::new(Mutex::new(identity)), seen.clone(), observed.clone(), stop_tx), 32);
    let (sender, mut receiver) = stream.split();
    let sender = Arc::new(sender);
    let session = Arc::new(Mutex::new(Session::default()));
    // However this task ends - returning, or aborted by a disconnect - the handle it published goes with it, and with the
    // last sender gone the library closes the stream (a footer, not a dropped socket). Without this a disconnected account
    // stayed signed in at the server, holding its resource.
    struct Release(AppState, String, Arc<Mutex<Session>>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.runtime.clear_xmpp_handle_of(&self.1, &self.2);
        }
    }
    let _release = Release(state.clone(), account_id.to_string(), session.clone());
    let mut current: Option<Handle> = None;
    // Monotonic time stops while the computer sleeps; the wall clock does not. When they drift apart the computer was asleep,
    // and a connection that was up before is asked at once whether it still is - rather than in the 45 seconds the timers
    // above would take.
    let mut wake_tick = tokio::time::interval(WAKE_TICK);
    let mut last_tick = (std::time::Instant::now(), std::time::SystemTime::now());
    loop {
        tokio::select! {
            _ = wake_tick.tick() => {
                let (mono, wall) = (std::time::Instant::now(), std::time::SystemTime::now());
                let slept = wall.duration_since(last_tick.1).unwrap_or_default().saturating_sub(mono.duration_since(last_tick.0));
                last_tick = (mono, wall);
                if slept >= WAKE_DRIFT {
                    if let Some(handle) = state.runtime.xmpp_handle(account_id).filter(|h| h.is_of(&session)) {
                        tracing::info!("xmpp[{account_id}]: the computer slept for {} s; checking the connection", slept.as_secs());
                        let observed = observed.clone();
                        let account_id = account_id.to_string();
                        tokio::spawn(async move {
                            let domain = handle.own().domain().as_str().parse::<Jid>().ok();
                            let ping = handle.request(domain, false, super::xml::empty("ping", "urn:xmpp:ping"));
                            // Any answer, an error included, is a connection that works.
                            let alive = match tokio::time::timeout(WAKE_PING, ping).await {
                                Ok(Ok(_)) => true,
                                Ok(Err(f)) => f.condition != "remote-server-timeout" && f.condition != "disconnected",
                                Err(_) => false,
                            };
                            if !alive {
                                tracing::info!("xmpp[{account_id}]: the connection did not survive the sleep; reconnecting");
                                super::wire::cut(&observed);
                            }
                        });
                    }
                }
            }
            stop = stop_rx.recv() => {
                if let Some(stop) = stop {
                    state.runtime.clear_xmpp_handle(account_id);
                    return stopped(&stop);
                }
            }
            event = receiver.next() => {
                let Some(event) = event else {
                    // The library only ends the stream when it has failed for good.
                    state.runtime.clear_xmpp_handle(account_id);
                    state.runtime.set_conn_state(state, account_id, ConnState::Disconnected, Some("the connection ended"));
                    return;
                };
                match event {
                    Event::Stream(StreamEvent::Reset { bound_jid, features }) => {
                        tracing::info!("xmpp[{account_id}]: signed in as {bound_jid}");
                        // The resource is what was bound, however it was (Bind2 lets the server choose it): kept, so the next
                        // start asks for the same one.
                        if let Some(r) = bound_jid.resource().map(|r| r.to_string()) {
                            let _ = state.accounts.mutate_xmpp(account_id, move |a| if a.resource != r { a.resource = r });
                        }
                        // A stream that began afresh has lost what the server knew of this session: presence, carbons,
                        // the roster's push subscription. Everything is set up again.
                        {
                            let mut s = session.lock().unwrap_or_else(|e| e.into_inner());
                            s.present = false;
                            // Client state indication, if the server offers it (it says so among the stream's features).
                            let offered = features.others.iter().any(|e| e.is("csi", "urn:xmpp:csi:0"));
                            s.csi = Some((tap.clone(), offered));
                        }
                        let handle = Handle::new(account_id, bound_jid, sender.clone(), session.clone());
                        state.runtime.set_xmpp_handle(account_id, handle.clone());
                        state.runtime.set_own_identity(account_id, handle.own().as_str());
                        state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
                        current = Some(handle.clone());
                        tokio::spawn({
                            let state = state.clone();
                            async move { super::init::run(&state, &handle).await }
                        });
                    }
                    Event::Stream(StreamEvent::Suspended) => {
                        // If the server said why, it is said here too.
                        let why = observed.lock().unwrap_or_else(|e| e.into_inner()).stream_error.clone().map(|e| super::login::stream_error_words(&e).0);
                        tracing::info!("xmpp[{account_id}]: the connection dropped{}; the session is kept for resuming", why.as_deref().map(|w| format!(" ({w})")).unwrap_or_default());
                        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, why.as_deref())
                    }
                    Event::Stream(StreamEvent::Resumed) => {
                        tracing::info!("xmpp[{account_id}]: the session was resumed, nothing lost");
                        state.runtime.set_conn_state(state, account_id, ConnState::Connected, None)
                    }
                    Event::Stanza(stanza) => {
                        if let Some(handle) = &current {
                            super::dispatch::stanza(state, handle, stanza).await;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(refused: &str) -> Option<Seen> {
        Some(Seen { sha256: "ab".repeat(32), refused: refused.into(), der: vec![] })
    }

    #[test]
    fn a_wrong_password_stops_the_account_instead_of_being_tried_again() {
        let e = Error::Auth(tokio_xmpp::error::AuthError::Fail(DefinedCondition::NotAuthorized));
        assert!(matches!(classify(&e, &None), Some(Stop::Credentials(m)) if m.contains("password")));
        for cond in [DefinedCondition::CredentialsExpired, DefinedCondition::AccountDisabled, DefinedCondition::InvalidAuthzid] {
            assert!(matches!(classify(&Error::Auth(tokio_xmpp::error::AuthError::Fail(cond)), &None), Some(Stop::Credentials(_))));
        }
    }

    #[test]
    fn a_server_having_a_bad_moment_is_tried_again() {
        for cond in [DefinedCondition::TemporaryAuthFailure, DefinedCondition::Aborted] {
            assert_eq!(classify(&Error::Auth(tokio_xmpp::error::AuthError::Fail(cond)), &None), None);
        }
        assert_eq!(classify(&Error::Disconnected, &None), None);
        assert_eq!(classify(&Error::Io(std::io::Error::other("connection reset")), &None), None);
    }

    #[test]
    fn a_refused_certificate_stops_the_account_and_says_what_the_server_showed() {
        let io = Error::Io(std::io::Error::other("invalid peer certificate"));
        let stop = classify(&io, &seen("UnknownIssuer")).expect("stops");
        let Stop::Certificate(m) = stop else { panic!("{stop:?}") };
        assert!(m.contains("UnknownIssuer") && m.contains(&"ab".repeat(32)), "{m}");
        // The same I/O error with a certificate that was accepted is just a network problem.
        assert_eq!(classify(&io, &seen("")), None);
        assert_eq!(classify(&io, &None), None);
    }

    #[test]
    fn a_server_with_nothing_we_can_use_is_a_setup_problem_not_a_retry() {
        assert!(matches!(classify(&Error::Auth(tokio_xmpp::error::AuthError::NoMechanism), &None), Some(Stop::Setup(_))));
    }

    #[test]
    fn jids_are_kept_in_the_form_the_servers_compare_them() {
        assert_eq!(normalise_jid("  Alice@Example.ORG ").unwrap().as_str(), "alice@example.org");
        assert_eq!(normalise_jid("alice@example.org/phone").unwrap().as_str(), "alice@example.org");
        assert!(normalise_jid("example.org").is_err(), "a domain alone is not an account");
        assert!(normalise_jid("").is_err());
        assert!(normalise_jid("a b@example.org").is_err());
    }

    #[test]
    fn an_internationalised_domain_is_kept_in_one_form_however_it_was_typed() {
        // RFC 7622: A-labels become U-labels, so the account typed either way is the same account.
        assert_eq!(normalise_jid("idn@xn--bcher-kva.example").unwrap().as_str(), "idn@bücher.example");
        assert_eq!(normalise_jid("idn@BÜCHER.example").unwrap().as_str(), "idn@bücher.example");
        assert_eq!(normalise_jid("alice@example.org.").unwrap().as_str(), "alice@example.org", "a trailing dot is not part of the domain");
    }

    #[test]
    fn a_resource_is_made_once_and_looks_like_this_client() {
        let r = new_resource();
        assert!(r.starts_with("moho.") && r.len() == 13, "{r}");
        assert_ne!(r, new_resource());
    }
}
