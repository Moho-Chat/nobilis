//! The emotes a Twitch chat is written in that Twitch itself does not carry:
//! BetterTTV's, FrankerFaceZ's and 7TV's, global and each channel's, and the
//! test of whether one of Twitch's own is animated.
//!
//! The three services' tables are public and need no key. They are fetched
//! when a channel is joined (and the global ones once), kept in memory, and
//! consulted as messages arrive: a word in a message that is the name of one
//! becomes a token (`[twitch:bttv:<id>:<name>]`) the window draws a picture
//! for, the way it does Twitch's own. A message is converted once, as it is
//! recorded, so what is stored is what is drawn.
//!
//! Each service has its own switch (Settings, Twitch), and a switched-off one
//! is neither asked nor consulted: it is somebody else's server being told
//! which channel is being opened. The switches are the window's, sent to the
//! daemon when it starts and when they change, and until it says otherwise
//! all three are on.

use crate::model::{EmojiEntry, EmojiSource};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Provider {
    Bttv,
    Ffz,
    SevenTv,
}

impl Provider {
    const ALL: [Provider; 3] = [Provider::Bttv, Provider::Ffz, Provider::SevenTv];

    /// What the token calls it, and what the window knows it by.
    pub fn code(self) -> &'static str {
        match self {
            Provider::Bttv => "bttv",
            Provider::Ffz => "ffz",
            Provider::SevenTv => "7tv",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Provider::Bttv => "BetterTTV",
            Provider::Ffz => "FrankerFaceZ",
            Provider::SevenTv => "7TV",
        }
    }

    /// The picture for an emote, at the size chat draws it. Public and built
    /// from the id, so the window needs nothing more than the token.
    pub fn image(self, id: &str) -> String {
        match self {
            Provider::Bttv => format!("https://cdn.betterttv.net/emote/{id}/2x.webp"),
            Provider::Ffz => format!("https://cdn.frankerfacez.com/emote/{id}/2"),
            Provider::SevenTv => format!("https://cdn.7tv.app/emote/{id}/2x.webp"),
        }
    }
}

fn switches() -> &'static [AtomicBool; 3] {
    static ON: [AtomicBool; 3] = [AtomicBool::new(true), AtomicBool::new(true), AtomicBool::new(true)];
    &ON
}

fn index(p: Provider) -> usize {
    match p {
        Provider::Bttv => 0,
        Provider::Ffz => 1,
        Provider::SevenTv => 2,
    }
}

pub fn enabled(p: Provider) -> bool {
    switches()[index(p)].load(Ordering::Relaxed)
}

/// The window's choice of which of the three to use.
pub fn set_enabled(bttv: bool, ffz: bool, seventv: bool) {
    for (p, on) in [(Provider::Bttv, bttv), (Provider::Ffz, ffz), (Provider::SevenTv, seventv)] {
        switches()[index(p)].store(on, Ordering::Relaxed);
    }
}

/// One emote of the three services.
#[derive(Clone, Debug, PartialEq)]
pub struct Emote {
    pub provider: Provider,
    pub id: String,
    pub name: String,
    pub animated: bool,
}

impl Emote {
    /// What goes in a message in the emote's place.
    pub fn token(&self) -> String {
        // A name cannot close its own token.
        format!("[twitch:{}:{}:{}]", self.provider.code(), self.id, self.name.replace(']', "_"))
    }
}

/// A set of emotes, found by the word they are typed as.
#[derive(Default, Clone, Debug)]
pub struct Table {
    by_name: HashMap<String, Emote>,
    order: Vec<String>,
}

impl Table {
    fn add(&mut self, e: Emote) {
        if !self.by_name.contains_key(&e.name) {
            self.order.push(e.name.clone());
        }
        self.by_name.insert(e.name.clone(), e);
    }

    #[cfg(test)]
    pub fn add_for_test(&mut self, e: Emote) {
        self.add(e);
    }

    /// Takes one out by the word it is typed as.
    fn remove_name(&mut self, name: &str) {
        if self.by_name.remove(name).is_some() {
            self.order.retain(|n| n != name);
        }
    }

    /// Takes one out by its id, for a service that says which emote went and not what it was called.
    fn remove_id(&mut self, id: &str) {
        let names: Vec<String> = self.by_name.values().filter(|e| e.id == id).map(|e| e.name.clone()).collect();
        for name in names {
            self.remove_name(&name);
        }
    }

    pub fn get(&self, word: &str) -> Option<&Emote> {
        self.by_name.get(word)
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Emote> {
        self.order.iter().filter_map(|n| self.by_name.get(n))
    }
}

// ---------------------------------------------------------------------------
// Reading the three services' answers
// ---------------------------------------------------------------------------

fn text(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(str::to_string)
}

/// BetterTTV's emotes in either of its two answers: a list (the global set),
/// or an object with `channelEmotes` and `sharedEmotes`.
pub fn read_bttv(answer: &Value) -> Table {
    let mut table = Table::default();
    let mut add = |list: &Value| {
        for e in list.as_array().into_iter().flatten() {
            if let (Some(id), Some(code)) = (text(e, "id"), text(e, "code")) {
                // A modifier is drawn over another emote, which this does not do.
                if e.get("modifier").and_then(|m| m.as_bool()).unwrap_or(false) {
                    continue;
                }
                table.add(Emote { provider: Provider::Bttv, id, name: code, animated: e.get("animated").and_then(|a| a.as_bool()).unwrap_or(false) });
            }
        }
    };
    if answer.is_array() {
        add(answer);
    } else {
        add(&answer["channelEmotes"]);
        add(&answer["sharedEmotes"]);
    }
    table
}

/// FrankerFaceZ's: the global answer names its default sets, a channel's names its one.
pub fn read_ffz(answer: &Value) -> Table {
    let mut table = Table::default();
    let sets = &answer["sets"];
    let mut wanted: Vec<String> = answer["default_sets"].as_array().map(|a| a.iter().filter_map(|n| n.as_u64().map(|n| n.to_string())).collect()).unwrap_or_default();
    if let Some(set) = answer.pointer("/room/set").and_then(|s| s.as_u64()) {
        wanted.push(set.to_string());
    }
    for set in wanted {
        for e in sets[&set]["emoticons"].as_array().into_iter().flatten() {
            let (Some(id), Some(name)) = (e.get("id").and_then(|i| i.as_u64()), text(e, "name")) else { continue };
            if e.get("modifier").and_then(|m| m.as_bool()).unwrap_or(false) {
                continue;
            }
            table.add(Emote { provider: Provider::Ffz, id: id.to_string(), name, animated: e.get("animated").is_some_and(|a| !a.is_null()) });
        }
    }
    table
}

/// 7TV's, from a global set (`emotes` at the top) or a user (`emote_set.emotes`).
pub fn read_seventv(answer: &Value) -> Table {
    let mut table = Table::default();
    let list = answer.get("emotes").or_else(|| answer.pointer("/emote_set/emotes"));
    for e in list.and_then(|l| l.as_array()).into_iter().flatten() {
        let (Some(id), Some(name)) = (text(e, "id"), text(e, "name")) else { continue };
        // Bit 0 of the flags on an active emote: drawn over another one, which this does not do.
        if e.get("flags").and_then(|f| f.as_u64()).unwrap_or(0) & 1 == 1 {
            continue;
        }
        table.add(Emote { provider: Provider::SevenTv, id, name, animated: e.pointer("/data/animated").and_then(|a| a.as_bool()).unwrap_or(false) });
    }
    table
}

// ---------------------------------------------------------------------------
// What is held
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Held {
    global: HashMap<Provider, Arc<Table>>,
    /// By channel login.
    channels: HashMap<String, HashMap<Provider, Arc<Table>>>,
    /// The 7TV emote set each channel uses, which 7TV's live changes are keyed by.
    seventv_sets: HashMap<String, String>,
}

fn held() -> &'static Mutex<Held> {
    static HELD: std::sync::OnceLock<Mutex<Held>> = std::sync::OnceLock::new();
    HELD.get_or_init(Default::default)
}

/// Keeps a table for a channel (or, with no channel, the global one).
pub fn keep(provider: Provider, channel: Option<&str>, table: Table) {
    let mut held = held().lock().unwrap();
    match channel {
        Some(channel) => {
            held.channels.entry(channel.to_string()).or_default().insert(provider, Arc::new(table));
        }
        None => {
            held.global.insert(provider, Arc::new(table));
        }
    }
}

/// Forgets a channel's tables, when it is closed.
pub fn forget(channel: &str) {
    let mut held = held().lock().unwrap();
    held.channels.remove(channel);
    held.seventv_sets.remove(channel);
}

/// The 7TV set a channel's emotes came from, if it has one.
pub fn seventv_set(channel: &str) -> Option<String> {
    held().lock().unwrap().seventv_sets.get(channel).cloned()
}

/// One change to a table that a service announced as it happened.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Added, or - if the word is already one - replaced.
    Add(Emote),
    /// Gone, by the word it was typed as.
    RemoveName(String),
    /// Gone, by id.
    RemoveId(String),
}

/// Applies changes to a channel's table for one service. A channel not held (closed since) is left alone, and
/// so is a change to nothing: the next full fetch is the truth, this keeps it fresh in between.
pub fn apply(provider: Provider, channel: &str, changes: &[Change]) {
    if changes.is_empty() {
        return;
    }
    let mut held = held().lock().unwrap();
    let Some(slot) = held.channels.get_mut(channel).and_then(|c| c.get_mut(&provider)) else { return };
    let mut table = (**slot).clone();
    for change in changes {
        match change {
            Change::Add(e) => table.add(e.clone()),
            Change::RemoveName(n) => table.remove_name(n),
            Change::RemoveId(id) => table.remove_id(id),
        }
    }
    *slot = Arc::new(table);
}

/// What a 7TV `emote_set.update` dispatch changed: emotes pushed in, pulled out, or renamed or swapped.
pub fn read_seventv_changes(body: &Value) -> Vec<Change> {
    let emote = |v: &Value| -> Option<Emote> {
        let (id, name) = (text(v, "id")?, text(v, "name")?);
        Some(Emote { provider: Provider::SevenTv, id, name, animated: v.pointer("/data/animated").and_then(|a| a.as_bool()).unwrap_or(false) })
    };
    let mut out = Vec::new();
    let about_emotes = |c: &Value| c.get("key").and_then(|k| k.as_str()) == Some("emotes");
    for c in body.get("pulled").and_then(|p| p.as_array()).into_iter().flatten().filter(|c| about_emotes(c)) {
        if let Some(name) = c.get("old_value").and_then(|v| text(v, "name")) {
            out.push(Change::RemoveName(name));
        }
    }
    for c in body.get("updated").and_then(|p| p.as_array()).into_iter().flatten().filter(|c| about_emotes(c)) {
        if let Some(name) = c.get("old_value").and_then(|v| text(v, "name")) {
            out.push(Change::RemoveName(name));
        }
        if let Some(e) = c.get("value").and_then(emote) {
            if !c.pointer("/value/flags").and_then(|f| f.as_u64()).is_some_and(|f| f & 1 == 1) {
                out.push(Change::Add(e));
            }
        }
    }
    for c in body.get("pushed").and_then(|p| p.as_array()).into_iter().flatten().filter(|c| about_emotes(c)) {
        if let Some(e) = c.get("value").and_then(emote) {
            if !c.pointer("/value/flags").and_then(|f| f.as_u64()).is_some_and(|f| f & 1 == 1) {
                out.push(Change::Add(e));
            }
        }
    }
    out
}

/// What a BetterTTV event changed.
pub fn read_bttv_change(event: &Value) -> Option<Change> {
    let data = event.get("data")?;
    match event.get("name")?.as_str()? {
        "emote_create" | "emote_update" => {
            let e = data.get("emote")?;
            let (id, code) = (text(e, "id")?, text(e, "code")?);
            let animated = e.get("animated").and_then(|a| a.as_bool()).unwrap_or_else(|| e.get("imageType").and_then(|t| t.as_str()) == Some("gif"));
            Some(Change::Add(Emote { provider: Provider::Bttv, id, name: code, animated }))
        }
        "emote_delete" => text(data, "emoteId").map(Change::RemoveId),
        _ => None,
    }
}


/// The emote a word is in a channel, if it is one of an enabled service's.
/// A channel's own come before the global ones, and the services in a fixed
/// order, so one word is always the same emote.
pub fn find(channel: &str, word: &str) -> Option<Emote> {
    let held = held().lock().unwrap();
    for provider in Provider::ALL {
        if !enabled(provider) {
            continue;
        }
        if let Some(e) = held.channels.get(channel).and_then(|c| c.get(&provider)).and_then(|t| t.get(word)) {
            return Some(e.clone());
        }
    }
    for provider in Provider::ALL {
        if !enabled(provider) {
            continue;
        }
        if let Some(e) = held.global.get(&provider).and_then(|t| t.get(word)) {
            return Some(e.clone());
        }
    }
    None
}

/// The words an emote token stands for: `[twitch:bttv:9:Pog]` is sent as `Pog`.
///
/// The window puts a picture in the message box where an emote was picked, and what that picture is made of is
/// this token (the same one an incoming message carries, so one piece of code draws both). Twitch reads an emote
/// as its word, so that is what leaves.
pub fn tokens_to_names(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("[twitch:") {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let parsed = after.find(']').and_then(|end| {
            let mut parts = after[1..end].splitn(4, ':');
            let (_, kind, _id, name) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
            matches!(kind, "emote" | "aemote" | "bttv" | "ffz" | "7tv").then_some((name, end))
        });
        match parsed {
            Some((name, end)) => {
                out.push_str(name);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("[twitch:");
                rest = &after["[twitch:".len()..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// A channel's own emote by word, whichever provider, ignoring the switches: for tests of what is held.
#[cfg(test)]
pub fn find_in(channel: &str, word: &str) -> Option<Emote> {
    let held = held().lock().unwrap();
    held.channels.get(channel)?.values().find_map(|t| t.get(word).cloned())
}

/// Every emote of an enabled service the channel can use, as the picker's sections.
pub fn sources(channel: &str, buffer_id: &str) -> Vec<EmojiSource> {
    let held = held().lock().unwrap();
    let mut out = Vec::new();
    for provider in Provider::ALL {
        if !enabled(provider) {
            continue;
        }
        for (scope, table) in [("channel", held.channels.get(channel).and_then(|c| c.get(&provider))), ("global", held.global.get(&provider))] {
            let Some(table) = table.filter(|t| !t.is_empty()) else { continue };
            out.push(EmojiSource {
                id: format!("{}:{scope}:{channel}", provider.code()),
                name: format!("{} {scope}", provider.name()),
                service: "twitch".to_string(),
                // What choosing one does: its name goes into the message, which is how Twitch's chat reads it.
                kind: "text".to_string(),
                icon_url: None,
                buffers: if scope == "channel" { vec![buffer_id.to_string()] } else { Vec::new() },
                sendable_anywhere: scope == "global",
                emoji: table.iter().map(|e| EmojiEntry { id: e.token(), name: e.name.clone(), url: Some(provider.image(&e.id)), animated: e.animated, locked: false }).collect(),
            });
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

pub struct Roots {
    pub bttv: &'static str,
    pub ffz: &'static str,
    pub seventv: &'static str,
}

pub const ROOTS: Roots = Roots { bttv: "https://api.betterttv.net/3", ffz: "https://api.frankerfacez.com/v1", seventv: "https://7tv.io/v3" };

async fn fetch(http: &reqwest::Client, url: &str) -> Option<Value> {
    let res = http.get(url).send().await.ok()?;
    // A channel that has no set with a service answers 404, which is an empty table and not a failure.
    if !res.status().is_success() {
        return None;
    }
    res.json().await.ok()
}

/// Fetches the global tables of every enabled service that is not held yet.
pub async fn load_global(http: &reqwest::Client, roots: &Roots) {
    for provider in Provider::ALL {
        if !enabled(provider) || held().lock().unwrap().global.contains_key(&provider) {
            continue;
        }
        let answer = match provider {
            Provider::Bttv => fetch(http, &format!("{}/cached/emotes/global", roots.bttv)).await.map(|a| read_bttv(&a)),
            Provider::Ffz => fetch(http, &format!("{}/set/global", roots.ffz)).await.map(|a| read_ffz(&a)),
            Provider::SevenTv => fetch(http, &format!("{}/emote-sets/global", roots.seventv)).await.map(|a| read_seventv(&a)),
        };
        if let Some(table) = answer {
            keep(provider, None, table);
        }
    }
}

/// Fetches a channel's tables from every enabled service. `room_id` is
/// Twitch's number for the channel, which all three key their answers by.
pub async fn load_channel(http: &reqwest::Client, roots: &Roots, login: &str, room_id: &str) {
    for provider in Provider::ALL {
        if !enabled(provider) {
            continue;
        }
        let answer = match provider {
            Provider::Bttv => fetch(http, &format!("{}/cached/users/twitch/{room_id}", roots.bttv)).await.map(|a| read_bttv(&a)),
            Provider::Ffz => fetch(http, &format!("{}/room/id/{room_id}", roots.ffz)).await.map(|a| read_ffz(&a)),
            Provider::SevenTv => fetch(http, &format!("{}/users/twitch/{room_id}", roots.seventv)).await.map(|a| {
                if let Some(set) = a.pointer("/emote_set/id").and_then(|i| i.as_str()) {
                    held().lock().unwrap().seventv_sets.insert(login.to_string(), set.to_string());
                }
                read_seventv(&a)
            }),
        };
        // Held even when empty: a channel that has none is not asked again.
        keep(provider, Some(login), answer.unwrap_or_default());
    }
}

// ---------------------------------------------------------------------------
// Twitch's own, animated or not
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Animated {
    known: HashMap<String, bool>,
    asking: HashSet<String>,
}

fn animated_cache() -> &'static Mutex<Animated> {
    static CACHE: std::sync::OnceLock<Mutex<Animated>> = std::sync::OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// Whether one of Twitch's own emotes is animated, if that is known yet.
///
/// Twitch's picture address has an animated form for those that are and answers
/// 404 for those that are not, and the message carries no word on which, so the
/// first time an emote is seen it is asked about, in the background, and drawn
/// still until the answer is in; every message after that draws it moving.
pub fn animated(id: &str) -> Option<bool> {
    let mut cache = animated_cache().lock().unwrap();
    if let Some(known) = cache.known.get(id) {
        return Some(*known);
    }
    if cache.asking.insert(id.to_string()) {
        // Only where there is something to ask with: a unit test calling this has no runtime.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let id = id.to_string();
            runtime.spawn(async move {
                let yes = match super::client() {
                    Ok(http) => http.head(animated_url(&id)).send().await.map(|r| r.status().is_success()).unwrap_or(false),
                    Err(_) => false,
                };
                let mut cache = animated_cache().lock().unwrap();
                cache.asking.remove(&id);
                // Bounded: a long session sees a great many emotes.
                if cache.known.len() > 20_000 {
                    cache.known.clear();
                }
                cache.known.insert(id, yes);
            });
        } else {
            cache.asking.remove(id);
        }
    }
    None
}

/// Twitch's address for the moving form of one of its own emotes.
pub fn animated_url(id: &str) -> String {
    format!("https://static-cdn.jtvnw.net/emoticons/v2/{id}/animated/dark/2.0")
}

/// For a test: says how an emote is, as if it had been asked.
#[cfg(test)]
pub fn say_animated(id: &str, yes: bool) {
    animated_cache().lock().unwrap().known.insert(id.to_string(), yes);
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_picked_emote_leaves_as_its_word_and_nothing_else_is_touched() {
        assert_eq!(tokens_to_names("hi [twitch:7tv:01ABC:catJAM] and [twitch:emote:25:Kappa]!"), "hi catJAM and Kappa!");
        assert_eq!(tokens_to_names("[twitch:aemote:emotesv2_ab12:Wow]"), "Wow");
        assert_eq!(tokens_to_names("[twitch:cheer:cheer:100] stays, [twitch:bad"), "[twitch:cheer:cheer:100] stays, [twitch:bad");
        assert_eq!(tokens_to_names("plain [text] here"), "plain [text] here");
    }

    use super::*;
    use serde_json::json;

    #[test]
    fn bettertvs_two_shapes_are_read_and_modifiers_are_left_out() {
        let global = read_bttv(&json!([
            { "id": "a1", "code": "OMEGALUL", "imageType": "png", "animated": false },
            { "id": "a2", "code": "cvMask", "imageType": "png", "animated": false, "modifier": true }
        ]));
        assert_eq!(global.len(), 1);
        assert_eq!(global.get("OMEGALUL").unwrap().token(), "[twitch:bttv:a1:OMEGALUL]");
        let channel = read_bttv(&json!({ "id": "x", "channelEmotes": [{ "id": "c1", "code": "xqcL", "imageType": "gif", "animated": true }], "sharedEmotes": [{ "id": "s1", "code": "Shared", "imageType": "png", "animated": false }] }));
        assert_eq!(channel.len(), 2);
        assert!(channel.get("xqcL").unwrap().animated);
    }

    #[test]
    fn frankerfacezs_sets_are_the_default_ones_and_a_rooms_own() {
        let answer = json!({
            "default_sets": [3],
            "room": { "set": 99 },
            "sets": { "3": { "emoticons": [{ "id": 9, "name": "ZrehplaR" }] }, "99": { "emoticons": [{ "id": 5, "name": "Mine", "animated": { "1": "u" } }, { "id": 6, "name": "Over", "modifier": true }] }, "7": { "emoticons": [{ "id": 1, "name": "NotMine" }] } }
        });
        let table = read_ffz(&answer);
        assert_eq!(table.len(), 2, "a set nobody named is not read, and a modifier is left out");
        assert_eq!(table.get("Mine").unwrap().token(), "[twitch:ffz:5:Mine]");
        assert!(table.get("Mine").unwrap().animated && !table.get("ZrehplaR").unwrap().animated);
    }

    #[test]
    fn seventvs_emotes_come_from_a_set_or_a_user_and_zero_width_ones_are_left_out() {
        let set = json!({ "emotes": [{ "id": "01FG", "name": "reckH", "flags": 0, "data": { "animated": false } }, { "id": "01XX", "name": "Wide", "flags": 1, "data": { "animated": true } }] });
        let user = json!({ "emote_set": { "emotes": [{ "id": "01GA", "name": "GAMBA", "flags": 0, "data": { "animated": true } }] } });
        assert_eq!(read_seventv(&set).len(), 1);
        let t = read_seventv(&user);
        assert_eq!(t.get("GAMBA").unwrap().token(), "[twitch:7tv:01GA:GAMBA]");
        assert!(t.get("GAMBA").unwrap().animated);
    }

    #[test]
    fn a_channels_own_emote_beats_a_global_one_of_the_same_name_and_a_switched_off_service_is_not_asked() {
        set_enabled(true, true, true);
        keep(Provider::Bttv, None, read_bttv(&json!([{ "id": "g", "code": "SameName", "animated": false }])));
        keep(Provider::SevenTv, Some("chan"), read_seventv(&json!({ "emotes": [{ "id": "c", "name": "SameName", "flags": 0, "data": {} }] })));
        // The channel's comes first even though BetterTTV is checked first among global ones.
        assert_eq!(find("chan", "SameName").unwrap().id, "c");
        assert_eq!(find("elsewhere", "SameName").unwrap().id, "g");
        assert!(find("chan", "NoSuchEmote").is_none());
        set_enabled(false, true, false);
        assert!(find("chan", "SameName").is_none());
        assert!(find("elsewhere", "SameName").is_none());
        set_enabled(true, true, true);
        forget("chan");
        assert_eq!(find("chan", "SameName").unwrap().id, "g");
    }

    #[test]
    fn the_picker_gets_a_section_for_each_service_that_has_emotes_and_names_are_what_is_sent() {
        set_enabled(true, true, true);
        keep(Provider::Ffz, Some("pickerchan"), read_ffz(&json!({ "room": { "set": 1 }, "sets": { "1": { "emoticons": [{ "id": 4, "name": "Hello" }] } } })));
        let sources = sources("pickerchan", "twitch:~anonymous|pickerchan");
        let ffz = sources.iter().find(|s| s.id == "ffz:channel:pickerchan").expect("a section");
        assert_eq!((ffz.kind.as_str(), ffz.sendable_anywhere), ("text", false));
        assert_eq!(ffz.buffers, vec!["twitch:~anonymous|pickerchan"]);
        // Named by the token the box draws a picture from, which leaves as the word (see `tokens_to_names`).
        assert_eq!((ffz.emoji[0].id.as_str(), ffz.emoji[0].name.as_str(), ffz.emoji[0].url.as_deref()), ("[twitch:ffz:4:Hello]", "Hello", Some("https://cdn.frankerfacez.com/emote/4/2")));
        assert_eq!(tokens_to_names(&ffz.emoji[0].id), "Hello");
        forget("pickerchan");
    }

    #[test]
    fn twitchs_own_animated_form_is_a_different_address_and_an_unasked_emote_is_unknown() {
        assert_eq!(animated_url("301445381"), "https://static-cdn.jtvnw.net/emoticons/v2/301445381/animated/dark/2.0");
        assert_eq!(animated("never-asked-about"), None);
        say_animated("known-moving", true);
        assert_eq!(animated("known-moving"), Some(true));
    }
}
