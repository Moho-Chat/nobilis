//! What an account does once it is signed in, in the order RFC 6121 and the extensions need it: find out what the server
//! offers, ask for carbons, fetch the roster, only then say it is here (so the presence that comes back finds a roster
//! to be shown against), and read what was said while away.

use super::session::Handle;
use super::{caps, mam, ns, roster, xml};
use crate::state::AppState;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;

pub async fn run(state: &AppState, handle: &Handle) {
    let Ok(domain) = handle.own().domain().as_str().parse::<Jid>() else { return };
    server_features(handle, &domain).await;
    if handle.request(None, true, xml::empty("enable", ns::CARBONS)).await.is_err() {
        tracing::info!("xmpp[{}]: the server does not do carbons", handle.account_id);
    }
    super::blocking::load(handle).await;
    {
        // This device's OMEMO keys: opened (or made, the first time) and published, so people can write to it.
        let path = crate::default_data_dir().join("xmpp").join(format!("{}.omemo", handle.own()));
        let omemo = std::sync::Arc::new(super::omemo::Omemo::open(Some(path)));
        handle.session().omemo = Some(omemo.clone());
        super::omemo::publish_device(handle, &omemo).await;
    }
    roster::fetch(state, handle).await;
    let (show, status) = {
        let s = handle.session();
        (if s.own_show.is_empty() { "online".to_string() } else { s.own_show.clone() }, s.own_status.clone())
    };
    roster::set_own(handle, &show, &status).await;
    super::bookmarks::load(state, handle).await;
    // The window may be hidden already: a new session is told.
    if handle.session().inactive {
        handle.set_active(false);
    }
    mam::catch_up(state, handle).await;
    super::mds::load(state, handle).await;
    let _ = caps::NAME;
}

/// What the account's server says it can do.
async fn server_features(handle: &Handle, domain: &Jid) {
    match handle.request(Some(domain.clone()), false, Element::builder("query", ns::DISCO_INFO).build()).await {
        Ok(Some(q)) => {
            let features: Vec<String> = q.children().filter(|c| c.name() == "feature").filter_map(|c| c.attr("var").map(str::to_string)).collect();
            tracing::info!("xmpp[{}]: the server offers {} features", handle.account_id, features.len());
            handle.session().server_features = features.into_iter().collect();
        }
        Ok(None) => {}
        Err(e) => tracing::warn!("xmpp[{}]: what the server offers: {e}", handle.account_id),
    }
}
