//! How an account's library reaches Signal: directly, or through the account's route (moho's Tor, or the SOCKS5 proxy
//! in the network settings), and under moho's own name.
//!
//! The library makes every connection - REST, the attachment servers, both websockets - through one HTTP client per
//! `PushService`, built deep inside presage. The patched copy of libsignal-service-rs in `vendor/` reads a per-thread
//! setting when it builds that client, and each account's library runs on a thread of its own (see `local`), so the
//! setting is made here at the start of every go at connecting, linking or unlinking, before the library is touched.

use super::local;
use presage::libsignal_service::push_service::network::{self, Network};

/// What Signal is told this client is.
pub const USER_AGENT: &str = concat!("moho/", env!("CARGO_PKG_VERSION"));

/// Makes the route ready if `routed`, and sets this thread's connections to go through it. An error is in words; a
/// routed account never goes out directly because the route was not ready.
pub async fn prepare(routed: bool) -> Result<(), String> {
    let proxy = if routed {
        let main = local::main_runtime().ok_or_else(|| "the route could not be started from here".to_string())?;
        // Tor is started on the daemon's runtime, where it lives; this thread only waits for it.
        let ready = main.spawn(async { crate::net::route::router().ready(|_| {}).await.map(|_| ()).map_err(|e| format!("{e:#}")) });
        ready.await.map_err(|e| e.to_string())??;
        Some(crate::net::route::router().proxy_if(true).unwrap_or_else(|| network::NOWHERE.to_string()))
    } else {
        None
    };
    // Link previews start off; the phone's own setting turns them on (`allow_link_previews`) once it is known.
    network::set_for_this_thread(Network { proxy, user_agent: Some(USER_AGENT.to_string()), link_previews: false });
    Ok(())
}

/// Whether a message sent from this thread may have a preview built for its first link (by fetching the page, through
/// the route): what the phone's "Generate link previews" says. Not known is no.
pub fn allow_link_previews(allowed: bool) {
    let mut now = network::for_this_thread();
    now.link_previews = allowed;
    network::set_for_this_thread(now);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_direct_thread_is_told_the_name_and_no_proxy() {
        let got = local::run("test", || async {
            prepare(false).await.unwrap();
            network::for_this_thread()
        })
        .await
        .unwrap();
        assert_eq!((got.proxy, got.user_agent.as_deref(), got.link_previews), (None, Some(USER_AGENT), false));
        assert!(USER_AGENT.starts_with("moho/"));
    }

    #[tokio::test]
    async fn previews_follow_the_phones_setting_and_keep_the_route() {
        let got = local::run("test", || async {
            network::set_for_this_thread(Network { proxy: Some("socks5h://127.0.0.1:1".into()), user_agent: None, link_previews: false });
            allow_link_previews(true);
            network::for_this_thread()
        })
        .await
        .unwrap();
        assert_eq!((got.proxy.as_deref(), got.link_previews), (Some("socks5h://127.0.0.1:1"), true));
    }

    #[tokio::test]
    async fn the_setting_belongs_to_the_thread_that_made_it() {
        local::run("test", || async { network::set_for_this_thread(Network { proxy: Some("socks5h://127.0.0.1:1".into()), user_agent: None, link_previews: true }) }).await.unwrap();
        let other = local::run("test", || async { network::for_this_thread() }).await.unwrap();
        assert_eq!(other.proxy, None);
    }
}
