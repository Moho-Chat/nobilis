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

    state.runtime.report_progress(state, account_id, "signing in...");
    restore_session(&session, config);
    if config.cookies.is_empty() {
        bail!("{}", auth::SIGN_IN_AGAIN);
    }
    session.ensure_signed_in().await.context("signing in")?;
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
    super::chat::run_connection(state, account_id, &session, &host).await
}

