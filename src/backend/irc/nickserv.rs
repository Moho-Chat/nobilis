use irc::client::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

/// Replicates daemon/nobilis/actions.c's
/// nobilis_await_nickserv_then_autojoin()/nobilis_notice_possible_nickserv_reply():
/// NickServ IDENTIFY has no ACK we can wait on the way SASL does, so after
/// sending IDENTIFY, arm an 8s fallback timer and fire autojoin either when
/// a message from NickServ containing "identified" arrives, or the timer
/// fires - whichever is first. Confirmed live against Libera.Chat in the
/// original C implementation: NickServ's real reply arrived a full ~6s
/// after IDENTIFY, not the ~1.5s a first cut assumed - keep the 8s margin.
pub struct NickservWait {
    notify: Arc<Notify>,
    fired: Arc<AtomicBool>,
}

impl NickservWait {
    /// Arms the wait and spawns the fallback timer + autojoin action.
    /// `autojoin` runs at most once, whichever of (NickServ reply, timeout)
    /// happens first.
    pub fn arm(sender: Sender, autojoin_csv: String) -> Self {
        let notify = Arc::new(Notify::new());
        let fired = Arc::new(AtomicBool::new(false));

        let notify2 = notify.clone();
        let fired2 = fired.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(8000)) => {}
                _ = notify2.notified() => {}
            }
            if !fired2.swap(true, Ordering::SeqCst) {
                crate::backend::irc::send_autojoin(&sender, &autojoin_csv);
            }
        });

        Self { notify, fired }
    }

    /// Call for every inbound PRIVMSG/NOTICE while a wait is pending -
    /// no-ops unless `from` is NickServ and `body` looks like a success
    /// reply. Returns true if this call resolved the wait.
    pub fn notice(&self, from: &str, body: &str) -> bool {
        if self.fired.load(Ordering::SeqCst) {
            return false;
        }
        if from.eq_ignore_ascii_case("nickserv") && body.to_lowercase().contains("identified") {
            self.notify.notify_one();
            true
        } else {
            false
        }
    }
}
