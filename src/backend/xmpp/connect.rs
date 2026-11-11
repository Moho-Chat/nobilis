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
use std::borrow::Cow;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::parsers::sasl::DefinedCondition;
use tokio_xmpp::stanzastream::{Connection, Event, StanzaStream, StreamEvent};
use tokio_xmpp::xmlstream::{StreamHeader, Timeouts, XmppStream};
use tokio_xmpp::Error;

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(5);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);
/// Silence tolerated before the library probes the connection (a ping), and then before it gives the connection up.
/// Tight enough that a connection a sleeping laptop left half-open is noticed in under a minute.
const TIMEOUTS: Timeouts = Timeouts { read_timeout: Duration::from_secs(45), response_timeout: Duration::from_secs(25) };

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
    if jid.node().is_none() {
        return Err(format!("{input:?} has no user part: an account is written user@server"));
    }
    Ok(jid.to_bare())
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

/// Signs in on a fresh connection: connect, offer SASL, authenticate, and come back with a stream that is ready for
/// resource binding. This is the library's own `client_auth`, which it does not export.
async fn authenticate(connector: &Connector, jid: &Jid, password: &str) -> Result<(tokio_xmpp::parsers::stream_features::StreamFeatures, XmppStream<<Connector as tokio_xmpp::connect::ServerConnector>::Stream>), Error> {
    use tokio_xmpp::connect::ServerConnector;
    let username = jid.node().ok_or(Error::InvalidState)?.as_str();
    let (pending, binding) = connector.connect(jid, tokio_xmpp::parsers::ns::JABBER_CLIENT, TIMEOUTS).await?;
    let (features, stream) = pending.recv_features().await?;
    let creds = sasl::common::Credentials::default().with_username(username).with_password(password).with_channel_binding(binding);
    let stream = tokio_xmpp::client_login(stream, features.sasl_mechanisms, creds).await?;
    let stream = stream.send_header(StreamHeader { to: Some(Cow::Borrowed(jid.domain().as_str())), from: None, id: None }).await?;
    Ok(stream.recv_features().await?)
}

/// What the library calls to get a connection, now and after every drop it cannot resume.
fn reconnector(state: AppState, account_id: String, connector: Connector, identity: Jid, password: String, seen: SeenSlot, stop: mpsc::UnboundedSender<Stop>) -> Box<dyn FnMut(Option<String>, oneshot::Sender<Connection>) + Send + 'static> {
    Box::new(move |_preferred_location, mut slot| {
        let (state, account_id, connector, identity, password, seen, stop) = (state.clone(), account_id.clone(), connector.clone(), identity.clone(), password.clone(), seen.clone(), stop.clone());
        tokio::spawn(async move {
            let mut delay = RECONNECT_INITIAL_DELAY;
            loop {
                if slot.is_closed() {
                    return;
                }
                *seen.lock().unwrap() = None;
                match authenticate(&connector, &identity, &password).await {
                    Ok((features, stream)) => {
                        let connection = Connection { stream: stream.box_stream(), features, identity };
                        if slot.send(connection).is_err() {
                            tracing::debug!("xmpp[{account_id}]: the stream was dropped while connecting");
                        }
                        return;
                    }
                    Err(e) => {
                        let shown = seen.lock().unwrap().clone();
                        if let Some(why) = classify(&e, &shown) {
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
                        tracing::warn!("xmpp[{account_id}]: {e}");
                        state.runtime.set_conn_state(&state, &account_id, ConnState::Connecting, None);
                        let wait = state.runtime.next_retry_delay(&account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY);
                        state.runtime.report_retry(&state, &account_id, &format!("{e}"), wait);
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
    let stopped = |stop: &Stop| {
        let (conn, advice) = match stop {
            Stop::Credentials(_) => (ConnState::AuthFailed, " Enter the password again from Accounts."),
            Stop::Certificate(_) => (ConnState::AuthFailed, " Trust the certificate or add its authority from Accounts."),
            Stop::Setup(_) => (ConnState::AuthFailed, " Check the account's settings in Accounts."),
        };
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
    let seen: SeenSlot = Default::default();
    let settings = Settings { account_id: account_id.to_string(), host: config.host.clone(), port: config.port, direct: config.security == "direct", ca_pem: config.ca_pem.clone(), pin_sha256: config.pin_sha256.clone() };
    let connector = match Connector::new(settings, &seen) {
        Ok(c) => c,
        Err(e) => return stopped(&Stop::Setup(format!("{e:#}"))),
    };
    let (stop_tx, mut stop_rx) = mpsc::unbounded_channel();
    let stream = StanzaStream::new(reconnector(state.clone(), account_id.to_string(), connector, identity, config.password.clone(), seen.clone(), stop_tx), 32);
    let (sender, mut receiver) = stream.split();
    let sender = Arc::new(sender);
    let session = Arc::new(Mutex::new(Session::default()));
    let mut current: Option<Handle> = None;
    loop {
        tokio::select! {
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
                    Event::Stream(StreamEvent::Reset { bound_jid, .. }) => {
                        tracing::info!("xmpp[{account_id}]: signed in as {bound_jid}");
                        // A stream that began afresh has lost what the server knew of this session: presence, carbons,
                        // the roster's push subscription. Everything is set up again.
                        session.lock().unwrap_or_else(|e| e.into_inner()).present = false;
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
                        tracing::info!("xmpp[{account_id}]: the connection dropped; the session is kept for resuming");
                        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None)
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
    fn a_resource_is_made_once_and_looks_like_this_client() {
        let r = new_resource();
        assert!(r.starts_with("moho.") && r.len() == 13, "{r}");
        assert_ne!(r, new_resource());
    }
}
