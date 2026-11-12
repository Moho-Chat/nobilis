//! Calls between two people (XEP-0353 Jingle Message Initiation, XEP-0166 Jingle).
//!
//! The media is the window's: a call is WebRTC, and the window is a browser (see `lib/webrtc.ts`), so what crosses this process
//! is signalling only. The window already speaks one dialect of that for Matrix - an offer, an answer, network candidates and a
//! hangup - so this module translates between that and what other XMPP clients speak, and the window's whole call machinery
//! (ringing, the bar, mute, the camera) is the same for both. The events keep the name `matrixCall` for that reason: it is the
//! window's call vocabulary, not a statement about the protocol under it.
//!
//! A call goes: the caller says it wants to call (`propose`, to the person), a device that is willing says so (`proceed`, to the
//! caller's own device), and only then does the caller send the session (`session-initiate`) to that one device, which answers
//! with `session-accept`. Candidates trickle both ways as `transport-info`. The receiving window has not been asked yet when the
//! `propose` arrives and there is no offer to show, so it is rung with the kind of call and answers by saying `proceed`; the offer
//! comes next and is answered in the ordinary way.

pub mod jingle;
pub mod renegotiate;
pub mod sdp;

use super::session::Handle;
use super::xml::{self, child, Attr};
use crate::model::Buffer;
use crate::state::AppState;
use sdp::{Candidate, Media, Sdp};
use serde_json::{json, Value};
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::parsers::message::{Id, Message};
use tokio_xmpp::parsers::stanza_error::{DefinedCondition, ErrorType, StanzaError};
use tokio_xmpp::Stanza;

pub const JMI: &str = "urn:xmpp:jingle-message:0";
pub const EXTDISCO: &str = "urn:xmpp:extdisco:2";
const UNKNOWN_SESSION: &str = "urn:xmpp:jingle:errors:1";

/// How far a call has got.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    /// Offered, and nobody has agreed to take it.
    Proposed,
    /// Agreed to, and the session has not arrived (the receiving side).
    Proceeded,
    /// The session has been sent.
    Initiated,
    /// Answered.
    Accepted,
}

#[derive(Debug, Clone)]
pub struct Call {
    pub buffer_id: String,
    pub peer: BareJid,
    /// The other end's device, once known.
    pub full: Option<Jid>,
    pub outgoing: bool,
    pub phase: Phase,
    /// Our offer, kept until somebody agrees to take it.
    pub offer: Option<Sdp>,
    /// The names of the contents, in the order of the descriptions: candidates are placed by them.
    pub mids: Vec<String>,
    /// Each content's ICE credentials as we sent them, which a transport-info repeats.
    pub creds: Vec<(String, String, String)>,
    /// Candidates of ours with nowhere to go yet.
    pub queued: Vec<Candidate>,
    /// The other end's candidates that arrived before the window could use them, as (mid, index, candidate).
    pub held: Vec<(String, usize, Candidate)>,
    /// Whether the window has the other end's description, so a candidate for it is usable.
    pub ready: bool,
    /// The last description each end gave, which a change to the call is read against.
    pub local: Option<Sdp>,
    pub remote: Option<Sdp>,
    /// Our offer of a change, while the other end has not answered it; and theirs, while the window has not.
    pub pending: Option<Sdp>,
    pub peer_offer: Option<Sdp>,
    /// Who made each section (`initiator` or `responder`), where it is not the initiator.
    pub creators: Vec<(String, String)>,
}

impl Call {
    fn new(buffer_id: &str, peer: BareJid, full: Option<Jid>, outgoing: bool, phase: Phase) -> Self {
        Self { buffer_id: buffer_id.to_string(), peer, full, outgoing, phase, offer: None, mids: Vec::new(), creds: Vec::new(), queued: Vec::new(), held: Vec::new(), ready: false, local: None, remote: None, pending: None, peer_offer: None, creators: Vec::new() }
    }
}

/// Tells the window something happened in a call. `who` is (the other person, whether this is our own signalling coming back,
/// which of their devices).
fn emit(state: &AppState, handle: &Handle, buffer_id: &str, kind: &str, sid: &str, who: (&str, bool, &str), mut content: Value) {
    let (from, own, party) = who;
    content["call_id"] = json!(sid);
    content["party_id"] = json!(party);
    content["version"] = json!("1");
    state.events.emit("matrixCall", json!({ "accountId": handle.account_id, "bufferId": buffer_id, "kind": kind, "callId": sid, "from": from, "own": own, "partyId": party, "content": content }));
}

/// Only people on the contact list may ring: anybody else's call is not shown (and nobody is told).
fn known(handle: &Handle, who: &BareJid) -> bool {
    handle.session().roster.contains_key(who)
}

fn reply(handle: &Handle, to: Option<Jid>, id: &str, error: Option<(DefinedCondition, Option<Element>)>) -> Stanza {
    let _ = handle;
    Stanza::Iq(match error {
        None => Iq::Result { from: None, to, id: id.to_string(), payload: None },
        Some((condition, extra)) => {
            let mut e = StanzaError::new(ErrorType::Cancel, condition, "en", "");
            if let Some(x) = extra {
                e.other = Some(x);
            }
            Iq::Error { from: None, to, id: id.to_string(), payload: None, error: e }
        }
    })
}

fn jmi_message(to: Jid, tag: &str, sid: &str, media: &[String]) -> Message {
    let mut m = Message::chat(to);
    m.id = Some(Id(xml::new_id()));
    let mut e = Element::builder(tag, JMI).at("id", sid).build();
    for kind in media {
        e.append_child(Element::builder("description", jingle::RTP).at("media", kind.as_str()).build());
    }
    m.payloads.push(e);
    m.payloads.push(xml::empty("store", super::ns::HINTS));
    m
}

fn media_kinds(sdp: &Sdp) -> Vec<String> {
    let mut kinds: Vec<String> = Vec::new();
    for m in &sdp.media {
        if !kinds.contains(&m.kind) {
            kinds.push(m.kind.clone());
        }
    }
    kinds
}

/// Sends a jingle iq and watches for what became of it: a refusal of the session itself ends the call.
async fn send_jingle(state: &AppState, handle: &Handle, sid: &str, to: Jid, jingle: Element, fatal: bool) {
    let rx = handle.fire(Some(to), jingle).await;
    let (state, handle, sid) = (state.clone(), handle.clone(), sid.to_string());
    tokio::spawn(async move {
        if let Ok(Ok(Iq::Error { error, .. })) = tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
            tracing::debug!("xmpp[{}]: a call stanza was refused: {:?}", handle.account_id, error.defined_condition);
            if fatal {
                let call = handle.session().calls.remove(&sid);
                if let Some(call) = call {
                    emit(&state, &handle, &call.buffer_id, "m.call.hangup", &sid, (call.peer.as_str(), false, call.peer.as_str()), json!({ "reason": "ice_failed" }));
                }
            }
        }
    });
}

fn jingle_el(action: &str, sid: &str) -> Element {
    Element::builder("jingle", jingle::JINGLE).at("action", action).at("sid", sid).build()
}

/// What the window says, as the other end hears it.
pub async fn from_window(state: &AppState, handle: &Handle, buffer: &Buffer, event_type: &str, content: &Value) -> Result<(), String> {
    let sid = content["call_id"].as_str().filter(|s| !s.is_empty()).ok_or("a call event with no call id")?.to_string();
    match event_type {
        "m.call.invite" => {
            if buffer.kind != "dm" {
                return Err("calls on XMPP are between two people".into());
            }
            let peer: BareJid = buffer.remote_id.as_deref().and_then(|j| j.parse().ok()).ok_or("this is not a conversation with a person")?;
            let offer = sdp::parse(content["offer"]["sdp"].as_str().ok_or("an invitation with no offer")?);
            if offer.media.is_empty() {
                return Err("an offer with nothing in it".into());
            }
            let media = media_kinds(&offer);
            let direct = direct_device(handle, &peer).await;
            let mut call = Call::new(&buffer.id, peer.clone(), direct.clone(), true, if direct.is_some() { Phase::Initiated } else { Phase::Proposed });
            call.mids = offer.media.iter().map(|m| m.mid.clone()).collect();
            call.creds = offer.media.iter().map(|m| (m.mid.clone(), m.ufrag.clone(), m.pwd.clone())).collect();
            call.local = Some(offer.clone());
            match direct {
                // A device that takes Jingle calls but not the message exchange (an older client): the session goes to it.
                Some(full) => {
                    handle.session().calls.insert(sid.clone(), call);
                    send_jingle(state, handle, &sid, full, session_initiate(handle, &sid, &offer), true).await;
                }
                None => {
                    call.offer = Some(offer);
                    handle.session().calls.insert(sid.clone(), call);
                    handle.send_message(jmi_message(Jid::from(peer), "propose", &sid, &media)).await;
                }
            }
            Ok(())
        }
        "m.call.proceed" => {
            let to = {
                let mut s = handle.session();
                let call = s.calls.get_mut(&sid).filter(|c| !c.outgoing && c.phase == Phase::Proposed).ok_or("no such call to take")?;
                call.phase = Phase::Proceeded;
                call.full.clone().unwrap_or_else(|| Jid::from(call.peer.clone()))
            };
            handle.send_message(jmi_message(to, "proceed", &sid, &[])).await;
            Ok(())
        }
        "m.call.candidates" => {
            let list: Vec<Candidate> = content["candidates"].as_array().into_iter().flatten().filter_map(|c| c["candidate"].as_str().and_then(sdp::parse_candidate)).collect();
            let indices: Vec<Option<String>> = content["candidates"].as_array().into_iter().flatten().map(|c| c["sdpMid"].as_str().map(str::to_string)).collect();
            let (to, ready) = {
                let mut s = handle.session();
                let call = s.calls.get_mut(&sid).ok_or("no such call")?;
                let ready = matches!(call.phase, Phase::Initiated | Phase::Accepted) && call.full.is_some();
                if !ready {
                    call.queued.extend(list.iter().cloned());
                    return Ok(());
                }
                (call.full.clone().unwrap(), ready)
            };
            let _ = ready;
            send_transport(state, handle, &sid, to, list, indices).await;
            Ok(())
        }
        "m.call.answer" => {
            let answer = sdp::parse(content["answer"]["sdp"].as_str().ok_or("an answer with nothing in it")?);
            let (to, held, buffer_id, peer) = {
                let mut s = handle.session();
                let call = s.calls.get_mut(&sid).filter(|c| !c.outgoing && c.phase == Phase::Initiated).ok_or("no such call to answer")?;
                call.phase = Phase::Accepted;
                call.ready = true;
                call.creds = answer.media.iter().map(|m| (m.mid.clone(), m.ufrag.clone(), m.pwd.clone())).collect();
                call.local = Some(answer.clone());
                (call.full.clone().ok_or("nobody to answer")?, std::mem::take(&mut call.held), call.buffer_id.clone(), call.peer.clone())
            };
            let mut j = Element::builder("jingle", jingle::JINGLE).at("action", "session-accept").at("sid", sid.as_str()).at("responder", handle.bound.to_string()).build();
            for g in jingle::groups(&answer) {
                j.append_child(g);
            }
            for c in jingle::contents(&answer, "initiator", false) {
                j.append_child(c);
            }
            send_jingle(state, handle, &sid, to, j, true).await;
            release(state, handle, &sid, &buffer_id, peer.as_str(), held);
            Ok(())
        }
        "m.call.hangup" | "m.call.reject" => {
            let reason = content["reason"].as_str().unwrap_or("user_hangup").to_string();
            let call = handle.session().calls.remove(&sid);
            let Some(call) = call else { return Ok(()) };
            match (call.phase, call.outgoing) {
                (Phase::Proposed, true) => handle.send_message(jmi_message(Jid::from(call.peer), "retract", &sid, &[])).await,
                (Phase::Proposed, false) | (Phase::Proceeded, false) => {
                    let to = call.full.unwrap_or_else(|| Jid::from(call.peer));
                    handle.send_message(jmi_message(to, "reject", &sid, &[])).await
                }
                (phase, outgoing) => {
                    let Some(to) = call.full else { return Ok(()) };
                    let condition = match (reason.as_str(), phase, outgoing) {
                        ("user_busy", ..) => "busy",
                        (_, Phase::Initiated, true) => "cancel",
                        (_, Phase::Initiated, false) => "decline",
                        _ => "success",
                    };
                    let mut j = jingle_el("session-terminate", &sid);
                    j.append_child(Element::builder("reason", jingle::JINGLE).append(Element::builder(condition, jingle::JINGLE).build()).build());
                    send_jingle(state, handle, &sid, to, j, false).await;
                }
            }
            Ok(())
        }
        "m.call.negotiate" => negotiate(state, handle, &sid, content).await,
        _ => Ok(()),
    }
}

fn session_initiate(handle: &Handle, sid: &str, offer: &Sdp) -> Element {
    let mut j = Element::builder("jingle", jingle::JINGLE).at("action", "session-initiate").at("sid", sid).at("initiator", handle.bound.to_string()).build();
    for g in jingle::groups(offer) {
        j.append_child(g);
    }
    for c in jingle::contents(offer, "initiator", true) {
        j.append_child(c);
    }
    j
}

/// The device to send a call's session to directly: when none of the person's online devices speaks Jingle Message Initiation
/// but one takes Jingle calls (older clients, which ring on the session itself). `None` to propose, as every newer client expects.
async fn direct_device(handle: &Handle, peer: &BareJid) -> Option<Jid> {
    let resources: Vec<String> = handle.session().presence.get(peer).map(|m| m.keys().cloned().collect()).unwrap_or_default();
    let asks = resources.into_iter().filter_map(|r| peer.with_resource_str(&r).ok()).map(|full| async move {
        let q = xml::empty("query", super::ns::DISCO_INFO);
        let answer = tokio::time::timeout(std::time::Duration::from_secs(4), handle.request(Some(Jid::from(full.clone())), false, q)).await.ok()?.ok()??;
        let features: Vec<String> = answer.children().filter(|c| c.name() == "feature").filter_map(|f| f.attr("var").map(str::to_string)).collect();
        Some((full, features))
    });
    let found: Vec<_> = futures::future::join_all(asks).await.into_iter().flatten().collect();
    if found.is_empty() || found.iter().any(|(_, f)| f.iter().any(|x| x == JMI)) {
        return None;
    }
    found.into_iter().find(|(_, f)| f.iter().any(|x| x == jingle::RTP)).map(|(j, _)| Jid::from(j))
}

/// Who made a section: the initiator unless it was added later by the other end.
fn creator_of(call: &Call, mid: &str) -> String {
    call.creators.iter().find(|(m, _)| m == mid).map(|(_, c)| c.clone()).unwrap_or_else(|| "initiator".into())
}

/// The window changing the call (it offers the whole call again, or answers the other end's offer of a change).
async fn negotiate(state: &AppState, handle: &Handle, sid: &str, content: &Value) -> Result<(), String> {
    let description = &content["description"];
    let new = sdp::parse(description["sdp"].as_str().ok_or("a change with nothing in it")?);
    match description["type"].as_str() {
        Some("offer") => {
            let (to, changes, ours, buffer_id, peer) = {
                let mut s = handle.session();
                let call = s.calls.get_mut(sid).filter(|c| c.phase == Phase::Accepted).ok_or("the call is not up yet")?;
                if call.pending.is_some() {
                    return Err("a change to the call is already under way".into());
                }
                let local = call.local.clone().ok_or("the call has no description yet")?;
                let changes = renegotiate::changes(&local, &new);
                call.pending = Some(new.clone());
                let ours = if call.outgoing { "initiator" } else { "responder" };
                for mid in &changes.added {
                    call.creators.push((mid.clone(), ours.to_string()));
                }
                (call.full.clone().ok_or("nobody to tell")?, changes, ours, call.buffer_id.clone(), call.peer.clone())
            };
            let outgoing = ours == "initiator";
            let mut waiting = None;
            if !changes.added.is_empty() {
                let mut j = jingle_el("content-add", sid);
                let only: Sdp = Sdp { media: new.media.iter().filter(|m| changes.added.contains(&m.mid)).cloned().collect(), ..new.clone() };
                for c in jingle::contents(&only, ours, outgoing) {
                    j.append_child(c);
                }
                for g in jingle::groups(&new) {
                    j.append_child(g);
                }
                waiting = Some(handle.fire(Some(to.clone()), j).await);
            }
            if !changes.modified.is_empty() {
                let mut j = jingle_el("content-modify", sid);
                {
                    let s = handle.session();
                    let call = s.calls.get(sid);
                    for mid in &changes.modified {
                        let Some(m) = new.media.iter().find(|m| m.mid == *mid) else { continue };
                        let creator = call.map(|c| creator_of(c, mid)).unwrap_or_else(|| "initiator".into());
                        j.append_child(Element::builder("content", jingle::JINGLE).at("creator", creator).at("name", mid.as_str()).at("senders", jingle::senders(&m.direction, outgoing)).build());
                    }
                }
                let rx = handle.fire(Some(to.clone()), j).await;
                // A modification is answered by the iq's result alone: that is the other end agreeing.
                if changes.added.is_empty() {
                    let (state, handle, sid) = (state.clone(), handle.clone(), sid.to_string());
                    let (buffer_id, peer) = (buffer_id.clone(), peer.clone());
                    tokio::spawn(async move {
                        match tokio::time::timeout(std::time::Duration::from_secs(20), rx).await {
                            Ok(Ok(Iq::Result { .. })) => answered(&state, &handle, &sid, &buffer_id, &peer, &Sdp::default()),
                            _ => {
                                if let Some(c) = handle.session().calls.get_mut(&sid) {
                                    c.pending = None;
                                }
                            }
                        }
                    });
                }
            }
            if changes.added.is_empty() && changes.modified.is_empty() {
                answered(state, handle, sid, &buffer_id, &peer, &Sdp::default());
            }
            // An addition the other end refuses outright is a section that is turned down.
            if let Some(rx) = waiting {
                let (state, handle, sid) = (state.clone(), handle.clone(), sid.to_string());
                tokio::spawn(async move {
                    if let Ok(Ok(Iq::Error { .. })) = tokio::time::timeout(std::time::Duration::from_secs(30), rx).await {
                        answered(&state, &handle, &sid, &buffer_id, &peer, &Sdp::default());
                    }
                });
            }
            Ok(())
        }
        Some("answer") => {
            let (to, offer, local, ours_is_initiator, creators) = {
                let mut s = handle.session();
                let call = s.calls.get_mut(sid).filter(|c| c.phase == Phase::Accepted).ok_or("the call is not up")?;
                let offer = call.peer_offer.take().ok_or("there is nothing to answer")?;
                let local = call.local.clone().ok_or("the call has no description yet")?;
                (call.full.clone().ok_or("nobody to tell")?, offer, local, call.outgoing, call.creators.clone())
            };
            let changes = renegotiate::changes(&local, &new);
            let their_role = if ours_is_initiator { "responder" } else { "initiator" };
            let creator = |mid: &str| creators.iter().find(|(m, _)| m == mid).map(|(_, c)| c.clone()).unwrap_or_else(|| their_role.to_string());
            let (taken, turned_down): (Vec<&String>, Vec<&String>) = changes.added.iter().partition(|mid| new.media.iter().any(|m| m.mid == **mid && m.port != 0));
            if !taken.is_empty() {
                let mut j = jingle_el("content-accept", sid);
                let only: Sdp = Sdp { media: new.media.iter().filter(|m| taken.contains(&&m.mid)).cloned().collect(), ..new.clone() };
                for c in jingle::contents(&only, &creator(&only.media[0].mid), ours_is_initiator) {
                    j.append_child(c);
                }
                send_jingle(state, handle, sid, to.clone(), j, false).await;
            }
            if !turned_down.is_empty() {
                let mut j = jingle_el("content-reject", sid);
                for mid in turned_down {
                    j.append_child(Element::builder("content", jingle::JINGLE).at("creator", creator(mid)).at("name", mid.as_str()).build());
                }
                send_jingle(state, handle, sid, to, j, false).await;
            }
            if let Some(c) = handle.session().calls.get_mut(sid) {
                c.local = Some(new);
                c.remote = Some(offer);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The other end has agreed to our offer of a change: the window gets the answer, built from what it said.
fn answered(state: &AppState, handle: &Handle, sid: &str, buffer_id: &str, peer: &BareJid, accepted: &Sdp) {
    let answer = {
        let mut s = handle.session();
        let Some(call) = s.calls.get_mut(sid) else { return };
        let (Some(offer), Some(remote)) = (call.pending.take(), call.remote.clone()) else { return };
        let answer = renegotiate::answer(&offer, &remote, accepted);
        call.local = Some(offer);
        call.remote = Some(answer.clone());
        answer
    };
    emit(state, handle, buffer_id, "m.call.negotiate", sid, (peer.as_str(), false, peer.as_str()), json!({ "description": { "type": "answer", "sdp": sdp::write(&answer) } }));
}

async fn send_transport(state: &AppState, handle: &Handle, sid: &str, to: Jid, list: Vec<Candidate>, mids: Vec<Option<String>>) {
    let by_mid: Vec<(String, Vec<Candidate>)> = {
        let s = handle.session();
        let Some(call) = s.calls.get(sid) else { return };
        let mut groups: Vec<(String, Vec<Candidate>)> = Vec::new();
        for (i, c) in list.into_iter().enumerate() {
            let mid = mids.get(i).cloned().flatten().or_else(|| call.mids.first().cloned()).unwrap_or_default();
            match groups.iter_mut().find(|(m, _)| *m == mid) {
                Some((_, v)) => v.push(c),
                None => groups.push((mid, vec![c])),
            }
        }
        groups
    };
    for (mid, candidates) in by_mid {
        let (ufrag, pwd) = handle.session().calls.get(sid).and_then(|c| c.creds.iter().find(|(m, ..)| *m == mid).map(|(_, u, p)| (u.clone(), p.clone()))).unwrap_or_default();
        let mut transport = Element::builder("transport", jingle::ICE).at("ufrag", ufrag).at("pwd", pwd).build();
        for c in &candidates {
            transport.append_child(jingle::candidate(c));
        }
        let mut j = jingle_el("transport-info", sid);
        j.append_child(Element::builder("content", jingle::JINGLE).at("creator", "initiator").at("name", mid).append(transport).build());
        send_jingle(state, handle, sid, to.clone(), j, false).await;
    }
}

/// Hands the window the other end's candidates it could not take until now.
fn release(state: &AppState, handle: &Handle, sid: &str, buffer_id: &str, from: &str, held: Vec<(String, usize, Candidate)>) {
    if held.is_empty() {
        return;
    }
    let list: Vec<Value> = held.iter().map(|(mid, i, c)| json!({ "candidate": sdp::write_candidate(c), "sdpMid": mid, "sdpMLineIndex": i })).collect();
    emit(state, handle, buffer_id, "m.call.candidates", sid, (from, false, from), json!({ "candidates": list }));
}

/// Call messages (XEP-0353). Returns whether the message was one. `carbon` is `Some(true)` for a copy of what another of this
/// account's devices sent.
pub fn message(state: &AppState, handle: &Handle, message: &Message, carbon: Option<bool>) -> bool {
    let Some(e) = message.payloads.iter().find(|p| p.ns() == JMI) else { return false };
    let Some(sid) = e.attr("id").map(str::to_string) else { return true };
    let Some(from) = message.from.clone() else { return true };
    let tag = e.name().to_string();
    // What another device of ours did about a call: a ringing one stops ringing here.
    if carbon == Some(true) {
        let call = {
            let mut s = handle.session();
            if s.calls.get(&sid).is_some_and(|c| !c.outgoing && c.phase == Phase::Proposed) {
                s.calls.remove(&sid)
            } else {
                None
            }
        };
        if let (Some(call), "proceed" | "reject") = (call, tag.as_str()) {
            emit(state, handle, &call.buffer_id, "m.call.answer", &sid, (call.peer.as_str(), true, "elsewhere"), json!({}));
        }
        return true;
    }
    let peer = from.to_bare();
    if peer == handle.own() || !known(handle, &peer) {
        tracing::debug!("xmpp[{}]: a call message from {from}, who is not a contact", handle.account_id);
        return true;
    }
    match tag.as_str() {
        "propose" => {
            let kinds: Vec<String> = e.children().filter(|d| d.is("description", jingle::RTP)).filter_map(|d| d.attr("media").map(str::to_string)).collect();
            if kinds.is_empty() || handle.session().calls.contains_key(&sid) {
                return true;
            }
            let buffer = super::roster::ensure_buffer(state, handle, &peer);
            let stub = Sdp { session_id: "1".into(), media: kinds.iter().enumerate().map(|(i, k)| Media { kind: k.clone(), mid: i.to_string(), port: 9, proto: "UDP/TLS/RTP/SAVPF".into(), direction: "sendrecv".into(), ..Default::default() }).collect(), ..Default::default() };
            handle.session().calls.insert(sid.clone(), Call::new(&buffer.id, peer.clone(), Some(from.clone()), false, Phase::Proposed));
            // The offer is not here yet: this rings the window, which answers by taking the call.
            emit(state, handle, &buffer.id, "m.call.invite", &sid, (peer.as_str(), false, &from.to_string()), json!({ "offer": { "type": "offer", "sdp": sdp::write(&stub) }, "lifetime": 60000, "moho_deferred": true }));
        }
        "proceed" => {
            let (offer, buffer_id) = {
                let mut s = handle.session();
                let Some(call) = s.calls.get_mut(&sid).filter(|c| c.outgoing && c.phase == Phase::Proposed && c.peer == peer) else { return true };
                call.phase = Phase::Initiated;
                call.full = Some(from.clone());
                (call.offer.take(), call.buffer_id.clone())
            };
            let Some(offer) = offer else { return true };
            let j = session_initiate(handle, &sid, &offer);
            let (state, handle, queued) = (state.clone(), handle.clone(), std::mem::take(&mut handle.session().calls.get_mut(&sid).map(|c| std::mem::take(&mut c.queued)).unwrap_or_default()));
            let _ = buffer_id;
            tokio::spawn(async move {
                send_jingle(&state, &handle, &sid, from.clone(), j, true).await;
                if !queued.is_empty() {
                    send_transport(&state, &handle, &sid, from, queued, Vec::new()).await;
                }
            });
        }
        "reject" | "retract" => {
            let call = {
                let mut s = handle.session();
                if s.calls.get(&sid).is_some_and(|c| c.peer == peer && c.phase != Phase::Accepted) {
                    s.calls.remove(&sid)
                } else {
                    None
                }
            };
            if let Some(call) = call {
                let (kind, reason) = if tag == "reject" { ("m.call.reject", "user_hangup") } else { ("m.call.hangup", "user_hangup") };
                emit(state, handle, &call.buffer_id, kind, &sid, (peer.as_str(), false, &from.to_string()), json!({ "reason": reason }));
            }
        }
        _ => {}
    }
    true
}

/// A Jingle request from another client.
pub async fn jingle_iq(state: &AppState, handle: &Handle, from: Option<Jid>, id: &str, j: &Element) {
    let action = j.attr("action").unwrap_or("");
    let sid = j.attr("sid").unwrap_or("").to_string();
    let Some(from) = from else { return };
    let peer = from.to_bare();
    let unknown = || Some((DefinedCondition::ItemNotFound, Some(xml::empty("unknown-session", UNKNOWN_SESSION))));
    if action == "session-initiate" {
        if !known(handle, &peer) || peer == handle.own() {
            tracing::debug!("xmpp[{}]: a call from {from}, who is not a contact", handle.account_id);
            return handle.send(reply(handle, Some(from), id, Some((DefinedCondition::ServiceUnavailable, None)))).await;
        }
        let offer = jingle::to_sdp(j, true);
        if offer.media.is_empty() {
            return handle.send(reply(handle, Some(from), id, Some((DefinedCondition::FeatureNotImplemented, None)))).await;
        }
        handle.send(reply(handle, Some(from.clone()), id, None)).await;
        let buffer = super::roster::ensure_buffer(state, handle, &peer);
        {
            let mut s = handle.session();
            match s.calls.get_mut(&sid) {
                // The one that was agreed to.
                Some(c) if !c.outgoing && c.phase == Phase::Proceeded && c.peer == peer => {
                    c.phase = Phase::Initiated;
                    c.full = Some(from.clone());
                }
                Some(_) => return,
                // A client that does not do the message exchange: the session is the first we hear of it.
                None => {
                    let mut c = Call::new(&buffer.id, peer.clone(), Some(from.clone()), false, Phase::Initiated);
                    c.ready = false;
                    s.calls.insert(sid.clone(), c);
                }
            }
            if let Some(c) = s.calls.get_mut(&sid) {
                c.mids = offer.media.iter().map(|m| m.mid.clone()).collect();
                c.remote = Some(offer.clone());
            }
        }
        emit(state, handle, &buffer.id, "m.call.invite", &sid, (peer.as_str(), false, &from.to_string()), json!({ "offer": { "type": "offer", "sdp": sdp::write(&offer) }, "lifetime": 60000 }));
        return;
    }
    let found = handle.session().calls.get(&sid).filter(|c| c.peer == peer).map(|c| (c.buffer_id.clone(), c.outgoing, c.phase));
    let Some((buffer_id, outgoing, phase)) = found else {
        return handle.send(reply(handle, Some(from), id, unknown())).await;
    };
    match action {
        "session-accept" if outgoing && phase == Phase::Initiated => {
            handle.send(reply(handle, Some(from.clone()), id, None)).await;
            let answer = jingle::to_sdp(j, false);
            let held = {
                let mut s = handle.session();
                let Some(c) = s.calls.get_mut(&sid) else { return };
                c.phase = Phase::Accepted;
                c.ready = true;
                c.full = Some(from.clone());
                c.remote = Some(answer.clone());
                std::mem::take(&mut c.held)
            };
            emit(state, handle, &buffer_id, "m.call.answer", &sid, (peer.as_str(), false, &from.to_string()), json!({ "answer": { "type": "answer", "sdp": sdp::write(&answer) } }));
            release(state, handle, &sid, &buffer_id, peer.as_str(), held);
        }
        "transport-info" => {
            handle.send(reply(handle, Some(from.clone()), id, None)).await;
            let mut ready = false;
            let mut now: Vec<Value> = Vec::new();
            {
                let mut s = handle.session();
                let Some(c) = s.calls.get_mut(&sid) else { return };
                for content in j.children().filter(|x| x.is("content", jingle::JINGLE)) {
                    let name = content.attr("name").unwrap_or("").to_string();
                    let index = c.mids.iter().position(|m| *m == name).unwrap_or(0);
                    for cand in child(content, "transport", jingle::ICE).into_iter().flat_map(|t| t.children()).filter(|x| x.is("candidate", jingle::ICE)).filter_map(jingle::read_candidate) {
                        if c.ready {
                            now.push(json!({ "candidate": sdp::write_candidate(&cand), "sdpMid": name, "sdpMLineIndex": index }));
                        } else {
                            c.held.push((name.clone(), index, cand));
                        }
                    }
                }
                ready |= c.ready;
            }
            if ready && !now.is_empty() {
                emit(state, handle, &buffer_id, "m.call.candidates", &sid, (peer.as_str(), false, &from.to_string()), json!({ "candidates": now }));
            }
        }
        "session-terminate" => {
            handle.send(reply(handle, Some(from.clone()), id, None)).await;
            handle.session().calls.remove(&sid);
            let reason = j.children().find(|c| c.name() == "reason").and_then(|r| r.children().next()).map(|c| c.name().to_string()).unwrap_or_else(|| "success".into());
            let kind = if matches!(reason.as_str(), "decline" | "busy") { "m.call.reject" } else { "m.call.hangup" };
            emit(state, handle, &buffer_id, kind, &sid, (peer.as_str(), false, &from.to_string()), json!({ "reason": if reason == "busy" { "user_busy" } else { "user_hangup" } }));
        }
        // The other end changing the call: a picture added, a section used another way or put away.
        "content-add" | "content-modify" | "content-remove" if phase == Phase::Accepted => {
            handle.send(reply(handle, Some(from.clone()), id, None)).await;
            let offer = {
                let mut s = handle.session();
                let Some(c) = s.calls.get_mut(&sid) else { return };
                let Some(remote) = c.remote.clone() else { return };
                let mut offer = remote.clone();
                match action {
                    "content-add" => {
                        let added = jingle::to_sdp(j, !outgoing);
                        for content in j.children().filter(|x| x.is("content", jingle::JINGLE)) {
                            if let (Some(name), Some(creator)) = (content.attr("name"), content.attr("creator")) {
                                c.creators.push((name.to_string(), creator.to_string()));
                            }
                        }
                        offer = renegotiate::with_added(&remote, added);
                    }
                    _ => {
                        for content in j.children().filter(|x| x.is("content", jingle::JINGLE)) {
                            let Some(name) = content.attr("name") else { continue };
                            let how = if action == "content-remove" { "inactive" } else { jingle::direction(content.attr("senders").unwrap_or("both"), !outgoing) };
                            offer = renegotiate::with_direction(&offer, name, how);
                        }
                    }
                }
                c.peer_offer = Some(offer.clone());
                offer
            };
            emit(state, handle, &buffer_id, "m.call.negotiate", &sid, (peer.as_str(), false, &from.to_string()), json!({ "description": { "type": "offer", "sdp": sdp::write(&offer) }, "lifetime": 30000 }));
        }
        // What the other end says to a change we offered.
        "content-accept" | "content-reject" if phase == Phase::Accepted => {
            handle.send(reply(handle, Some(from.clone()), id, None)).await;
            let accepted = if action == "content-accept" { let mut a = jingle::to_sdp(j, !outgoing); renegotiate::inherit_transport(&mut a); a } else { Sdp::default() };
            answered(state, handle, &sid, &buffer_id, &peer, &accepted);
        }
        // Somebody's phone saying it rings, or has been muted: nothing to do and nothing wrong.
        "session-info" => handle.send(reply(handle, Some(from), id, None)).await,
        _ => handle.send(reply(handle, Some(from), id, Some((DefinedCondition::FeatureNotImplemented, None)))).await,
    }
}

/// The relays and mirrors the server offers for a call (XEP-0215), in the shape the window's ICE setup takes.
pub async fn turn_servers(handle: &Handle) -> Value {
    let supported = handle.session().server_features.contains(EXTDISCO);
    if !supported {
        return json!({ "uris": [] });
    }
    let Ok(server) = Jid::new(handle.own().domain().as_str()) else { return json!({ "uris": [] }) };
    match handle.request(Some(server), false, Element::builder("services", EXTDISCO).build()).await {
        Ok(Some(services)) => servers_from(&services),
        _ => json!({ "uris": [] }),
    }
}

/// The server's `<services>` as one set of addresses with the credentials they share (what the window can use as one server).
pub fn servers_from(services: &Element) -> Value {
    let mut uris = Vec::new();
    let mut credentials: Option<(String, String)> = None;
    for s in services.children().filter(|s| s.name() == "service") {
        let (Some(kind), Some(host)) = (s.attr("type"), s.attr("host")) else { continue };
        if !matches!(kind, "stun" | "stuns" | "turn" | "turns") {
            continue;
        }
        let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
        let port = s.attr("port").map(|p| format!(":{p}")).unwrap_or_default();
        let mut uri = format!("{kind}:{host}{port}");
        if kind.starts_with("turn") {
            if let Some(t) = s.attr("transport") {
                uri.push_str(&format!("?transport={t}"));
            }
            let own = (s.attr("username").unwrap_or("").to_string(), s.attr("password").unwrap_or("").to_string());
            match &credentials {
                None => credentials = Some(own),
                Some(c) if *c != own => continue,
                _ => {}
            }
        }
        uris.push(uri);
    }
    let (username, password) = credentials.unwrap_or_default();
    json!({ "uris": uris, "username": username, "password": password, "fallback": [] })
}

/// The features a server or another client can see this one calls with.
pub const FEATURES: &[&str] = &[
    JMI,
    jingle::JINGLE,
    jingle::RTP,
    "urn:xmpp:jingle:apps:rtp:audio",
    "urn:xmpp:jingle:apps:rtp:video",
    jingle::ICE,
    jingle::DTLS,
    jingle::GROUPING,
    jingle::RTCP_FB,
    jingle::HDREXT,
    jingle::SSMA,
    "urn:ietf:rfc:5761",
    "urn:ietf:rfc:5888",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_servers_relays_become_one_set_of_addresses_with_their_credentials() {
        let services: Element = "<services xmlns='urn:xmpp:extdisco:2'><service type='stun' host='turn.example.org' port='3478' transport='udp'/><service type='turn' host='turn.example.org' port='3478' transport='udp' username='u1' password='p1'/><service type='turns' host='2001:db8::1' port='5349' transport='tcp' username='u1' password='p1'/><service type='turn' host='other.example.org' username='x' password='y'/><service type='ftp' host='nope'/></services>".parse().unwrap();
        let v = servers_from(&services);
        assert_eq!(v["uris"], json!(["stun:turn.example.org:3478", "turn:turn.example.org:3478?transport=udp", "turns:[2001:db8::1]:5349?transport=tcp"]));
        assert_eq!((v["username"].as_str(), v["password"].as_str()), (Some("u1"), Some("p1")));
        assert_eq!(servers_from(&"<services xmlns='urn:xmpp:extdisco:2'/>".parse().unwrap())["uris"], json!([]));
    }

    #[test]
    fn a_proposal_names_the_media_it_would_carry() {
        let m = jmi_message("a@example.org/x".parse().unwrap(), "propose", "sid1", &["audio".to_string(), "video".to_string()]);
        let e = m.payloads.iter().find(|p| p.is("propose", JMI)).unwrap();
        assert_eq!(e.attr("id"), Some("sid1"));
        assert_eq!(e.children().filter_map(|d| d.attr("media")).collect::<Vec<_>>(), vec!["audio", "video"]);
        assert!(m.payloads.iter().any(|p| p.is("store", super::super::ns::HINTS)));
    }
}
