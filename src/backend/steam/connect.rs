//! An account's connection, from its token to its goodbye.
//!
//! One task per account. It finds a connection manager, connects, signs in
//! with the saved token, and then does nothing but listen: Steam tells it who
//! the friends are, how they are doing, and what they say, and it keeps the
//! daemon's picture of that current. When the connection ends it decides what
//! kind of ending it was, because that decides what happens next:
//!
//! - **the network** - a server that did not answer, a connection that was
//!   cut: try again, further apart each time, and another server;
//! - **Steam saying wait** - rate limited: wait much longer, not seconds;
//! - **the account** - the saved sign-in no longer works, or the account signed
//!   in somewhere else: stop, and say so. Trying again would not help and the
//!   second case would turn two clients into a fight over the account.

use super::auth;
use super::chat;
use super::cm;
use super::eresult;
use super::friends::Roster;
use super::friendship;
use super::link::{self, Identity, Link};
use super::presence;
use super::proto::*;
use super::publish::Conversations;
use super::steamid;
use super::wire::{emsg, Packet};
use crate::accounts::SteamAccountConfig;
use crate::runtime::ConnState;
use crate::state::AppState;
use anyhow::{anyhow, Context, Result};
use futures::FutureExt;
use prost::Message;
use std::time::Duration;

/// The protocol version the current official client sends at sign-in.
pub const PROTOCOL_VERSION: u32 = 65581;

/// How long Steam has to answer a sign-in.
const LOGON_TIMEOUT: Duration = Duration::from_secs(30);

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(10);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(300);

/// How long to leave an account alone after Steam says it is being asked too
/// often. Seconds would be asking again; this is the length of time that the
/// limit is measured in.
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(300);

/// What to ask about each friend: their status, name, presence, when they
/// were last seen, the game they are in, and their rich presence.
const PERSONA_FIELDS: u32 = 1 | 2 | 16 | 64 | 256 | 4096;

/// How many people to ask about in one message.
const ASK_ABOUT_AT_A_TIME: usize = 200;
/// A friend's new nickname, from the notification that says it: who, and what.
fn decode_nickname_changed(body: &[u8]) -> Option<(u32, Option<String>)> {
    use super::proto::FriendNicknameChanged;
    let n = FriendNicknameChanged::decode(body).ok()?;
    Some((n.accountid?, n.nickname.filter(|n| !n.trim().is_empty())))
}

/// How long after the friends list to read what was missed.
pub(super) static CATCH_UP_AFTER_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(4000);
/// How many times a catch-up that found conversations not yet named is tried again.
const CATCH_UP_RETRIES: u32 = 4;
/// The most that reading what was missed may hold up the connection for.
const CATCH_UP_FOR: Duration = Duration::from_secs(20);
/// How long a notice that somebody is typing holds.
const TYPING_FOR: Duration = Duration::from_secs(10);

/// How a connection ended, and what follows.
#[derive(Debug, PartialEq)]
pub enum Ending {
    /// The connection went away; try again.
    Closed,
    /// Do not try again: the person has something to do first.
    Stop(String),
    /// Try again, but not for this long.
    Later(Duration, String),
}

/// What Steam said to a sign-in.
#[derive(Debug, PartialEq)]
pub enum Logon {
    Welcome { heartbeat: Duration, steamid: u64, session: i32, account_flags: u32 },
    Refused(Ending),
    /// This server cannot take it; another may.
    TryAnother(String),
}

/// Reads the answer to a sign-in.
pub fn read_logon(packet: &Packet) -> Result<Logon> {
    let response = ClientLogonResponse::decode(packet.body.as_slice()).context("reading the sign-in reply")?;
    let code = response.eresult.unwrap_or(eresult::FAIL);
    if code == eresult::OK {
        let heartbeat = response.heartbeat_seconds.filter(|s| *s > 0).unwrap_or(27);
        return Ok(Logon::Welcome {
            // A heartbeat sooner than every five seconds is a server asking to
            // be flooded; an hour is one that would have us dropped.
            heartbeat: Duration::from_secs(heartbeat.clamp(5, 300) as u64),
            steamid: packet.header.steamid.or(response.client_supplied_steamid).unwrap_or_default(),
            session: packet.header.client_sessionid.unwrap_or_default(),
            account_flags: response.account_flags.unwrap_or(0),
        });
    }
    Ok(match code {
        c if eresult::is_rate_limit(c) => Logon::Refused(Ending::Later(RATE_LIMIT_WAIT, eresult::describe(c))),
        c if eresult::means_sign_in_again(c) => Logon::Refused(Ending::Stop(format!("{}. Sign in to Steam again from Accounts.", eresult::describe(c)))),
        c if eresult::is_final(c) => Logon::Refused(Ending::Stop(eresult::describe(c))),
        // Whatever else is something a server did, or did not, and another may do better.
        c => Logon::TryAnother(eresult::describe(c)),
    })
}

/// The message that signs the account in.
pub fn logon_message(config: &SteamAccountConfig, language: &str) -> ClientLogon {
    ClientLogon {
        protocol_version: Some(PROTOCOL_VERSION),
        cell_id: Some(0),
        client_package_version: Some(1771),
        client_language: Some(language.to_string()),
        client_os_type: Some(auth::os_type()),
        should_remember_password: Some(true),
        // Where the machine is on its own network is nothing Steam needs to
        // be told; the field exists and asks to be filled.
        obfuscated_private_ip: Some(IpAddress { ip: Some(ip_address::Ip::V4(auth::logon_id())) }),
        chat_mode: Some(2),
        account_name: Some(config.username.clone()),
        machine_name: Some(auth::device_name()),
        machine_id: Some(auth::machine_id()),
        supports_rate_limit_response: Some(true),
        // The refresh token, which is what this field takes whatever its name.
        access_token: Some(config.refresh_token.clone()),
        ..ClientLogon::default()
    }
}

/// How long after the friends list to read what was missed.
fn catch_up_after() -> Duration {
    Duration::from_millis(CATCH_UP_AFTER_MS.load(std::sync::atomic::Ordering::Relaxed))
}

/// Starts (or restarts) an account's connection.
pub fn spawn(state: AppState, config: SteamAccountConfig) {
    let account_id = config.account_id();
    crate::net::route::router().mark(&account_id, config.use_tor);
    state.runtime.reset_connection(&account_id);
    let join_handle = tokio::spawn({
        let account_id = account_id.clone();
        let state = state.clone();
        async move {
            run_with_retry(&state, &config, &account_id).await;
            state.runtime.remove_task_handle(&account_id);
        }
    });
    state.runtime.insert_task_handle(&account_id, join_handle.abort_handle());
}

async fn run_with_retry(state: &AppState, config: &SteamAccountConfig, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        // The account as it is now, not as it was when this task began: a
        // reconnect after the token was replaced has to use the new one.
        let current = state.accounts.get_steam(account_id);
        let config = current.as_ref().unwrap_or(config);
        let result = std::panic::AssertUnwindSafe(run(state, config, account_id)).catch_unwind().await;
        let (detail, wait) = match result {
            Ok(Ok(Ending::Stop(why))) => {
                tracing::info!("steam[{account_id}]: stopped: {why}");
                state.runtime.set_conn_state(state, account_id, ConnState::Disconnected, Some(&why));
                return;
            }
            Ok(Ok(Ending::Later(wait, why))) => (why, Some(wait)),
            Ok(Ok(Ending::Closed)) => ("connection ended".to_string(), None),
            Ok(Err(e)) => {
                tracing::warn!("steam[{account_id}]: {e:#}");
                (format!("{e:#}"), None)
            }
            Err(_) => {
                tracing::error!("steam[{account_id}]: connection task panicked");
                ("internal error (see nobilis logs)".to_string(), None)
            }
        };
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
        let wait = wait.unwrap_or_else(|| state.runtime.next_retry_delay(account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY));
        tracing::info!("steam[{account_id}]: not connected ({detail}); trying again in {}s", wait.as_secs());
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

/// One go at being connected: pick servers until one answers, then stay on it.
async fn run(state: &AppState, config: &SteamAccountConfig, account_id: &str) -> Result<Ending> {
    if config.refresh_token.is_empty() {
        return Ok(Ending::Stop("this account is not signed in to Steam; sign in from Accounts".to_string()));
    }
    let router = crate::net::route::router();
    if router.routed(account_id) {
        router.ready(|msg| state.runtime.report_progress(state, account_id, msg)).await.context("starting Tor or reaching the proxy")?;
    }
    state.runtime.report_progress(state, account_id, "asking Steam for a server");
    let servers = cm::discover(account_id, 0).await?;
    let mut last: Option<anyhow::Error> = None;
    for server in servers.into_iter().take(4) {
        state.runtime.report_progress(state, account_id, &format!("connecting to {}", server.endpoint));
        match session(state, config, account_id, &server.url()).await {
            Ok(Outcome::Ended(ending)) => return Ok(ending),
            Ok(Outcome::Unreached(why)) => {
                tracing::info!("steam[{account_id}]: {}: {why}", server.endpoint);
                last = Some(anyhow!("{}: {why}", server.endpoint));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("no server answered")))
}

/// Whether a server was ever really reached.
pub enum Outcome {
    /// It was, and the session that followed ended like this.
    Ended(Ending),
    /// It was not, or would not sign us in: another may.
    Unreached(String),
}

/// Takes the connection out of the runtime when the task that owns it ends
/// for any reason - including being cancelled, which runs no code of its own.
struct Registered<'a> {
    state: &'a AppState,
    account_id: &'a str,
}

impl Drop for Registered<'_> {
    fn drop(&mut self) {
        super::groups::clear(self.account_id);
        super::account::clear(self.account_id);
        super::blocked::clear(self.account_id);
        super::chatgroups::clear(self.account_id);
        self.state.runtime.take_steam_link(self.account_id);
    }
}

/// Signs in on one server and stays until it ends.
pub async fn session(state: &AppState, config: &SteamAccountConfig, account_id: &str, url: &str) -> Result<Outcome> {
    let began = std::time::Instant::now();
    let (link, mut told) = match link::open(account_id, url).await {
        Ok(opened) => opened,
        Err(e) => return Ok(Outcome::Unreached(format!("{e:#}"))),
    };

    link.set_identity(Identity { steamid: config.steamid, session: 0 });
    link.send(emsg::CLIENT_HELLO, &ClientHello { protocol_version: Some(PROTOCOL_VERSION) })?;
    state.runtime.report_progress(state, account_id, "signing in");
    link.send(emsg::CLIENT_LOGON, &logon_message(config, "english"))?;

    let reply = loop {
        match tokio::time::timeout(LOGON_TIMEOUT, told.recv()).await {
            Err(_) => return Ok(Outcome::Unreached("Steam did not answer the sign-in".to_string())),
            Ok(None) => return Ok(Outcome::Unreached("the connection ended before Steam answered".to_string())),
            Ok(Some(p)) if p.emsg == emsg::CLIENT_LOG_ON_RESPONSE => break p,
            // Anything else this early is not what is being waited for.
            Ok(Some(_)) => continue,
        }
    };
    let (heartbeat, steamid, session, account_flags) = match read_logon(&reply)? {
        Logon::Welcome { heartbeat, steamid, session, account_flags } => (heartbeat, steamid, session, account_flags),
        Logon::Refused(ending) => return Ok(Outcome::Ended(ending)),
        Logon::TryAnother(why) => return Ok(Outcome::Unreached(why)),
    };
    link.set_identity(Identity { steamid, session });
    if config.steamid != steamid && steamid != 0 {
        let _ = state.accounts.mutate_steam(account_id, |a| a.steamid = steamid);
    }

    state.runtime.set_steam_link(account_id, link.clone());
    let _registered = Registered { state, account_id };
    // Until Steam says what the account is called, what it signs in with.
    if state.runtime.own_identity(account_id).is_none() {
        state.runtime.set_own_identity(account_id, &config.username);
    }
    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);
    // What the account is and what Steam holds back from it, and keeping its
    // sign-in alive: asked in the background, so nothing waits for either.
    tokio::spawn(super::account::load(state.clone(), link.clone(), account_id.to_string(), steamid, account_flags));
    {
        let (state, link, config) = (state.clone(), link.clone(), config.clone());
        tokio::spawn(async move { super::account::renew_if_due(&state, &link, &config).await });
    }

    // Appearing at all takes saying so: an account that signs in and says
    // nothing is not shown as online to anyone. The status somebody chose is
    // what is said.
    let chosen = state.runtime.effective_status(state, account_id);
    presence::send_status(&link, &chosen, true)?;

    let ending = Drive {
        state,
        account_id,
        link: &link,
        own: steamid::account_of(steamid).unwrap_or_default(),
        // Starts from what people were called last time, so a conversation, a member list or a
        // message is not drawn with a number until Steam has said who they are.
        roster: {
            let mut roster = Roster::default();
            roster.remember(state.store.steam_names(account_id).unwrap_or_default());
            roster
        },
        asked: Default::default(),
        catch_up_at: None,
        catch_up_tries: 0,
        prefs_wanted: None,
        apps: AppNames::new(),
        groups_wanted: None,
        shown: Conversations::new(account_id),
    }
    .run(&mut told, heartbeat)
    .await;
    tracing::info!("steam[{account_id}]: the session lasted {}s and ended: {ending:?}", began.elapsed().as_secs());
    Ok(Outcome::Ended(ending))
}

/// What is known while connected, and what to do with each thing Steam says.
struct Drive<'a> {
    state: &'a AppState,
    account_id: &'a str,
    link: &'a Link,
    /// This account's own number, to tell its own details from a friend's.
    own: u32,
    roster: Roster,
    shown: Conversations,
    /// Who has been announced as asking to be our friend.
    asked: std::collections::HashSet<u32>,
    /// When to ask what was missed: a moment after the friends list, so their
    /// conversations exist to put it in.
    catch_up_at: Option<tokio::time::Instant>,
    catch_up_tries: u32,
    /// What the account keeps about its friends, on its way.
    prefs_wanted: Option<tokio::sync::oneshot::Receiver<Vec<PerFriendPreferences>>>,
    /// What the games people are in are called: asked of the store, once each.
    apps: AppNames,
    /// The chat groups, on their way; and whether to read them again.
    groups_wanted: Option<tokio::sync::oneshot::Receiver<Result<Vec<u32>, String>>>,
}

/// Games whose names have been asked for, so none is asked about twice, and
/// where the answers come back to.
struct AppNames {
    asked: std::collections::HashSet<u64>,
    heard: tokio::sync::mpsc::UnboundedReceiver<(u64, Option<String>)>,
    to: tokio::sync::mpsc::UnboundedSender<(u64, Option<String>)>,
}

impl AppNames {
    fn new() -> Self {
        let (to, heard) = tokio::sync::mpsc::unbounded_channel();
        Self { asked: Default::default(), heard, to }
    }
}

impl Drive<'_> {
    /// Asks the store what any game somebody is in is called, once each. Steam
    /// names only a game that is not its own.
    /// Keeps what people are called now, in place of what they were, for the next start.
    fn keep_names(&mut self) {
        let fresh = self.roster.names_to_keep();
        if !fresh.is_empty() {
            if let Err(e) = self.state.store.remember_steam_names(self.account_id, &fresh) {
                tracing::debug!("steam[{}]: names were not kept: {e}", self.account_id);
            }
        }
    }

    fn ask_game_names(&mut self) {
        for app in self.roster.unnamed_apps() {
            if !self.apps.asked.insert(app) {
                continue;
            }
            let (to, key) = (self.apps.to.clone(), self.account_id.to_string());
            tokio::spawn(async move {
                let _ = to.send((app, super::apps::lookup(&key, app).await));
            });
        }
    }

    async fn run(mut self, told: &mut tokio::sync::mpsc::UnboundedReceiver<Packet>, heartbeat: Duration) -> Ending {
        self.shown.adopt(self.state, self.account_id);
        // The warnings still waiting to be seen from before this was started.
        super::notices::restore(self.state, self.account_id);
        // What can be put in a message, for the picker; it arrives when it arrives.
        tokio::spawn(super::emoticons::load(self.state.clone(), self.link.clone(), self.account_id.to_string()));
        // What the account keeps about its friends - nicknames and the like -
        // is asked for in the background and taken in when it arrives, so that
        // a Steam slow to answer holds nothing up.
        let (prefs_tx, prefs_rx) = tokio::sync::oneshot::channel();
        self.prefs_wanted = Some(prefs_rx);
        tokio::spawn({
            let link = self.link.clone();
            async move {
                if let Ok(all) = super::prefs::load(&link).await {
                    let _ = prefs_tx.send(all);
                }
            }
        });
        self.load_groups();
        let poked = super::chatgroups::pokes(self.account_id);
        let mut beat = tokio::time::interval(heartbeat);
        beat.tick().await; // the first tick is immediate
        loop {
            let due = self.catch_up_at;
            tokio::select! {
                _ = async { match due { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await } } => {
                    self.catch_up_at = None;
                    // Bounded: the connection is not heard from while this runs, and
                    // a Steam that stops answering must not stop that for long.
                    let work = tokio::time::timeout(CATCH_UP_FOR, async {
                        let unnamed = super::history::catch_up(self.state, self.link, self.account_id, self.own, &self.roster, &self.shown).await;
                        super::chatgroups::catch_up(self.state, self.link, self.account_id, &self.roster).await;
                        unnamed
                    });
                    tokio::pin!(work);
                    // The heartbeat goes on meanwhile: an account with many
                    // conversations makes this long, and a connection that says
                    // nothing for that long is one Steam is entitled to drop.
                    let unnamed = loop {
                        tokio::select! {
                            done = &mut work => break done.unwrap_or(0),
                            _ = beat.tick() => {
                                if self.link.send(emsg::CLIENT_HEART_BEAT, &ClientHeartBeat::default()).is_err() {
                                    return Ending::Closed;
                                }
                            }
                        }
                    };
                    // Conversations whose friends had not been named yet have
                    // nowhere to be read into: a few more tries, as the names
                    // arrive, and then they are left for opening.
                    if unnamed > 0 && self.catch_up_tries < CATCH_UP_RETRIES {
                        self.catch_up_tries += 1;
                        self.catch_up_at = Some(tokio::time::Instant::now() + catch_up_after());
                    }
                }
                loaded = async { match self.prefs_wanted.as_mut() { Some(rx) => rx.await, None => std::future::pending().await } } => {
                    self.prefs_wanted = None;
                    if let Ok(all) = loaded {
                        let mut renamed = false;
                        for prefs in &all {
                            renamed |= self.roster.set_preferences(prefs);
                        }
                        if renamed {
                            self.keep_names();
                            self.shown.publish(self.state, &self.roster);
                        }
                        self.shown.apply_mutes(self.state, &self.roster);
                    }
                }
                _ = poked.notified() => {
                    super::chatgroups::publish(self.state, self.account_id, &self.roster);
                    let unknown = super::chatgroups::unknown(self.account_id, &self.roster);
                    self.ask_about(&unknown);
                }
                loaded = async { match self.groups_wanted.as_mut() { Some(rx) => rx.await, None => std::future::pending().await } } => {
                    self.groups_wanted = None;
                    match loaded {
                        Ok(Ok(people)) => {
                            let unknown: Vec<u32> = people.into_iter().filter(|p| !self.roster.has_name(*p)).collect();
                            self.ask_about(&unknown);
                            super::chatgroups::publish(self.state, self.account_id, &self.roster);
                            // What was said in them while away.
                            self.catch_up_tries = CATCH_UP_RETRIES;
                            self.catch_up_at = Some(tokio::time::Instant::now() + Duration::from_millis(500));
                        }
                        Ok(Err(why)) => tracing::info!("steam[{}]: the chat groups could not be read: {why}", self.account_id),
                        Err(_) => {}
                    }
                }
                heard = self.apps.heard.recv() => {
                    if let Some((app, Some(name))) = heard {
                        self.roster.set_app_name(app, name);
                        self.shown.publish(self.state, &self.roster);
                    }
                }
                packet = told.recv() => match packet {
                    None => return Ending::Closed,
                    Some(packet) => {
                        if let Some(ending) = self.handle(packet) {
                            return ending;
                        }
                    }
                },
                _ = beat.tick() => {
                    if self.link.send(emsg::CLIENT_HEART_BEAT, &ClientHeartBeat::default()).is_err() {
                        return Ending::Closed;
                    }
                }
            }
        }
    }

    /// One thing Steam said. Returns how the connection ended, if that was it.
    fn handle(&mut self, packet: Packet) -> Option<Ending> {
        match packet.emsg {
            emsg::CLIENT_FRIENDS_LIST => {
                if let Ok(list) = ClientFriendsList::decode(packet.body.as_slice()) {
                    let changed = self.roster.apply_friends_list(&list);
                    // Everybody on a full list, and whoever is new on a
                    // change to one: their names and states are not in it.
                    // Whoever has asked to be a friend is asked about too, so the
                    // request can say who it is from.
                    let known = |a: &u32| self.roster.is_friend(*a) || self.roster.asked_us().contains(a);
                    let ask: Vec<u32> = if list.bincremental.unwrap_or(false) {
                        changed.into_iter().filter(|a| known(a) || self.roster.blocked().contains(a)).collect()
                    } else {
                        let mut all = self.roster.friends();
                        all.extend(self.roster.asked_us());
                        all.extend(self.roster.blocked());
                        all
                    };
                    self.ask_about(&ask);
                    self.shown.publish(self.state, &self.roster);
                    // Conversations that were not there at the start are now.
                    super::notices::restore(self.state, self.account_id);
                    super::blocked::publish(self.state, self.account_id, &self.roster);
                    friendship::publish(self.state, self.account_id, &self.roster, &mut self.asked);
                    // Their names arrive in the next moments; what was missed is
                    // read into conversations that have them.
                    if !list.bincremental.unwrap_or(false) {
                        self.catch_up_tries = 0;
                        self.catch_up_at = Some(tokio::time::Instant::now() + catch_up_after());
                    }
                }
            }
            emsg::CLIENT_PERSONA_STATE => {
                if let Ok(update) = ClientPersonaState::decode(packet.body.as_slice()) {
                    let changed = self.roster.apply_persona(&update);
                    self.keep_names();
                    if changed.iter().any(|a| self.roster.asked_us().contains(a) || self.roster.asked_by_us().contains(a)) {
                        friendship::publish(self.state, self.account_id, &self.roster, &mut self.asked);
                    }
                    if let Some(name) = self.roster.persona(self.own).and_then(|p| p.name.clone()) {
                        self.state.runtime.set_own_identity(self.account_id, &name);
                    }
                    self.ask_game_names();
                    self.shown.publish(self.state, &self.roster);
                    super::blocked::publish(self.state, self.account_id, &self.roster);
                    if super::chatgroups::involves(self.account_id, &changed) {
                        super::chatgroups::publish(self.state, self.account_id, &self.roster);
                    }
                }
            }
            // The groups the account sorts its friends into, and changes to them.
            emsg::CLIENT_FRIENDS_GROUPS_LIST => {
                if let Ok(list) = ClientFriendsGroupsList::decode(packet.body.as_slice()) {
                    super::groups::heard(self.state, self.account_id, &list);
                }
            }
            // Everything the account may put in a message, as one list.
            emsg::CLIENT_EMOTICON_LIST => {
                if let Ok(list) = ClientEmoticonList::decode(packet.body.as_slice()) {
                    super::emoticons::keep(self.state, self.account_id, &list);
                }
            }
            emsg::CLIENT_ACCOUNT_INFO => {
                if let Some(name) = ClientAccountInfo::decode(packet.body.as_slice()).ok().and_then(|i| i.persona_name).filter(|n| !n.is_empty()) {
                    self.state.runtime.set_own_identity(self.account_id, &name);
                }
            }
            emsg::CLIENT_LOGGED_OFF => {
                let code = ClientLoggedOff::decode(packet.body.as_slice()).ok().and_then(|m| m.eresult).unwrap_or(eresult::FAIL);
                tracing::info!("steam[{}]: Steam signed this session out: {} ({code})", self.account_id, eresult::describe(code));
                return Some(match code {
                    c if eresult::means_sign_in_again(c) => Ending::Stop(format!("{}. Sign in to Steam again from Accounts.", eresult::describe(c))),
                    c if eresult::is_final(c) => Ending::Stop(eresult::describe(c)),
                    _ => Ending::Closed,
                });
            }
            emsg::SERVICE_METHOD | emsg::SERVICE_METHOD_SEND_TO_CLIENT => match packet.header.target_job_name.as_deref() {
                Some("FriendMessagesClient.IncomingMessage#1") => {
                    tracing::debug!("steam[{}]: an incoming message notification: {:?}", self.account_id, FriendMessagesIncoming::decode(packet.body.as_slice()).ok());
                    if let Some(typing) = FriendMessagesIncoming::decode(packet.body.as_slice()).ok().as_ref().and_then(chat::read_typing) {
                        self.friend_is_typing(typing);
                    } else if let Some(incoming) = FriendMessagesIncoming::decode(packet.body.as_slice()).ok().and_then(|n| chat::read_incoming(&n)) {
                        let own_name = self.state.runtime.own_identity(self.account_id).unwrap_or_default();
                        let (_, stranger) = chat::record_incoming(self.state, self.account_id, &mut self.roster, &mut self.shown, &own_name, &incoming);
                        if stranger {
                            self.ask_about(&[incoming.account]);
                        }
                    }
                }
                // What the account keeps about a friend changed, here or elsewhere.
                Some("PlayerClient.NotifyPerFriendPreferencesChanged#1") => {
                    if let Ok(changed) = PerFriendPreferencesChanged::decode(packet.body.as_slice()) {
                        if let Some(prefs) = changed.preferences.filter(|p| p.accountid.is_some() || changed.accountid.is_some()) {
                            let prefs = PerFriendPreferences { accountid: prefs.accountid.or(changed.accountid), ..prefs };
                            if self.roster.set_preferences(&prefs) {
                                self.keep_names();
                                self.shown.publish(self.state, &self.roster);
                            }
                            self.shown.apply_mutes(self.state, &self.roster);
                        }
                    }
                }
                Some("PlayerClient.NotifyFriendNicknameChanged#1") => {
                    if let Some(account) = packet.body.as_slice().first().and(Some(&packet.body)).and_then(|b| decode_nickname_changed(b)) {
                        if self.roster.set_nickname(account.0, account.1.as_deref()) {
                            self.shown.publish(self.state, &self.roster);
                        }
                    }
                }
                // Steam flagging a conversation as suspicious.
                Some("FriendMessagesClient.SessionNotice#1") => {
                    if let Ok(notice) = SessionNoticeNotification::decode(packet.body.as_slice()) {
                        if let Some(name) = notice.steamid_friend.and_then(steamid::account_of).and_then(|f| self.shown.name_of(f)) {
                            super::notices::heard(self.state, self.account_id, name, &notice);
                        }
                    }
                }
                // Somebody reacted to a message, or took it back.
                Some("FriendMessagesClient.MessageReaction#1") => {
                    if let Ok(notice) = MessageReactionNotice::decode(packet.body.as_slice()) {
                        if let Some(name) = notice.steamid_friend.and_then(steamid::account_of).and_then(|f| self.shown.name_of(f)) {
                            super::reactions::heard(self.state, &crate::model::buffer_id(self.account_id, name), self.own, &notice);
                        }
                    }
                }
                // Read on another of this account's clients.
                Some("FriendMessagesClient.NotifyAckMessageEcho#1") => {
                    if let Ok(ack) = AckMessage::decode(packet.body.as_slice()) {
                        self.read_elsewhere(&ack);
                    }
                }
                Some(method) if method.starts_with("VoiceChatClient.") || method.starts_with("WebRTCClientNotifications.") || method == "ChatRoomClient.NotifyShouldRejoinChatRoomVoiceChat#1" => {
                    super::voice::heard(self.state, self.account_id, method, &packet.body);
                }
                Some(method) if method.starts_with("ChatRoomClient.") => {
                    if let Some(heard) = super::chatgroups::heard(self.state, self.account_id, &self.roster, method, &packet.body) {
                        self.ask_about(&heard.ask);
                        if heard.reload {
                            self.load_groups();
                        }
                    }
                }
                other => tracing::info!("steam[{}]: a notification this client does not use: {other:?}", self.account_id),
            },
            other => tracing::debug!("steam[{}]: message {other}", self.account_id),
        }
        None
    }

    /// Reads the account's chat groups in the background; they are taken in
    /// when they arrive. A read already on its way is left to finish.
    fn load_groups(&mut self) {
        if self.groups_wanted.is_some() {
            return;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.groups_wanted = Some(rx);
        let (link, account_id, own) = (self.link.clone(), self.account_id.to_string(), self.own);
        tokio::spawn(async move {
            let _ = tx.send(super::chatgroups::load(&link, &account_id, own).await);
        });
    }

    /// Somebody is writing to us. Said only for a conversation that exists:
    /// anybody can send one of these, and a stranger typing should not put a
    /// conversation on screen before they have said anything.
    fn friend_is_typing(&self, friend: u32) {
        let Some(name) = self.shown.name_of(friend) else { return };
        self.state.events.emit(
            "typing",
            serde_json::json!({
                "accountId": self.account_id,
                "bufferId": crate::model::buffer_id(self.account_id, name),
                "nicks": [name],
                "expiresInMs": TYPING_FOR.as_millis() as u64,
            }),
        );
    }

    /// A conversation was read on another client: it is read here too, if what
    /// was read is everything this log holds.
    fn read_elsewhere(&self, ack: &AckMessage) {
        let Some(friend) = ack.steamid_partner.and_then(steamid::account_of) else { return };
        let Some(name) = self.shown.name_of(friend) else { return };
        let buffer_id = crate::model::buffer_id(self.account_id, name);
        let newest = self.state.store.get_backlog(&buffer_id, 0, 1).ok().and_then(|m| m.last().map(|m| m.ts)).unwrap_or(0);
        if i64::from(ack.timestamp.unwrap_or(0)) >= newest {
            self.state.events.emit("bufferRead", serde_json::json!({ "accountId": self.account_id, "bufferId": buffer_id }));
        }
    }

    fn ask_about(&self, accounts: &[u32]) {
        for chunk in accounts.chunks(ASK_ABOUT_AT_A_TIME) {
            let _ = self.link.send(
                emsg::CLIENT_REQUEST_FRIEND_DATA,
                &ClientRequestFriendData { persona_state_requested: Some(PERSONA_FIELDS), friends: chunk.iter().map(|a| steamid::steamid_of(*a)).collect() },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::kick::testkit::simulated_daemon;
    use crate::backend::steam::link::fake::*;
    use crate::backend::steam::wire::{self, emsg, Packet};
    use std::sync::{Arc, Mutex};

    const ACCOUNT: &str = "steam:tester";
    const ME: u64 = 76561197960287930; // account 22202
    const FRIEND: u32 = 100;
    const STRANGER: u32 = 200;

    fn config() -> SteamAccountConfig {
        SteamAccountConfig { username: "tester".into(), steamid: ME, refresh_token: "the-refresh-token".into(), session: "chat".into(), ..Default::default() }
    }

    fn reply_logon(code: i32, heartbeat: i32) -> Packet {
        Packet::new(
            emsg::CLIENT_LOG_ON_RESPONSE,
            Header { steamid: Some(ME), client_sessionid: Some(4242), eresult: Some(code), ..Header::default() },
            ClientLogonResponse { eresult: Some(code), heartbeat_seconds: Some(heartbeat), ..Default::default() }.encode_to_vec(),
        )
    }

    fn tell_packet(kind: u32, body: Vec<u8>) -> Packet {
        Packet::new(kind, Header::default(), body)
    }

    fn incoming(from: u32, text: &str, ts: u32, ordinal: u32, echo: bool) -> Packet {
        incoming_as(emsg::SERVICE_METHOD, from, text, ts, ordinal, echo)
    }

    fn incoming_as(kind: u32, from: u32, text: &str, ts: u32, ordinal: u32, echo: bool) -> Packet {
        Packet::new(
            kind,
            Header { target_job_name: Some("FriendMessagesClient.IncomingMessage#1".into()), ..Header::default() },
            FriendMessagesIncoming {
                steamid_friend: Some(steamid::steamid_of(from)),
                chat_entry_type: Some(1),
                message: Some(text.into()),
                message_no_bbcode: Some(text.into()),
                rtime32_server_timestamp: Some(ts),
                ordinal: Some(ordinal),
                local_echo: Some(echo),
                ..Default::default()
            }
            .encode_to_vec(),
        )
    }

    /// A Steam with one friend, who is online and called Gabe, and who says
    /// hello once the account has said how it is showing.
    fn steam(seen: Arc<Mutex<Vec<Packet>>>, code: i32) -> Script {
        Box::new(move |p| {
            seen.lock().unwrap().push(p.clone());
            match p.emsg {
                emsg::CLIENT_LOGON => Some(vec![reply_logon(code, 5)]),
                emsg::CLIENT_CHANGE_STATUS if code == 1 => {
                    let inside = [
                        tell_packet(emsg::CLIENT_ACCOUNT_INFO, ClientAccountInfo { persona_name: Some("Me Myself".into()), ..Default::default() }.encode_to_vec()),
                        tell_packet(
                            emsg::CLIENT_FRIENDS_LIST,
                            ClientFriendsList {
                                bincremental: Some(false),
                                friends: vec![
                                    FriendsListEntry { ulfriendid: Some(steamid::steamid_of(FRIEND)), efriendrelationship: Some(3) },
                                    FriendsListEntry { ulfriendid: Some(steamid::steamid_of(900)), efriendrelationship: Some(2) },
                                ],
                                ..Default::default()
                            }
                            .encode_to_vec(),
                        ),
                        tell_packet(
                            emsg::CLIENT_PERSONA_STATE,
                            ClientPersonaState {
                                friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(FRIEND)), player_name: Some("Gabe".into()), persona_state: Some(1), avatar_hash: Some(vec![1; 20]), ..Default::default() }],
                                ..Default::default()
                            }
                            .encode_to_vec(),
                        ),
                    ];
                    Some(vec![tell_packet(emsg::MULTI, wire::pack_multi(&inside, true)), incoming(FRIEND, "hello there", 1790000000, 0, false)])
                }
                _ => Some(Vec::new()),
            }
        })
    }

    /// Waits for something to become true, which it does in milliseconds.
    async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
        for _ in 0..250 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("never happened: {what}");
    }

    fn start(state: &AppState, url: String) -> tokio::task::JoinHandle<Result<Outcome>> {
        CATCH_UP_AFTER_MS.store(60, std::sync::atomic::Ordering::Relaxed);
        let state = state.clone();
        tokio::spawn(async move { session(&state, &config(), ACCOUNT, &url).await })
    }

    #[tokio::test]
    async fn an_account_signs_in_learns_its_friends_and_hears_them() {
        let state = simulated_daemon("steam-connect-whole");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = serve(steam(seen.clone(), 1)).await;
        state.accounts.add_steam(config()).unwrap();
        let task = start(&state, url);

        let history = || state.store.get_backlog(&crate::model::buffer_id(ACCOUNT, "Gabe"), 0, 10).unwrap();
        eventually("the friend's message arrives in their conversation", || history().len() == 1).await;

        // What the window sees.
        let buffer = state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).expect("a conversation for Gabe");
        assert_eq!(buffer.kind, "dm");
        assert_eq!(buffer.remote_id.as_deref(), Some("76561197960265828"));
        assert!(buffer.avatar_url.is_some());
        assert!(state.runtime.list_buffers().iter().filter(|b| b.account_id == ACCOUNT).count() == 1, "the pending request is not a conversation");
        assert_eq!(history()[0].body, "hello there");
        assert_eq!(state.runtime.own_identity(ACCOUNT).as_deref(), Some("Me Myself"), "what Steam calls the account");
        assert!(state.runtime.steam_link(ACCOUNT).is_some(), "reachable for sending while connected");
        let presence = state.runtime.list_accounts(&state);
        assert!(presence.iter().any(|a| a.id == ACCOUNT && a.state == "connected" && a.display_name == "Me Myself"), "{presence:?}");

        // What was said to Steam, in order - once it has asked about the friends,
        // which follows the list and is not the same moment as their message.
        eventually("the account to ask who its friends are", || seen.lock().unwrap().iter().any(|p| p.emsg == emsg::CLIENT_REQUEST_FRIEND_DATA)).await;
        let seen = seen.lock().unwrap().clone();
        let kinds: Vec<u32> = seen.iter().map(|p| p.emsg).collect();
        // Hello, then the sign-in; what follows includes the status, but the account's
        // emoticons and preferences are asked for in the background and may be in between.
        assert_eq!(kinds[..2], [emsg::CLIENT_HELLO, emsg::CLIENT_LOGON]);
        assert!(kinds.contains(&emsg::CLIENT_CHANGE_STATUS), "{kinds:?}");
        assert!(kinds.contains(&emsg::CLIENT_REQUEST_FRIEND_DATA), "asked who the friends are: {kinds:?}");
        assert_eq!(ClientHello::decode(seen[0].body.as_slice()).unwrap().protocol_version, Some(65581));
        let logon = ClientLogon::decode(seen[1].body.as_slice()).unwrap();
        assert_eq!(logon.account_name.as_deref(), Some("tester"));
        assert_eq!(logon.access_token.as_deref(), Some("the-refresh-token"), "the token, and never a password");
        assert_eq!((logon.protocol_version, logon.chat_mode), (Some(65581), Some(2)));
        assert_eq!(seen[1].header.steamid, Some(ME), "as the account being signed in");
        let status = ClientChangeStatus::decode(seen.iter().find(|p| p.emsg == emsg::CLIENT_CHANGE_STATUS).unwrap().body.as_slice()).unwrap();
        assert_eq!((status.persona_state, status.persona_set_by_user), (Some(1), Some(true)), "online, since nothing else was chosen");
        let ask = seen.iter().find(|p| p.emsg == emsg::CLIENT_REQUEST_FRIEND_DATA).unwrap();
        assert_eq!(ask.header.client_sessionid, Some(4242), "with the session Steam gave");
        let ask = ClientRequestFriendData::decode(ask.body.as_slice()).unwrap();
        assert_eq!(ask.friends, vec![steamid::steamid_of(FRIEND), steamid::steamid_of(900)], "the friends, and the person who has asked to be one, so the request can say who it is from");
        assert_eq!(state.runtime.steam_requests(ACCOUNT)["incoming"][0]["steamid"], steamid::text_of(900), "and the request is there for the window to show");
        assert_eq!(ask.persona_state_requested, Some(PERSONA_FIELDS));

        task.abort();
    }

    #[tokio::test]
    async fn a_session_that_the_server_ends_reports_the_end_and_lets_go_of_the_connection() {
        let state = simulated_daemon("steam-connect-ends");
        let url = serve(Box::new(|p| match p.emsg {
            emsg::CLIENT_LOGON => Some(vec![reply_logon(1, 5)]),
            emsg::CLIENT_CHANGE_STATUS => None,
            _ => Some(Vec::new()),
        }))
        .await;
        let outcome = start(&state, url).await.unwrap().unwrap();
        assert!(matches!(outcome, Outcome::Ended(Ending::Closed)), "{}", match outcome { Outcome::Ended(e) => format!("{e:?}"), Outcome::Unreached(w) => w });
        assert!(state.runtime.steam_link(ACCOUNT).is_none(), "nothing left to send on");
    }

    #[tokio::test]
    async fn a_second_message_from_a_stranger_and_one_we_sent_elsewhere_both_land_right() {
        let state = simulated_daemon("steam-connect-more");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let url = {
            let seen = seen.clone();
            let mut told = false;
            serve(Box::new(move |p| {
                seen.lock().unwrap().push(p.clone());
                match p.emsg {
                    emsg::CLIENT_LOGON => Some(vec![reply_logon(1, 5)]),
                    emsg::CLIENT_CHANGE_STATUS if !told => {
                        told = true;
                        Some(vec![
                            tell_packet(emsg::CLIENT_FRIENDS_LIST, ClientFriendsList { bincremental: Some(false), friends: vec![FriendsListEntry { ulfriendid: Some(steamid::steamid_of(FRIEND)), efriendrelationship: Some(3) }], ..Default::default() }.encode_to_vec()),
                            tell_packet(emsg::CLIENT_PERSONA_STATE, ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(FRIEND)), player_name: Some("Gabe".into()), persona_state: Some(1), ..Default::default() }], ..Default::default() }.encode_to_vec()),
                            incoming(STRANGER, "do I know you", 1790000001, 0, false),
                            incoming_as(emsg::SERVICE_METHOD_SEND_TO_CLIENT, FRIEND, "sent from my phone", 1790000002, 0, true),
                            // The same message again, as a replay would send it.
                            incoming(FRIEND, "sent from my phone", 1790000002, 0, true),
                        ])
                    }
                    _ => Some(Vec::new()),
                }
            }))
            .await
        };
        let task = start(&state, url);
        let stranger_name = steamid::text_of(STRANGER);
        let stranger_buffer = crate::model::buffer_id(ACCOUNT, &stranger_name);
        eventually("the stranger is heard", || state.store.get_backlog(&stranger_buffer, 0, 10).map(|m| m.len()).unwrap_or(0) == 1).await;
        let gabe = crate::model::buffer_id(ACCOUNT, "Gabe");
        eventually("the message from the phone", || state.store.get_backlog(&gabe, 0, 10).map(|m| m.len()).unwrap_or(0) == 1).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let mine = state.store.get_backlog(&gabe, 0, 10).unwrap();
        assert_eq!(mine.len(), 1, "the replay is one message");
        assert!(mine[0].is_own, "sent by this account from somewhere else");

        // And the stranger was asked after, so they get a name.
        let asked: Vec<Vec<u64>> = seen.lock().unwrap().iter().filter(|p| p.emsg == emsg::CLIENT_REQUEST_FRIEND_DATA).map(|p| ClientRequestFriendData::decode(p.body.as_slice()).unwrap().friends).collect();
        assert!(asked.iter().any(|f| f == &vec![steamid::steamid_of(STRANGER)]), "{asked:?}");
        task.abort();
    }

    fn typing(from: u32) -> Packet {
        Packet::new(
            emsg::SERVICE_METHOD,
            Header { target_job_name: Some("FriendMessagesClient.IncomingMessage#1".into()), ..Header::default() },
            FriendMessagesIncoming { steamid_friend: Some(steamid::steamid_of(from)), chat_entry_type: Some(2), ..Default::default() }.encode_to_vec(),
        )
    }

    fn read_echo(friend: u32, at: u32) -> Packet {
        Packet::new(
            emsg::SERVICE_METHOD,
            Header { target_job_name: Some("FriendMessagesClient.NotifyAckMessageEcho#1".into()), ..Header::default() },
            AckMessage { steamid_partner: Some(steamid::steamid_of(friend)), timestamp: Some(at) }.encode_to_vec(),
        )
    }

    #[tokio::test]
    async fn a_friend_typing_and_a_conversation_read_elsewhere_are_heard_and_only_when_they_mean_something() {
        let state = simulated_daemon("steam-connect-typing");
        let url = serve(Box::new(move |p| match p.emsg {
            emsg::CLIENT_LOGON => Some(vec![reply_logon(1, 5)]),
            emsg::CLIENT_CHANGE_STATUS => Some(vec![
                tell_packet(emsg::CLIENT_FRIENDS_LIST, ClientFriendsList { bincremental: Some(false), friends: vec![FriendsListEntry { ulfriendid: Some(steamid::steamid_of(FRIEND)), efriendrelationship: Some(3) }], ..Default::default() }.encode_to_vec()),
                tell_packet(emsg::CLIENT_PERSONA_STATE, ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(FRIEND)), player_name: Some("Gabe".into()), persona_state: Some(1), ..Default::default() }], ..Default::default() }.encode_to_vec()),
                typing(FRIEND),
                // A stranger typing is not a reason to open a conversation.
                typing(STRANGER),
                incoming(FRIEND, "hello", 200, 0, false),
                // Read up to before the newest: not everything, so still unread here.
                read_echo(FRIEND, 150),
                // Read up to the newest: read.
                read_echo(FRIEND, 200),
            ]),
            _ => Some(Vec::new()),
        }))
        .await;
        state.accounts.add_steam(config()).unwrap();
        let mut rx = state.events.subscribe();
        let task = start(&state, url);
        let gabe = crate::model::buffer_id(ACCOUNT, "Gabe");
        eventually("the friend's message", || state.store.get_backlog(&gabe, 0, 10).map(|m| m.len()).unwrap_or(0) == 1).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        let (mut typed, mut read) = (vec![], vec![]);
        while let Ok(e) = rx.try_recv() {
            match e.name {
                "typing" => typed.push(e.data),
                "bufferRead" => read.push(e.data),
                _ => {}
            }
        }
        assert_eq!(typed.len(), 1, "the friend, and not the stranger: {typed:?}");
        assert_eq!((typed[0]["bufferId"].as_str(), typed[0]["nicks"][0].as_str()), (Some(gabe.as_str()), Some("Gabe")));
        assert_eq!(read.len(), 1, "only the echo that reached the newest message");
        assert_eq!(read[0]["bufferId"].as_str(), Some(gabe.as_str()));
        assert_eq!(state.store.get_backlog(&gabe, 0, 10).unwrap().len(), 1, "typing is not a message");
        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, &steamid::text_of(STRANGER))).is_none());
        task.abort();
    }

    #[tokio::test]
    async fn what_was_missed_while_away_is_read_in_once_the_friends_are_known() {
        let state = simulated_daemon("steam-connect-catchup");
        let url = serve(Box::new(move |p| match p.emsg {
            emsg::CLIENT_LOGON => Some(vec![reply_logon(1, 5)]),
            emsg::CLIENT_CHANGE_STATUS => Some(vec![
                tell_packet(emsg::CLIENT_FRIENDS_LIST, ClientFriendsList { bincremental: Some(false), friends: vec![FriendsListEntry { ulfriendid: Some(steamid::steamid_of(FRIEND)), efriendrelationship: Some(3) }], ..Default::default() }.encode_to_vec()),
                tell_packet(emsg::CLIENT_PERSONA_STATE, ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(FRIEND)), player_name: Some("Gabe".into()), persona_state: Some(1), ..Default::default() }], ..Default::default() }.encode_to_vec()),
            ]),
            emsg::SERVICE_METHOD_CALL_FROM_CLIENT => {
                let method = p.header.target_job_name.clone().unwrap_or_default();
                let body = if method.contains("GetActiveMessageSessions") {
                    GetActiveSessionsResponse { message_sessions: vec![MessageSession { accountid_friend: Some(FRIEND), last_message: Some(300), last_view: Some(100), unread_message_count: Some(2), notices: vec![] }], timestamp: Some(1) }.encode_to_vec()
                } else {
                    GetRecentMessagesResponse {
                        messages: vec![
                            PastMessage { accountid: Some(FRIEND), timestamp: Some(300), message: Some("second".into()), ordinal: Some(0), reactions: vec![] },
                            PastMessage { accountid: Some(FRIEND), timestamp: Some(250), message: Some("first".into()), ordinal: Some(0), reactions: vec![] },
                        ],
                        more_available: Some(false),
                    }
                    .encode_to_vec()
                };
                Some(vec![answer(&p, 1, body)])
            }
            _ => Some(Vec::new()),
        }))
        .await;
        state.accounts.add_steam(config()).unwrap();
        // The log already holds an older message, so this conversation is one
        // that is behind rather than one never opened.
        let gabe = crate::model::buffer_id(ACCOUNT, "Gabe");
        let mut rx = state.events.subscribe();
        let task = start(&state, url);
        eventually("the conversation to exist", || state.runtime.get_buffer(&gabe).is_some()).await;
        let buffer = state.runtime.get_buffer(&gabe).unwrap();
        crate::backend::steam::history::store_quietly(&state, &buffer, 22202, &[crate::backend::steam::history::Past { account: FRIEND, timestamp: 90, ordinal: 0, text: "before".into(), reactions: vec![], embeds: vec![], invite: None }]);

        eventually("what was missed", || state.store.get_backlog(&gabe, 0, 10).map(|m| m.len()).unwrap_or(0) == 3).await;
        let texts: Vec<String> = state.store.get_backlog(&gabe, 0, 10).unwrap().into_iter().map(|m| m.body).collect();
        assert_eq!(texts, ["before", "first", "second"]);
        let mut news = vec![];
        while let Ok(e) = rx.try_recv() {
            if e.name == "message" {
                news.push(e.data["body"].as_str().unwrap_or_default().to_string());
            }
        }
        assert!(news.contains(&"first".to_string()) && news.contains(&"second".to_string()), "unread, so announced: {news:?}");
        task.abort();
    }

    #[tokio::test]
    async fn a_friend_is_called_what_the_account_calls_them_and_follows_a_change_made_elsewhere() {
        let state = simulated_daemon("steam-connect-nickname");
        let renamed = Packet::new(
            emsg::SERVICE_METHOD,
            Header { target_job_name: Some("PlayerClient.NotifyFriendNicknameChanged#1".into()), ..Header::default() },
            FriendNicknameChanged { accountid: Some(FRIEND), nickname: Some("Pal".into()), is_echo_to_self: Some(false) }.encode_to_vec(),
        );
        let mut told = false;
        let url = serve(Box::new(move |p| match p.emsg {
            emsg::CLIENT_LOGON => Some(vec![reply_logon(1, 5)]),
            emsg::CLIENT_CHANGE_STATUS if !told => {
                told = true;
                Some(vec![
                    tell_packet(emsg::CLIENT_FRIENDS_LIST, ClientFriendsList { bincremental: Some(false), friends: vec![FriendsListEntry { ulfriendid: Some(steamid::steamid_of(FRIEND)), efriendrelationship: Some(3) }], ..Default::default() }.encode_to_vec()),
                    tell_packet(emsg::CLIENT_PERSONA_STATE, ClientPersonaState { friends: vec![PersonaFriend { friendid: Some(steamid::steamid_of(FRIEND)), player_name: Some("Gabe".into()), persona_state: Some(1), ..Default::default() }], ..Default::default() }.encode_to_vec()),
                ])
            }
            emsg::SERVICE_METHOD_CALL_FROM_CLIENT if p.header.target_job_name.as_deref() == Some("Player.GetPerFriendPreferences#1") => Some(vec![answer(
                &p,
                1,
                GetPerFriendPreferencesResponse { preferences: vec![PerFriendPreferences { accountid: Some(FRIEND), nickname: Some("Buddy".into()), ..Default::default() }] }.encode_to_vec(),
            )]),
            // Somebody changes it on the phone, once the account has asked about its friends.
            emsg::CLIENT_REQUEST_FRIEND_DATA => Some(vec![renamed.clone()]),
            _ => Some(Vec::new()),
        }))
        .await;
        state.accounts.add_steam(config()).unwrap();
        let task = start(&state, url);
        eventually("the friend, called what the account calls them", || state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Buddy")).is_some() || state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Pal")).is_some()).await;
        // Then what it is changed to.
        eventually("the change made elsewhere", || state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Pal")).is_some()).await;
        assert!(state.runtime.get_buffer(&crate::model::buffer_id(ACCOUNT, "Gabe")).is_none(), "never by their own name once the account has one for them");
        task.abort();
    }

    #[test]
    fn steams_heartbeat_interval_is_kept_to_something_sensible() {
        let welcomed = read_logon(&reply_logon(1, 1)).unwrap();
        assert_eq!(welcomed, Logon::Welcome { heartbeat: Duration::from_secs(5), steamid: ME, session: 4242, account_flags: 0 }, "never faster than every five seconds");
        assert!(matches!(read_logon(&reply_logon(1, 99999)).unwrap(), Logon::Welcome { heartbeat, .. } if heartbeat == Duration::from_secs(300)));
        assert!(matches!(read_logon(&reply_logon(1, 0)).unwrap(), Logon::Welcome { heartbeat, .. } if heartbeat == Duration::from_secs(27)), "no interval means the usual one");
    }

    /// The loop itself, with an interval short enough to watch: Steam drops a
    /// session that goes quiet, so the beat has to keep coming.
    #[tokio::test]
    async fn the_heartbeat_keeps_coming_while_the_connection_is_up() {
        let state = simulated_daemon("steam-connect-beat");
        let beats = Arc::new(Mutex::new(0usize));
        let url = serve({
            let beats = beats.clone();
            Box::new(move |p| {
                if p.emsg == emsg::CLIENT_HEART_BEAT {
                    *beats.lock().unwrap() += 1;
                }
                Some(Vec::new())
            })
        })
        .await;
        let (link, mut told) = link::open(ACCOUNT, &url).await.unwrap();
        link.set_identity(Identity { steamid: ME, session: 1 });
        let drive = Drive { state: &state, account_id: ACCOUNT, link: &link, own: 22202, roster: Roster::default(), shown: Conversations::new(ACCOUNT), asked: Default::default(), catch_up_at: None, catch_up_tries: 0, prefs_wanted: None, apps: AppNames::new(), groups_wanted: None };
        let running = tokio::time::timeout(Duration::from_secs(5), async {
            // Ends when the test has seen enough and the server is dropped.
            let watcher = async {
                eventually("several beats", || *beats.lock().unwrap() >= 3).await;
            };
            tokio::select! {
                ending = drive.run(&mut told, Duration::from_millis(40)) => panic!("ended on its own: {ending:?}"),
                _ = watcher => {}
            }
        })
        .await;
        assert!(running.is_ok());
        assert!(*beats.lock().unwrap() >= 3);
    }

    #[tokio::test]
    async fn a_server_that_refuses_the_sign_in_is_classified_by_what_it_said() {
        let refuse = |code| read_logon(&reply_logon(code, 5)).unwrap();
        // Wrong or revoked: only a new sign-in helps, and the person is told how.
        for code in [eresult::INVALID_PASSWORD, eresult::REVOKED, eresult::EXPIRED, eresult::ACCOUNT_LOGON_DENIED] {
            assert!(matches!(refuse(code), Logon::Refused(Ending::Stop(why)) if why.contains("Sign in to Steam again")), "{code}");
        }
        // Signed in elsewhere: stop, and do not fight for the account.
        assert!(matches!(refuse(eresult::LOGGED_IN_ELSEWHERE), Logon::Refused(Ending::Stop(why)) if why.contains("somewhere else")));
        // Suspended, banned: stop.
        assert!(matches!(refuse(eresult::BANNED), Logon::Refused(Ending::Stop(_))));
        // Too often: wait minutes, not seconds.
        assert!(matches!(refuse(eresult::RATE_LIMIT_EXCEEDED), Logon::Refused(Ending::Later(wait, _)) if wait >= Duration::from_secs(60)));
        // The server's trouble, not the account's: another may do.
        for code in [eresult::SERVICE_UNAVAILABLE, eresult::TRY_ANOTHER_CM, eresult::BUSY, eresult::NO_CONNECTION, 9999] {
            assert!(matches!(refuse(code), Logon::TryAnother(_)), "{code}");
        }
        // A reply that is not one.
        assert!(read_logon(&Packet::new(emsg::CLIENT_LOG_ON_RESPONSE, Header::default(), vec![0xff, 0xff])).is_err());
    }

    #[tokio::test]
    async fn a_refused_sign_in_ends_the_session_without_a_connection_to_leave() {
        let state = simulated_daemon("steam-connect-refused");
        let url = serve(Box::new(|p| match p.emsg {
            emsg::CLIENT_LOGON => Some(vec![reply_logon(eresult::INVALID_PASSWORD, 5)]),
            _ => Some(Vec::new()),
        }))
        .await;
        let outcome = start(&state, url).await.unwrap().unwrap();
        assert!(matches!(outcome, Outcome::Ended(Ending::Stop(_))));
        assert!(state.runtime.steam_link(ACCOUNT).is_none());
        assert!(!state.runtime.list_accounts(&state).iter().any(|a| a.id == ACCOUNT && a.state == "connected"));
    }

    #[tokio::test]
    async fn being_signed_out_by_steam_is_reported_as_what_it_was() {
        for (code, stops) in [(eresult::LOGGED_IN_ELSEWHERE, true), (eresult::REVOKED, true), (eresult::SERVICE_UNAVAILABLE, false)] {
            let state = simulated_daemon("steam-connect-loggedoff");
            let url = serve(Box::new(move |p| match p.emsg {
                emsg::CLIENT_LOGON => Some(vec![reply_logon(1, 5)]),
                emsg::CLIENT_CHANGE_STATUS => Some(vec![tell_packet(emsg::CLIENT_LOGGED_OFF, ClientLoggedOff { eresult: Some(code) }.encode_to_vec())]),
                _ => Some(Vec::new()),
            }))
            .await;
            let outcome = start(&state, url).await.unwrap().unwrap();
            match outcome {
                Outcome::Ended(Ending::Stop(_)) => assert!(stops, "{code}"),
                Outcome::Ended(Ending::Closed) => assert!(!stops, "{code}"),
                other => panic!("{code}: {}", match other { Outcome::Ended(e) => format!("{e:?}"), Outcome::Unreached(w) => w }),
            }
        }
    }

    #[tokio::test]
    async fn a_server_that_never_answers_the_sign_in_is_given_up_on_for_another() {
        // The answer is never sent; the wait is the real thirty seconds, so
        // this checks the cheaper half - a server that hangs up at once.
        let state = simulated_daemon("steam-connect-silent");
        let url = serve(Box::new(|_| None)).await;
        let outcome = start(&state, url).await.unwrap().unwrap();
        assert!(matches!(outcome, Outcome::Unreached(why) if why.contains("ended")));
        let outcome = session(&state, &config(), ACCOUNT, "ws://127.0.0.1:1").await.unwrap();
        assert!(matches!(outcome, Outcome::Unreached(_)), "nothing listening is a server not reached, not a failure of the account");
    }

    #[test]
    fn the_sign_in_carries_the_token_and_what_the_current_client_says_about_itself() {
        let logon = logon_message(&config(), "english");
        assert_eq!(logon.access_token.as_deref(), Some("the-refresh-token"));
        assert_eq!(logon.account_name.as_deref(), Some("tester"));
        assert_eq!(logon.client_language.as_deref(), Some("english"));
        assert_eq!(logon.client_os_type, Some(auth::os_type()));
        assert_eq!(logon.supports_rate_limit_response, Some(true), "so a limit is said, not a dropped connection");
        assert!(logon.machine_name.as_deref().is_some_and(|n| n.starts_with("moho")));
    }
}
