//! Where each kind of stanza goes.

use super::session::Handle;
use super::{iq, messages, ns, roster};
use crate::state::AppState;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::Stanza;

pub async fn stanza(state: &AppState, handle: &Handle, stanza: Stanza) {
    tracing::trace!("xmpp[{}]: <- {stanza:?}", handle.account_id);
    match stanza {
        Stanza::Message(m) => messages::handle(state, handle, m).await,
        Stanza::Presence(p) => roster::handle_presence(state, handle, &p).await,
        Stanza::Iq(request) => match &request {
            Iq::Result { .. } | Iq::Error { .. } => {
                if !handle.session().resolve(request) {
                    tracing::debug!("xmpp[{}]: an answer nobody was waiting for", handle.account_id);
                }
            }
            // A roster push. Only the account's own server (no sender, or the account itself) may change the roster.
            Iq::Set { from, id, payload, .. } if payload.is("query", ns::ROSTER) => {
                let genuine = from.as_ref().is_none_or(|f| f.to_bare() == handle.own());
                if genuine {
                    roster::push(state, handle, payload);
                    handle.send(Stanza::Iq(Iq::Result { from: None, to: None, id: id.clone(), payload: None })).await;
                } else if let Some(reply) = iq::answer(&request) {
                    handle.send(Stanza::Iq(reply)).await;
                }
            }
            _ => {
                if let Some(reply) = iq::answer(&request) {
                    handle.send(Stanza::Iq(reply)).await;
                }
            }
        },
    }
}
