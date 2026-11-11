//! What people publish about themselves, and what this account publishes: nicknames (XEP-0172), mood (XEP-0107), activity
//! (XEP-0108), tune (XEP-0118) and location (XEP-0080) - each a PEP node a contact's server sends to us when it changes, once
//! our capabilities say `+notify` - and the vCard (vCard4 over PEP, XEP-0292, with the older vcard-temp, XEP-0054).
//!
//! Read: kept per contact for as long as the session lasts, shown in the member list (a tune, a mood or an activity when there
//! are no status words) and on the person's card with their vCard and the clients they use. Location is shown in words - the
//! town, the country - never the coordinates. Written: this account's own name, nickname, e-mail, website and words about
//! itself, published to both vCard forms (keeping the picture the older form carries) and the nickname node.

use super::session::Handle;
use super::xml::{self, Attr};
use crate::state::AppState;
use serde_json::{json, Value};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;

pub const NICK: &str = "http://jabber.org/protocol/nick";
pub const MOOD: &str = "http://jabber.org/protocol/mood";
pub const ACTIVITY: &str = "http://jabber.org/protocol/activity";
pub const TUNE: &str = "http://jabber.org/protocol/tune";
pub const GEOLOC: &str = "http://jabber.org/protocol/geoloc";
pub const VCARD4: &str = "urn:ietf:params:xml:ns:vcard-4.0";
pub const VCARD4_NODE: &str = "urn:xmpp:vcard4";
pub const VCARD_TEMP: &str = "vcard-temp";

/// The nodes we ask to be told about (`+notify` in our capabilities).
pub const NOTIFY: &[&str] = &["http://jabber.org/protocol/nick+notify", "http://jabber.org/protocol/mood+notify", "http://jabber.org/protocol/activity+notify", "http://jabber.org/protocol/tune+notify", "http://jabber.org/protocol/geoloc+notify"];

/// What one person currently publishes.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Published {
    pub nick: Option<String>,
    /// The mood (its element name, `happy`) and the words with it.
    pub mood: Option<(String, String)>,
    /// The activity (`relaxing`, and the specific one, `reading`) and the words with it.
    pub activity: Option<(String, Option<String>, String)>,
    /// Artist and title (and the album), as one line.
    pub tune: Option<String>,
    /// Where, in words: the place, the town, the country.
    pub location: Option<String>,
}

fn words(name: &str) -> String {
    name.replace('_', " ")
}

impl Published {
    /// One line for the member list: a tune first (what is playing changes), then a mood, then an activity.
    pub fn line(&self) -> Option<Value> {
        if let Some(t) = &self.tune {
            return Some(json!({ "kind": "music", "text": t }));
        }
        if let Some((m, text)) = &self.mood {
            return Some(json!({ "kind": "custom", "text": if text.is_empty() { format!("Feeling {}", words(m)) } else { format!("Feeling {}: {text}", words(m)) } }));
        }
        if let Some((general, specific, text)) = &self.activity {
            let what = specific.as_deref().map(words).unwrap_or_else(|| words(general));
            return Some(json!({ "kind": "custom", "text": if text.is_empty() { what } else { format!("{what}: {text}") } }));
        }
        None
    }
}

fn text_of(e: &Element, name: &str) -> Option<String> {
    e.children().find(|c| c.name() == name).map(|c| c.text().trim().to_string()).filter(|t| !t.is_empty())
}

/// Reads one published item into what a person publishes. An empty item (or a retraction) clears that part.
pub fn apply(p: &mut Published, node: &str, payload: Option<&Element>) {
    match node {
        NICK => p.nick = payload.map(|e| e.text().trim().to_string()).filter(|t| !t.is_empty()),
        MOOD => {
            p.mood = payload.and_then(|e| {
                let m = e.children().find(|c| c.name() != "text")?;
                Some((m.name().to_string(), text_of(e, "text").unwrap_or_default()))
            })
        }
        ACTIVITY => {
            p.activity = payload.and_then(|e| {
                let general = e.children().find(|c| c.name() != "text")?;
                let specific = general.children().next().map(|s| s.name().to_string());
                Some((general.name().to_string(), specific, text_of(e, "text").unwrap_or_default()))
            })
        }
        TUNE => {
            p.tune = payload.and_then(|e| {
                let (artist, title, source) = (text_of(e, "artist"), text_of(e, "title"), text_of(e, "source"));
                let main = match (&artist, &title) {
                    (Some(a), Some(t)) => format!("{a} – {t}"),
                    (None, Some(t)) => t.clone(),
                    (Some(a), None) => a.clone(),
                    (None, None) => return None,
                };
                Some(match source {
                    Some(s) => format!("{main} ({s})"),
                    None => main,
                })
            })
        }
        GEOLOC => {
            // In words only: a person's coordinates are not drawn on a card, whatever their client published.
            p.location = payload.and_then(|e| {
                let parts: Vec<String> = ["text", "building", "street", "locality", "region", "country"].iter().filter_map(|n| text_of(e, n)).collect();
                (!parts.is_empty()).then(|| parts.join(", "))
            })
        }
        _ => {}
    }
}

/// A PEP notification from somebody: one of the nodes above changed.
pub fn event(state: &AppState, handle: &Handle, from: &BareJid, event: &Element) -> bool {
    let mut changed = false;
    for items in event.children().filter(|c| c.name() == "items") {
        let Some(node) = items.attr("node") else { continue };
        if ![NICK, MOOD, ACTIVITY, TUNE, GEOLOC].contains(&node) {
            continue;
        }
        let item = items.children().find(|c| c.name() == "item");
        let retracted = items.children().any(|c| c.name() == "retract");
        let payload = if retracted { None } else { item.and_then(|i| i.children().next()) };
        let mut s = handle.session();
        let p = s.published.entry(from.clone()).or_default();
        let before = p.clone();
        apply(p, node, payload);
        changed |= *p != before;
    }
    if changed {
        super::roster::publish_presence(state, handle, from);
    }
    changed
}

/// One vCard, in either form, as labelled lines and a name.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Card {
    pub full_name: Option<String>,
    pub nickname: Option<String>,
    pub lines: Vec<(String, String)>,
    pub note: Option<String>,
}

/// vCard4 (`<vcard xmlns='urn:ietf:params:xml:ns:vcard-4.0'>`).
pub fn read_vcard4(v: &Element) -> Card {
    let field = |name: &str, inner: &str| v.children().filter(|c| c.name() == name).filter_map(|c| text_of(c, inner)).collect::<Vec<_>>();
    let mut c = Card { full_name: field("fn", "text").into_iter().next(), nickname: field("nickname", "text").into_iter().next(), note: field("note", "text").into_iter().next(), ..Default::default() };
    for (label, name, inner) in [("E-mail", "email", "text"), ("Website", "url", "uri"), ("Organisation", "org", "text"), ("Title", "title", "text"), ("Role", "role", "text"), ("Telephone", "tel", "uri"), ("Birthday", "bday", "date"), ("Pronouns", "pronouns", "text")] {
        for value in field(name, inner) {
            c.lines.push((label.to_string(), value.trim_start_matches("tel:").to_string()));
        }
    }
    for adr in v.children().filter(|c| c.name() == "adr") {
        let parts: Vec<String> = ["locality", "region", "country"].iter().filter_map(|n| text_of(adr, n)).collect();
        if !parts.is_empty() {
            c.lines.push(("Lives in".to_string(), parts.join(", ")));
        }
    }
    c
}

/// vcard-temp (`<vCard xmlns='vcard-temp'>`).
pub fn read_vcard_temp(v: &Element) -> Card {
    // DESC is vcard-temp's; NOTE is what a server that makes it from vCard4 (Prosody) writes.
    let mut c = Card { full_name: text_of(v, "FN"), nickname: text_of(v, "NICKNAME"), note: text_of(v, "DESC").or_else(|| text_of(v, "NOTE")), ..Default::default() };
    for e in v.children().filter(|e| e.name() == "EMAIL") {
        if let Some(a) = text_of(e, "USERID") {
            c.lines.push(("E-mail".into(), a));
        }
    }
    if let Some(u) = text_of(v, "URL") {
        c.lines.push(("Website".into(), u));
    }
    if let Some(o) = v.children().find(|e| e.name() == "ORG").and_then(|o| text_of(o, "ORGNAME")) {
        c.lines.push(("Organisation".into(), o));
    }
    for (label, name) in [("Title", "TITLE"), ("Role", "ROLE"), ("Birthday", "BDAY")] {
        if let Some(t) = text_of(v, name) {
            c.lines.push((label.into(), t));
        }
    }
    for t in v.children().filter(|e| e.name() == "TEL") {
        if let Some(n) = text_of(t, "NUMBER") {
            c.lines.push(("Telephone".into(), n));
        }
    }
    for adr in v.children().filter(|e| e.name() == "ADR") {
        let parts: Vec<String> = ["LOCALITY", "REGION", "CTRY"].iter().filter_map(|n| text_of(adr, n)).collect();
        if !parts.is_empty() {
            c.lines.push(("Lives in".into(), parts.join(", ")));
        }
    }
    c
}

/// Somebody's vCard: vCard4 from their PEP, else vcard-temp.
pub async fn fetch_card(handle: &Handle, jid: &BareJid) -> Option<Card> {
    let to: Option<Jid> = jid.as_str().parse().ok();
    let pubsub = Element::builder("pubsub", super::ns::PUBSUB).append(Element::builder("items", super::ns::PUBSUB).at("node", VCARD4_NODE).build()).build();
    if let Ok(Some(answer)) = handle.request(to.clone(), false, pubsub).await {
        if let Some(v) = answer.children().find(|c| c.name() == "items").and_then(|i| i.children().find(|c| c.name() == "item")).and_then(|i| i.children().find(|c| c.is("vcard", VCARD4))) {
            return Some(read_vcard4(v));
        }
    }
    match handle.request(to, false, xml::empty("vCard", VCARD_TEMP)).await {
        Ok(Some(v)) if v.is("vCard", VCARD_TEMP) => Some(read_vcard_temp(&v)),
        _ => None,
    }
}

/// The client a resource runs, by asking it (XEP-0092): name and version.
async fn client_of(handle: &Handle, full: &str) -> Option<String> {
    let to: Jid = full.parse().ok()?;
    let ask = handle.request(Some(to), false, xml::empty("query", super::ns::VERSION));
    let answer = tokio::time::timeout(std::time::Duration::from_secs(5), ask).await.ok()?.ok()??;
    let name = text_of(&answer, "name")?;
    Some(match text_of(&answer, "version") {
        Some(v) => format!("{name} {v}"),
        None => name,
    })
}

/// The card for a person: their vCard, what they publish, their presence and the clients they use.
pub async fn profile(state: &AppState, account_id: &str, buffer_id: &str, who: &str, user_id: &str) -> Value {
    let mut card = crate::profile::pending("xmpp", account_id, who);
    card["pending"] = json!(false);
    let Some(handle) = state.runtime.xmpp_handle(account_id) else { return card };
    // The address: what the window names them by, else the conversation's own person, else a room's occupant as the room sees them.
    let buffer = state.runtime.get_buffer(buffer_id);
    let jid = [user_id, who].iter().find_map(|s| super::connect::normalise_jid(s).ok()).or_else(|| {
        let b = buffer.as_ref()?;
        if b.kind == "dm" {
            b.remote_id.as_deref().and_then(|r| r.parse::<BareJid>().ok())
        } else {
            let room: BareJid = b.remote_id.as_deref()?.parse().ok()?;
            let s = handle.session();
            s.rooms.get(&room).and_then(|r| r.occupants.get(who)).and_then(|o| o.jid.clone())
        }
    });
    let Some(jid) = jid else { return card };
    card["id"] = json!(jid.to_string());
    card["handle"] = json!(jid.to_string());
    let (presence, published, avatar, resources) = {
        let s = handle.session();
        let p = s.best_presence(&jid).cloned();
        let resources: Vec<String> = s.presence.get(&jid).map(|m| m.keys().cloned().collect()).unwrap_or_default();
        (p, s.published.get(&jid).cloned().unwrap_or_default(), s.avatars.get(&jid).cloned(), resources)
    };
    if let Some(a) = avatar {
        card["avatarUrl"] = json!(a);
    }
    match &presence {
        Some(p) => {
            card["status"] = json!(super::roster::status_word(&p.show));
            if !p.status.is_empty() {
                card["away"] = json!(p.status);
            }
        }
        None => card["status"] = json!("offline"),
    }
    if let Some(line) = published.line() {
        card["activity"] = line;
    }
    if let Some(found) = fetch_card(&handle, &jid).await {
        if let Some(n) = found.full_name.clone().or(found.nickname.clone()).or(published.nick.clone()) {
            card["name"] = json!(n);
        }
        if let Some(nick) = found.nickname.filter(|n| Some(n) != found.full_name.as_ref()) {
            crate::profile::note(&mut card, "Nickname", nick);
        }
        for (label, value) in found.lines {
            crate::profile::note(&mut card, &label, value);
        }
        if let Some(note) = found.note {
            crate::profile::note(&mut card, "About", note);
        }
    } else if let Some(n) = &published.nick {
        card["name"] = json!(n);
    }
    if let Some(n) = &published.nick {
        if card["name"].as_str() != Some(n.as_str()) {
            crate::profile::note(&mut card, "Calls themselves", n.clone());
        }
    }
    if let Some((m, text)) = &published.mood {
        crate::profile::note(&mut card, "Mood", if text.is_empty() { words(m) } else { format!("{} - {text}", words(m)) });
    }
    if let Some((general, specific, text)) = &published.activity {
        let what = match specific {
            Some(s) => format!("{} ({})", words(general), words(s)),
            None => words(general),
        };
        crate::profile::note(&mut card, "Doing", if text.is_empty() { what } else { format!("{what} - {text}") });
    }
    if let Some(t) = &published.tune {
        crate::profile::note(&mut card, "Listening to", t.clone());
    }
    if let Some(l) = &published.location {
        crate::profile::note(&mut card, "Where", l.clone());
    }
    // The clients behind each device that is online, asked at once.
    let asks = resources.iter().map(|r| {
        let full = format!("{jid}/{r}");
        let handle = handle.clone();
        async move { client_of(&handle, &full).await }
    });
    let clients: Vec<String> = futures::future::join_all(asks).await.into_iter().flatten().collect();
    if !clients.is_empty() {
        crate::profile::note(&mut card, if clients.len() == 1 { "Client" } else { "Clients" }, clients.join(", "));
    }
    card
}

/// This account's own profile as it can be edited: the fields moho writes, from whichever vCard the server has.
pub async fn own(handle: &Handle) -> Value {
    let me = handle.own();
    let card = fetch_card(handle, &me).await.unwrap_or_default();
    let pick = |label: &str| card.lines.iter().find(|(l, _)| l == label).map(|(_, v)| v.clone()).unwrap_or_default();
    let nick = {
        let pubsub = Element::builder("pubsub", super::ns::PUBSUB).append(Element::builder("items", super::ns::PUBSUB).at("node", NICK).build()).build();
        let to: Option<Jid> = me.as_str().parse().ok();
        match handle.request(to, false, pubsub).await {
            Ok(Some(a)) => a.children().find(|c| c.name() == "items").and_then(|i| i.children().find(|c| c.name() == "item")).and_then(|i| i.children().find(|c| c.is("nick", NICK))).map(|n| n.text().trim().to_string()),
            _ => None,
        }
    };
    json!({
        "fullName": card.full_name.unwrap_or_default(),
        "nickname": nick.or(card.nickname).unwrap_or_default(),
        "email": pick("E-mail"),
        "url": pick("Website"),
        "note": card.note.unwrap_or_default(),
    })
}

/// What the account says about itself.
#[derive(Debug, Default, Clone)]
pub struct Own {
    pub full_name: String,
    pub nickname: String,
    pub email: String,
    pub url: String,
    pub note: String,
}

/// The vCard4 element for these fields.
pub fn vcard4_of(o: &Own) -> Element {
    let mut v = Element::builder("vcard", VCARD4).build();
    let mut put = |name: &str, inner: &str, value: &str| {
        if !value.trim().is_empty() {
            v.append_child(Element::builder(name, VCARD4).append(Element::builder(inner, VCARD4).append(value.trim()).build()).build());
        }
    };
    put("fn", "text", &o.full_name);
    put("nickname", "text", &o.nickname);
    put("email", "text", &o.email);
    put("url", "uri", &o.url);
    put("note", "text", &o.note);
    v
}

/// The vcard-temp element: the old one with these fields replaced, everything else (the picture first of all) kept.
pub fn vcard_temp_of(old: Option<&Element>, o: &Own) -> Element {
    let ours = ["FN", "NICKNAME", "EMAIL", "URL", "DESC"];
    let mut v = Element::builder("vCard", VCARD_TEMP).build();
    if let Some(old) = old {
        for c in old.children().filter(|c| !ours.contains(&c.name())) {
            v.append_child(c.clone());
        }
    }
    let simple = |name: &str, value: &str| Element::builder(name, VCARD_TEMP).append(value.trim()).build();
    for (name, value) in [("FN", &o.full_name), ("NICKNAME", &o.nickname), ("URL", &o.url), ("DESC", &o.note)] {
        if !value.trim().is_empty() {
            v.append_child(simple(name, value));
        }
    }
    if !o.email.trim().is_empty() {
        v.append_child(Element::builder("EMAIL", VCARD_TEMP).append(xml::empty("INTERNET", VCARD_TEMP)).append(simple("USERID", &o.email)).build());
    }
    v
}

fn publish(node: &str, item: Element, access: &str) -> Element {
    let mut options = Element::builder("x", super::ns::DATA).at("type", "submit").build();
    for (var, value) in [("FORM_TYPE", "http://jabber.org/protocol/pubsub#publish-options"), ("pubsub#persist_items", "true"), ("pubsub#access_model", access)] {
        options.append_child(Element::builder("field", super::ns::DATA).at("var", var).append(Element::builder("value", super::ns::DATA).append(value).build()).build());
    }
    Element::builder("pubsub", super::ns::PUBSUB)
        .append(Element::builder("publish", super::ns::PUBSUB).at("node", node).append(Element::builder("item", super::ns::PUBSUB).at("id", "current").append(item).build()).build())
        .append(Element::builder("publish-options", super::ns::PUBSUB).append(options).build())
        .build()
}

/// Publishes the account's own profile: vCard4 and vcard-temp (both are read by somebody), and the nickname node.
pub async fn set_own(handle: &Handle, o: &Own) -> Result<(), String> {
    let me: Jid = handle.own().as_str().parse().map_err(|_| "the account's address is not usable".to_string())?;
    handle.request(None, true, publish(VCARD4_NODE, vcard4_of(o), "open")).await.map_err(|e| e.words())?;
    let old = match handle.request(Some(me), false, xml::empty("vCard", VCARD_TEMP)).await {
        Ok(Some(v)) if v.is("vCard", VCARD_TEMP) => Some(v),
        _ => None,
    };
    if let Err(e) = handle.request(None, true, vcard_temp_of(old.as_ref(), o)).await {
        // A server without vcard-temp still has the vCard4 one.
        tracing::info!("xmpp[{}]: the older vCard was not kept: {}", handle.account_id, e.words());
    }
    let nick = Element::builder("nick", NICK).append(o.nickname.trim()).build();
    handle.request(None, true, publish(NICK, nick, "presence")).await.map_err(|e| e.words())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn el(s: &str) -> Element {
        s.parse().unwrap()
    }

    #[test]
    fn what_people_publish_is_read_and_cleared() {
        let mut p = Published::default();
        apply(&mut p, TUNE, Some(&el("<tune xmlns='http://jabber.org/protocol/tune'><artist>Yes</artist><title>Heart of the Sunrise</title><source>Fragile</source></tune>")));
        apply(&mut p, MOOD, Some(&el("<mood xmlns='http://jabber.org/protocol/mood'><happy/><text>Yay, the weekend!</text></mood>")));
        apply(&mut p, ACTIVITY, Some(&el("<activity xmlns='http://jabber.org/protocol/activity'><relaxing><partying/></relaxing><text>My nurse's birthday!</text></activity>")));
        apply(&mut p, GEOLOC, Some(&el("<geoloc xmlns='http://jabber.org/protocol/geoloc'><lat>45.44</lat><lon>12.33</lon><locality>Venice</locality><country>Italy</country></geoloc>")));
        apply(&mut p, NICK, Some(&el("<nick xmlns='http://jabber.org/protocol/nick'>Ishmael</nick>")));
        assert_eq!(p.tune.as_deref(), Some("Yes – Heart of the Sunrise (Fragile)"));
        assert_eq!(p.mood, Some(("happy".into(), "Yay, the weekend!".into())));
        assert_eq!(p.activity, Some(("relaxing".into(), Some("partying".into()), "My nurse's birthday!".into())));
        assert_eq!(p.location.as_deref(), Some("Venice, Italy"), "in words, without the coordinates");
        assert_eq!(p.nick.as_deref(), Some("Ishmael"));
        assert_eq!(p.line().unwrap()["kind"], "music");
        apply(&mut p, TUNE, Some(&el("<tune xmlns='http://jabber.org/protocol/tune'/>")));
        assert_eq!(p.tune, None, "an empty tune is nothing playing");
        assert_eq!(p.line().unwrap()["text"], "Feeling happy: Yay, the weekend!");
    }

    #[test]
    fn both_vcard_forms_are_read_alike() {
        let v4 = read_vcard4(&el("<vcard xmlns='urn:ietf:params:xml:ns:vcard-4.0'><fn><text>Peter Saint-Andre</text></fn><nickname><text>stpeter</text></nickname><email><text>peter@example.org</text></email><url><uri>https://stpeter.im/</uri></url><note><text>More info</text></note><adr><locality>Parker</locality><country>USA</country></adr></vcard>"));
        let temp = read_vcard_temp(&el("<vCard xmlns='vcard-temp'><FN>Peter Saint-Andre</FN><NICKNAME>stpeter</NICKNAME><EMAIL><INTERNET/><USERID>peter@example.org</USERID></EMAIL><URL>https://stpeter.im/</URL><DESC>More info</DESC><ADR><LOCALITY>Parker</LOCALITY><CTRY>USA</CTRY></ADR></vCard>"));
        assert_eq!(v4, temp);
        assert_eq!(v4.lines.iter().find(|(l, _)| l == "Lives in").map(|(_, v)| v.as_str()), Some("Parker, USA"));
    }

    #[test]
    fn the_older_vcard_keeps_its_picture_when_the_fields_are_written() {
        let old = el("<vCard xmlns='vcard-temp'><FN>Old</FN><PHOTO><TYPE>image/png</TYPE><BINVAL>AAAA</BINVAL></PHOTO></vCard>");
        let o = Own { full_name: "New Name".into(), email: "a@b.example".into(), ..Default::default() };
        let v = vcard_temp_of(Some(&old), &o);
        let s = String::from(&v);
        assert!(s.contains("<BINVAL>AAAA</BINVAL>") && s.contains("<FN>New Name</FN>") && !s.contains("<FN>Old</FN>") && s.contains("<USERID>a@b.example</USERID>"), "{s}");
        assert_eq!(read_vcard4(&vcard4_of(&o)).full_name.as_deref(), Some("New Name"));
    }
}
