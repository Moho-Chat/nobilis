//! The archive (XEP-0313): what was said while this client was not there, and what was said before the oldest thing it
//! has.
//!
//! A query goes to the archive's owner (the account's own server for one-to-one conversations, a room for a room) and
//! the answer is a run of `<message>` results followed by an iq saying whether that was all. Paging is by the archive's
//! own ids (XEP-0059), never by time: two messages in the same second are two messages. Everything fetched goes through
//! the same path as a live message (`messages::handle_one`), whose rule that an id already held is not held twice is
//! what makes an overlapping page harmless.

use super::messages::{self, Origin};
use super::xml::Attr;
use super::session::{Archived, Failure, Handle};
use super::{ns, xml};
use crate::model::Buffer;
use crate::state::AppState;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;

/// How many results a page asks for.
pub const PAGE: u32 = 100;
/// The most pages a catch-up follows: a client away for a year has not asked for a year of chat all at once.
pub const MAX_PAGES: usize = 30;

/// What to ask for.
#[derive(Debug, Clone, Default)]
pub struct Query {
    /// Only messages with this person (or, for a room, from this occupant).
    pub with: Option<String>,
    pub start: Option<i64>,
    pub end: Option<i64>,
    /// Page backwards from the end (`Some(None)`) or from this id (`Some(Some(id))`).
    pub before: Option<Option<String>>,
    /// Page forwards from this id.
    pub after: Option<String>,
    pub max: u32,
}

/// One page of the answer.
#[derive(Debug, Default)]
pub struct Page {
    pub items: Vec<Archived>,
    /// The archive has nothing more in this direction.
    pub complete: bool,
    pub last: Option<String>,
}

/// The iq for a query.
pub fn build(query_id: &str, q: &Query) -> Element {
    let mut form = Element::builder("x", ns::DATA).at("type", "submit").build();
    let mut field = |var: &str, ty: Option<&str>, value: &str| {
        let mut f = Element::builder("field", ns::DATA).at("var", var);
        if let Some(ty) = ty {
            f = f.at("type", ty);
        }
        let mut f = f.build();
        f.append_child(Element::builder("value", ns::DATA).append(value.to_string()).build());
        form.append_child(f);
    };
    field("FORM_TYPE", Some("hidden"), ns::MAM);
    if let Some(with) = &q.with {
        field("with", None, with);
    }
    if let Some(start) = q.start {
        field("start", None, &xml::stamp(start));
    }
    if let Some(end) = q.end {
        field("end", None, &xml::stamp(end));
    }
    let mut set = Element::builder("set", ns::RSM).append(Element::builder("max", ns::RSM).append(q.max.max(1).to_string()).build()).build();
    match (&q.before, &q.after) {
        (Some(before), _) => {
            set.append_child(Element::builder("before", ns::RSM).append(before.clone().unwrap_or_default()).build());
        }
        (None, Some(after)) => {
            set.append_child(Element::builder("after", ns::RSM).append(after.clone()).build());
        }
        _ => {}
    }
    Element::builder("query", ns::MAM).at("queryid", query_id).append(form).append(set).build()
}

/// Reads the closing iq: whether it was all, and the first and last ids.
pub fn read_fin(fin: &Element) -> (bool, Option<String>, Option<String>) {
    let complete = fin.attr("complete") == Some("true");
    let set = xml::child(fin, "set", ns::RSM);
    let first = set.and_then(|s| xml::child_text(s, "first", ns::RSM)).filter(|s| !s.is_empty());
    let last = set.and_then(|s| xml::child_text(s, "last", ns::RSM)).filter(|s| !s.is_empty());
    (complete, first, last)
}

/// Asks an archive for a page.
pub async fn run(handle: &Handle, to: Option<Jid>, q: &Query) -> Result<Page, Failure> {
    let query_id = xml::new_id();
    handle.session().archive.insert(query_id.clone(), Vec::new());
    let answer = handle.request(to, true, build(&query_id, q)).await;
    let items = handle.session().archive.remove(&query_id).unwrap_or_default();
    let fin = answer?;
    let (complete, _, last) = fin.as_ref().map(read_fin).unwrap_or((true, None, None));
    Ok(Page { items, complete, last })
}

/// Shows what a page held, as the conversations' messages. Returns how many were new.
pub async fn show(state: &AppState, handle: &Handle, page: Page) -> usize {
    let mut news = 0;
    for a in page.items {
        let id = a.archive_id.clone();
        messages::handle_one(state, handle, a.message, Origin::Archive { id, stamp: a.stamp }).await;
        news += 1;
    }
    news
}

/// After connecting: what was said while this client was away. Goes forward from the newest thing it has, or takes the
/// latest page for an account with nothing yet.
pub async fn catch_up(state: &AppState, handle: &Handle) {
    let newest = state.runtime.list_buffers().into_iter().filter(|b| b.account_id == handle.account_id && b.kind == "dm").map(|b| b.last_activity_ts).max().unwrap_or(0);
    if newest == 0 {
        match run(handle, None, &Query { before: Some(None), max: PAGE, ..Default::default() }).await {
            Ok(page) => {
                let n = show(state, handle, page).await;
                tracing::info!("xmpp[{}]: the archive's latest {n} messages", handle.account_id);
            }
            Err(e) => tracing::info!("xmpp[{}]: no archive to read: {e}", handle.account_id),
        }
        return;
    }
    // A little before it, in case a message was archived out of order; what is already held is skipped.
    let mut query = Query { start: Some(newest - 120), max: PAGE, ..Default::default() };
    let mut total = 0;
    for _ in 0..MAX_PAGES {
        match run(handle, None, &query).await {
            Ok(page) => {
                let (complete, last) = (page.complete, page.last.clone());
                total += show(state, handle, page).await;
                match (complete, last) {
                    (false, Some(last)) => query = Query { start: query.start, after: Some(last), max: PAGE, ..Default::default() },
                    _ => break,
                }
            }
            Err(e) => {
                tracing::info!("xmpp[{}]: catching up: {e}", handle.account_id);
                break;
            }
        }
    }
    tracing::info!("xmpp[{}]: caught up on {total} archived messages", handle.account_id);
}

/// After joining a room: what was said in it since this client last had anything, or its latest page if it has nothing.
pub async fn room_catch_up(state: &AppState, handle: &Handle, room: &BareJid) {
    tracing::debug!("xmpp[{}]: reading the archive of {room}", handle.account_id);
    let Some(buffer) = super::muc::find_buffer(state, handle, room) else { return };
    if !handle.session().rooms.get(room).is_some_and(|r| r.has_archive()) {
        tracing::debug!("xmpp[{}]: {room} keeps no archive", handle.account_id);
        return;
    }
    let to = Jid::from(room.clone());
    let mut query = if buffer.last_activity_ts == 0 { Query { before: Some(None), max: 50, ..Default::default() } } else { Query { start: Some(buffer.last_activity_ts - 120), max: PAGE, ..Default::default() } };
    for _ in 0..MAX_PAGES {
        match run(handle, Some(to.clone()), &query).await {
            Ok(page) => {
                let (complete, last) = (page.complete, page.last.clone());
                tracing::debug!("xmpp[{}]: {} archived messages of {room}, complete: {complete}", handle.account_id, page.items.len());
                show(state, handle, page).await;
                match (complete, last, query.before.is_some()) {
                    (false, Some(last), false) => query = Query { start: query.start, after: Some(last), max: PAGE, ..Default::default() },
                    _ => break,
                }
            }
            Err(e) => {
                tracing::debug!("xmpp[{}]: the archive of {room}: {e}", handle.account_id);
                break;
            }
        }
    }
}

/// Older messages for a conversation, from the archive: the page before the oldest this client holds.
pub async fn scrollback(state: &AppState, handle: &Handle, buffer: &Buffer, oldest_ts: i64) {
    let Some(peer) = buffer.remote_id.as_deref().and_then(|j| j.parse::<BareJid>().ok()) else { return };
    // A room is its own archive; a person's messages are in the account's.
    let (to, with) = if buffer.kind == "channel" { (Some(Jid::from(peer.clone())), None) } else { (None, Some(peer.to_string())) };
    let q = Query { with, end: (oldest_ts > 0).then_some(oldest_ts), before: Some(None), max: 50, ..Default::default() };
    match run(handle, to, &q).await {
        Ok(page) => {
            let n = show(state, handle, page).await;
            tracing::debug!("xmpp[{}]: {n} older messages with {peer}", handle.account_id);
        }
        Err(e) => tracing::debug!("xmpp[{}]: history with {peer}: {e}", handle.account_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_query_names_the_form_the_person_and_the_page_in_the_order_the_specification_gives() {
        let q = build("q1", &Query { with: Some("bob@example.org".into()), start: Some(1794225600), before: Some(None), max: 50, ..Default::default() });
        assert_eq!((q.name(), q.ns().as_str(), q.attr("queryid")), ("query", ns::MAM, Some("q1")));
        let form = xml::child(&q, "x", ns::DATA).unwrap();
        let fields: Vec<(String, String)> = form.children().map(|f| (f.attr("var").unwrap().to_string(), f.children().next().unwrap().text())).collect();
        assert_eq!(fields, vec![("FORM_TYPE".into(), ns::MAM.into()), ("with".into(), "bob@example.org".into()), ("start".into(), "2026-11-09T12:00:00Z".into())]);
        let set = xml::child(&q, "set", ns::RSM).unwrap();
        assert_eq!(xml::child_text(set, "max", ns::RSM).as_deref(), Some("50"));
        assert!(xml::child(set, "before", ns::RSM).is_some());
    }

    #[test]
    fn paging_forward_names_the_last_id_seen() {
        let q = build("q", &Query { after: Some("A9".into()), max: 100, ..Default::default() });
        let set = xml::child(&q, "set", ns::RSM).unwrap();
        assert_eq!(xml::child_text(set, "after", ns::RSM).as_deref(), Some("A9"));
        assert!(xml::child(set, "before", ns::RSM).is_none());
    }

    #[test]
    fn the_closing_answer_says_whether_that_was_all_and_where_the_page_ended() {
        let fin = Element::from_str("<fin xmlns='urn:xmpp:mam:2' complete='true'><set xmlns='http://jabber.org/protocol/rsm'><first index='0'>F1</first><last>L9</last><count>42</count></set></fin>").unwrap();
        assert_eq!(read_fin(&fin), (true, Some("F1".into()), Some("L9".into())));
        let more = Element::from_str("<fin xmlns='urn:xmpp:mam:2'><set xmlns='http://jabber.org/protocol/rsm'/></fin>").unwrap();
        assert_eq!(read_fin(&more), (false, None, None));
    }
}
