//! Making an account on a server (in-band registration, XEP-0077), before there is an account to sign in as.
//!
//! The connection is made the way an account's is - through the same route and with the same trust - and used for nothing
//! but asking the server what it needs and sending the answers. A server that wants more than a name and a password -
//! a captcha, an e-mail or phone check, a page to visit - is said so in words and left to its own page: moho never answers a
//! captcha, and a form with a picture to read out in it is refused as such.

use super::dataform::{self, Field, Form};
use super::xml::Attr;
use super::transport::{Connector, SeenSlot, Settings};
use super::{ns, xml};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::iq::Iq;
use tokio_xmpp::xmlstream::{ReadError, Timeouts, XmppStream, XmppStreamElement};
use tokio_xmpp::Stanza;

pub const REGISTER: &str = "jabber:iq:register";

/// What a server asks of somebody who wants an account.
#[derive(Debug, Serialize, Clone, PartialEq)]
pub struct Registration {
    pub form: Option<Form>,
    pub instructions: String,
    /// A page the server wants the person to use instead (an out-of-band form, XEP-0066).
    pub web: Option<String>,
    /// The server already has an account for this connection (registered earlier).
    pub registered: bool,
}

/// Reads the answer to the registration query. `Err` says why moho will not go on: a captcha, or a form that needs reading.
pub fn read_query(query: &Element) -> Result<Registration, String> {
    let instructions = query.children().find(|c| c.name() == "instructions").map(|c| c.text()).unwrap_or_default();
    let web = query.children().find(|c| c.is("x", ns::OOB)).and_then(|x| x.children().find(|u| u.name() == "url").map(|u| u.text()));
    let registered = query.children().any(|c| c.name() == "registered");
    if let Some(x) = query.children().find(|c| c.is("x", ns::DATA)) {
        let form = dataform::parse(x);
        let captcha = form.fields.iter().any(|f| f.var == "FORM_TYPE" && f.values.iter().any(|v| v.contains("captcha"))) || !form.media.is_empty() || query.children().any(|c| c.is("captcha", "urn:xmpp:captcha"));
        if captcha {
            return Err("This server asks for a captcha, which moho does not answer. Make the account on the server's own page, then sign in here".to_string());
        }
        return Ok(Registration { instructions: if form.instructions.is_empty() { instructions } else { form.instructions.clone() }, form: Some(form), web, registered });
    }
    // The old form: a fixed set of named fields.
    let mut fields = Vec::new();
    for c in query.children() {
        let name = c.name();
        if matches!(name, "instructions" | "registered" | "x") {
            continue;
        }
        fields.push(Field { var: name.to_string(), kind: if name == "password" { "text-private".into() } else { "text-single".into() }, label: name.to_string(), required: matches!(name, "username" | "password"), values: vec![c.text()], ..Default::default() });
    }
    if fields.is_empty() && web.is_some() {
        return Ok(Registration { form: None, instructions, web, registered });
    }
    if fields.is_empty() {
        return Err("This server does not take new accounts here".to_string());
    }
    Ok(Registration { form: Some(Form { kind: "form".into(), fields, ..Default::default() }), instructions, web, registered })
}

/// The `<query/>` that sends the answers: a form, or the old fields.
pub fn submission(form: &Form, answers: &serde_json::Value) -> Element {
    let mut q = Element::builder("query", REGISTER).build();
    if form.kind == "form" && form.fields.iter().any(|f| f.var == "FORM_TYPE") {
        q.append_child(dataform::submit(form, answers));
        return q;
    }
    for f in &form.fields {
        let value = answers.get(&f.var).and_then(|v| v.as_str()).unwrap_or_default();
        q.append_child(Element::builder(f.var.as_str(), REGISTER).append(value).build());
    }
    q
}

/// Opens a stream to the server and asks one thing of it.
async fn ask(settings: Settings, domain: &str, set: bool, payload: Element, preauth: &str) -> Result<Element, String> {
    let seen: SeenSlot = Default::default();
    let connector = Connector::new(settings, &seen).map_err(|e| format!("{e:#}"))?;
    let jid: Jid = domain.parse().map_err(|_| format!("{domain:?} is not a server address"))?;
    use tokio_xmpp::connect::ServerConnector;
    let (pending, _binding) = connector.connect(&jid, ns::CLIENT, Timeouts::default()).await.map_err(|e| {
        let shown = seen.lock().unwrap().clone();
        match shown {
            Some(s) if !s.refused.is_empty() => format!("The server's certificate was not trusted ({}). Its fingerprint is {}", s.refused, s.sha256),
            _ => format!("Could not reach {domain}: {e}"),
        }
    })?;
    let (_features, mut stream): (_, XmppStream<_>) = pending.recv_features().await.map_err(|e| format!("{e}"))?;
    // An invitation's token is said first, on its own and on this same stream (XEP-0401 4.2): the server then lets this
    // registration in, and links the new account to whoever invited it.
    if !preauth.is_empty() {
        let id = xml::new_id();
        let said = Element::builder("preauth", "urn:xmpp:pars:0").at("token", preauth).build();
        stream.send(&XmppStreamElement::Stanza(Stanza::Iq(Iq::Set { from: None, to: None, id: id.clone(), payload: said }))).await.map_err(|e| format!("{e}"))?;
        answer(&mut stream, &id).await.map_err(|e| format!("The invitation was not accepted ({e}): it may have been used or have run out"))?;
    }
    let id = xml::new_id();
    let iq = if set { Iq::Set { from: None, to: None, id: id.clone(), payload } } else { Iq::Get { from: None, to: None, id: id.clone(), payload } };
    stream.send(&XmppStreamElement::Stanza(Stanza::Iq(iq))).await.map_err(|e| format!("{e}"))?;
    let outcome = answer(&mut stream, &id).await;
    // The connection is dropped, which ends it; a clean close is not needed for a stream that did one thing.
    drop(stream);
    outcome
}

/// The answer to the request `id`, over a stream that does nothing else.
async fn answer<S: tokio::io::AsyncBufRead + tokio::io::AsyncWrite + Unpin>(stream: &mut XmppStream<S>, id: &str) -> Result<Element, String> {
    loop {
        match tokio::time::timeout(std::time::Duration::from_secs(30), stream.next()).await {
            Err(_) => break Err("The server did not answer".to_string()),
            Ok(None) | Ok(Some(Err(_))) => break Err("The connection ended".to_string()),
            Ok(Some(Ok(item))) => match item.into_read_error() {
                Ok(XmppStreamElement::Stanza(Stanza::Iq(Iq::Result { id: got, payload, .. }))) if got == id => break Ok(payload.unwrap_or_else(|| xml::empty("query", REGISTER))),
                Ok(XmppStreamElement::Stanza(Stanza::Iq(Iq::Error { id: got, error, .. }))) if got == id => break Err(refusal(&error)),
                Ok(_) | Err(ReadError::SoftTimeout) => {}
                Err(_) => break Err("The connection ended".to_string()),
            },
        }
    }
}

fn refusal(error: &tokio_xmpp::parsers::stanza_error::StanzaError) -> String {
    use tokio_xmpp::parsers::stanza_error::DefinedCondition as C;
    let text = error.texts.values().next().cloned().unwrap_or_default();
    let said = match &error.defined_condition {
        C::Conflict => "That name is already taken".to_string(),
        C::NotAcceptable => "The server did not accept that (check the name and the password)".to_string(),
        C::NotAllowed | C::Forbidden => "This server does not take new accounts from here".to_string(),
        C::ServiceUnavailable | C::FeatureNotImplemented => "This server does not take new accounts over the connection".to_string(),
        C::ResourceConstraint | C::PolicyViolation => "The server is not taking registrations from you for now".to_string(),
        other => format!("The server refused ({other:?})"),
    };
    if text.is_empty() {
        said
    } else {
        format!("{said}: {text}")
    }
}

/// What the server needs to make an account (with an invitation's token, what it needs of somebody invited).
pub async fn form(settings: Settings, domain: &str, preauth: &str) -> Result<Registration, String> {
    let query = ask(settings, domain, false, xml::empty("query", REGISTER), preauth).await?;
    read_query(&query)
}

/// Sends the answers. `Ok` when the server made the account. The token, if there is one, is said again on this stream: the
/// form and the answers are asked on connections of their own.
pub async fn submit(settings: Settings, domain: &str, form: &Form, answers: &serde_json::Value, preauth: &str) -> Result<(), String> {
    ask(settings, domain, true, submission(form, answers), preauth).await.map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use serde_json::json;

    #[test]
    fn a_server_that_wants_a_name_and_a_password_in_the_old_form_is_read_as_a_form() {
        let q = Element::from_str("<query xmlns='jabber:iq:register'><instructions>Choose a name</instructions><username/><password/></query>").unwrap();
        let r = read_query(&q).unwrap();
        let f = r.form.unwrap();
        assert_eq!((f.fields.iter().map(|f| f.var.as_str()).collect::<Vec<_>>(), r.instructions.as_str()), (vec!["username", "password"], "Choose a name"));
        assert_eq!(f.fields[1].kind, "text-private");
    }

    #[test]
    fn a_data_form_is_read_and_answered_as_one() {
        let q = Element::from_str("<query xmlns='jabber:iq:register'><x xmlns='jabber:x:data' type='form'><field var='FORM_TYPE' type='hidden'><value>jabber:iq:register</value></field><field var='username' type='text-single'/><field var='password' type='text-private'/></x></query>").unwrap();
        let r = read_query(&q).unwrap();
        let sent = submission(r.form.as_ref().unwrap(), &json!({ "username": "alice", "password": "pw" }));
        let x = sent.children().find(|c| c.name() == "x").unwrap();
        let v = |var: &str| x.children().find(|c| c.attr("var") == Some(var)).map(|c| c.children().next().unwrap().text());
        assert_eq!((v("username").as_deref(), v("password").as_deref()), (Some("alice"), Some("pw")));
    }

    #[test]
    fn the_old_form_is_answered_in_the_old_fields() {
        let q = Element::from_str("<query xmlns='jabber:iq:register'><username/><password/></query>").unwrap();
        let r = read_query(&q).unwrap();
        let sent = submission(r.form.as_ref().unwrap(), &json!({ "username": "bob", "password": "x" }));
        assert_eq!(sent.children().map(|c| (c.name().to_string(), c.text())).collect::<Vec<_>>(), vec![("username".to_string(), "bob".to_string()), ("password".to_string(), "x".to_string())]);
    }

    #[test]
    fn a_captcha_is_refused_in_words_and_a_page_to_visit_is_offered_as_a_page() {
        let c = Element::from_str("<query xmlns='jabber:iq:register'><x xmlns='jabber:x:data' type='form'><field var='FORM_TYPE' type='hidden'><value>urn:xmpp:captcha</value></field><field var='ocr'><media xmlns='urn:xmpp:media-element'><uri type='image/png'>cid:x</uri></media></field></x></query>").unwrap();
        assert!(read_query(&c).unwrap_err().contains("captcha"));
        let w = Element::from_str("<query xmlns='jabber:iq:register'><instructions>Use the page</instructions><x xmlns='jabber:x:oob'><url>https://example.org/signup</url></x></query>").unwrap();
        let r = read_query(&w).unwrap();
        assert_eq!((r.form.is_none(), r.web.as_deref()), (true, Some("https://example.org/signup")));
    }

    #[test]
    fn a_server_with_nothing_to_ask_does_not_take_accounts_here() {
        assert!(read_query(&Element::from_str("<query xmlns='jabber:iq:register'/>").unwrap()).is_err());
    }
}
