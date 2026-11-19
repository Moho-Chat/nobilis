//! Signing in to the forum and keeping the account up.
//!
//! The chat is a feature of a forum, so a session here is a forum session -
//! obtained with a password, restored from cookies, or handed over by a
//! browser - and everything else in this folder depends on having one.

use super::*;

/// Kiwi Farms' hidden service, used by an account set to connect through
/// Tor. Without Tor an account connects to `kiwifarms.st` directly - see
/// `SneedChatAccountConfig::site_host`.
pub const DEFAULT_ONION: &str = "kiwifarmsaaf4t2h7gc3dfc5ojhmqruw2nit3uejrpiagrxeuxiyxcyd.onion";

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; rv:128.0) Gecko/20100101 Firefox/128.0";

/// Spawns the background task that keeps a Sneedchat account's connection
/// alive - the real-time equivalent of backend::discord::spawn. Called both
/// right after a fresh addSneedChatAccount and, from main.rs, for every
/// saved account on daemon startup.
pub fn spawn(state: AppState, config: SneedChatAccountConfig) {
    let account_id = config.account_id();
    crate::net::route::router().mark(&account_id, config.use_tor);
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
                (format!("{e:#}"), config.use_tor && crate::net::tor::is_tor_failure(&e))
            }
            Err(_) => {
                tracing::error!("sneedchat[{account_id}]: connection task panicked");
                ("internal error (see nobilis logs)".to_string(), false)
            }
        };
        state.runtime.set_conn_state(state, account_id, ConnState::Connecting, None);

        // Signing in failed, so no room got as far as trying. Asked which
        // part is down, so each room can say - unless the forum answers, in
        // which case this was a refusal (a password, a captcha) rather than an
        // outage, and the account's own message says that better.
        let outage = match transport_for(state, config, |_| {}).await {
            Ok(transport) => super::health::diagnose(account_id, &transport, &config.site_host()).await,
            Err(e) => super::health::Outage::Tor(format!("Tor isn't connecting: {e:#}")),
        };
        if outage != super::health::Outage::Chat {
            let host = config.site_host();
            detail = outage.describe(&host);
            for room in &config.rooms {
                let buffer_id = state.runtime.ensure_buffer(state, account_id, &super::rooms::room_buffer_name(&room.name), "channel").id;
                state.runtime.set_buffer_link(state, &buffer_id, Some(outage.link(&host)));
            }
        }

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

        let wait = state.runtime.next_retry_delay(account_id, &mut delay, RECONNECT_INITIAL_DELAY, RECONNECT_MAX_DELAY);
        state.runtime.report_retry(state, account_id, &detail, wait);
        tokio::time::sleep(wait).await;
    }
}

/// How this account reaches the forum.
///
/// Directly, unless the account is set to use Tor - and only then is Tor
/// touched at all: an account on the open internet never bootstraps it. With
/// Tor, the daemon-wide choice between the embedded client and an external
/// proxy applies.
pub(super) async fn transport_for(state: &AppState, config: &SneedChatAccountConfig, on_progress: impl FnOnce(&str)) -> Result<Transport> {
    let router = crate::net::route::router();
    if !config.use_tor && !router.tunnel_all() {
        return Ok(Transport::Direct);
    }
    let settings = router.settings();
    if settings.tor_mode == "proxy" {
        let proxy = settings.proxy.as_deref().ok_or_else(|| anyhow!("the network settings name a proxy but give no address"))?;
        return Transport::socks_from_url(proxy);
    }
    if config.use_tor {
        // Straight to moho's own Tor client: the onion address needs no relay.
        let client = state.tor.get_or_bootstrap(on_progress).await.context("bootstrapping Tor")?;
        return Ok(Transport::Tor(client));
    }
    // Everything routed, this account on the open internet: through Tor to
    // kiwifarms.st, by way of the relay everything else uses.
    let (host, port) = router.ready(on_progress).await?;
    Ok(Transport::Socks { host, port })
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

/// How this account's requests reach the site through the web helper:
/// directly, or by the SOCKS5 address Tor or the proxy listens on.
///
/// Only the helper's route: the site's gate admits Chromium's network stack
/// and nothing else, so even a Tor account's traffic is Chromium's, sent
/// through the same relay everything else routed uses.
pub(super) async fn browser_route(state: &AppState, config: &SneedChatAccountConfig, account_id: &str) -> Result<http::BrowserRoute> {
    if !crate::net::webhelper::available() {
        bail!("Sneedchat needs the moho app open: the site admits only a browser, and moho's own browser engine carries the connection");
    }
    let router = crate::net::route::router();
    if !config.use_tor && !router.tunnel_all() {
        return Ok(http::BrowserRoute { proxy: None });
    }
    let progress = |msg: &str| state.runtime.report_progress(state, account_id, msg);
    let (host, port) = router.ready(progress).await.context("starting Tor")?;
    Ok(http::BrowserRoute { proxy: Some(format!("socks5://{host}:{port}")) })
}

/// Signs in, then keeps the account's chat connection up. Returns (bails)
/// only if signing in fails; under normal operation the connection retries on
/// its own and this never returns.
pub(super) async fn run(state: &AppState, config: &SneedChatAccountConfig, account_id: &str) -> Result<()> {
    let route = browser_route(state, config, account_id).await?;
    let host = config.site_host();
    let session = Session::via_browser(route, format!("https://{host}"));
    let two_factor = match &config.totp_secret {
        Some(secret) => TwoFactor::Totp(totp::decode_secret(secret).context("stored TOTP secret is not valid base32")?),
        None => TwoFactor::None,
    };
    let creds = Credentials { username: config.username.clone(), password: config.password.clone() };

    state.runtime.report_progress(state, account_id, "logging in...");
    restore_session(&session, config);
    session.ensure_authenticated(&creds, &two_factor).await.map_err(no_way_in).context("logging in")?;
    if config.use_tor {
        state.tor.note_success().await;
    }
    remember_session(state, account_id, &session);
    super::media::note_client(account_id, &session.http, &host);
    if let Some(uid) = session.user_id() {
        let _ = state.accounts.set_sneedchat_user_id(account_id, uid);
    }

    let me = config.display_name.clone().filter(|n| !n.is_empty()).unwrap_or_else(|| config.username.clone());
    state.runtime.set_own_identity(account_id, &me);
    state.runtime.set_conn_state(state, account_id, ConnState::Connected, None);

    struct Forget(String);
    impl Drop for Forget {
        fn drop(&mut self) {
            super::chat::forget_chat(&self.0);
        }
    }
    let _forget = Forget(account_id.to_string());
    super::chat::run_connection(state, account_id, &session, &host, &creds, &two_factor).await
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

    emit_progress("starting...");
    let route = browser_route(state, config, &config.account_id()).await?;
    let session = Session::via_browser(route, format!("https://{}", config.site_host()));
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
