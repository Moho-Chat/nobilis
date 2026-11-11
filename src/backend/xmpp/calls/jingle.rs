//! SDP as Jingle (XEP-0166): what a call looks like to other XMPP clients. Audio and video are RTP (XEP-0167) over ICE-UDP
//! (XEP-0176) with DTLS-SRTP (XEP-0320), the media in one transport bundled (XEP-0338), the way every client that calls does it.

use super::sdp::{Candidate, Media, Payload, Sdp, Ssrc};
use crate::backend::xmpp::xml::{child, Attr};
use tokio_xmpp::minidom::Element;

pub const JINGLE: &str = "urn:xmpp:jingle:1";
pub const RTP: &str = "urn:xmpp:jingle:apps:rtp:1";
pub const RTCP_FB: &str = "urn:xmpp:jingle:apps:rtp:rtcp-fb:0";
pub const HDREXT: &str = "urn:xmpp:jingle:apps:rtp:rtp-hdrext:0";
pub const SSMA: &str = "urn:xmpp:jingle:apps:rtp:ssma:0";
pub const ICE: &str = "urn:xmpp:jingle:transports:ice-udp:1";
pub const DTLS: &str = "urn:xmpp:jingle:apps:dtls:0";
pub const GROUPING: &str = "urn:xmpp:jingle:apps:grouping:0";

/// The `senders` of a content, given what the SDP says the local end does: `local_initiator` says whether the local end is the
/// one that began the session (`senders` is always from the initiator's point of view).
pub fn senders(direction: &str, local_initiator: bool) -> &'static str {
    match (direction, local_initiator) {
        ("sendrecv", _) => "both",
        ("sendonly", true) | ("recvonly", false) => "initiator",
        ("recvonly", true) | ("sendonly", false) => "responder",
        _ => "none",
    }
}

/// The SDP direction (what the local end does) for a content's `senders`.
pub fn direction(senders: &str, local_initiator: bool) -> &'static str {
    match (senders, local_initiator) {
        ("initiator", true) | ("responder", false) => "sendonly",
        ("initiator", false) | ("responder", true) => "recvonly",
        ("none", _) => "inactive",
        _ => "sendrecv",
    }
}

fn candidate_element(c: &Candidate, id: &str) -> Element {
    let mut b = Element::builder("candidate", ICE)
        .at("component", c.component.to_string())
        .at("foundation", c.foundation.as_str())
        .at("generation", c.generation.to_string())
        .at("id", id)
        .at("ip", c.ip.as_str())
        .at("network", "0")
        .at("port", c.port.to_string())
        .at("priority", c.priority.to_string())
        .at("protocol", c.protocol.as_str())
        .at("type", c.kind.as_str());
    if let (Some(a), Some(p)) = (&c.rel_addr, c.rel_port) {
        b = b.at("rel-addr", a.as_str()).at("rel-port", p.to_string());
    }
    if let Some(t) = &c.tcp_type {
        b = b.at("tcptype", t.as_str());
    }
    b.build()
}

/// A candidate as a Jingle element.
pub fn candidate(c: &Candidate) -> Element {
    candidate_element(c, &crate::backend::xmpp::xml::new_id()[5..])
}

pub fn read_candidate(e: &Element) -> Option<Candidate> {
    Some(Candidate {
        foundation: e.attr("foundation")?.to_string(),
        component: e.attr("component")?.parse().ok()?,
        protocol: e.attr("protocol")?.to_lowercase(),
        priority: e.attr("priority")?.parse().ok()?,
        ip: e.attr("ip")?.to_string(),
        port: e.attr("port")?.parse().ok()?,
        kind: e.attr("type")?.to_string(),
        rel_addr: e.attr("rel-addr").map(str::to_string),
        rel_port: e.attr("rel-port").and_then(|p| p.parse().ok()),
        generation: e.attr("generation").and_then(|g| g.parse().ok()).unwrap_or(0),
        tcp_type: e.attr("tcptype").map(str::to_string),
    })
}

/// A transport with its credentials, its fingerprint and the candidates found so far.
pub fn transport(m: &Media) -> Element {
    let mut t = Element::builder("transport", ICE).at("ufrag", m.ufrag.as_str()).at("pwd", m.pwd.as_str()).build();
    if let Some((hash, fp)) = &m.fingerprint {
        let mut b = Element::builder("fingerprint", DTLS).at("hash", hash.as_str());
        if let Some(s) = &m.setup {
            b = b.at("setup", s.as_str());
        }
        t.append_child(b.append(fp.as_str()).build());
    }
    for c in &m.candidates {
        t.append_child(candidate(c));
    }
    t
}

fn description(m: &Media) -> Element {
    let mut d = Element::builder("description", RTP).at("media", m.kind.as_str()).build();
    for p in &m.payloads {
        let mut b = Element::builder("payload-type", RTP).at("id", p.id.to_string()).at("name", p.name.as_str()).at("clockrate", p.clock.to_string());
        if let Some(c) = p.channels {
            b = b.at("channels", c.to_string());
        }
        let mut e = b.build();
        for (k, v) in &p.params {
            let mut pb = Element::builder("parameter", RTP).at("name", k.as_str());
            if !v.is_empty() {
                pb = pb.at("value", v.as_str());
            }
            e.append_child(pb.build());
        }
        for (kind, sub) in &p.feedback {
            let mut fb = Element::builder("rtcp-fb", RTCP_FB).at("type", kind.as_str());
            if let Some(s) = sub {
                fb = fb.at("subtype", s.as_str());
            }
            e.append_child(fb.build());
        }
        d.append_child(e);
    }
    if m.rtcp_mux {
        d.append_child(Element::builder("rtcp-mux", RTP).build());
    }
    for (id, uri) in &m.extmaps {
        d.append_child(Element::builder("rtp-hdrext", HDREXT).at("id", id.to_string()).at("uri", uri.as_str()).build());
    }
    for (sem, ids) in &m.ssrc_groups {
        let mut g = Element::builder("ssrc-group", SSMA).at("semantics", sem.as_str()).build();
        for id in ids {
            g.append_child(Element::builder("source", SSMA).at("ssrc", id.to_string()).build());
        }
        d.append_child(g);
    }
    for s in &m.ssrcs {
        let mut src = Element::builder("source", SSMA).at("ssrc", s.id.to_string()).build();
        for (k, v) in &s.attrs {
            let mut pb = Element::builder("parameter", SSMA).at("name", k.as_str());
            if !v.is_empty() {
                pb = pb.at("value", v.as_str());
            }
            src.append_child(pb.build());
        }
        d.append_child(src);
    }
    d
}

/// The `<content>` elements of an SDP. `creator` is who made the content (`initiator` for everything in a first offer).
pub fn contents(sdp: &Sdp, creator: &str, local_initiator: bool) -> Vec<Element> {
    sdp.media
        .iter()
        .map(|m| {
            let mut c = Element::builder("content", JINGLE).at("creator", creator).at("name", m.mid.as_str()).at("senders", senders(&m.direction, local_initiator)).build();
            c.append_child(description(m));
            c.append_child(transport(m));
            c
        })
        .collect()
}

/// The `<group>` elements (XEP-0338) of an SDP: which contents travel in one transport.
pub fn groups(sdp: &Sdp) -> Vec<Element> {
    sdp.groups
        .iter()
        .map(|(sem, mids)| {
            let mut g = Element::builder("group", GROUPING).at("semantics", sem.as_str()).build();
            for mid in mids {
                g.append_child(Element::builder("content", GROUPING).at("name", mid.as_str()).build());
            }
            g
        })
        .collect()
}

/// Reads a `<jingle>` element's contents as an SDP. `remote_initiator` says whether the sender began the session.
pub fn to_sdp(jingle: &Element, remote_initiator: bool) -> Sdp {
    let mut sdp = Sdp { session_id: "1".into(), msid_semantic: Some("WMS *".into()), ..Default::default() };
    for g in jingle.children().filter(|c| c.is("group", GROUPING)) {
        sdp.groups.push((g.attr("semantics").unwrap_or("BUNDLE").to_string(), g.children().filter_map(|c| c.attr("name").map(str::to_string)).collect()));
    }
    for c in jingle.children().filter(|c| c.is("content", JINGLE)) {
        let Some(d) = child(c, "description", RTP) else { continue };
        let t = child(c, "transport", ICE);
        let mut m = Media {
            kind: d.attr("media").unwrap_or("audio").to_string(),
            mid: c.attr("name").unwrap_or("").to_string(),
            port: 9,
            proto: "UDP/TLS/RTP/SAVPF".into(),
            // Every client that calls multiplexes RTCP, and the browser will not take a description that does not.
            rtcp_mux: true,
            direction: direction(c.attr("senders").unwrap_or("both"), !remote_initiator).to_string(),
            ..Default::default()
        };
        for p in d.children().filter(|p| p.is("payload-type", RTP)) {
            let Some(id) = p.attr("id").and_then(|i| i.parse().ok()) else { continue };
            let mut payload = Payload {
                id,
                name: p.attr("name").unwrap_or("").to_string(),
                clock: p.attr("clockrate").and_then(|c| c.parse().ok()).unwrap_or(0),
                channels: p.attr("channels").and_then(|c| c.parse().ok()).filter(|c: &u8| *c > 1 || p.attr("name").is_some_and(|n| n.eq_ignore_ascii_case("opus"))),
                ..Default::default()
            };
            for x in p.children() {
                if x.is("parameter", RTP) {
                    payload.params.push((x.attr("name").unwrap_or("").to_string(), x.attr("value").unwrap_or("").to_string()));
                } else if x.is("rtcp-fb", RTCP_FB) {
                    payload.feedback.push((x.attr("type").unwrap_or("").to_string(), x.attr("subtype").map(str::to_string)));
                }
            }
            m.payloads.push(payload);
        }
        for x in d.children() {
            if x.is("rtp-hdrext", HDREXT) {
                if let (Some(id), Some(uri)) = (x.attr("id").and_then(|i| i.parse().ok()), x.attr("uri")) {
                    m.extmaps.push((id, uri.to_string()));
                }
            } else if x.is("source", SSMA) {
                if let Some(id) = x.attr("ssrc").and_then(|i| i.parse().ok()) {
                    m.ssrcs.push(Ssrc { id, attrs: x.children().map(|p| (p.attr("name").unwrap_or("").to_string(), p.attr("value").unwrap_or("").to_string())).collect() });
                }
            } else if x.is("ssrc-group", SSMA) {
                m.ssrc_groups.push((x.attr("semantics").unwrap_or("").to_string(), x.children().filter_map(|s| s.attr("ssrc").and_then(|i| i.parse().ok())).collect()));
            }
        }
        if let Some(t) = t {
            m.ufrag = t.attr("ufrag").unwrap_or("").to_string();
            m.pwd = t.attr("pwd").unwrap_or("").to_string();
            m.ice_options = Some("trickle".into());
            if let Some(f) = child(t, "fingerprint", DTLS) {
                m.fingerprint = Some((f.attr("hash").unwrap_or("sha-256").to_string(), f.text().trim().to_string()));
                m.setup = f.attr("setup").map(str::to_string);
            }
            m.candidates = t.children().filter(|x| x.is("candidate", ICE)).filter_map(read_candidate).collect();
        }
        // What the browser needs to tie the sources to a stream.
        if m.msid.is_none() {
            if let Some(msid) = m.ssrcs.iter().flat_map(|s| s.attrs.iter()).find(|(k, _)| k == "msid") {
                m.msid = Some(msid.1.clone());
            }
        }
        sdp.media.push(m);
    }
    if sdp.groups.is_empty() && sdp.media.len() > 1 {
        sdp.groups.push(("BUNDLE".into(), sdp.media.iter().map(|m| m.mid.clone()).collect()));
    }
    sdp
}

#[cfg(test)]
mod tests {
    use super::super::sdp::{parse, SAMPLE};
    use super::*;

    fn jingle(sdp: &Sdp, initiator: bool) -> Element {
        let mut j = Element::builder("jingle", JINGLE).at("action", "session-initiate").at("sid", "s1").build();
        for g in groups(sdp) {
            j.append_child(g);
        }
        for c in contents(sdp, "initiator", initiator) {
            j.append_child(c);
        }
        j
    }

    #[test]
    fn what_a_browser_offers_is_what_a_jingle_client_reads() {
        let sdp = parse(SAMPLE);
        let j = jingle(&sdp, true);
        let contents: Vec<_> = j.children().filter(|c| c.is("content", JINGLE)).collect();
        assert_eq!(contents.len(), 2);
        let audio = contents[0];
        assert_eq!((audio.attr("name"), audio.attr("senders"), audio.attr("creator")), (Some("0"), Some("both"), Some("initiator")));
        let d = child(audio, "description", RTP).unwrap();
        assert_eq!(d.attr("media"), Some("audio"));
        let opus = d.children().find(|p| p.is("payload-type", RTP)).unwrap();
        assert_eq!((opus.attr("id"), opus.attr("name"), opus.attr("clockrate"), opus.attr("channels")), (Some("111"), Some("opus"), Some("48000"), Some("2")));
        assert!(opus.children().any(|p| p.is("parameter", RTP) && p.attr("name") == Some("useinbandfec") && p.attr("value") == Some("1")));
        assert!(opus.children().any(|p| p.is("rtcp-fb", RTCP_FB) && p.attr("type") == Some("transport-cc")));
        assert!(d.children().any(|x| x.is("rtcp-mux", RTP)));
        let t = child(audio, "transport", ICE).unwrap();
        assert_eq!((t.attr("ufrag"), t.attr("pwd")), (Some("Zt3k"), Some("aSecretPasswordForIce12345")));
        let fp = child(t, "fingerprint", DTLS).unwrap();
        assert_eq!((fp.attr("hash"), fp.attr("setup")), (Some("sha-256"), Some("actpass")));
        let cand = t.children().find(|c| c.is("candidate", ICE)).unwrap();
        assert_eq!((cand.attr("type"), cand.attr("rel-addr"), cand.attr("ip")), (Some("srflx"), Some("10.0.0.5"), Some("192.0.2.10")));
        let bundle = j.children().find(|g| g.is("group", GROUPING)).unwrap();
        assert_eq!((bundle.attr("semantics"), bundle.children().count()), (Some("BUNDLE"), 2));
    }

    #[test]
    fn an_sdp_survives_the_trip_to_jingle_and_back() {
        let sdp = parse(SAMPLE);
        let back = to_sdp(&jingle(&sdp, true), true);
        assert_eq!(back.groups, sdp.groups);
        assert_eq!(back.media.len(), sdp.media.len());
        for (a, b) in sdp.media.iter().zip(&back.media) {
            assert_eq!((&a.kind, &a.mid, &a.ufrag, &a.pwd, &a.fingerprint, &a.setup, &a.payloads, &a.extmaps, &a.ssrcs, &a.ssrc_groups, &a.candidates), (&b.kind, &b.mid, &b.ufrag, &b.pwd, &b.fingerprint, &b.setup, &b.payloads, &b.extmaps, &b.ssrcs, &b.ssrc_groups, &b.candidates));
            assert!(b.rtcp_mux);
            assert_eq!(b.direction, a.direction);
        }
    }

    #[test]
    fn who_sends_is_said_from_the_initiators_side_whichever_end_wrote_it() {
        // The caller sends only: "initiator". The answerer, writing its own SDP, receives only: also "initiator".
        assert_eq!((senders("sendonly", true), senders("recvonly", false)), ("initiator", "initiator"));
        assert_eq!((senders("recvonly", true), senders("sendonly", false)), ("responder", "responder"));
        assert_eq!((senders("sendrecv", true), senders("inactive", false)), ("both", "none"));
        // And read back at the other end, who is the opposite party.
        assert_eq!((direction("initiator", false), direction("initiator", true)), ("recvonly", "sendonly"));
        assert_eq!((direction("responder", false), direction("responder", true)), ("sendonly", "recvonly"));
        assert_eq!((direction("both", true), direction("none", false)), ("sendrecv", "inactive"));
    }

    #[test]
    fn a_description_without_a_group_is_bundled_so_a_browser_will_take_it() {
        let sdp = parse(SAMPLE);
        let mut j = jingle(&sdp, true);
        j.remove_child("group", GROUPING);
        assert_eq!(to_sdp(&j, true).groups, vec![("BUNDLE".to_string(), vec!["0".to_string(), "1".to_string()])]);
    }

    #[test]
    fn a_candidate_reads_from_what_a_client_writes() {
        let e: Element = "<candidate xmlns='urn:xmpp:jingle:transports:ice-udp:1' component='1' foundation='2' generation='0' id='x' ip='203.0.113.4' network='1' port='3478' priority='2130706431' protocol='UDP' type='relay' rel-addr='198.51.100.1' rel-port='9'/>".parse().unwrap();
        let c = read_candidate(&e).unwrap();
        assert_eq!((c.protocol.as_str(), c.kind.as_str(), c.port, c.rel_port), ("udp", "relay", 3478, Some(9)));
        assert_eq!(read_candidate(&candidate(&c)), Some(c));
    }
}
