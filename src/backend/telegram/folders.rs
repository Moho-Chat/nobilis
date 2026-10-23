//! Chat folders: the tabs Telegram lets an account sort its chats into, kept in step with moho's categories.
//!
//! A folder is a name and a rule for what is in it: chats named one by one, and kinds of chat (contacts, groups,
//! channels, bots) with some taken back out (muted, read, archived, or named one by one). The window draws each folder
//! as a category on the account's page, and a change on either side is made on the other:
//!
//! - a folder made, renamed, deleted or moved in order on Telegram turns up as the same change to the category
//!   (`telegramFolders`, said whenever the account's folders are read: on connect and on every
//!   `updateDialogFilter*` Telegram sends);
//! - a category made, renamed, deleted or moved in order here is made on Telegram, and a conversation filed under a
//!   category goes into that folder.
//!
//! A chat is in one category here and can be in several folders there. It is drawn under the first folder (in
//! Telegram's order) that holds it, and filing it somewhere else takes it out of that one and puts it in the new one;
//! the folders it is in besides are left as they are. Taking it out of a folder it is in by a rule - a chat in
//! "Groups" because it is a group - is done the way Telegram's own apps do it, by adding it to the folder's
//! exceptions.
//!
//! What Telegram does not allow is also not allowed here, and said in its words: a folder needs something in it, so
//! a category is made on Telegram when its first conversation is filed into it, and the last one cannot be taken
//! out. Groups are not conversations on the account's page (each is an entry on the rail), so they are not shown
//! under a category, but a folder that holds them still does, and an edit made here never touches them.
//!
//! Which chats a rule holds is worked out from what is known of each chat when the conversations are read - whether
//! it is a contact, muted, unread or archived - so a folder defined by "unread" is as current as that read.

use super::send::{explain, peer_for};
use super::{peers, Handle};
use crate::model::Buffer;
use crate::state::AppState;
use anyhow::{anyhow, Result};
use grammers_client::tl;
use grammers_session::types::PeerId;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

/// The first number a folder made by hand may have: 0 is the list of every chat.
const FIRST_ID: i32 = 2;

/// What kind of chat a conversation is, as a folder's rules tell them apart.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Person,
    Bot,
    Group,
    Broadcast,
}

/// What a folder's rules look at in a chat.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Facts {
    pub kind: Kind,
    pub contact: bool,
    pub muted: bool,
    pub unread: bool,
    pub archived: bool,
}

/// A folder's rules, read from Telegram's description of it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rules {
    contacts: bool,
    non_contacts: bool,
    groups: bool,
    broadcasts: bool,
    bots: bool,
    exclude_muted: bool,
    exclude_read: bool,
    exclude_archived: bool,
    /// Chats named one by one, pinned ones among them, as the ids this client calls them by.
    include: Vec<String>,
    exclude: Vec<String>,
}

impl Rules {
    /// Whether a chat is in by the kinds of chat the folder takes (and not taken back out), leaving the chats named
    /// one by one aside.
    fn by_kind(&self, facts: &Facts) -> bool {
        let wanted = match facts.kind {
            Kind::Person => (self.contacts && facts.contact) || (self.non_contacts && !facts.contact),
            Kind::Bot => self.bots,
            Kind::Group => self.groups,
            Kind::Broadcast => self.broadcasts,
        };
        wanted && !(self.exclude_muted && facts.muted) && !(self.exclude_read && !facts.unread) && !(self.exclude_archived && facts.archived)
    }

    /// Whether the folder holds a chat.
    pub fn holds(&self, remote: &str, facts: Option<&Facts>) -> bool {
        if self.include.iter().any(|r| r == remote) {
            return true;
        }
        if self.exclude.iter().any(|r| r == remote) {
            return false;
        }
        facts.is_some_and(|f| self.by_kind(f))
    }
}

/// A folder as Telegram holds it, with what is needed to change it.
#[derive(Clone, Debug)]
pub struct Folder {
    pub id: i32,
    pub title: String,
    pub emoticon: Option<String>,
    pub rules: Rules,
    /// Shared folders (a link anyone can join by) have no rules and no exceptions.
    pub shared: bool,
    raw: tl::enums::DialogFilter,
}

/// The chat an input peer names, as this client calls it; `None` for none.
pub fn remote_of_input(peer: &tl::enums::InputPeer, own: i64) -> Option<String> {
    use tl::enums::InputPeer as P;
    let id = match peer {
        P::PeerSelf => PeerId::user_unchecked(own),
        P::User(u) => PeerId::user_unchecked(u.user_id),
        P::UserFromMessage(u) => PeerId::user_unchecked(u.user_id),
        P::Chat(c) => PeerId::chat_unchecked(c.chat_id),
        P::Channel(c) => PeerId::channel_unchecked(c.channel_id),
        P::ChannelFromMessage(c) => PeerId::channel_unchecked(c.channel_id),
        P::Empty => return None,
    };
    Some(peers::remote_id(id))
}

fn text_of(title: &tl::enums::TextWithEntities) -> String {
    let tl::enums::TextWithEntities::Entities(t) = title;
    t.text.clone()
}

fn remotes(list: &[tl::enums::InputPeer], own: i64) -> Vec<String> {
    list.iter().filter_map(|p| remote_of_input(p, own)).collect()
}

/// A folder read from Telegram's description of it; `None` for the entry that stands for every chat.
pub fn folder_of(raw: &tl::enums::DialogFilter, own: i64) -> Option<Folder> {
    match raw {
        tl::enums::DialogFilter::Default => None,
        tl::enums::DialogFilter::Filter(f) => {
            let mut include = remotes(&f.include_peers, own);
            for pinned in remotes(&f.pinned_peers, own) {
                if !include.contains(&pinned) {
                    include.push(pinned);
                }
            }
            Some(Folder {
                id: f.id,
                title: text_of(&f.title),
                emoticon: f.emoticon.clone(),
                rules: Rules {
                    contacts: f.contacts,
                    non_contacts: f.non_contacts,
                    groups: f.groups,
                    broadcasts: f.broadcasts,
                    bots: f.bots,
                    exclude_muted: f.exclude_muted,
                    exclude_read: f.exclude_read,
                    exclude_archived: f.exclude_archived,
                    include,
                    exclude: remotes(&f.exclude_peers, own),
                },
                shared: false,
                raw: raw.clone(),
            })
        }
        tl::enums::DialogFilter::Chatlist(f) => {
            let mut include = remotes(&f.include_peers, own);
            for pinned in remotes(&f.pinned_peers, own) {
                if !include.contains(&pinned) {
                    include.push(pinned);
                }
            }
            Some(Folder { id: f.id, title: text_of(&f.title), emoticon: f.emoticon.clone(), rules: Rules { include, ..Default::default() }, shared: true, raw: raw.clone() })
        }
    }
}

/// The conversations a folder holds, by the ids this client calls them by: every one it names, and every one of the
/// chats whose facts are known that its rules take.
pub fn members(folder: &Folder, facts: &HashMap<String, Facts>) -> Vec<String> {
    let mut out: Vec<String> = folder.rules.include.clone();
    let mut seen: HashSet<&str> = folder.rules.include.iter().map(String::as_str).collect();
    let mut found: Vec<&String> = facts.keys().collect();
    found.sort();
    for remote in found {
        if !seen.contains(remote.as_str()) && folder.rules.holds(remote, facts.get(remote)) {
            seen.insert(remote);
            out.push(remote.clone());
        }
    }
    out
}

/// The first folder, in the order given, that holds a chat: the one it is drawn under.
pub fn home_of(folders: &[Folder], facts: &HashMap<String, Facts>, remote: &str) -> Option<i32> {
    folders.iter().find(|f| f.rules.holds(remote, facts.get(remote))).map(|f| f.id)
}

/// The folder with a chat put in it.
pub fn with_peer(raw: &tl::enums::DialogFilter, peer: &tl::enums::InputPeer, own: i64) -> Option<tl::enums::DialogFilter> {
    let remote = remote_of_input(peer, own)?;
    let mut edited = raw.clone();
    match &mut edited {
        tl::enums::DialogFilter::Default => return None,
        tl::enums::DialogFilter::Filter(f) => {
            f.exclude_peers.retain(|p| remote_of_input(p, own).as_deref() != Some(remote.as_str()));
            if !f.include_peers.iter().chain(&f.pinned_peers).any(|p| remote_of_input(p, own).as_deref() == Some(remote.as_str())) {
                f.include_peers.push(peer.clone());
            }
        }
        tl::enums::DialogFilter::Chatlist(f) => {
            if !f.include_peers.iter().chain(&f.pinned_peers).any(|p| remote_of_input(p, own).as_deref() == Some(remote.as_str())) {
                f.include_peers.push(peer.clone());
            }
        }
    }
    Some(edited)
}

/// The folder with a chat taken out: no longer named, and - if its rules would still take it - made an exception.
pub fn without_peer(raw: &tl::enums::DialogFilter, peer: &tl::enums::InputPeer, own: i64, facts: Option<&Facts>) -> Option<tl::enums::DialogFilter> {
    let remote = remote_of_input(peer, own)?;
    let named = |p: &tl::enums::InputPeer| remote_of_input(p, own).as_deref() == Some(remote.as_str());
    let mut edited = raw.clone();
    match &mut edited {
        tl::enums::DialogFilter::Default => return None,
        tl::enums::DialogFilter::Filter(f) => {
            f.include_peers.retain(|p| !named(p));
            f.pinned_peers.retain(|p| !named(p));
            let kept = folder_of(&tl::enums::DialogFilter::Filter(f.clone()), own)?;
            if facts.is_some_and(|facts| kept.rules.by_kind(facts)) && !f.exclude_peers.iter().any(named) {
                f.exclude_peers.push(peer.clone());
            }
        }
        tl::enums::DialogFilter::Chatlist(f) => {
            f.include_peers.retain(|p| !named(p));
            f.pinned_peers.retain(|p| !named(p));
        }
    }
    Some(edited)
}

/// A folder with another name.
pub fn renamed(raw: &tl::enums::DialogFilter, name: &str) -> Option<tl::enums::DialogFilter> {
    let title = tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities { text: name.to_string(), entities: Vec::new() });
    let mut edited = raw.clone();
    match &mut edited {
        tl::enums::DialogFilter::Default => return None,
        tl::enums::DialogFilter::Filter(f) => f.title = title,
        tl::enums::DialogFilter::Chatlist(f) => f.title = title,
    }
    Some(edited)
}

/// A folder that holds one chat and nothing else: how a category becomes a folder.
pub fn starting_with(id: i32, name: &str, peer: &tl::enums::InputPeer) -> tl::enums::DialogFilter {
    tl::enums::DialogFilter::Filter(tl::types::DialogFilter {
        contacts: false,
        non_contacts: false,
        groups: false,
        broadcasts: false,
        bots: false,
        exclude_muted: false,
        exclude_read: false,
        exclude_archived: false,
        title_noanimate: false,
        id,
        title: tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities { text: name.to_string(), entities: Vec::new() }),
        emoticon: None,
        color: None,
        pinned_peers: Vec::new(),
        include_peers: vec![peer.clone()],
        exclude_peers: Vec::new(),
    })
}

/// The number for a folder made now: the next one after those in use.
pub fn next_id(folders: &[tl::enums::DialogFilter]) -> i32 {
    folders
        .iter()
        .map(|f| match f {
            tl::enums::DialogFilter::Default => 0,
            tl::enums::DialogFilter::Filter(f) => f.id,
            tl::enums::DialogFilter::Chatlist(f) => f.id,
        })
        .max()
        .map_or(FIRST_ID, |m| (m + 1).max(FIRST_ID))
}

fn id_of(raw: &tl::enums::DialogFilter) -> i32 {
    match raw {
        tl::enums::DialogFilter::Default => 0,
        tl::enums::DialogFilter::Filter(f) => f.id,
        tl::enums::DialogFilter::Chatlist(f) => f.id,
    }
}

// What is known of each account's chats and folders, kept for the window to ask for and for the edits below.

type FactsByAccount = Mutex<HashMap<String, HashMap<String, Facts>>>;

fn all_facts() -> &'static FactsByAccount {
    static F: std::sync::OnceLock<FactsByAccount> = std::sync::OnceLock::new();
    F.get_or_init(Default::default)
}

type Held = Mutex<HashMap<String, Vec<Value>>>;

fn held() -> &'static Held {
    static H: std::sync::OnceLock<Held> = std::sync::OnceLock::new();
    H.get_or_init(Default::default)
}

/// Notes what a folder's rules look at in a chat, as the conversations are read.
pub fn note(account_id: &str, remote: String, facts: Facts) {
    all_facts().lock().unwrap_or_else(|e| e.into_inner()).entry(account_id.to_string()).or_default().insert(remote, facts);
}

fn facts_of(account_id: &str) -> HashMap<String, Facts> {
    all_facts().lock().unwrap_or_else(|e| e.into_inner()).get(account_id).cloned().unwrap_or_default()
}

/// Forgets an account's folders when it goes.
pub fn forget(account_id: &str) {
    all_facts().lock().unwrap_or_else(|e| e.into_inner()).remove(account_id);
    held().lock().unwrap_or_else(|e| e.into_inner()).remove(account_id);
}

/// The folders as the window is told of them: in Telegram's order, each with the conversations it holds.
pub fn summary(folders: &[Folder], facts: &HashMap<String, Facts>) -> Vec<Value> {
    folders
        .iter()
        .map(|f| json!({ "id": f.id, "name": f.title, "emoticon": f.emoticon, "members": members(f, facts), "shared": f.shared }))
        .collect()
}

/// What the window last heard of an account's folders.
pub fn current(account_id: &str) -> Value {
    Value::Array(held().lock().unwrap_or_else(|e| e.into_inner()).get(account_id).cloned().unwrap_or_default())
}

async fn read(handle: &Handle) -> Result<Vec<tl::enums::DialogFilter>> {
    let answer = handle.client.invoke(&tl::functions::messages::GetDialogFilters {}).await.map_err(|e| anyhow!("{}", explain(&e)))?;
    let tl::enums::messages::DialogFilters::Filters(found) = answer;
    Ok(found.filters)
}

/// Reads the account's folders and tells the window, if they are not what it was last told.
pub async fn sync(state: &AppState, account_id: &str, handle: &Handle) -> Result<Vec<Value>> {
    let raws = read(handle).await?;
    let folders: Vec<Folder> = raws.iter().filter_map(|r| folder_of(r, handle.own.user_id)).collect();
    let list = summary(&folders, &facts_of(account_id));
    let changed = {
        let mut all = held().lock().unwrap_or_else(|e| e.into_inner());
        let before = all.insert(account_id.to_string(), list.clone());
        before.as_ref() != Some(&list)
    };
    if changed {
        state.events.emit("telegramFolders", json!({ "accountId": account_id, "folders": list }));
    }
    Ok(list)
}

/// Telegram says the account's folders changed, on this client or another: read them again.
pub fn changed(state: &AppState, account_id: &str, handle: &Handle) {
    let (state, account_id, handle) = (state.clone(), account_id.to_string(), handle.clone());
    tokio::spawn(async move {
        if let Err(e) = sync(&state, &account_id, &handle).await {
            tracing::debug!("telegram[{account_id}]: reading the folders: {e:#}");
        }
    });
}

fn handle_for(state: &AppState, account_id: &str) -> Result<Handle, String> {
    state.runtime.telegram_handle(account_id).ok_or_else(|| "that Telegram account is not connected".to_string())
}

async fn save(handle: &Handle, id: i32, filter: Option<tl::enums::DialogFilter>) -> Result<(), String> {
    handle.client.invoke(&tl::functions::messages::UpdateDialogFilter { id, filter }).await.map_err(|e| folder_words(&explain(&e)))?;
    Ok(())
}

/// Telegram's refusals about folders, said so a person can act on them.
fn folder_words(raw: &str) -> String {
    let upper = raw.to_uppercase();
    for (code, words) in [
        ("FILTER_INCLUDE_EMPTY", "Telegram does not keep an empty folder: it needs at least one chat in it. Delete the folder instead"),
        ("FILTER_TITLE_EMPTY", "A folder needs a name"),
        ("DIALOG_FILTERS_TOO_MUCH", "This account already has as many folders as Telegram allows"),
        ("FILTER_NOT_SUPPORTED", "Telegram does not allow that change to this folder"),
        ("CHATLISTS_TOO_MUCH", "Telegram will not share any more folders from this account"),
        ("PEERS_LIST_EMPTY", "Telegram does not keep an empty folder: it needs at least one chat in it. Delete the folder instead"),
    ] {
        if upper.contains(code) {
            return words.to_string();
        }
    }
    raw.to_string()
}

/// A folder by number, in the order Telegram has them.
fn find(folders: &[Folder], id: i32) -> Result<&Folder, String> {
    folders.iter().find(|f| f.id == id).ok_or_else(|| "Telegram has no such folder any more".to_string())
}

/// Makes a folder from a conversation: how a category made here comes to exist on Telegram.
pub async fn create(state: &AppState, buffer: &Buffer, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("A folder needs a name".to_string());
    }
    let (handle, peer) = peer_for(state, buffer).await?;
    let raws = read(&handle).await.map_err(|e| e.to_string())?;
    let filter = starting_with(next_id(&raws), name, &peer.into());
    save(&handle, id_of(&filter), Some(filter)).await?;
    sync(state, &buffer.account_id, &handle).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Gives a folder another name.
pub async fn rename(state: &AppState, account_id: &str, id: i32, name: &str) -> Result<(), String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("A folder needs a name".to_string());
    }
    let handle = handle_for(state, account_id)?;
    let raws = read(&handle).await.map_err(|e| e.to_string())?;
    let raw = raws.iter().find(|r| id_of(r) == id && id != 0).ok_or("Telegram has no such folder any more")?;
    save(&handle, id, renamed(raw, name)).await?;
    sync(state, account_id, &handle).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Deletes a folder. The chats in it stay where they are; only the tab goes.
pub async fn delete(state: &AppState, account_id: &str, id: i32) -> Result<(), String> {
    let handle = handle_for(state, account_id)?;
    if id == 0 {
        return Err("The list of every chat cannot be deleted".to_string());
    }
    save(&handle, id, None).await?;
    sync(state, account_id, &handle).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// Puts the account's folders in the order given. Folders not named keep their places after those that are.
pub async fn reorder(state: &AppState, account_id: &str, order: &[i32]) -> Result<(), String> {
    let handle = handle_for(state, account_id)?;
    let raws = read(&handle).await.map_err(|e| e.to_string())?;
    let all: Vec<i32> = raws.iter().map(id_of).collect();
    let wanted = ordered(&all, order);
    if wanted == all {
        return Ok(());
    }
    handle.client.invoke(&tl::functions::messages::UpdateDialogFiltersOrder { order: wanted }).await.map_err(|e| folder_words(&explain(&e)))?;
    sync(state, account_id, &handle).await.map_err(|e| e.to_string())?;
    Ok(())
}

/// The folders' numbers in the order asked for. Those named take the places of those named, in the order named; any
/// not named - the entry for every chat, which is not drawn and so cannot have been moved - stay where they were.
pub fn ordered(all: &[i32], asked: &[i32]) -> Vec<i32> {
    let mut named: Vec<i32> = Vec::new();
    for id in asked {
        if all.contains(id) && !named.contains(id) {
            named.push(*id);
        }
    }
    let mut fill = named.clone().into_iter();
    let mut out = all.to_vec();
    for slot in out.iter_mut().filter(|id| named.contains(id)) {
        *slot = fill.next().unwrap_or(*slot);
    }
    out
}

/// Files a conversation under a folder (`to`), or under none: out of the one it is drawn under and into the new one.
pub async fn file(state: &AppState, buffer: &Buffer, to: Option<i32>) -> Result<(), String> {
    let (handle, peer) = peer_for(state, buffer).await?;
    let own = handle.own.user_id;
    let input: tl::enums::InputPeer = peer.into();
    let remote = remote_of_input(&input, own).ok_or("that is not a chat Telegram can file")?;
    let raws = read(&handle).await.map_err(|e| e.to_string())?;
    let folders: Vec<Folder> = raws.iter().filter_map(|r| folder_of(r, own)).collect();
    let facts = facts_of(&buffer.account_id);
    let from = home_of(&folders, &facts, &remote);
    if from == to {
        return Ok(());
    }
    // Put in first: if Telegram refuses to take it out of the old one, it is at least where it was asked to go.
    if let Some(id) = to {
        let folder = find(&folders, id)?;
        let edited = with_peer(&folder.raw, &input, own).ok_or("Telegram has no such folder any more")?;
        save(&handle, id, Some(edited)).await?;
    }
    if let Some(id) = from {
        let folder = find(&folders, id)?;
        let edited = without_peer(&folder.raw, &input, own, facts.get(&remote)).ok_or("Telegram has no such folder any more")?;
        if let Err(e) = save(&handle, id, Some(edited)).await {
            let _ = sync(state, &buffer.account_id, &handle).await;
            return Err(format!("It is in the new folder, but could not be taken out of \"{}\": {e}", folder.title));
        }
    }
    sync(state, &buffer.account_id, &handle).await.map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: i64 = 1;

    fn user(id: i64) -> tl::enums::InputPeer {
        tl::enums::InputPeer::User(tl::types::InputPeerUser { user_id: id, access_hash: 7 })
    }

    fn channel(id: i64) -> tl::enums::InputPeer {
        tl::enums::InputPeer::Channel(tl::types::InputPeerChannel { channel_id: id, access_hash: 7 })
    }

    fn filter(id: i32, name: &str, edit: impl FnOnce(&mut tl::types::DialogFilter)) -> tl::enums::DialogFilter {
        let tl::enums::DialogFilter::Filter(mut f) = starting_with(id, name, &user(100)) else { unreachable!() };
        f.include_peers.clear();
        edit(&mut f);
        tl::enums::DialogFilter::Filter(f)
    }

    /// A channel's id as this client writes it.
    fn ch(id: i64) -> String {
        peers::remote_id(PeerId::channel_unchecked(id))
    }

    fn facts(kind: Kind) -> Facts {
        Facts { kind, contact: false, muted: false, unread: true, archived: false }
    }

    #[test]
    fn a_chat_is_called_by_the_id_this_client_gives_it() {
        assert_eq!(remote_of_input(&user(100), OWN).as_deref(), Some("100"));
        assert_eq!(remote_of_input(&channel(555), OWN), Some(ch(555)));
        assert!(ch(555).starts_with("-100") && ch(555).ends_with("555"));
        assert_eq!(remote_of_input(&tl::enums::InputPeer::Chat(tl::types::InputPeerChat { chat_id: 9 }), OWN).as_deref(), Some("-9"));
        assert_eq!(remote_of_input(&tl::enums::InputPeer::PeerSelf, OWN).as_deref(), Some("1"), "saved messages are the account's own chat");
        assert_eq!(remote_of_input(&tl::enums::InputPeer::Empty, OWN), None);
    }

    #[test]
    fn a_folder_holds_the_chats_it_names_and_the_kinds_it_takes_less_the_exceptions() {
        let raw = filter(2, "Work", |f| {
            f.bots = true;
            f.groups = true;
            f.exclude_muted = true;
            f.include_peers = vec![user(100)];
            f.pinned_peers = vec![user(101)];
            f.exclude_peers = vec![user(200)];
        });
        let folder = folder_of(&raw, OWN).unwrap();
        assert_eq!((folder.id, folder.title.as_str(), folder.shared), (2, "Work", false));
        let mut known = HashMap::new();
        known.insert("100".to_string(), facts(Kind::Person));
        known.insert("200".to_string(), facts(Kind::Bot));
        known.insert("201".to_string(), facts(Kind::Bot));
        known.insert(ch(300), facts(Kind::Group));
        known.insert(ch(301), Facts { muted: true, ..facts(Kind::Group) });
        known.insert(ch(302), facts(Kind::Broadcast));
        known.insert("102".to_string(), facts(Kind::Person));
        // Named ones first (pinned among them, though Telegram keeps them apart), then the kinds, in a steady order.
        assert_eq!(members(&folder, &known), vec!["100".to_string(), "101".to_string(), ch(300), "201".to_string()]);
    }

    #[test]
    fn contacts_and_everyone_else_are_told_apart() {
        let raw = filter(2, "People", |f| f.contacts = true);
        let folder = folder_of(&raw, OWN).unwrap();
        let friend = Facts { contact: true, ..facts(Kind::Person) };
        assert!(folder.rules.holds("5", Some(&friend)));
        assert!(!folder.rules.holds("6", Some(&facts(Kind::Person))));
        assert!(!folder.rules.holds("7", None), "a chat nothing is known of is held only if it is named");
        let others = folder_of(&filter(3, "Strangers", |f| f.non_contacts = true), OWN).unwrap();
        assert!(others.rules.holds("6", Some(&facts(Kind::Person))));
        assert!(!others.rules.holds("5", Some(&friend)));
    }

    #[test]
    fn read_and_archived_chats_can_be_left_out_of_a_folder() {
        let raw = filter(2, "Unread", |f| {
            f.groups = true;
            f.exclude_read = true;
            f.exclude_archived = true;
        });
        let folder = folder_of(&raw, OWN).unwrap();
        assert!(folder.rules.holds("-1", Some(&facts(Kind::Group))));
        assert!(!folder.rules.holds("-1", Some(&Facts { unread: false, ..facts(Kind::Group) })));
        assert!(!folder.rules.holds("-1", Some(&Facts { archived: true, ..facts(Kind::Group) })));
    }

    #[test]
    fn the_entry_for_every_chat_is_not_a_folder() {
        assert!(folder_of(&tl::enums::DialogFilter::Default, OWN).is_none());
    }

    #[test]
    fn a_chat_is_drawn_under_the_first_folder_that_holds_it() {
        let a = folder_of(&filter(2, "Groups", |f| f.groups = true), OWN).unwrap();
        let b = folder_of(&filter(3, "Mine", |f| f.include_peers = vec![channel(300)]), OWN).unwrap();
        let mut known = HashMap::new();
        known.insert(ch(300), facts(Kind::Group));
        known.insert("5".to_string(), facts(Kind::Person));
        assert_eq!(home_of(&[a.clone(), b.clone()], &known, &ch(300)), Some(2));
        assert_eq!(home_of(&[b, a], &known, &ch(300)), Some(3));
        assert_eq!(home_of(&[], &known, "5"), None);
    }

    #[test]
    fn putting_a_chat_in_names_it_once_and_lifts_an_exception() {
        let raw = filter(2, "Work", |f| f.exclude_peers = vec![user(100)]);
        let put = with_peer(&raw, &user(100), OWN).unwrap();
        let tl::enums::DialogFilter::Filter(f) = &put else { unreachable!() };
        assert_eq!(f.include_peers.len(), 1);
        assert!(f.exclude_peers.is_empty());
        let again = with_peer(&put, &user(100), OWN).unwrap();
        let tl::enums::DialogFilter::Filter(f) = &again else { unreachable!() };
        assert_eq!(f.include_peers.len(), 1, "a chat already named is not named twice");
    }

    #[test]
    fn taking_a_chat_out_of_a_named_list_is_taking_it_off_the_list() {
        let raw = filter(2, "Work", |f| {
            f.include_peers = vec![user(100), user(101)];
            f.pinned_peers = vec![user(100)];
        });
        let out = without_peer(&raw, &user(100), OWN, Some(&facts(Kind::Person))).unwrap();
        let tl::enums::DialogFilter::Filter(f) = &out else { unreachable!() };
        assert_eq!(remotes(&f.include_peers, OWN), vec!["101"]);
        assert!(f.pinned_peers.is_empty());
        assert!(f.exclude_peers.is_empty(), "nothing else held it, so there is no exception to make");
    }

    #[test]
    fn taking_a_chat_out_of_a_folder_that_holds_it_by_a_rule_makes_it_an_exception() {
        let raw = filter(2, "Groups", |f| f.groups = true);
        let out = without_peer(&raw, &channel(300), OWN, Some(&facts(Kind::Group))).unwrap();
        let tl::enums::DialogFilter::Filter(f) = &out else { unreachable!() };
        assert_eq!(remotes(&f.exclude_peers, OWN), vec![ch(300)]);
        let folder = folder_of(&out, OWN).unwrap();
        assert!(!folder.rules.holds(&ch(300), Some(&facts(Kind::Group))));
        assert!(folder.rules.holds(&ch(301), Some(&facts(Kind::Group))), "the others are still held");
        // And not twice.
        let again = without_peer(&out, &channel(300), OWN, Some(&facts(Kind::Group))).unwrap();
        let tl::enums::DialogFilter::Filter(f) = &again else { unreachable!() };
        assert_eq!(f.exclude_peers.len(), 1);
    }

    #[test]
    fn a_shared_folder_has_no_exceptions_to_make() {
        let raw = tl::enums::DialogFilter::Chatlist(tl::types::DialogFilterChatlist {
            has_my_invites: false,
            title_noanimate: false,
            id: 4,
            title: tl::enums::TextWithEntities::Entities(tl::types::TextWithEntities { text: "Shared".into(), entities: vec![] }),
            emoticon: None,
            color: None,
            pinned_peers: vec![],
            include_peers: vec![user(100), user(101)],
        });
        let folder = folder_of(&raw, OWN).unwrap();
        assert!(folder.shared);
        let out = without_peer(&raw, &user(100), OWN, Some(&facts(Kind::Person))).unwrap();
        assert_eq!(folder_of(&out, OWN).unwrap().rules.include, vec!["101"]);
        assert_eq!(folder_of(&renamed(&out, "Team").unwrap(), OWN).unwrap().title, "Team");
    }

    #[test]
    fn renaming_keeps_everything_but_the_name() {
        let raw = filter(2, "Work", |f| {
            f.bots = true;
            f.include_peers = vec![user(100)];
            f.emoticon = Some("💼".into());
        });
        let renamed = renamed(&raw, "Job").unwrap();
        let (before, after) = (folder_of(&raw, OWN).unwrap(), folder_of(&renamed, OWN).unwrap());
        assert_eq!(after.title, "Job");
        assert_eq!((before.rules, before.emoticon, before.id), (after.rules, after.emoticon, after.id));
        assert!(super::renamed(&tl::enums::DialogFilter::Default, "x").is_none());
    }

    #[test]
    fn a_new_folder_has_the_next_number_after_those_in_use_and_never_below_two() {
        assert_eq!(next_id(&[]), 2);
        assert_eq!(next_id(&[tl::enums::DialogFilter::Default]), 2);
        assert_eq!(next_id(&[tl::enums::DialogFilter::Default, filter(2, "A", |_| {}), filter(7, "B", |_| {})]), 8);
    }

    #[test]
    fn a_new_folder_starts_with_the_one_chat_it_was_made_for() {
        let made = folder_of(&starting_with(3, "Friends", &user(100)), OWN).unwrap();
        assert_eq!((made.id, made.title.as_str()), (3, "Friends"));
        assert_eq!(made.rules.include, vec!["100"]);
        assert!(!made.rules.holds("101", Some(&facts(Kind::Person))), "no kind of chat is taken until asked for");
    }

    #[test]
    fn folders_are_put_in_the_order_asked_for_and_the_entry_for_every_chat_keeps_its_place() {
        assert_eq!(ordered(&[0, 2, 3, 4], &[4, 2, 3]), vec![0, 4, 2, 3]);
        assert_eq!(ordered(&[2, 3, 0, 4], &[4, 3]), vec![2, 4, 0, 3], "the two named swap places, the rest stay");
        assert_eq!(ordered(&[0, 2, 3], &[9, 3, 3, 2]), vec![0, 3, 2], "numbers that are not folders, and repeats, are dropped");
        assert_eq!(ordered(&[0, 2, 3], &[]), vec![0, 2, 3]);
    }

    #[test]
    fn the_summary_is_in_telegrams_order_with_each_folders_conversations() {
        let a = folder_of(&filter(3, "Second", |f| f.include_peers = vec![user(100)]), OWN).unwrap();
        let b = folder_of(&filter(2, "First", |f| f.bots = true), OWN).unwrap();
        let mut known = HashMap::new();
        known.insert("200".to_string(), facts(Kind::Bot));
        let out = summary(&[a, b], &known);
        assert_eq!(out[0]["id"], 3);
        assert_eq!(out[0]["members"], json!(["100"]));
        assert_eq!(out[1]["name"], "First");
        assert_eq!(out[1]["members"], json!(["200"]));
    }

    #[test]
    fn telegrams_refusals_about_folders_are_said_in_words() {
        assert!(folder_words("rpc error 400: FILTER_INCLUDE_EMPTY").contains("empty folder"));
        assert!(folder_words("DIALOG_FILTERS_TOO_MUCH").contains("as many folders"));
        assert_eq!(folder_words("SOMETHING_ELSE"), "SOMETHING_ELSE");
    }
}
