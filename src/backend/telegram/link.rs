//! One connection to Telegram: the library's sender pool, its client and the stream of what Telegram sends.
//!
//! Everything that talks to Telegram - a sign-in, a connected account - opens one of these and closes it when it
//! is done. The pool connects to each datacenter on demand, and a closed one drops its connections gracefully.

use super::session::FileSession;
use crate::state::AppState;
use anyhow::{Context, Result};
use grammers_client::Client;
use grammers_mtsender::{SenderPool, SenderPoolFatHandle};
use grammers_session::updates::UpdatesLike;
use tokio::sync::mpsc::Receiver;
use std::sync::Arc;

pub struct Link {
    pub client: Client,
    pub session: Arc<FileSession>,
    /// What Telegram sends unasked, to be handed to `Client::stream_updates` once (taken by whoever reads it).
    pub updates: Option<Receiver<UpdatesLike>>,
    handle: SenderPoolFatHandle,
    runner: tokio::task::JoinHandle<()>,
}

/// Opens a connection under `key` (the account's id, or a sign-in's pending key), by the account's route if it has
/// one: Tor or the proxy, as a SOCKS5 address the library connects through.
pub async fn open(state: &AppState, key: &str, api_id: i32, session: Arc<FileSession>) -> Result<Link> {
    let router = crate::net::route::router();
    let proxy = if router.routed(key) {
        let (host, port) = router.ready(|msg| state.runtime.report_progress(state, key, msg)).await.context("starting Tor or reaching the proxy")?;
        Some(format!("socks5://{host}:{port}"))
    } else {
        None
    };
    let SenderPool { runner, handle, updates } = SenderPool::with_configuration(session.clone(), api_id, super::connection_params(proxy.as_deref()));
    let client = Client::new(handle.clone());
    let runner = tokio::spawn(runner.run());
    Ok(Link { client, session, updates: Some(updates), handle, runner })
}

impl Link {
    /// Closes the connections, and writes the session if anything changed.
    pub async fn close(self) {
        self.handle.quit();
        let _ = self.runner.await;
        if let Err(e) = self.session.save_if_dirty() {
            tracing::warn!("telegram: saving the session: {e:#}");
        }
    }
}
