//! SDP (what the window's WebRTC speaks) as data: read from text, written back to text. Only what a call between two browsers
//! and the other XMPP clients' calls needs - not a general SDP library - and nothing is kept that is not understood and carried.

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Payload {
    pub id: u8,
    pub name: String,
    pub clock: u32,
    pub channels: Option<u8>,
    pub params: Vec<(String, String)>,
    /// `a=rtcp-fb`: type and optional subtype.
    pub feedback: Vec<(String, Option<String>)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Ssrc {
    pub id: u32,
    pub attrs: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Candidate {
    pub foundation: String,
    pub component: u8,
    pub protocol: String,
    pub priority: u32,
    pub ip: String,
    pub port: u16,
    pub kind: String,
    pub rel_addr: Option<String>,
    pub rel_port: Option<u16>,
    pub generation: u32,
    pub tcp_type: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Media {
    pub kind: String,
    pub mid: String,
    pub port: u16,
    pub proto: String,
    pub ufrag: String,
    pub pwd: String,
    pub ice_options: Option<String>,
    pub fingerprint: Option<(String, String)>,
    pub setup: Option<String>,
    /// `sendrecv`, `sendonly`, `recvonly`, `inactive`.
    pub direction: String,
    pub rtcp_mux: bool,
    pub payloads: Vec<Payload>,
    pub extmaps: Vec<(u8, String)>,
    pub ssrcs: Vec<Ssrc>,
    /// `a=ssrc-group`: semantics and the ssrcs.
    pub ssrc_groups: Vec<(String, Vec<u32>)>,
    pub msid: Option<String>,
    pub candidates: Vec<Candidate>,
    pub end_of_candidates: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Sdp {
    pub session_id: String,
    /// `a=group:`: semantics and the mids.
    pub groups: Vec<(String, Vec<String>)>,
    pub msid_semantic: Option<String>,
    pub media: Vec<Media>,
}

pub fn parse_candidate(line: &str) -> Option<Candidate> {
    let body = line.strip_prefix("a=").unwrap_or(line).strip_prefix("candidate:")?;
    let mut t = body.split_whitespace();
    let foundation = t.next()?.to_string();
    let component = t.next()?.parse().ok()?;
    let protocol = t.next()?.to_lowercase();
    let priority = t.next()?.parse().ok()?;
    let ip = t.next()?.to_string();
    let port = t.next()?.parse().ok()?;
    if t.next()? != "typ" {
        return None;
    }
    let kind = t.next()?.to_string();
    let mut c = Candidate { foundation, component, protocol, priority, ip, port, kind, ..Default::default() };
    while let Some(k) = t.next() {
        let v = t.next();
        match (k, v) {
            ("raddr", Some(v)) => c.rel_addr = Some(v.to_string()),
            ("rport", Some(v)) => c.rel_port = v.parse().ok(),
            ("generation", Some(v)) => c.generation = v.parse().unwrap_or(0),
            ("tcptype", Some(v)) => c.tcp_type = Some(v.to_string()),
            _ => {}
        }
    }
    Some(c)
}

pub fn write_candidate(c: &Candidate) -> String {
    let mut s = format!("candidate:{} {} {} {} {} {} typ {}", c.foundation, c.component, c.protocol, c.priority, c.ip, c.port, c.kind);
    if let (Some(a), Some(p)) = (&c.rel_addr, c.rel_port) {
        s.push_str(&format!(" raddr {a} rport {p}"));
    }
    if let Some(t) = &c.tcp_type {
        s.push_str(&format!(" tcptype {t}"));
    }
    s.push_str(&format!(" generation {}", c.generation));
    s
}

pub fn parse(text: &str) -> Sdp {
    let mut sdp = Sdp::default();
    let mut media: Option<Media> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (key, value) = line.split_at(2.min(line.len()));
        if line.starts_with("m=") {
            if let Some(m) = media.take() {
                sdp.media.push(m);
            }
            let mut t = value.split_whitespace();
            let kind = t.next().unwrap_or("").to_string();
            let port = t.next().and_then(|p| p.parse().ok()).unwrap_or(9);
            let proto = t.next().unwrap_or("").to_string();
            let ids: Vec<u8> = t.filter_map(|x| x.parse().ok()).collect();
            media = Some(Media { kind, port, proto, payloads: ids.into_iter().map(|id| Payload { id, ..Default::default() }).collect(), direction: "sendrecv".into(), ..Default::default() });
            continue;
        }
        if key == "o=" && media.is_none() {
            sdp.session_id = value.split_whitespace().nth(1).unwrap_or("").to_string();
            continue;
        }
        let Some(attr) = line.strip_prefix("a=") else { continue };
        let (name, val) = attr.split_once(':').map(|(n, v)| (n, Some(v))).unwrap_or((attr, None));
        let Some(m) = media.as_mut() else {
            match (name, val) {
                ("group", Some(v)) => {
                    let mut t = v.split_whitespace();
                    let sem = t.next().unwrap_or("").to_string();
                    sdp.groups.push((sem, t.map(str::to_string).collect()));
                }
                ("msid-semantic", Some(v)) => sdp.msid_semantic = Some(v.trim().to_string()),
                _ => {}
            }
            continue;
        };
        match (name, val) {
            ("mid", Some(v)) => m.mid = v.to_string(),
            ("ice-ufrag", Some(v)) => m.ufrag = v.to_string(),
            ("ice-pwd", Some(v)) => m.pwd = v.to_string(),
            ("ice-options", Some(v)) => m.ice_options = Some(v.to_string()),
            ("fingerprint", Some(v)) => {
                if let Some((h, f)) = v.split_once(' ') {
                    m.fingerprint = Some((h.to_string(), f.trim().to_string()));
                }
            }
            ("setup", Some(v)) => m.setup = Some(v.to_string()),
            ("sendrecv" | "sendonly" | "recvonly" | "inactive", None) => m.direction = name.to_string(),
            ("rtcp-mux", None) => m.rtcp_mux = true,
            ("msid", Some(v)) => m.msid = Some(v.to_string()),
            ("end-of-candidates", None) => m.end_of_candidates = true,
            ("extmap", Some(v)) => {
                let mut t = v.split_whitespace();
                let id = t.next().and_then(|i| i.split('/').next()).and_then(|i| i.parse().ok());
                if let (Some(id), Some(uri)) = (id, t.next()) {
                    m.extmaps.push((id, uri.to_string()));
                }
            }
            ("rtpmap", Some(v)) => {
                if let Some((id, rest)) = v.split_once(' ') {
                    if let Some(p) = id.parse::<u8>().ok().and_then(|id| m.payloads.iter_mut().find(|p| p.id == id)) {
                        let mut t = rest.split('/');
                        p.name = t.next().unwrap_or("").to_string();
                        p.clock = t.next().and_then(|c| c.parse().ok()).unwrap_or(0);
                        p.channels = t.next().and_then(|c| c.parse().ok());
                    }
                }
            }
            ("fmtp", Some(v)) => {
                if let Some((id, rest)) = v.split_once(' ') {
                    if let Some(p) = id.parse::<u8>().ok().and_then(|id| m.payloads.iter_mut().find(|p| p.id == id)) {
                        p.params = rest.split(';').filter(|x| !x.trim().is_empty()).map(|kv| match kv.trim().split_once('=') {
                            Some((k, v)) => (k.to_string(), v.to_string()),
                            None => (kv.trim().to_string(), String::new()),
                        }).collect();
                    }
                }
            }
            ("rtcp-fb", Some(v)) => {
                let mut t = v.split_whitespace();
                let target = t.next().unwrap_or("");
                let kind = t.next().unwrap_or("").to_string();
                let sub = {
                    let rest: Vec<&str> = t.collect();
                    (!rest.is_empty()).then(|| rest.join(" "))
                };
                for p in m.payloads.iter_mut().filter(|p| target == "*" || target.parse::<u8>().ok() == Some(p.id)) {
                    p.feedback.push((kind.clone(), sub.clone()));
                }
            }
            ("ssrc", Some(v)) => {
                if let Some((id, rest)) = v.split_once(' ') {
                    if let Ok(id) = id.parse::<u32>() {
                        let (k, val) = rest.split_once(':').map(|(k, v)| (k.to_string(), v.to_string())).unwrap_or((rest.to_string(), String::new()));
                        match m.ssrcs.iter_mut().find(|s| s.id == id) {
                            Some(s) => s.attrs.push((k, val)),
                            None => m.ssrcs.push(Ssrc { id, attrs: vec![(k, val)] }),
                        }
                    }
                }
            }
            ("ssrc-group", Some(v)) => {
                let mut t = v.split_whitespace();
                let sem = t.next().unwrap_or("").to_string();
                m.ssrc_groups.push((sem, t.filter_map(|x| x.parse().ok()).collect()));
            }
            ("candidate", _) => {
                if let Some(c) = parse_candidate(line) {
                    m.candidates.push(c);
                }
            }
            _ => {}
        }
    }
    if let Some(m) = media.take() {
        sdp.media.push(m);
    }
    sdp
}

pub fn write(sdp: &Sdp) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push_str("\r\n");
    };
    line("v=0".into());
    line(format!("o=- {} 2 IN IP4 127.0.0.1", if sdp.session_id.is_empty() { "1" } else { &sdp.session_id }));
    line("s=-".into());
    line("t=0 0".into());
    for (sem, mids) in &sdp.groups {
        line(format!("a=group:{sem} {}", mids.join(" ")));
    }
    if let Some(m) = &sdp.msid_semantic {
        line(format!("a=msid-semantic: {m}"));
    }
    for m in &sdp.media {
        let ids: Vec<String> = m.payloads.iter().map(|p| p.id.to_string()).collect();
        line(format!("m={} {} {} {}", m.kind, m.port, m.proto, ids.join(" ")));
        line("c=IN IP4 0.0.0.0".into());
        line("a=rtcp:9 IN IP4 0.0.0.0".into());
        if !m.ufrag.is_empty() {
            line(format!("a=ice-ufrag:{}", m.ufrag));
            line(format!("a=ice-pwd:{}", m.pwd));
        }
        if let Some(o) = &m.ice_options {
            line(format!("a=ice-options:{o}"));
        }
        if let Some((h, f)) = &m.fingerprint {
            line(format!("a=fingerprint:{h} {f}"));
        }
        if let Some(s) = &m.setup {
            line(format!("a=setup:{s}"));
        }
        line(format!("a=mid:{}", m.mid));
        for (id, uri) in &m.extmaps {
            line(format!("a=extmap:{id} {uri}"));
        }
        line(format!("a={}", m.direction));
        if let Some(msid) = &m.msid {
            line(format!("a=msid:{msid}"));
        }
        if m.rtcp_mux {
            line("a=rtcp-mux".into());
        }
        for p in &m.payloads {
            let ch = p.channels.map(|c| format!("/{c}")).unwrap_or_default();
            line(format!("a=rtpmap:{} {}/{}{}", p.id, p.name, p.clock, ch));
            for (kind, sub) in &p.feedback {
                line(format!("a=rtcp-fb:{} {}{}", p.id, kind, sub.as_ref().map(|s| format!(" {s}")).unwrap_or_default()));
            }
            if !p.params.is_empty() {
                let params: Vec<String> = p.params.iter().map(|(k, v)| if v.is_empty() { k.clone() } else { format!("{k}={v}") }).collect();
                line(format!("a=fmtp:{} {}", p.id, params.join(";")));
            }
        }
        for (sem, ids) in &m.ssrc_groups {
            line(format!("a=ssrc-group:{sem} {}", ids.iter().map(u32::to_string).collect::<Vec<_>>().join(" ")));
        }
        for s in &m.ssrcs {
            for (k, v) in &s.attrs {
                line(if v.is_empty() { format!("a=ssrc:{} {k}", s.id) } else { format!("a=ssrc:{} {k}:{v}", s.id) });
            }
        }
        for c in &m.candidates {
            line(format!("a={}", write_candidate(c)));
        }
        if m.end_of_candidates {
            line("a=end-of-candidates".into());
        }
    }
    out
}

#[cfg(test)]
pub const SAMPLE: &str = "v=0\r\no=- 4611731400430051336 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\na=extmap-allow-mixed\r\na=msid-semantic: WMS stream1\r\nm=audio 9 UDP/TLS/RTP/SAVPF 111 63 0\r\nc=IN IP4 0.0.0.0\r\na=rtcp:9 IN IP4 0.0.0.0\r\na=ice-ufrag:Zt3k\r\na=ice-pwd:aSecretPasswordForIce12345\r\na=ice-options:trickle\r\na=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\na=setup:actpass\r\na=mid:0\r\na=extmap:1 urn:ietf:params:rtp-hdrext:ssrc-audio-level\r\na=extmap:2 http://www.webrtc.org/experiments/rtp-hdrext/abs-send-time\r\na=sendrecv\r\na=msid:stream1 track0\r\na=rtcp-mux\r\na=rtpmap:111 opus/48000/2\r\na=rtcp-fb:111 transport-cc\r\na=fmtp:111 minptime=10;useinbandfec=1\r\na=rtpmap:63 red/48000/2\r\na=fmtp:63 111/111\r\na=rtpmap:0 PCMU/8000\r\na=ssrc:1001 cname:cn1\r\na=ssrc:1001 msid:stream1 track0\r\na=candidate:842163049 1 udp 1677729535 192.0.2.10 54321 typ srflx raddr 10.0.0.5 rport 54321 generation 0\r\nm=video 9 UDP/TLS/RTP/SAVPF 96 97\r\nc=IN IP4 0.0.0.0\r\na=rtcp:9 IN IP4 0.0.0.0\r\na=ice-ufrag:Zt3k\r\na=ice-pwd:aSecretPasswordForIce12345\r\na=fingerprint:sha-256 AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99\r\na=setup:actpass\r\na=mid:1\r\na=extmap:3 urn:ietf:params:rtp-hdrext:toffset\r\na=sendrecv\r\na=msid:stream1 track1\r\na=rtcp-mux\r\na=rtpmap:96 VP8/90000\r\na=rtcp-fb:96 goog-remb\r\na=rtcp-fb:96 nack\r\na=rtcp-fb:96 nack pli\r\na=rtpmap:97 rtx/90000\r\na=fmtp:97 apt=96\r\na=ssrc-group:FID 2001 2002\r\na=ssrc:2001 cname:cn1\r\na=ssrc:2002 cname:cn1\r\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browsers_offer_is_read_into_its_parts() {
        let s = parse(SAMPLE);
        assert_eq!((s.session_id.as_str(), s.groups.clone(), s.media.len()), ("4611731400430051336", vec![("BUNDLE".to_string(), vec!["0".to_string(), "1".to_string()])], 2));
        let a = &s.media[0];
        assert_eq!((a.kind.as_str(), a.mid.as_str(), a.ufrag.as_str(), a.setup.as_deref(), a.direction.as_str(), a.rtcp_mux), ("audio", "0", "Zt3k", Some("actpass"), "sendrecv", true));
        assert_eq!(a.payloads.len(), 3);
        let opus = &a.payloads[0];
        assert_eq!((opus.name.as_str(), opus.clock, opus.channels, opus.params.clone(), opus.feedback.clone()), ("opus", 48000, Some(2), vec![("minptime".to_string(), "10".to_string()), ("useinbandfec".to_string(), "1".to_string())], vec![("transport-cc".to_string(), None)]));
        assert_eq!(a.extmaps.len(), 2);
        assert_eq!(a.ssrcs[0].attrs, vec![("cname".to_string(), "cn1".to_string()), ("msid".to_string(), "stream1 track0".to_string())]);
        assert_eq!(a.candidates[0].kind, "srflx");
        assert_eq!(a.candidates[0].rel_addr.as_deref(), Some("10.0.0.5"));
        let v = &s.media[1];
        assert_eq!(v.ssrc_groups, vec![("FID".to_string(), vec![2001, 2002])]);
        assert_eq!(v.payloads[0].feedback.len(), 3);
        assert_eq!(v.payloads[1].params, vec![("apt".to_string(), "96".to_string())]);
    }

    #[test]
    fn what_is_written_reads_back_as_the_same_thing() {
        let s = parse(SAMPLE);
        assert_eq!(parse(&write(&s)), s);
    }

    #[test]
    fn a_candidate_line_round_trips_with_and_without_a_related_address() {
        let c = parse_candidate("a=candidate:1 1 udp 2122260223 192.168.1.2 50000 typ host generation 0 network-id 1").unwrap();
        assert_eq!((c.kind.as_str(), c.port, c.rel_addr.clone()), ("host", 50000, None));
        assert_eq!(parse_candidate(&format!("a={}", write_candidate(&c))), Some(c));
        let r = parse_candidate("candidate:2 1 UDP 1677729535 198.51.100.7 40000 typ srflx raddr 192.168.1.2 rport 50000 generation 0").unwrap();
        assert_eq!((r.protocol.as_str(), r.rel_port), ("udp", Some(50000)));
        assert!(parse_candidate("a=candidate:garbage").is_none());
    }
}
