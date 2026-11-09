//! Removing a Signal account takes this device off the account at Signal as well, so the phone's list of linked devices
//! does not keep a device that no longer exists, and then deletes the account's file here: its keys are of no use to
//! anything once the device is gone.
//!
//! Best effort: with no network the device is still removed here, and the person can remove it from the phone's list
//! (Settings, Linked devices). Said in the log, not in a dialog, because the account is gone from moho either way.

use super::local;
use super::store::{self, OnNewIdentity, SignalStore};
use presage::Manager;

/// Ends the device at Signal and deletes the account's store. Returns at once; the work is on a thread of its own.
pub fn start(aci: String) {
    let path = store::path_for(&aci);
    if !path.exists() {
        return;
    }
    let work = local::run("signal-unlink", move || async move {
        let ended = async {
            let store = SignalStore::open(&path, OnNewIdentity::Trust).map_err(|e| e.to_string())?;
            let manager = Manager::load_registered(store).await.map_err(|e| e.to_string())?;
            manager.unlink_self().await.map_err(|e| e.to_string())
        }
        .await;
        match ended {
            Ok(()) => tracing::info!("signal: this device was removed from the account"),
            Err(e) => tracing::info!("signal: could not remove this device from the account at Signal ({e}); it can be removed from the phone's linked devices"),
        }
        store::delete_files(&path);
    });
    // Nobody waits for it; the thread ends with the work.
    drop(work);
}
