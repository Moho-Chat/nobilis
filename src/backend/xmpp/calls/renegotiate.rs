//! Changing a call that is up: the camera coming on, a screen shared, somebody putting the picture away.
//!
//! The window speaks in whole descriptions (an offer with every section of the call in it, then the answer to it); Jingle speaks in
//! changes (`content-add`, `content-modify`, `content-accept`). So the daemon keeps the last description each end gave and turns
//! one into the other: a section that is new is an addition, one whose direction moved is a modification, and the answer to an
//! offer is put together from what the other end said and what it already had.

use super::sdp::{Media, Payload, Sdp};

/// What an offer changes about the call as it stood.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Changes {
    /// Sections the call did not have, by name (their `mid`).
    pub added: Vec<String>,
    /// Sections that are there still and are used in another way.
    pub modified: Vec<String>,
}

pub fn changes(old: &Sdp, new: &Sdp) -> Changes {
    let mut c = Changes::default();
    for m in &new.media {
        match old.media.iter().find(|o| o.mid == m.mid) {
            None => c.added.push(m.mid.clone()),
            Some(o) if o.direction != m.direction => c.modified.push(m.mid.clone()),
            Some(_) => {}
        }
    }
    c
}

/// What the other end does when we do this: the opposite direction.
pub fn mirror(direction: &str) -> &'static str {
    match direction {
        "sendonly" => "recvonly",
        "recvonly" => "sendonly",
        "inactive" => "inactive",
        _ => "sendrecv",
    }
}

/// A section that was turned down: the answer must have one for every section of the offer, and this is how one says no.
pub fn rejected(of: &Media) -> Media {
    Media {
        kind: of.kind.clone(),
        mid: of.mid.clone(),
        port: 0,
        proto: of.proto.clone(),
        direction: "inactive".into(),
        payloads: of.payloads.iter().take(1).map(|p| Payload { id: p.id, name: p.name.clone(), clock: p.clock, channels: p.channels, ..Default::default() }).collect(),
        ..Default::default()
    }
}

/// The section of an addition that is bundled with another carries no transport of its own: it uses the first section's.
pub fn inherit_transport(sdp: &mut Sdp) {
    let Some(first) = sdp.media.iter().find(|m| !m.ufrag.is_empty()).cloned() else { return };
    for m in sdp.media.iter_mut().filter(|m| m.ufrag.is_empty()) {
        m.ufrag = first.ufrag.clone();
        m.pwd = first.pwd.clone();
        m.ice_options = first.ice_options.clone();
        m.fingerprint = first.fingerprint.clone();
        m.setup = first.setup.clone();
    }
}

/// The answer to our offer, as the window takes it: one section for each of the offer's, in its order. A section the call had is
/// the other end's as it was, used in the way the offer asks; a new one is what it accepted; one it did not accept is turned down.
pub fn answer(offer: &Sdp, remote: &Sdp, accepted: &Sdp) -> Sdp {
    let mut out = Sdp { session_id: remote.session_id.clone(), groups: Vec::new(), msid_semantic: remote.msid_semantic.clone(), media: Vec::new() };
    for o in &offer.media {
        let section = match (remote.media.iter().find(|m| m.mid == o.mid), accepted.media.iter().find(|m| m.mid == o.mid)) {
            (Some(had), _) if had.port != 0 => Media { direction: mirror(&o.direction).into(), ..had.clone() },
            (_, Some(new)) => new.clone(),
            _ => rejected(o),
        };
        out.media.push(section);
    }
    inherit_transport(&mut out);
    let live: Vec<String> = out.media.iter().filter(|m| m.port != 0).map(|m| m.mid.clone()).collect();
    out.groups = offer.groups.iter().map(|(s, mids)| (s.clone(), mids.iter().filter(|m| live.contains(m)).cloned().collect())).collect();
    out
}

/// The other end's description with a section's use changed, as the offer for the window when it asked for that.
pub fn with_direction(remote: &Sdp, mid: &str, senders_direction: &str) -> Sdp {
    let mut out = remote.clone();
    if let Some(m) = out.media.iter_mut().find(|m| m.mid == mid) {
        m.direction = senders_direction.to_string();
    }
    out
}

/// The other end's description with sections added, as the offer for the window when they asked for it.
pub fn with_added(remote: &Sdp, added: Sdp) -> Sdp {
    let mut out = remote.clone();
    for m in added.media {
        if out.media.iter().all(|e| e.mid != m.mid) {
            out.media.push(m);
        }
    }
    inherit_transport(&mut out);
    let mids: Vec<String> = out.media.iter().map(|m| m.mid.clone()).collect();
    match out.groups.iter_mut().find(|(s, _)| s == "BUNDLE") {
        Some((_, list)) => *list = mids,
        None if mids.len() > 1 => out.groups.push(("BUNDLE".into(), mids)),
        None => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::sdp::{parse, SAMPLE};
    use super::*;

    fn audio_only() -> Sdp {
        let mut s = parse(SAMPLE);
        s.media.truncate(1);
        s.groups = vec![("BUNDLE".into(), vec!["0".into()])];
        s
    }

    #[test]
    fn a_new_section_is_an_addition_and_a_different_direction_a_modification() {
        let old = audio_only();
        let mut new = parse(SAMPLE);
        assert_eq!(changes(&old, &new), Changes { added: vec!["1".into()], modified: vec![] });
        new.media.truncate(1);
        new.media[0].direction = "recvonly".into();
        assert_eq!(changes(&old, &new), Changes { added: vec![], modified: vec!["0".into()] });
        assert_eq!(changes(&old, &old), Changes::default());
    }

    #[test]
    fn the_answer_has_a_section_for_each_of_the_offers_in_its_order() {
        let remote = audio_only();
        let offer = parse(SAMPLE);
        // The other end took the video: what it sent for it is the section.
        let mut accepted = parse(SAMPLE);
        accepted.media.remove(0);
        accepted.media[0].direction = "recvonly".into();
        let a = answer(&offer, &remote, &accepted);
        assert_eq!(a.media.iter().map(|m| (m.mid.as_str(), m.port, m.direction.as_str())).collect::<Vec<_>>(), vec![("0", 9, "sendrecv"), ("1", 9, "recvonly")]);
        assert_eq!(a.groups, vec![("BUNDLE".to_string(), vec!["0".to_string(), "1".to_string()])]);
        // Not taken: turned down, so the browser does not wait for a section that will not come.
        let none = answer(&offer, &remote, &Sdp::default());
        assert_eq!(none.media[1].port, 0);
        assert_eq!(none.groups, vec![("BUNDLE".to_string(), vec!["0".to_string()])]);
        assert!(super::super::sdp::write(&none).contains("m=video 0 "));
    }

    #[test]
    fn a_section_we_used_differently_is_answered_in_the_way_that_matches() {
        let remote = audio_only();
        let mut offer = audio_only();
        offer.media[0].direction = "sendonly".into();
        assert_eq!(answer(&offer, &remote, &Sdp::default()).media[0].direction, "recvonly");
        assert_eq!((mirror("recvonly"), mirror("inactive"), mirror("sendrecv")), ("sendonly", "inactive", "sendrecv"));
    }

    #[test]
    fn an_addition_with_no_transport_of_its_own_uses_the_first_sections() {
        let mut added = parse(SAMPLE);
        added.media.remove(0);
        added.media[0].ufrag.clear();
        added.media[0].pwd.clear();
        added.media[0].fingerprint = None;
        added.groups.clear();
        let offer = with_added(&audio_only(), added);
        assert_eq!(offer.media.len(), 2);
        assert_eq!((offer.media[1].ufrag.as_str(), offer.media[1].fingerprint.is_some()), ("Zt3k", true));
        assert_eq!(offer.groups, vec![("BUNDLE".to_string(), vec!["0".to_string(), "1".to_string()])]);
        // The same section twice is one.
        assert_eq!(with_added(&offer, parse(SAMPLE)).media.len(), 2);
    }

    #[test]
    fn a_modification_changes_only_the_direction_of_the_one_section() {
        let s = with_direction(&parse(SAMPLE), "1", "inactive");
        assert_eq!((s.media[0].direction.as_str(), s.media[1].direction.as_str()), ("sendrecv", "inactive"));
    }
}
