//! Signing in to the forum and keeping the account up.
//!
//! The chat is a feature of a forum, so a session here is a forum session -
//! obtained with a password, restored from cookies, or handed over by a
//! browser - and everything else in this folder depends on having one.

use super::*;

/// Default hidden service (Kiwi Farms). Clearnet fallback is
/// `kiwifarms.st`, but embedded Tor is used by default regardless of which
/// host is targeted.
pub const DEFAULT_ONION: &str = "kiwifarmsaaf4t2h7gc3dfc5ojhmqruw2nit3uejrpiagrxeuxiyxcyd.onion";

pub const ONION_PORT: u16 = 443;

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Spawns the background task that keeps a Sneedchat account's connection
/// alive - the real-time equivalent of backend::discord::spawn. Called both
/// right after a fresh addSneedChatAccount and, from main.rs, for every
/// saved account on daemon startup.
pub fn spawn(state: AppState, config: SneedChatAccountConfig) {
    let account_id = config.account_id();
    // Same guard as backend::discord::spawn - guarantees at most one live
    // connection per account (see Runtime::reset_connection's doc comment).
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

pub(super) const RECONNECT_INITIAL_DELAY: Duration = Duration::from_secs(3);

pub(super) const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(60);

/// Keeps re-establishing the connection for as long as this task lives -
/// same shape as backend::discord::run_gateway_with_retry, including "no
/// give up permanently" - a bad password just keeps failing visibly rather
/// than settling into a silently-stuck state.
pub(super) async fn run_with_retry(state: &AppState, config: &SneedChatAccountConfig, account_id: &str) {
    let mut delay = RECONNECT_INITIAL_DELAY;
    state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);
    loop {
        let result = std::panic::AssertUnwindSafe(run(state, config, account_id)).catch_unwind().await;
        let (mut detail, inside_tor) = match result {
            Ok(Ok(())) => ("connection ended".to_string(), false),
            Ok(Err(e)) => {
                tracing::warn!("sneedchat[{account_id}]: {e:#}");
                (format!("{e:#}"), crate::net::tor::is_tor_failure(&e))
            }
            Err(_) => {
                tracing::error!("sneedchat[{account_id}]: connection task panicked");
                ("internal error (see nobilis logs)".to_string(), false)
            }
        };
        state.runtime.clear_sneedchat_senders(account_id);
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);

        // A failure inside Tor is one this loop can do something about, and a
        // run of them is one it must: retrying the same broken client is what
        // turns a bad hour into a bad week. What it decides to do is the Tor
        // manager's business - see `stumbled` - but the loop stops waiting a
        // minute afterwards, because whatever just happened is a new thing to
        // try rather than another go at the old one.
        if inside_tor {
            match state.tor.stumbled().await {
                crate::net::tor::Recovery::Waited => {}
                crate::net::tor::Recovery::NewClient => {
                    detail.push_str(" - restarting Tor");
                    delay = RECONNECT_INITIAL_DELAY;
                }
                crate::net::tor::Recovery::FromScratch => {
                    detail.push_str(" - restarting Tor from scratch");
                    delay = RECONNECT_INITIAL_DELAY;
                }
            }
        }

        state.runtime.report_progress(state, account_id, &format!("{detail} - reconnecting in {}s...", delay.as_secs()));
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RECONNECT_MAX_DELAY);
    }
}

pub(super) async fn build_transport(state: &AppState, config: &SneedChatAccountConfig, account_id: &str) -> Result<Transport> {
    match config.tor_mode.as_str() {
        "proxy" => {
            let proxy = config.proxy.as_deref().ok_or_else(|| anyhow!("tor_mode is \"proxy\" but no proxy URL is configured"))?;
            Transport::socks_from_url(proxy)
        }
        _ => {
            let account_id = account_id.to_string();
            let state2 = state.clone();
            let client = state
                .tor
                .get_or_bootstrap(|msg| state2.runtime.report_progress(&state2, &account_id, msg))
                .await
                .context("bootstrapping Tor")?;
            Ok(Transport::Tor(client))
        }
    }
}

/// Hands a session whatever cookies were captured from a browser sign-in.
///
/// Done before the first request rather than after a failed login, because
/// `ensure_authenticated` asks the site whether it is already signed in and
/// only reaches for the password when the answer is no. With a live session
/// restored, that answer is yes and the login form - CAPTCHA and all - is
/// never involved.
pub(super) fn restore_session(session: &Session, config: &SneedChatAccountConfig) {
    if config.cookies.is_empty() {
        return;
    }
    session.http.jar.restore(config.cookies.clone());
}

/// Writes the session back after a successful connection.
///
/// The forum rotates its session cookie, so the copy captured from the
/// browser is the oldest one that will ever work. Saving what the jar holds
/// now is what keeps a browser sign-in good across restarts instead of good
/// until the site next rotates.
pub(super) fn remember_session(state: &AppState, account_id: &str, session: &Session) {
    let jar = session.http.jar.snapshot();
    if jar.is_empty() {
        return;
    }
    if let Err(e) = state.accounts.set_sneedchat_cookies(account_id, jar) {
        tracing::debug!("sneedchat[{account_id}]: keeping the session: {e:#}");
    }
}

/// What to say when there is no way in.
///
/// The forum's login form carries a verification widget that a client without
/// a browser cannot answer, so a stored password is no longer a way in by
/// itself. Said plainly and pointed at the thing that does work, because the
/// site's own wording ("you did not complete the CAPTCHA verification
/// properly") reads like something the person did wrong.
pub(super) fn no_way_in(e: anyhow::Error) -> anyhow::Error {
    let text = format!("{e:#}");
    // The captcha on the sign-in form is answered rather than surrendered to
    // (see captcha.rs), so this is no longer "moho cannot do this" - it is
    // one attempt that did not work. Which still needs saying in a sentence
    // that names the way out, because the way out is the same one.
    if text.to_lowercase().contains("captcha") {
        return anyhow::anyhow!(
            "could not get past the sign-in captcha ({text}) - if this keeps happening, \
             use \"Sign in with a browser\" in Accounts to complete it yourself"
        );
    }
    e
}

/// Logs in once, then opens one permanent connection per configured room.
/// Returns (bails) only if the login itself fails, or if every room task
/// somehow ends - under normal operation this parks forever, since each
/// room task retries its own connection internally; the account only gets
/// fully rebuilt from scratch (fresh transport, fresh login) if this
/// function returns, which `run_with_retry` treats as a failure like any
/// other.
pub(super) async fn run(state: &AppState, config: &SneedChatAccountConfig, account_id: &str) -> Result<()> {
    let transport = build_transport(state, config, account_id).await?;

    let base = format!("https://{}", config.host);
    let session = Session::new(transport.clone(), base, DEFAULT_USER_AGENT.to_string());
    let two_factor = match &config.totp_secret {
        Some(secret) => TwoFactor::Totp(totp::decode_secret(secret).context("stored TOTP secret is not valid base32")?),
        None => TwoFactor::None,
    };
    let creds = Credentials { username: config.username.clone(), password: config.password.clone() };

    state.runtime.report_progress(state, account_id, "logging in...");
    restore_session(&session, config);
    session.ensure_authenticated(&creds, &two_factor).await.map_err(no_way_in).context("logging in")?;
    // Reaching the site at all is what this records: whatever Tor was doing
    // before, it is working now, and the count of failures behind it is no
    // longer evidence of anything.
    state.tor.note_success().await;
    remember_session(state, account_id, &session);
    if let Some(uid) = session.user_id() {
        let _ = state.accounts.set_sneedchat_user_id(account_id, uid);
    }

    state.runtime.set_own_identity(account_id, &config.username);
    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);

    let rooms = effective_rooms(config);
    tracing::info!("sneedchat[{account_id}]: authenticated, connecting {} room(s)", rooms.len());

    // Only the first room's connection stores whispers - every room's
    // websocket appears to receive the same whisper pushes (they aren't
    // room-scoped), so storing them on all of them would show each one
    // duplicated once per connected room.
    let handles: Vec<_> = rooms
        .into_iter()
        .enumerate()
        .map(|(i, room)| {
            let state = state.clone();
            let transport = transport.clone();
            let session = session.clone();
            let host = config.host.clone();
            let username = config.username.clone();
            let password = config.password.clone();
            let totp_secret = config.totp_secret.clone();
            let account_id = account_id.to_string();
            let is_primary = i == 0;
            tokio::spawn(async move {
                let creds = Credentials { username, password };
                let two_factor = match &totp_secret {
                    Some(secret) => match totp::decode_secret(secret) {
                        Ok(bytes) => TwoFactor::Totp(bytes),
                        Err(_) => TwoFactor::None,
                    },
                    None => TwoFactor::None,
                };
                run_room(&state, &transport, &session, &creds, &two_factor, &account_id, &host, &room, is_primary).await;
            })
        })
        .collect();

    // Aborting the account-level task (disconnect/removeAccount/a newer
    // spawn() superseding this one) does not automatically cancel these
    // child tasks - tokio only cascades a JoinHandle's own cancellation,
    // not tasks it spawned - so without this guard every room's websocket
    // would leak and keep running orphaned in the background forever.
    struct AbortAllOnDrop(Vec<tokio::task::JoinHandle<()>>);
    impl Drop for AbortAllOnDrop {
        fn drop(&mut self) {
            for h in &self.0 {
                h.abort();
            }
        }
    }
    let _rooms_guard = AbortAllOnDrop(handles);

    // Each room task retries forever internally, so this never resolves on
    // its own - only ever by this whole task being aborted from outside,
    // at which point the guard above cleans up every room task too.
    std::future::pending::<()>().await;
    Ok(())
}

/// Kicks off a fresh account's first login in the background - unlike
/// spawn() (used for accounts that already have saved credentials and are
/// reconnecting), this is what addSneedChatAccount calls, since Tor
/// bootstrap + login + a possible proof-of-work solve can take anywhere
/// from instant to over a minute and must not block the RPC response.
/// Progress/result arrive via sneedChatLoginStatus/sneedChatLoginResult
/// events tagged with `login_id`, mirroring backend::discord::start_qr_login.
pub fn start_login(state: AppState, login_id: String, config: SneedChatAccountConfig) {
    tokio::spawn(async move {
        let result = std::panic::AssertUnwindSafe(try_login(&state, &login_id, &config)).catch_unwind().await;
        let error = match result {
            Ok(Ok(())) => return,
            Ok(Err(e)) => format!("{e:#}"),
            Err(_) => "internal error (see nobilis logs)".to_string(),
        };
        tracing::warn!("sneedchat login[{login_id}]: {error}");
        state.events.emit("sneedChatLoginResult", serde_json::json!({ "loginId": login_id, "success": false, "error": error }));
    });
}

pub(super) async fn try_login(state: &AppState, login_id: &str, config: &SneedChatAccountConfig) -> Result<()> {
    let emit_progress = |msg: &str| {
        state.events.emit("sneedChatLoginStatus", serde_json::json!({ "loginId": login_id, "detail": msg }));
    };

    // Saved before anything can fail, not after everything has succeeded.
    //
    // This used to persist only on the far side of a successful login, so a
    // wrong password - or a Tor bootstrap that never completed - threw the
    // account away along with everything typed into the form. The next
    // attempt started from an empty form, which is the worst moment to ask
    // somebody to retype a password: they have just been told it might be
    // wrong.
    //
    // The account id is derived from the username, so re-adding the same
    // account updates it in place rather than accumulating duplicates, and a
    // corrected password simply overwrites the stored one. What lands here is
    // an account that exists, holds what was entered, and is disconnected -
    // which is exactly the state Connect knows how to retry.
    state.accounts.add_sneedchat(config.clone())?;

    let transport = match config.tor_mode.as_str() {
        "proxy" => {
            let proxy = config.proxy.as_deref().ok_or_else(|| anyhow!("tor_mode is \"proxy\" but no proxy URL is configured"))?;
            Transport::socks_from_url(proxy)?
        }
        _ => {
            let client = state.tor.get_or_bootstrap(emit_progress).await.context("bootstrapping Tor")?;
            Transport::Tor(client)
        }
    };

    let base = format!("https://{}", config.host);
    let session = Session::new(transport, base, DEFAULT_USER_AGENT.to_string());
    let two_factor = match &config.totp_secret {
        Some(secret) => TwoFactor::Totp(totp::decode_secret(secret).context("TOTP secret is not valid base32")?),
        None => TwoFactor::None,
    };
    let creds = Credentials { username: config.username.clone(), password: config.password.clone() };

    emit_progress("logging in...");
    restore_session(&session, config);
    session.ensure_authenticated(&creds, &two_factor).await.map_err(no_way_in).context("logging in")?;

    let mut saved_config = config.clone();
    saved_config.cookies = session.http.jar.snapshot();
    saved_config.user_id = session.user_id();
    let saved = state.accounts.add_sneedchat(saved_config)?;
    let account = crate::accounts::sneedchat_account_to_json(&saved, "connecting");
    spawn(state.clone(), saved);

    state.events.emit("sneedChatLoginResult", serde_json::json!({ "loginId": login_id, "success": true, "account": account }));
    Ok(())
}
