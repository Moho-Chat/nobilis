//! Automatic Trust Management (XEP-0450) and the trust messages it sends (XEP-0434), and the `xmpp:` links a key is verified by.
//!
//! Comparing fingerprints by hand once per pair of devices does not scale: with ATM, once a device has been verified, it tells
//! the others what it verified. When this device verifies another, it sends (encrypted, to verified devices only) the key it
//! verified to this account's other devices, and its own account's keys to the device it verified; a device that receives such
//! a message applies it only when it has itself verified the device that sent it, and keeps it until then. A contact's device
//! may only speak for its own account's keys; this account's devices may speak for anyone's.
//!
//! A key is identified by its Curve25519 form (the fingerprint XEP-0384 names), in base64 inside a trust message and in hex in a
//! link. Links come in the two forms clients use: the XEP-0434 trust-message link (`xmpp:jid?trust-message;encryption=
//! urn:xmpp:omemo:2;trust=<hex>`), and the one Conversations, Dino, Monal and Gajim show as a QR code
//! (`xmpp:jid?omemo-sid-<device>=<hex>`, and `omemo2-sid-` for OMEMO 2).

use super::super::xml::Attr;
use base64::Engine;
use tokio_xmpp::minidom::Element;

pub const TM: &str = "urn:xmpp:tm:1";
pub const ATM: &str = "urn:xmpp:atm:1";

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// What one trust message, or one link, says: for each person, the keys to trust and the keys to distrust.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decisions {
    pub owners: Vec<Owner>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Owner {
    pub jid: String,
    pub trust: Vec<[u8; 32]>,
    pub distrust: Vec<[u8; 32]>,
}

impl Decisions {
    pub fn owner(&mut self, jid: &str) -> &mut Owner {
        if let Some(i) = self.owners.iter().position(|o| o.jid == jid) {
            return &mut self.owners[i];
        }
        self.owners.push(Owner { jid: jid.to_string(), ..Default::default() });
        self.owners.last_mut().expect("just pushed")
    }
}

/// A `<trust-message/>` for OMEMO 2 used by ATM; anything else (another usage, another encryption) is `None`.
pub fn parse(e: &Element) -> Option<Decisions> {
    if !e.is("trust-message", TM) || e.attr("usage") != Some(ATM) || e.attr("encryption") != Some(super::v2::NS) {
        return None;
    }
    let key = |c: &Element| -> Option<[u8; 32]> { b64().decode(c.text().trim()).ok()?.try_into().ok() };
    let mut d = Decisions::default();
    for o in e.children().filter(|c| c.is("key-owner", TM)) {
        let Some(jid) = o.attr("jid") else { continue };
        let owner = d.owner(jid);
        owner.trust.extend(o.children().filter(|c| c.is("trust", TM)).filter_map(key));
        owner.distrust.extend(o.children().filter(|c| c.is("distrust", TM)).filter_map(key));
    }
    Some(d)
}

pub fn element(d: &Decisions) -> Element {
    let mut e = Element::builder("trust-message", TM).at("usage", ATM).at("encryption", super::v2::NS).build();
    for o in d.owners.iter().filter(|o| !o.trust.is_empty() || !o.distrust.is_empty()) {
        let mut owner = Element::builder("key-owner", TM).at("jid", o.jid.as_str()).build();
        for k in &o.trust {
            owner.append_child(Element::builder("trust", TM).append(b64().encode(k)).build());
        }
        for k in &o.distrust {
            owner.append_child(Element::builder("distrust", TM).append(b64().encode(k)).build());
        }
        e.append_child(owner);
    }
    e
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let s = s.trim().to_ascii_lowercase();
    // The older namespace's form carries the key type (05) in front.
    let s = if s.len() == 66 && s.starts_with("05") { &s[2..] } else { s.as_str() };
    super::curve::unhex(s)?.try_into().ok()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// Reads a verification link (either form). The address and what it says about that person's keys.
pub fn parse_uri(uri: &str) -> Option<(String, Owner)> {
    let rest = uri.trim().strip_prefix("xmpp:")?;
    let (path, query) = rest.split_once('?')?;
    let jid = percent_decode(path.trim_start_matches("//")).split('/').next()?.to_string();
    if jid.is_empty() {
        return None;
    }
    let mut owner = Owner { jid: jid.clone(), ..Default::default() };
    let mut parts = query.split(';');
    let action = parts.next().unwrap_or_default();
    let mut other_encryption = false;
    let pairs: Vec<(String, String)> = std::iter::once(action).chain(parts).filter_map(|p| p.split_once('=')).map(|(k, v)| (percent_decode(k), percent_decode(v))).collect();
    for (k, v) in &pairs {
        if k == "encryption" && v != super::v2::NS {
            other_encryption = true;
        }
    }
    for (k, v) in pairs {
        if k.starts_with("omemo-sid-") || k.starts_with("omemo2-sid-") {
            owner.trust.extend(unhex32(&v));
        } else if action == "trust-message" && !other_encryption {
            match k.as_str() {
                "trust" => owner.trust.extend(unhex32(&v)),
                "distrust" => owner.distrust.extend(unhex32(&v)),
                _ => {}
            }
        }
    }
    (!owner.trust.is_empty() || !owner.distrust.is_empty()).then_some((jid, owner))
}

/// This device's links: the form most clients scan (`omemo-sid-` and `omemo2-sid-`, the same key) and the XEP-0434 one,
/// which also carries the account's other verified keys.
pub fn own_uris(jid: &str, device: u32, key: &[u8; 32], other_verified: &[[u8; 32]]) -> (String, String) {
    let hex = super::curve::hex(key);
    let sid = format!("xmpp:{jid}?omemo-sid-{device}=05{hex};omemo2-sid-{device}={hex}");
    let mut tm = format!("xmpp:{jid}?trust-message;encryption={};trust={hex}", super::v2::NS);
    for k in other_verified {
        tm.push_str(";trust=");
        tm.push_str(&super::curve::hex(k));
    }
    (sid, tm)
}

pub fn qr_svg(uri: &str) -> Option<String> {
    let code = qrcode::QrCode::new(uri.as_bytes()).ok()?;
    Some(code.render().min_dimensions(220, 220).dark_color(qrcode::render::svg::Color("#000000")).light_color(qrcode::render::svg::Color("#ffffff")).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trust_message_round_trips_and_one_for_something_else_is_not_read() {
        let mut d = Decisions::default();
        d.owner("bob@example.com").trust.push([1; 32]);
        d.owner("bob@example.com").distrust.push([2; 32]);
        d.owner("alice@example.org").trust.push([3; 32]);
        let e = element(&d);
        assert_eq!(parse(&e), Some(d));
        let other = Element::builder("trust-message", TM).at("usage", "urn:xmpp:other").at("encryption", super::super::v2::NS).build();
        assert_eq!(parse(&other), None);
    }

    /// XEP-0434's own example: the hex in the link is the key the trust message carries in base64.
    #[test]
    fn the_specifications_link_reads_as_its_trust_message_does() {
        let (jid, o) = parse_uri("xmpp:bob@example.com?trust-message;encryption=urn:xmpp:omemo:2;trust=623548d3835c6d33ef5cb680f7944ef381cf712bf23a0119dabe5c4f252cd02f;distrust=b423f5088de9a924d51b31581723d850c7cc67d0a4fe6b267c3d301ff56d2413").unwrap();
        assert_eq!(jid, "bob@example.com");
        assert_eq!(b64().encode(o.trust[0]), "YjVI04NcbTPvXLaA95RO84HPcSvyOgEZ2r5cTyUs0C8=");
        assert_eq!(b64().encode(o.distrust[0]), "tCP1CI3pqSTVGzFYFyPYUMfMZ9Ck/msmfD0wH/VtJBM=");
    }

    #[test]
    fn the_link_other_clients_show_is_read_with_or_without_the_key_type() {
        let key = [7u8; 32];
        let (sid, tm) = own_uris("me@example.org", 42, &key, &[[8; 32]]);
        let (jid, o) = parse_uri(&sid).unwrap();
        assert_eq!((jid.as_str(), o.trust.as_slice()), ("me@example.org", [key, key].as_slice()));
        let (_, o) = parse_uri(&tm).unwrap();
        assert_eq!(o.trust, vec![key, [8; 32]]);
        assert!(parse_uri("xmpp:me@example.org?message;body=hi").is_none());
        assert!(parse_uri("xmpp:me@example.org?trust-message;encryption=urn:xmpp:openpgp:0;trust=0707070707070707070707070707070707070707070707070707070707070707").is_none());
        assert!(qr_svg(&sid).is_some_and(|s| s.contains("<svg")));
    }
}
