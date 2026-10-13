//! Logging in with SASL on a connection that is already up.
//!
//! The case this exists for: the network's services were down when moho
//! connected, so the server offered no `sasl` and moho registered without
//! logging in. When services return, a server with `cap-notify` says so with
//! `CAP NEW sasl`. WeeChat and HexChat answer that by logging in over the live
//! connection, and so does this - nothing drops, no channel is left.
//!
//! The connect-time login in `sasl.rs` cannot do this: it reads the socket
//! itself and blocks until it is done, which is only possible before anything
//! else is listening. Here every line already goes through the router, so the
//! exchange is a small machine that is shown each line and says what to send.
//! It owns no socket, which is what lets the tests below drive it line by line.
//!
//! What it takes care never to do is believe it is logged in because the
//! server acknowledged `sasl`. Only `903` (or `907`, "you already are") counts.

use std::time::{Duration, Instant};

use base64::Engine;
use irc::proto::{CapSubCommand, Command, Message, Response};

use super::connect::{cap_list, sasl_transport_ok};
use super::sasl::{decode_challenge, nonce, preferred_mechanism, SaslMechanism, Scram};
use crate::accounts::IrcAccountConfig;

/// How long one step of the exchange may wait for the server before the
/// attempt is abandoned. The connection stays up either way.
pub(super) const STEP_TIMEOUT: Duration = Duration::from_secs(30);

/// What the connection should do, in order.
#[derive(Debug, PartialEq)]
pub(super) enum Out {
    /// A command to send as it is.
    Send(Command),
    /// Logged in. The account name, where the server said it (`900`).
    LoggedIn { account: Option<String> },
    /// The attempt is over and did not log in. `credentials` is true when the
    /// server looked at the username and password and said no - the one case
    /// where trying again, by reconnecting or otherwise, would only repeat it.
    Refused { why: String, credentials: bool },
}

enum Stage {
    Idle,
    /// `CAP REQ :sasl` sent; waiting for ACK or NAK.
    Requested,
    /// `AUTHENTICATE <mechanism>` sent; waiting for the server's `+`.
    Started(SaslMechanism),
    /// SCRAM under way: `first` waits for the server-first message, then for
    /// the server-final one.
    Scram { scram: Box<Scram>, first: bool },
    /// Everything sent; waiting for the verdict.
    Result,
}

pub(super) struct LateSasl {
    stage: Stage,
    since: Instant,
    logged_in: bool,
    /// Set by a password the server refused. Not retried on this connection:
    /// a wrong password sent again on every `CAP NEW` is how an account gets
    /// locked by services.
    given_up: bool,
    tried: Vec<SaslMechanism>,
    /// What a `908` said the server takes, used when its `904` arrives.
    offered: Option<Vec<String>>,
    account: Option<String>,
}

impl LateSasl {
    /// `logged_in` is whether registration already logged this connection in.
    pub(super) fn new(logged_in: bool) -> Self {
        Self {
            stage: Stage::Idle,
            since: Instant::now(),
            logged_in,
            given_up: false,
            tried: Vec::new(),
            offered: None,
            account: None,
        }
    }

    #[cfg(test)]
    pub(super) fn is_logged_in(&self) -> bool {
        self.logged_in
    }

    /// Whether this account could log in with SASL on this connection at all.
    fn can_try(config: &IrcAccountConfig) -> bool {
        if !config.sasl || !sasl_transport_ok(config.ssl, config.allow_plaintext_sasl) {
            return false;
        }
        match preferred_mechanism(config) {
            SaslMechanism::External => true,
            _ => config.password.as_deref().is_some_and(|p| !p.is_empty()),
        }
    }

    /// One line from the server.
    pub(super) fn observe(&mut self, message: &Message, config: &IrcAccountConfig, now: Instant) -> Vec<Out> {
        match &message.command {
            Command::CAP(_, CapSubCommand::NEW, param, suffix) => {
                let offered = cap_list(param.as_deref(), suffix.as_deref());
                if matches!(self.stage, Stage::Idle)
                    && !self.logged_in
                    && !self.given_up
                    && offers_sasl(offered)
                    && Self::can_try(config)
                {
                    self.tried.clear();
                    self.offered = None;
                    self.enter(Stage::Requested, now);
                    return vec![Out::Send(Command::CAP(None, CapSubCommand::REQ, None, Some("sasl".to_string())))];
                }
                Vec::new()
            }
            Command::CAP(_, CapSubCommand::ACK, param, suffix) if matches!(self.stage, Stage::Requested) => {
                if !offers_sasl(cap_list(param.as_deref(), suffix.as_deref())) {
                    return Vec::new();
                }
                self.start(preferred_mechanism(config), now)
            }
            Command::CAP(_, CapSubCommand::NAK, param, suffix) if matches!(self.stage, Stage::Requested) => {
                if !offers_sasl(cap_list(param.as_deref(), suffix.as_deref())) {
                    return Vec::new();
                }
                self.refuse("the server offered SASL and then declined to start it", false)
            }
            // Withdrawn mid-exchange: services went away again. Nothing to
            // abort on the wire - there is nobody left to answer.
            Command::CAP(_, CapSubCommand::DEL, param, suffix) => {
                if !matches!(self.stage, Stage::Idle) && offers_sasl(cap_list(param.as_deref(), suffix.as_deref())) {
                    return self.refuse("the server withdrew SASL before the login finished", false);
                }
                Vec::new()
            }
            Command::AUTHENTICATE(payload) => self.challenge(payload, config, now),
            Command::Response(code, args) => self.numeric(*code, args, now),
            _ => Vec::new(),
        }
    }

    /// The passage of time: gives up on a server that stopped answering.
    pub(super) fn tick(&mut self, now: Instant) -> Vec<Out> {
        if matches!(self.stage, Stage::Idle) || now.duration_since(self.since) < STEP_TIMEOUT {
            return Vec::new();
        }
        let mut out = Vec::new();
        // `*` aborts an exchange in progress; before one has started there is
        // nothing to abort.
        if !matches!(self.stage, Stage::Requested) {
            out.push(Out::Send(Command::AUTHENTICATE("*".to_string())));
        }
        out.extend(self.refuse("the server stopped answering during the login", false));
        out
    }

    fn enter(&mut self, stage: Stage, now: Instant) {
        self.stage = stage;
        self.since = now;
    }

    fn start(&mut self, mechanism: SaslMechanism, now: Instant) -> Vec<Out> {
        self.tried.push(mechanism);
        self.enter(Stage::Started(mechanism), now);
        vec![Out::Send(Command::AUTHENTICATE(mechanism.name().to_string()))]
    }

    fn challenge(&mut self, payload: &str, config: &IrcAccountConfig, now: Instant) -> Vec<Out> {
        let user = config.sasl_user.clone().unwrap_or_else(|| config.nick.clone());
        let pass = config.password.clone().unwrap_or_default();
        let encode = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
        match std::mem::replace(&mut self.stage, Stage::Idle) {
            Stage::Started(mechanism) => {
                if payload != "+" {
                    return self.refuse(&format!("the server answered {} with something unexpected", mechanism.name()), false);
                }
                let answer = match mechanism {
                    SaslMechanism::External => "+".to_string(),
                    SaslMechanism::Plain => encode(&format!("\0{user}\0{pass}")),
                    SaslMechanism::ScramSha256 => {
                        let scram = Scram::new(&user, &pass, &nonce());
                        let first = encode(&scram.client_first());
                        self.enter(Stage::Scram { scram: Box::new(scram), first: true }, now);
                        return vec![Out::Send(Command::AUTHENTICATE(first))];
                    }
                };
                self.enter(Stage::Result, now);
                vec![Out::Send(Command::AUTHENTICATE(answer))]
            }
            Stage::Scram { mut scram, first } => {
                let decoded = match decode_challenge(payload) {
                    Ok(text) => text,
                    Err(e) => return self.abort(&format!("{e:#}")),
                };
                if first {
                    match scram.client_final(&decoded) {
                        Ok(last) => {
                            let sent = encode(&last);
                            self.enter(Stage::Scram { scram, first: false }, now);
                            vec![Out::Send(Command::AUTHENTICATE(sent))]
                        }
                        Err(e) => self.abort(&format!("{e:#}")),
                    }
                } else {
                    // Checked before the verdict is believed, as at connect:
                    // a server that cannot prove it knew the password is not
                    // one to be logged in to.
                    match scram.verify(&decoded) {
                        Ok(()) => {
                            self.enter(Stage::Result, now);
                            vec![Out::Send(Command::AUTHENTICATE("+".to_string()))]
                        }
                        Err(e) => self.abort(&format!("{e:#}")),
                    }
                }
            }
            other => {
                // Not ours: nothing of ours is waiting for a challenge.
                self.stage = other;
                Vec::new()
            }
        }
    }

    fn numeric(&mut self, code: Response, args: &[String], now: Instant) -> Vec<Out> {
        match code {
            // Logged in - by this exchange, or by anything else (a NickServ
            // IDENTIFY answers with it too). Either way, no SASL is needed.
            Response::RPL_LOGGEDIN => {
                self.account = args.get(2).cloned();
                self.logged_in = true;
                Vec::new()
            }
            Response::RPL_LOGGEDOUT => {
                self.logged_in = false;
                Vec::new()
            }
            Response::RPL_SASLSUCCESS | Response::ERR_SASLALREADY if self.active() => {
                self.logged_in = true;
                self.enter(Stage::Idle, now);
                vec![Out::LoggedIn { account: self.account.clone() }]
            }
            Response::RPL_SASLMECHS if self.active() => {
                self.offered = Some(args.get(1).map(|list| list.split(',').map(|m| m.trim().to_string()).collect()).unwrap_or_default());
                Vec::new()
            }
            Response::ERR_SASLFAIL | Response::ERR_SASLTOOLONG | Response::ERR_SASLABORT | Response::ERR_NICKLOCKED
                if self.active() =>
            {
                // A 908 before this named what the server takes: the one
                // refusal worth answering with another mechanism.
                if let Some(offered) = self.offered.take() {
                    let next = offered.iter().filter_map(|n| SaslMechanism::parse(n)).find(|m| !self.tried.contains(m));
                    if let Some(next) = next {
                        return self.start(next, now);
                    }
                    return self.refuse(&format!("the server accepts {}, and this account is set up for another", offered.join(", ")), false);
                }
                let credentials = matches!(self.stage, Stage::Result) && code == Response::ERR_SASLFAIL;
                let why = if credentials {
                    "the server refused the SASL username or password".to_string()
                } else if code == Response::ERR_NICKLOCKED {
                    "services have locked this account".to_string()
                } else {
                    format!("the server refused the login ({})", args.last().map(String::as_str).unwrap_or("no reason given"))
                };
                self.refuse(&why, credentials)
            }
            // A server that only takes SASL during registration.
            Response::ERR_UNKNOWNCOMMAND | Response::ERR_ALREADYREGISTRED
                if self.active() && args.get(1).is_some_and(|c| c.eq_ignore_ascii_case("AUTHENTICATE") || code == Response::ERR_ALREADYREGISTRED) =>
            {
                self.refuse("this server only takes a SASL login while connecting", false)
            }
            _ => Vec::new(),
        }
    }

    fn active(&self) -> bool {
        !matches!(self.stage, Stage::Idle)
    }

    fn abort(&mut self, why: &str) -> Vec<Out> {
        let mut out = vec![Out::Send(Command::AUTHENTICATE("*".to_string()))];
        out.extend(self.refuse(why, false));
        out
    }

    fn refuse(&mut self, why: &str, credentials: bool) -> Vec<Out> {
        self.stage = Stage::Idle;
        self.offered = None;
        if credentials {
            self.given_up = true;
        }
        vec![Out::Refused { why: why.to_string(), credentials }]
    }
}

/// Whether a capability list names `sasl`, with or without a value.
fn offers_sasl(caps: &str) -> bool {
    caps.split_whitespace().any(|cap| cap.split('=').next() == Some("sasl"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(mechanism: Option<&str>) -> IrcAccountConfig {
        let mut config: IrcAccountConfig = serde_json::from_value(serde_json::json!({
            "nick": "tester", "host": "irc.example", "ssl": true, "sasl": true, "password": "hunter2"
        }))
        .unwrap();
        config.sasl_mechanism = mechanism.map(str::to_string);
        config
    }

    fn line(raw: &str) -> Message {
        raw.parse().unwrap()
    }

    fn sent(out: &[Out]) -> Vec<String> {
        out.iter()
            .filter_map(|o| match o {
                Out::Send(c) => Some(Message::from(c.clone()).to_string().trim_end().to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn services_coming_back_log_the_connection_in_with_plain() {
        let (config, t) = (config(None), Instant::now());
        let mut s = LateSasl::new(false);
        assert_eq!(sent(&s.observe(&line(":srv CAP tester NEW :sasl=PLAIN,EXTERNAL"), &config, t)), ["CAP REQ sasl"]);
        assert_eq!(sent(&s.observe(&line(":srv CAP tester ACK :sasl"), &config, t)), ["AUTHENTICATE PLAIN"]);
        let plain = base64::engine::general_purpose::STANDARD.encode("\0tester\0hunter2");
        assert_eq!(sent(&s.observe(&line("AUTHENTICATE +"), &config, t)), [format!("AUTHENTICATE {plain}")]);
        assert!(s.observe(&line(":srv 900 tester tester!u@h tester :You are now logged in as tester"), &config, t).is_empty());
        let done = s.observe(&line(":srv 903 tester :SASL authentication successful"), &config, t);
        assert_eq!(done, vec![Out::LoggedIn { account: Some("tester".to_string()) }]);
        assert!(s.is_logged_in());
        assert!(s.observe(&line(":srv CAP tester NEW :sasl"), &config, t).is_empty(), "and not again");
    }

    #[test]
    fn an_acknowledged_capability_is_not_a_login() {
        let (config, t) = (config(None), Instant::now());
        let mut s = LateSasl::new(false);
        s.observe(&line(":srv CAP tester NEW :sasl"), &config, t);
        s.observe(&line(":srv CAP tester ACK :sasl"), &config, t);
        assert!(!s.is_logged_in());
    }

    #[test]
    fn a_connection_already_logged_in_ignores_the_offer() {
        let (config, t) = (config(None), Instant::now());
        let mut s = LateSasl::new(true);
        assert!(s.observe(&line(":srv CAP tester NEW :sasl"), &config, t).is_empty());
    }

    #[test]
    fn an_account_without_sasl_or_a_password_is_left_alone() {
        let t = Instant::now();
        let mut off = config(None);
        off.sasl = false;
        assert!(LateSasl::new(false).observe(&line(":srv CAP tester NEW :sasl"), &off, t).is_empty());
        let mut nopass = config(None);
        nopass.password = None;
        assert!(LateSasl::new(false).observe(&line(":srv CAP tester NEW :sasl"), &nopass, t).is_empty());
        let mut clear = config(None);
        clear.ssl = false;
        assert!(LateSasl::new(false).observe(&line(":srv CAP tester NEW :sasl"), &clear, t).is_empty(), "no password in the clear");
    }

    #[test]
    fn a_wrong_password_is_reported_and_never_sent_again() {
        let (config, t) = (config(None), Instant::now());
        let mut s = LateSasl::new(false);
        s.observe(&line(":srv CAP tester NEW :sasl"), &config, t);
        s.observe(&line(":srv CAP tester ACK :sasl"), &config, t);
        s.observe(&line("AUTHENTICATE +"), &config, t);
        let out = s.observe(&line(":srv 904 tester :SASL authentication failed"), &config, t);
        assert!(matches!(&out[..], [Out::Refused { credentials: true, .. }]));
        assert!(s.observe(&line(":srv CAP tester NEW :sasl"), &config, t).is_empty());
    }

    #[test]
    fn a_server_that_only_takes_sasl_while_connecting_is_recognised() {
        let (config, t) = (config(None), Instant::now());
        let mut s = LateSasl::new(false);
        s.observe(&line(":srv CAP tester NEW :sasl"), &config, t);
        s.observe(&line(":srv CAP tester ACK :sasl"), &config, t);
        let out = s.observe(&line(":srv 421 tester AUTHENTICATE :Unknown command"), &config, t);
        assert!(matches!(&out[..], [Out::Refused { credentials: false, .. }]));
        assert!(!sent(&s.observe(&line(":srv CAP tester NEW :sasl"), &config, t)).is_empty(), "may try again later");
    }

    #[test]
    fn a_refused_mechanism_falls_back_to_one_the_server_names() {
        let (config, t) = (config(Some("SCRAM-SHA-256")), Instant::now());
        let mut s = LateSasl::new(false);
        s.observe(&line(":srv CAP tester NEW :sasl"), &config, t);
        assert_eq!(sent(&s.observe(&line(":srv CAP tester ACK :sasl"), &config, t)), ["AUTHENTICATE SCRAM-SHA-256"]);
        assert!(s.observe(&line(":srv 908 tester PLAIN,EXTERNAL :are available SASL mechanisms"), &config, t).is_empty());
        assert_eq!(sent(&s.observe(&line(":srv 904 tester :SASL authentication failed"), &config, t)), ["AUTHENTICATE PLAIN"]);
    }

    #[test]
    fn a_silent_server_is_aborted_and_the_connection_kept() {
        let (config, t) = (config(None), Instant::now());
        let mut s = LateSasl::new(false);
        s.observe(&line(":srv CAP tester NEW :sasl"), &config, t);
        s.observe(&line(":srv CAP tester ACK :sasl"), &config, t);
        assert!(s.tick(t + Duration::from_secs(5)).is_empty());
        let out = s.tick(t + STEP_TIMEOUT);
        assert_eq!(sent(&out), ["AUTHENTICATE *"]);
        assert!(matches!(out.last(), Some(Out::Refused { credentials: false, .. })));
    }

    #[test]
    fn sasl_withdrawn_mid_login_ends_the_attempt() {
        let (config, t) = (config(None), Instant::now());
        let mut s = LateSasl::new(false);
        s.observe(&line(":srv CAP tester NEW :sasl"), &config, t);
        let out = s.observe(&line(":srv CAP tester DEL :sasl"), &config, t);
        assert!(matches!(&out[..], [Out::Refused { credentials: false, .. }]));
    }

    #[test]
    fn a_nickserv_login_counts_as_logged_in() {
        let (config, t) = (config(None), Instant::now());
        let mut s = LateSasl::new(false);
        s.observe(&line(":srv 900 tester tester!u@h tester :You are now logged in as tester"), &config, t);
        assert!(s.observe(&line(":srv CAP tester NEW :sasl"), &config, t).is_empty());
    }
}
