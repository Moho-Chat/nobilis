//! What the server keeps (XEP-0441): the account's message archive can keep everything, only what is exchanged with people on the
//! roster, or nothing, with exceptions by address. Kept on the server, so it is the same for every client.

use super::session::Handle;
use super::xml::Attr;
use super::ns;
use serde::Serialize;
use tokio_xmpp::minidom::Element;

#[derive(Debug, Serialize, Clone, PartialEq, Default)]
pub struct Prefs {
    /// `always`, `never` or `roster`.
    pub default: String,
    pub always: Vec<String>,
    pub never: Vec<String>,
}

pub fn parse(prefs: &Element) -> Prefs {
    let list = |name: &str| prefs.children().find(|c| c.name() == name).map(|l| l.children().filter(|j| j.name() == "jid").map(|j| j.text()).collect()).unwrap_or_default();
    Prefs { default: prefs.attr("default").unwrap_or("always").to_string(), always: list("always"), never: list("never") }
}

pub fn build(p: &Prefs) -> Element {
    let mut e = Element::builder("prefs", ns::MAM).at("default", p.default.as_str()).build();
    for (name, list) in [("always", &p.always), ("never", &p.never)] {
        let mut l = Element::builder(name, ns::MAM).build();
        for j in list {
            l.append_child(Element::builder("jid", ns::MAM).append(j.as_str()).build());
        }
        e.append_child(l);
    }
    e
}

pub async fn get(handle: &Handle) -> Result<Prefs, String> {
    let answer = handle.request(None, false, Element::builder("prefs", ns::MAM).build()).await.map_err(|e| e.words())?;
    Ok(answer.as_ref().map(parse).unwrap_or_default())
}

pub async fn set(handle: &Handle, p: &Prefs) -> Result<(), String> {
    if !matches!(p.default.as_str(), "always" | "never" | "roster") {
        return Err("the archive keeps always, never or only what is with people on the roster".to_string());
    }
    handle.request(None, true, build(p)).await.map(|_| ()).map_err(|e| e.words())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn what_the_server_keeps_is_read_and_written_back_the_same() {
        let e = Element::from_str("<prefs xmlns='urn:xmpp:mam:2' default='roster'><always><jid>a@example.org</jid></always><never><jid>b@example.org</jid><jid>c.example</jid></never></prefs>").unwrap();
        let p = parse(&e);
        assert_eq!((p.default.as_str(), p.always.clone(), p.never.clone()), ("roster", vec!["a@example.org".to_string()], vec!["b@example.org".to_string(), "c.example".to_string()]));
        assert_eq!(parse(&build(&p)), p);
    }
}
