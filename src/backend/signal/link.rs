//! Adding an account: linking moho to a Signal account as a secondary device, the way Signal Desktop is linked.
//!
//! Nothing is typed. Moho asks Signal for a provisioning address, shows it as a QR code, and the person scans it with
//! Signal on their phone (Settings, Linked devices, Link new device). The phone then hands over the account's keys and
//! this device is registered under a name the phone's list shows. Moho never registers a number and never answers a
//! captcha: an account that does not exist yet is made in Signal's own app first.
//!
//! Like Telegram's and Steam's sign-ins it is a conversation that takes a while, so it runs as its own task and talks
//! to the window through events:
//!
//! - `signalLinkStatus` - what it is doing, for a line of text
//! - `signalLinkQr` - a picture to scan; sent again with a new one when the first is not scanned in time
//! - `signalLinkResult` - done, with the account, or the reason it failed
//!
//! and the window may stop it with `cancelSignalLink`.
//!
//! The library's futures cannot leave the thread they start on (see `local`), so the link runs on a thread of its own.
//! Its store is a file named for the link until the phone has said which account it is, and renamed to the account's
//! then.

use super::local;
use super::store::{self, OnNewIdentity, SignalStore};
use crate::accounts::SignalAccountConfig;
use crate::state::AppState;
use anyhow::{anyhow, bail, Context, Result};
use futures::future;
use presage::{libsignal_service::configuration::SignalServers, Manager};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;

/// How long one code is offered before a new one replaces it.
const CODE_LIFE: Duration = Duration::from_secs(120);

/// How long the whole link may take before it gives up: a form left open should not keep a thread.
const PATIENCE: Duration = Duration::from_secs(10 * 60);

/// The links that can still be stopped, by id.
fn waiting() -> &'static Mutex<HashMap<String, oneshot::Sender<()>>> {
    static WAITING: std::sync::OnceLock<Mutex<HashMap<String, oneshot::Sender<()>>>> = std::sync::OnceLock::new();
    WAITING.get_or_init(Default::default)
}

/// Stops a link that is waiting, because the form was closed.
pub fn cancel(login_id: &str) {
    if let Some(stop) = waiting().lock().unwrap().remove(login_id) {
        let _ = stop.send(());
    }
}

/// The name the phone's list of linked devices shows for this one.
pub fn device_name() -> String {
    format!("moho ({})", std::env::consts::OS)
}

/// What the phone said about the account, and nothing else: the library's own manager stays on its thread.
#[derive(Debug)]
struct Linked {
    aci: String,
    pni: String,
    number: String,
    device_id: u32,
    device_name: String,
    /// Whether the phone offered to send the account's message history (see Round 10 of the plan).
    history_offered: bool,
}

/// Starts a link. Answers at once; the rest is events.
pub fn start(state: AppState, login_id: String) {
    let (stop_tx, stop_rx) = oneshot::channel();
    waiting().lock().unwrap().insert(login_id.clone(), stop_tx);
    let events = state.clone();
    let id = login_id.clone();
    let done = local::run("signal-link", move || flow(events, id, stop_rx));
    tokio::spawn(async move {
        let outcome = done.await.unwrap_or_else(|_| Err(anyhow!("the link stopped unexpectedly")));
        waiting().lock().unwrap().remove(&login_id);
        match outcome {
            Ok(Some(config)) => {
                let account = crate::accounts::signal_account_to_json(&config, "connecting");
                state.events.emit("signalLinkResult", json!({ "loginId": login_id, "success": true, "account": account }));
            }
            // The form was closed; nobody is waiting for an answer.
            Ok(None) => {}
            Err(e) => {
                let error = format!("{e:#}");
                tracing::info!("signal: link {login_id}: {error}");
                state.events.emit("signalLinkResult", json!({ "loginId": login_id, "success": false, "error": error }));
            }
        }
    });
}

fn status(state: &AppState, login_id: &str, text: &str) {
    state.events.emit("signalLinkStatus", json!({ "loginId": login_id, "text": text }));
}

fn qr_path(login_id: &str) -> std::path::PathBuf {
    crate::media_cache::transient_dir().join(format!("moho-signal-qr-{}.png", login_id.replace(|c: char| !c.is_ascii_alphanumeric(), "-")))
}

/// The whole link, on its thread. `None` when the person stopped it.
async fn flow(state: AppState, login_id: String, mut stop: oneshot::Receiver<()>) -> Result<Option<SignalAccountConfig>> {
    // Through the route the form asked for, if any; the account made from this link keeps it.
    let route = crate::net::route::pending_key("signal");
    let routed = crate::net::route::router().routed(&route);
    super::network::prepare(routed).await.map_err(|e| anyhow!("the route is not ready: {e}"))?;
    let path = store::path_for_link(&login_id);
    let outcome = linking(&state, &login_id, &path, &mut stop).await;
    match outcome {
        Ok(Some(linked)) => {
            let config = SignalAccountConfig {
                aci: linked.aci,
                pni: linked.pni,
                number: linked.number,
                device_id: linked.device_id,
                device_name: linked.device_name,
                use_tor: crate::net::route::router().wanted(&route),
                ..Default::default()
            };
            tracing::info!("signal: linked as device {} (history offered: {})", config.device_id, linked.history_offered);
            // The store is the account's now, under its own name; the account is written after the keys are safe.
            store::relocate(&path, &store::path_for(&config.aci)).context("saving the account's keys")?;
            let saved = state.accounts.add_signal(config)?;
            let _ = std::fs::remove_file(qr_path(&login_id));
            Ok(Some(saved))
        }
        other => {
            store::delete_files(&path);
            let _ = std::fs::remove_file(qr_path(&login_id));
            other.map(|_| None)
        }
    }
}

/// Shows a code, waits for it to be scanned, and shows a new one when it is not. `None` when stopped.
async fn linking(state: &AppState, login_id: &str, path: &std::path::Path, stop: &mut oneshot::Receiver<()>) -> Result<Option<Linked>> {
    let began = tokio::time::Instant::now();
    let name = device_name();
    status(state, login_id, "Asking Signal for a code to scan");
    // Attempts in a row that failed before Signal gave a code: a dead Tor circuit or a dropped connection is worth
    // another go, a refusal three times over is not.
    let mut unreached = 0;
    loop {
        if began.elapsed() > PATIENCE {
            bail!("The code was not scanned in time. Start again to get a new one");
        }
        // Each attempt starts from an empty store: the library clears the registration itself, and a half-finished
        // one must not leave keys behind for the next.
        store::delete_files(path);
        let store = SignalStore::open(path, OnNewIdentity::Trust).context("opening the account's store")?;
        let (url_tx, url_rx) = futures::channel::oneshot::channel();
        let got_code = std::cell::Cell::new(false);
        let link = Manager::link_secondary_device(store.clone(), SignalServers::Production, name.clone(), url_tx);
        let show = async {
            if let Ok(url) = url_rx.await {
                got_code.set(true);
                let shown = qr_path(login_id);
                match super::super::steam::login::write_qr(url.as_str(), &shown) {
                    Ok(()) => state.events.emit("signalLinkQr", json!({ "loginId": login_id, "path": shown.to_string_lossy(), "expiresInSeconds": CODE_LIFE.as_secs() })),
                    Err(e) => tracing::warn!("signal: could not draw the code: {e}"),
                }
                status(state, login_id, "Scan the code with Signal on your phone: Settings, Linked devices, Link new device");
            }
        };
        let work = future::join(link, show);
        tokio::select! {
            (linked, ()) = work => {
                let manager = match linked {
                    Ok(manager) => manager,
                    Err(e) if !got_code.get() && unreached < 2 => {
                        unreached += 1;
                        tracing::info!("signal: no code from Signal ({e}); trying again");
                        status(state, login_id, "Could not reach Signal. Trying again");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    Err(e) => return Err(anyhow!("Signal did not finish linking: {e}")),
                };
                let data = manager.registration_data();
                let found = Linked {
                    aci: data.service_ids.aci.to_string(),
                    pni: data.service_ids.pni.to_string(),
                    number: data.phone_number.to_string(),
                    device_id: data.device_id.unwrap_or(0),
                    device_name: data.device_name().unwrap_or(&name).to_string(),
                    history_offered: data.ephemeral_backup_key().is_some(),
                };
                drop(manager);
                store.checkpoint().context("writing the account's keys")?;
                return Ok(Some(found));
            }
            _ = tokio::time::sleep(CODE_LIFE) => {
                // Not scanned: the work is dropped (which closes the provisioning socket) and a fresh code is made.
                status(state, login_id, "The code ran out. Making a new one");
            }
            _ = &mut *stop => return Ok(None),
        }
    }
}
