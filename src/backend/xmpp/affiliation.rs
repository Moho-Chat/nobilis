//! What somebody's server says about their account (XEP-0489): a guest, a registered account and since when, a member, an
//! administrator, and how far the server trusts it. Embedded by the server in a request from a stranger, so the person deciding
//! whether to answer has more to go on than an address - and believed only when that server says it embeds it (it is told to
//! strip any a client puts in itself; a server that says nothing may pass a forged one along).

use super::session::Handle;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;

pub const NS: &str = "urn:xmpp:raa:0";
/// The feature a server advertises when it adds the element to its users' subscription requests.
pub const EMBED_SUB: &str = "urn:xmpp:raa:0#embed-presence-sub";

/// The element in words: what the account is, since when, and how far its server trusts it. `None` when it says nothing usable.
pub fn words(info: &Element, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let since = info.attr("since").and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.with_timezone(&chrono::Utc));
    let age = since.map(|d| {
        let days = (now - d).num_days();
        match days {
            i64::MIN..=0 => "today".to_string(),
            1 => "yesterday".to_string(),
            2..=60 => format!("{days} days ago"),
            61..=730 => format!("{} months ago", days / 30),
            _ => format!("{} years ago", days / 365),
        }
    });
    let mut out = match info.attr("affiliation")? {
        "anonymous" => "a guest account, not registered".to_string(),
        "registered" => match &age {
            Some(a) => format!("registered {a}"),
            None => "a registered account".to_string(),
        },
        "member" => "a member their server knows".to_string(),
        "admin" => "an administrator of their server".to_string(),
        _ => return None,
    };
    if let Some(trust) = info.attr("trust").and_then(|t| t.parse::<u8>().ok()).filter(|t| *t <= 100) {
        out.push_str(&format!(" (their server rates its trust {trust}/100)"));
    }
    Some(out)
}

/// The element's words, if the sender's server says it embeds it in this kind of stanza.
pub async fn vouched(handle: &Handle, from: &BareJid, info: &Element, feature: &str) -> Option<String> {
    let domain = from.domain().to_string();
    let known = handle.session().domain_features.get(&domain).cloned();
    let features = match known {
        Some(f) => f,
        None => {
            let to: Jid = domain.parse().ok()?;
            let q = super::xml::empty("query", super::ns::DISCO_INFO);
            let answer = handle.request(Some(to), false, q).await.ok()??;
            let f: std::collections::HashSet<String> = answer.children().filter(|c| c.name() == "feature").filter_map(|c| c.attr("var").map(str::to_string)).collect();
            handle.session().domain_features.insert(domain, f.clone());
            f
        }
    };
    if !features.contains(feature) {
        tracing::debug!("xmpp[{}]: {from}'s server does not say it vouches for what it adds: ignored", handle.account_id);
        return None;
    }
    words(info, chrono::Utc::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn what_a_server_says_is_put_in_words() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-12T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let e = |s: &str| Element::from_str(&format!("<info xmlns='urn:xmpp:raa:0' {s}/>")).unwrap();
        assert_eq!(words(&e("affiliation='registered' since='2026-11-09T00:00:00Z' trust='47'"), now).as_deref(), Some("registered 3 days ago (their server rates its trust 47/100)"));
        assert_eq!(words(&e("affiliation='anonymous'"), now).as_deref(), Some("a guest account, not registered"));
        assert_eq!(words(&e("affiliation='admin'"), now).as_deref(), Some("an administrator of their server"));
        assert_eq!(words(&e("affiliation='registered' trust='400'"), now).as_deref(), Some("a registered account"), "a trust beyond 100 is not one");
        assert_eq!(words(&e("affiliation='astronaut'"), now), None);
    }
}
