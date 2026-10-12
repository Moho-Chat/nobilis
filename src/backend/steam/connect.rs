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

const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// How long to leave an account alone after Steam says it is being asked too
/// often. Seconds would be asking again; this is the length of time that the
/// limit is measured in.
const RATE_LIMIT_WAIT: Duration = Duration::from_secs(300);

/// What to ask about each friend: their status, name, presence, when they
/// were last seen, the game they are in, and their rich presence.
const PERSONA_FIELDS: u32 = 1 | 2 | 16 | 64 | 256 | 4096;

/// How many people to ask about in one message.
const ASK_ABOUT_AT_A_TIME: usize = 200;

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
    Welcome { heartbeat: Duration, steamid: u64, session: i32 },
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
        });
    }
    Ok(match code {
        eresult::RATE_LIMIT_EXCEEDED | eresult::ACCOUNT_LOGIN_DENIED_THROTTLE => Logon::Refused(Ending::Later(RATE_LIMIT_WAIT, eresult::describe(code))),
        eresult::LOGGED_IN_ELSEWHERE => Logon::Refused(Ending::Stop(eresult::describe(code))),
        c if eresult::means_sign_in_again(c) => Logon::Refused(Ending::Stop(format!("{}. Sign in to Steam again from Accounts.", eresult::describe(c)))),
        eresult::BANNED | eresult::SUSPENDED | eresult::ACCOUNT_DISABLED | eresult::ACCOUNT_HAS_BEEN_DELETED | eresult::ACCOUNT_LOCKED_DOWN | eresult::IP_LOGIN_RESTRICTION_FAILED => {
            Logon::Refused(Ending::Stop(eresult::describe(code)))
        }
        c if eresult::worth_another_server(c) => Logon::TryAnother(eresult::describe(c)),
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
        obfuscated_private_ip: Some(IpAddress { ip: Some(ip_address::Ip::V4(0)) }),
        chat_mode: Some(2),
        account_name: Some(config.username.clone()),
        machine_name: Some(auth::device_name()),
        supports_rate_limit_response: Some(true),
        // The refresh token, which is what this field takes whatever its name.
        access_token: Some(config.refresh_token.clone()),
        ..ClientLogon::default()
    }
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
        self.state.runtime.take_steam_link(self.account_id);
    }
}

/// Signs in on one server and stays until it ends.
pub async fn session(state: &AppState, config: &SteamAccountConfig, account_id: &str, url: &str) -> Result<Outcome> {
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
    let (heartbeat, steamid, session) = match read_logon(&reply)? {
        Logon::Welcome { heartbeat, steamid, session } => (heartbeat, steamid, session),
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
        roster: Roster::default(),
        asked: Default::default(),
        shown: Conversations::new(account_id),
    }
    .run(&mut told, heartbeat)
    .await;
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
}

impl Drive<'_> {
    async fn run(mut self, told: &mut tokio::sync::mpsc::UnboundedReceiver<Packet>, heartbeat: Duration) -> Ending {
        self.shown.adopt(self.state, self.account_id);
        let mut beat = tokio::time::interval(heartbeat);
        beat.tick().await; // the first tick is immediate
        loop {
            tokio::select! {
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
                        changed.into_iter().filter(known).collect()
                    } else {
                        let mut all = self.roster.friends();
                        all.extend(self.roster.asked_us());
                        all
                    };
                    self.ask_about(&ask);
                    self.shown.publish(self.state, &self.roster);
                    friendship::publish(self.state, self.account_id, &self.roster, &mut self.asked);
                }
            }
            emsg::CLIENT_PERSONA_STATE => {
                if let Ok(update) = ClientPersonaState::decode(packet.body.as_slice()) {
                    let changed = self.roster.apply_persona(&update);
                    if changed.iter().any(|a| self.roster.asked_us().contains(a) || self.roster.asked_by_us().contains(a)) {
                        friendship::publish(self.state, self.account_id, &self.roster, &mut self.asked);
                    }
                    if let Some(name) = self.roster.persona(self.own).and_then(|p| p.name.clone()) {
                        self.state.runtime.set_own_identity(self.account_id, &name);
                    }
                    self.shown.publish(self.state, &self.roster);
                }
            }
            emsg::CLIENT_ACCOUNT_INFO => {
                if let Some(name) = ClientAccountInfo::decode(packet.body.as_slice()).ok().and_then(|i| i.persona_name).filter(|n| !n.is_empty()) {
                    self.state.runtime.set_own_identity(self.account_id, &name);
                }
            }
            emsg::CLIENT_LOGGED_OFF => {
                let code = ClientLoggedOff::decode(packet.body.as_slice()).ok().and_then(|m| m.eresult).unwrap_or(eresult::FAIL);
                return Some(match code {
                    eresult::LOGGED_IN_ELSEWHERE => Ending::Stop(eresult::describe(code)),
                    c if eresult::means_sign_in_again(c) => Ending::Stop(format!("{}. Sign in to Steam again from Accounts.", eresult::describe(c))),
                    _ => Ending::Closed,
                });
            }
            emsg::SERVICE_METHOD_SEND_TO_CLIENT => match packet.header.target_job_name.as_deref() {
                Some("FriendMessagesClient.IncomingMessage#1") => {
                    if let Some(incoming) = FriendMessagesIncoming::decode(packet.body.as_slice()).ok().and_then(|n| chat::read_incoming(&n)) {
                        let own_name = self.state.runtime.own_identity(self.account_id).unwrap_or_default();
                        let (_, stranger) = chat::record_incoming(self.state, self.account_id, &mut self.roster, &mut self.shown, &own_name, &incoming);
                        if stranger {
                            self.ask_about(&[incoming.account]);
                        }
                    }
                }
                other => tracing::debug!("steam[{}]: a notification this client does not use: {other:?}", self.account_id),
            },
            other => tracing::trace!("steam[{}]: message {other}", self.account_id),
        }
        None
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
        Packet::new(
            emsg::SERVICE_METHOD_SEND_TO_CLIENT,
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

        // What was said to Steam, in order.
        let seen = seen.lock().unwrap().clone();
        let kinds: Vec<u32> = seen.iter().map(|p| p.emsg).collect();
        assert_eq!(kinds[..3], [emsg::CLIENT_HELLO, emsg::CLIENT_LOGON, emsg::CLIENT_CHANGE_STATUS]);
        assert!(kinds.contains(&emsg::CLIENT_REQUEST_FRIEND_DATA), "asked who the friends are: {kinds:?}");
        assert_eq!(ClientHello::decode(seen[0].body.as_slice()).unwrap().protocol_version, Some(65581));
        let logon = ClientLogon::decode(seen[1].body.as_slice()).unwrap();
        assert_eq!(logon.account_name.as_deref(), Some("tester"));
        assert_eq!(logon.access_token.as_deref(), Some("the-refresh-token"), "the token, and never a password");
        assert_eq!((logon.protocol_version, logon.chat_mode), (Some(65581), Some(2)));
        assert_eq!(seen[1].header.steamid, Some(ME), "as the account being signed in");
        let status = ClientChangeStatus::decode(seen[2].body.as_slice()).unwrap();
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
                            incoming(FRIEND, "sent from my phone", 1790000002, 0, true),
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

    #[test]
    fn steams_heartbeat_interval_is_kept_to_something_sensible() {
        let welcomed = read_logon(&reply_logon(1, 1)).unwrap();
        assert_eq!(welcomed, Logon::Welcome { heartbeat: Duration::from_secs(5), steamid: ME, session: 4242 }, "never faster than every five seconds");
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
        let drive = Drive { state: &state, account_id: ACCOUNT, link: &link, own: 22202, roster: Roster::default(), shown: Conversations::new(ACCOUNT), asked: Default::default() };
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
