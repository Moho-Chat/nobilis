//! Where each kind of stanza goes.

use super::session::Handle;
use super::{iq, messages, ns, roster};
use crate::state::AppState;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::Stanza;

pub async fn stanza(state: &AppState, handle: &Handle, stanza: Stanza) {
    tracing::trace!("xmpp[{}]: <- {stanza:?}", handle.account_id);
    match stanza {
        Stanza::Message(m) => {
            // A change another client made to the bookmarks.
            if let Some(event) = m.payloads.iter().find(|p| p.is("event", ns::PUBSUB_EVENT)) {
                let from = m.from.as_ref().map(|f| f.to_bare());
                // Each of these asks the server something, and the answer is read by this very task: so they are done on
                // tasks of their own, never waited for here.
                if from.as_ref() == Some(&handle.own()) {
                    let (s, h, e) = (state.clone(), handle.clone(), event.clone());
                    tokio::spawn(async move { super::bookmarks::notified(&s, &h, &e).await });
                }
                // What another device of this account has read.
                if from.as_ref() == Some(&handle.own()) {
                    if let Some(item) = event.children().find(|c| c.name() == "items" && c.attr("node") == Some(super::mds::NODE)).and_then(|i| i.children().find(|c| c.name() == "item")) {
                        super::mds::apply(state, handle, item);
                    }
                }
                // Somebody's OMEMO devices changed.
                if let (Some(from), Some(list)) = (from.clone(), event.children().find(|c| c.name() == "items" && c.attr("node") == Some(super::omemo::DEVICELIST_NODE)).and_then(|i| i.children().find(|c| c.name() == "item")).and_then(|i| i.children().next())) {
                    let omemo = handle.session().omemo.clone();
                    if let Some(o) = omemo {
                        o.store().state.devices.insert(from.to_string(), super::omemo::parse_devicelist(list));
                        o.store().save();
                    }
                }
                if let (Some(from), Some(items)) = (from, event.children().find(|c| c.name() == "items" && c.attr("node") == Some(ns::AVATAR_METADATA))) {
                    handle.session().avatar_source.insert(from.clone(), "pep".into());
                    let (s, h, items) = (state.clone(), handle.clone(), items.clone());
                    tokio::spawn(async move { super::avatars::metadata_event(&s, &h, &from, &items).await });
                }
                return;
            }
            messages::handle(state, handle, m).await
        }
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
            // A change to the block list, made here or by another client of the account.
            Iq::Set { from, id, payload, .. } if (payload.is("block", super::blocking::BLOCKING) || payload.is("unblock", super::blocking::BLOCKING)) && from.as_ref().is_none_or(|f| f.to_bare() == handle.own()) => {
                super::blocking::pushed(handle, payload);
                handle.send(Stanza::Iq(Iq::Result { from: None, to: None, id: id.clone(), payload: None })).await;
            }
            // Somebody calling.
            Iq::Set { from, id, payload, .. } if payload.is("jingle", super::calls::jingle::JINGLE) => super::calls::jingle_iq(state, handle, from.clone(), id, payload).await,
            _ => {
                if let Some(reply) = iq::answer(&request) {
                    handle.send(Stanza::Iq(reply)).await;
                }
            }
        },
    }
}
