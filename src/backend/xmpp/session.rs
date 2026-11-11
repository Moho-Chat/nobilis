//! What an account knows while it is connected, and the way to say things to its server.
//!
//! One [`Handle`] per connected account, kept in the runtime by account id. It is the sending half of the stream and a
//! shared [`Session`]: the roster, who is present, what the server supports, the requests waiting for an answer, and
//! the little tables that keep a repeated message from being shown twice.
//!
//! Everything that arrives is handled on the account's own task (`connect.rs`), which must not wait for an answer
//! from the server: an iq that needs one is made on a task of its own (`Handle::request`) and the answer is delivered
//! to it by [`Session::resolve`] when the stream's task sees it.

use super::xml;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::message::Message;
use tokio_xmpp::parsers::presence::Presence;
use tokio_xmpp::stanzastream::StanzaSender;
use tokio_xmpp::Stanza;

/// How long a request waits for the server before it is given up on.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// A person on the roster.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Contact {
    pub name: Option<String>,
    /// `none`, `to` (we see them), `from` (they see us) or `both`.
    pub subscription: String,
    /// A request to see their presence has been sent and not answered.
    pub ask: bool,
    pub groups: Vec<String>,
}

impl Contact {
    /// Whether we receive their presence.
    pub fn we_see_them(&self) -> bool {
        matches!(self.subscription.as_str(), "to" | "both")
    }
}

/// What a resource has said about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Presence_ {
    /// `online`, `away`, `xa`, `dnd` (the `show`, with `chat` and none both `online`).
    pub show: String,
    pub status: String,
    pub priority: i8,
}

#[derive(Default)]
pub struct Session {
    pub roster: BTreeMap<BareJid, Contact>,
    pub roster_version: Option<String>,
    /// Each contact's resources that are online.
    pub presence: HashMap<BareJid, BTreeMap<String, Presence_>>,
    /// What the account's own server says it can do (disco#info features of the domain).
    pub server_features: HashSet<String>,
    /// Requests waiting for their answer, by iq id.
    waiting: HashMap<String, oneshot::Sender<Iq>>,
    /// Archive results arriving for a query in progress, by its `queryid`.
    pub archive: HashMap<String, Vec<Archived>>,
    /// Ids of messages sent from here, so a copy that comes back (a carbon of our own, an archive's) is not a new one.
    pub sent: HashSet<String>,
    /// What each person has reacted with, by (conversation, message, who): reactions arrive as the whole set.
    pub reactions: HashMap<(String, String, String), Vec<String>>,
    /// The id of each message by the other ids it is known by (stanza id, archive id), to the id it is stored under.
    pub aliases: HashMap<String, String>,
    /// Receipts and markers we have already answered, by the message id.
    pub answered: HashSet<String>,
    /// Who we have said "composing" to, so "paused" is only said to them.
    pub typing_to: HashSet<String>,
    /// Whether the account has said it is present (an initial presence has gone out).
    pub present: bool,
    /// What the person chose to show: `online`, `away`, `xa`, `dnd`.
    pub own_show: String,
    pub own_status: String,
    /// People who have asked to see this account's presence and have not been answered: address to what they said.
    pub requests: BTreeMap<BareJid, String>,
    /// The last message each person sent, by their address: what a "displayed" marker is about.
    pub last_incoming: HashMap<String, String>,
    /// The rooms this account is in or is joining, by the room's address.
    pub rooms: HashMap<BareJid, super::muc::Room>,
    /// A message's id as the room stamped it (its stanza-id), by the id it is kept under: what reactions, retractions and
    /// replies in a room must name.
    pub room_ids: HashMap<String, String>,
    /// Invitations to rooms that have not been answered.
    pub room_invites: Vec<RoomInvite>,
    /// The server's file upload service (XEP-0363), once found, and the largest file it takes.
    pub upload: Option<(Jid, Option<u64>)>,
    /// The picture kept for each person, as the `file://` address the window loads, and where it was learned of (`pep`
    /// beats `vcard`, which is only asked for when PEP has not said).
    pub avatars: HashMap<BareJid, String>,
    pub avatar_source: HashMap<BareJid, String>,
}

/// Somebody asked this account into a room.
#[derive(Debug, Clone, PartialEq)]
pub struct RoomInvite {
    pub room: String,
    pub from: String,
    pub reason: String,
    pub password: Option<String>,
}

/// One message out of the archive, as it arrived.
#[derive(Debug, Clone)]
pub struct Archived {
    pub archive_id: String,
    pub message: Message,
    pub stamp: Option<i64>,
}

/// The connected account.
#[derive(Clone)]
pub struct Handle {
    pub account_id: String,
    /// The full JID the server bound.
    pub bound: Jid,
    sender: Arc<StanzaSender>,
    session: Arc<Mutex<Session>>,
}

impl Handle {
    pub fn new(account_id: &str, bound: Jid, sender: Arc<StanzaSender>, session: Arc<Mutex<Session>>) -> Self {
        Self { account_id: account_id.to_string(), bound, sender, session }
    }

    /// The account's own address, without the resource.
    pub fn own(&self) -> BareJid {
        self.bound.to_bare()
    }

    pub fn session(&self) -> MutexGuard<'_, Session> {
        self.session.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub async fn send(&self, stanza: Stanza) {
        let _ = self.sender.send(Box::new(stanza)).await;
    }

    pub async fn send_message(&self, message: Message) {
        self.send(Stanza::Message(message)).await;
    }

    pub async fn send_presence(&self, presence: Presence) {
        self.send(Stanza::Presence(presence)).await;
    }

    /// Asks the server (or another entity) something and waits for the answer: the payload of the result, `None` for
    /// an empty result, or what the error was.
    pub async fn request(&self, to: Option<Jid>, set: bool, payload: Element) -> Result<Option<Element>, Failure> {
        let id = xml::new_id();
        let (tx, rx) = oneshot::channel();
        self.session().waiting.insert(id.clone(), tx);
        let iq = if set { Iq::Set { from: None, to, id: id.clone(), payload } } else { Iq::Get { from: None, to, id: id.clone(), payload } };
        self.send(Stanza::Iq(iq)).await;
        let answer = tokio::time::timeout(REQUEST_TIMEOUT, rx).await;
        self.session().waiting.remove(&id);
        match answer {
            Err(_) => Err(Failure { condition: "remote-server-timeout".into(), text: "the server did not answer".into() }),
            Ok(Err(_)) => Err(Failure { condition: "disconnected".into(), text: "the connection ended".into() }),
            Ok(Ok(Iq::Result { payload, .. })) => Ok(payload),
            Ok(Ok(Iq::Error { error, .. })) => Err(Failure::from(&error)),
            Ok(Ok(_)) => Err(Failure { condition: "bad-request".into(), text: "an answer that is not an answer".into() }),
        }
    }
}

/// An error a request was answered with.
#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    pub condition: String,
    pub text: String,
}

impl Failure {
    pub fn from(error: &tokio_xmpp::parsers::stanza_error::StanzaError) -> Self {
        let text = error.texts.values().next().cloned().unwrap_or_default();
        Self { condition: format!("{:?}", error.defined_condition), text }
    }

    /// In words.
    pub fn words(&self) -> String {
        if self.text.is_empty() {
            self.condition.clone()
        } else {
            format!("{} ({})", self.text, self.condition)
        }
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.words())
    }
}

impl Session {
    /// Delivers an answer to whoever asked. `true` when somebody was waiting for it.
    pub fn resolve(&mut self, iq: Iq) -> bool {
        match self.waiting.remove(iq.id()) {
            Some(tx) => tx.send(iq).is_ok(),
            None => false,
        }
    }

    /// The most available resource of a contact, the way RFC 6121 ranks them: highest priority, then the one that
    /// is most available.
    pub fn best_presence(&self, jid: &BareJid) -> Option<&Presence_> {
        let rank = |p: &Presence_| (p.priority, match p.show.as_str() { "online" => 3, "away" => 2, "xa" => 1, _ => 0 });
        self.presence.get(jid)?.values().max_by_key(|p| rank(p))
    }

    /// What to call a person: their name on the roster, or their address.
    pub fn display_name(&self, jid: &BareJid) -> String {
        self.roster.get(jid).and_then(|c| c.name.clone()).filter(|n| !n.trim().is_empty()).unwrap_or_else(|| jid.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(show: &str, priority: i8) -> Presence_ {
        Presence_ { show: show.into(), status: String::new(), priority }
    }

    #[test]
    fn the_resource_with_the_highest_priority_speaks_for_a_contact_then_the_most_available() {
        let jid: BareJid = "bob@example.org".parse().unwrap();
        let mut s = Session::default();
        s.presence.entry(jid.clone()).or_default().insert("phone".into(), p("away", 5));
        s.presence.entry(jid.clone()).or_default().insert("desktop".into(), p("online", 0));
        assert_eq!(s.best_presence(&jid).unwrap().show, "away", "priority wins");
        s.presence.get_mut(&jid).unwrap().get_mut("desktop").unwrap().priority = 5;
        assert_eq!(s.best_presence(&jid).unwrap().show, "online", "then availability");
        assert!(s.best_presence(&"nobody@example.org".parse().unwrap()).is_none());
    }

    #[test]
    fn a_contact_is_called_by_its_roster_name_or_else_its_address() {
        let jid: BareJid = "bob@example.org".parse().unwrap();
        let mut s = Session::default();
        assert_eq!(s.display_name(&jid), "bob@example.org");
        s.roster.insert(jid.clone(), Contact { name: Some("Bob".into()), ..Default::default() });
        assert_eq!(s.display_name(&jid), "Bob");
        s.roster.get_mut(&jid).unwrap().name = Some("  ".into());
        assert_eq!(s.display_name(&jid), "bob@example.org");
    }

    #[test]
    fn an_answer_goes_to_the_request_that_asked_and_to_nobody_else() {
        let mut s = Session::default();
        let (tx, mut rx) = oneshot::channel();
        s.waiting.insert("a".into(), tx);
        assert!(!s.resolve(Iq::Result { from: None, to: None, id: "b".into(), payload: None }));
        assert!(rx.try_recv().is_err());
        assert!(s.resolve(Iq::Result { from: None, to: None, id: "a".into(), payload: None }));
        assert!(matches!(rx.try_recv(), Ok(Iq::Result { .. })));
    }
}
