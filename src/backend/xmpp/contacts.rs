//! The contact list in and out of a file: the roster as RFC 6121 writes it (`<query xmlns='jabber:iq:roster'>` with each item's
//! address, name and groups), which other clients (Psi, Gajim's export) and server exports (XEP-0227, whose roster is the same
//! element) read and write. Imported from that, from a XEP-0227 file (its rosters, whoever's they are, are read the same way),
//! or from a plain list of addresses, one a line.

use super::session::Handle;
use super::xml::Attr;
use tokio_xmpp::jid::BareJid;
use tokio_xmpp::minidom::Element;

const ROSTER: &str = "jabber:iq:roster";

/// One contact as a file holds it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub jid: BareJid,
    pub name: Option<String>,
    pub groups: Vec<String>,
}

/// The roster as a file: `<query xmlns='jabber:iq:roster'>` with one item a contact.
pub fn export(handle: &Handle) -> String {
    let s = handle.session();
    let mut q = Element::builder("query", ROSTER).build();
    for (jid, c) in &s.roster {
        let mut item = Element::builder("item", ROSTER).at("jid", jid.to_string());
        if let Some(n) = c.name.as_deref().filter(|n| !n.is_empty()) {
            item = item.at("name", n);
        }
        let mut item = item.build();
        for g in &c.groups {
            item.append_child(Element::builder("group", ROSTER).append(g.clone()).build());
        }
        q.append_child(item);
    }
    format!("<?xml version='1.0' encoding='UTF-8'?>\n{}\n", String::from(&q))
}

/// Reads a file: roster XML (alone, or anywhere inside, as in a XEP-0227 export), or addresses one a line (anything after the
/// address on a line - a comma, a space - is its name).
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let t = text.trim_start_matches('\u{feff}').trim();
    let mut out: Vec<Entry> = Vec::new();
    let push = |e: Entry, out: &mut Vec<Entry>| {
        if !out.iter().any(|x| x.jid == e.jid) {
            out.push(e);
        }
    };
    if t.starts_with('<') {
        let body = t.trim_start_matches(|c| c != '>').get(1..).filter(|_| t.starts_with("<?")).unwrap_or(t);
        let root: Element = body.trim().parse().map_err(|e| format!("the file is not XML moho can read: {e}"))?;
        fn walk(e: &Element, found: &mut Vec<Element>) {
            for c in e.children() {
                if c.is("item", ROSTER) {
                    found.push(c.clone());
                } else {
                    walk(c, found);
                }
            }
        }
        let mut items = Vec::new();
        if root.is("item", ROSTER) {
            items.push(root.clone());
        }
        walk(&root, &mut items);
        for i in items {
            let Some(jid) = i.attr("jid").and_then(|j| super::connect::normalise_jid(j).ok()) else { continue };
            let groups = i.children().filter(|g| g.name() == "group").map(|g| g.text().trim().to_string()).filter(|g| !g.is_empty()).collect();
            push(Entry { jid, name: i.attr("name").map(str::to_string).filter(|n| !n.is_empty()), groups }, &mut out);
        }
        if out.is_empty() {
            return Err("the file holds no roster items".to_string());
        }
        return Ok(out);
    }
    for line in t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let (addr, rest) = line.split_once([',', ';', '\t', ' ']).unwrap_or((line, ""));
        let addr = addr.trim().trim_start_matches("xmpp:");
        let Ok(jid) = super::connect::normalise_jid(addr) else { continue };
        let name = rest.trim().trim_matches(|c| c == ',' || c == '"').trim().to_string();
        push(Entry { jid, name: (!name.is_empty()).then_some(name), groups: Vec::new() }, &mut out);
    }
    if out.is_empty() {
        return Err("the file holds no addresses".to_string());
    }
    Ok(out)
}

/// What an import did.
#[derive(Debug, Default, serde::Serialize)]
pub struct Imported {
    pub added: usize,
    /// Already on the roster: left as they were.
    pub kept: usize,
    pub failed: Vec<String>,
}

/// Adds what a file holds to the roster. `ask`: also ask each new contact to share their presence (as adding by hand does).
pub async fn import(handle: &Handle, entries: &[Entry], ask: bool) -> Imported {
    let mut done = Imported::default();
    for e in entries {
        if e.jid == handle.own() {
            continue;
        }
        if handle.session().roster.contains_key(&e.jid) {
            done.kept += 1;
            continue;
        }
        let group = e.groups.first().map(String::as_str);
        let result = if ask {
            super::roster::add_contact(handle, &e.jid, e.name.as_deref(), group, None).await
        } else {
            super::roster::set_item(handle, &e.jid, e.name.as_deref(), Some(e.groups.clone())).await
        };
        match result {
            Ok(()) => done.added += 1,
            Err(why) => done.failed.push(format!("{}: {why}", e.jid)),
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_xml_and_a_xep0227_file_and_a_list_of_addresses_are_all_read() {
        let roster = "<?xml version='1.0'?>\n<query xmlns='jabber:iq:roster'><item jid='Romeo@Example.net' name='Romeo'><group>Friends</group></item><item jid='mercutio@example.org'/></query>";
        let r = parse(roster).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!((r[0].jid.as_str(), r[0].name.as_deref(), r[0].groups.clone()), ("romeo@example.net", Some("Romeo"), vec!["Friends".to_string()]));
        let xep0227 = "<server-data xmlns='urn:xmpp:pie:0'><host jid='capulet.example'><user name='juliet'><query xmlns='jabber:iq:roster'><item jid='romeo@montague.example' name='Romeo' subscription='both'><group>Lovers</group></item></query></user></host></server-data>";
        assert_eq!(parse(xep0227).unwrap()[0].jid.as_str(), "romeo@montague.example");
        let list = "# my friends\nnurse@capulet.example, The Nurse\nxmpp:benvolio@montague.example\nnot an address\nnurse@capulet.example\n";
        let l = parse(list).unwrap();
        assert_eq!(l.iter().map(|e| e.jid.as_str()).collect::<Vec<_>>(), vec!["nurse@capulet.example", "benvolio@montague.example"]);
        assert_eq!(l[0].name.as_deref(), Some("The Nurse"));
        assert!(parse("nothing here").is_err());
        assert!(parse("<query xmlns='jabber:iq:roster'/>").is_err());
    }
}
