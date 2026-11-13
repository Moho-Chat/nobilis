//! Data forms (XEP-0004): the way XMPP asks for a set of answers - a room's settings, an in-band registration, an
//! ad-hoc command's steps - and so the one place a form is turned into what the window draws and back.
//!
//! The window is handed a form as plain JSON (`Form`) and answers with plain JSON (`{var: value}`); nothing else in the
//! daemon needs to know XML to ask or to answer one.

use super::ns;
use super::xml::Attr;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_xmpp::minidom::Element;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Option_ {
    pub label: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Field {
    pub var: String,
    /// `text-single`, `text-private`, `text-multi`, `boolean`, `list-single`, `list-multi`, `jid-single`, `jid-multi`,
    /// `fixed`, `hidden`.
    #[serde(rename = "type")]
    pub kind: String,
    pub label: String,
    pub desc: String,
    pub required: bool,
    pub values: Vec<String>,
    pub options: Vec<Option_>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Form {
    /// `form`, `submit`, `cancel` or `result`.
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub instructions: String,
    pub fields: Vec<Field>,
    /// A link a captcha or a media element points at, if any: such a form is not answered here.
    pub media: Vec<String>,
}

pub fn parse(x: &Element) -> Form {
    let text = |e: &Element, name: &str| e.children().find(|c| c.name() == name).map(|c| c.text()).unwrap_or_default();
    let mut form = Form { kind: x.attr("type").unwrap_or("form").to_string(), title: text(x, "title"), instructions: x.children().filter(|c| c.name() == "instructions").map(|c| c.text()).collect::<Vec<_>>().join("\n"), ..Default::default() };
    for f in x.children().filter(|c| c.name() == "field") {
        let kind = f.attr("type").unwrap_or("text-single").to_string();
        let mut field = Field { var: f.attr("var").unwrap_or_default().to_string(), kind, label: f.attr("label").unwrap_or_default().to_string(), desc: text(f, "desc"), required: f.children().any(|c| c.name() == "required"), ..Default::default() };
        field.values = f.children().filter(|c| c.name() == "value").map(|c| c.text()).collect();
        field.options = f.children().filter(|c| c.name() == "option").map(|o| Option_ { label: o.attr("label").unwrap_or_default().to_string(), value: text(o, "value") }).collect();
        for media in f.children().filter(|c| c.name() == "media") {
            for uri in media.children().filter(|c| c.name() == "uri") {
                form.media.push(uri.text());
            }
        }
        form.fields.push(field);
    }
    form
}

/// The answers to a form as `<x type='submit'>`: each of `answers` is a variable and its value(s) - a string, a bool, a list
/// of strings. Variables the form has that the answers do not mention keep their default, which is what a form is for.
pub fn submit(form: &Form, answers: &Value) -> Element {
    let mut x = Element::builder("x", ns::DATA).at("type", "submit").build();
    for f in &form.fields {
        if f.kind == "fixed" {
            continue;
        }
        let values: Vec<String> = match answers.get(&f.var) {
            Some(Value::String(s)) => s.lines().map(str::to_string).collect::<Vec<_>>().into_iter().take(if f.kind == "text-multi" { usize::MAX } else { 1 }).collect(),
            Some(Value::Bool(b)) => vec![if *b { "1".into() } else { "0".into() }],
            Some(Value::Number(n)) => vec![n.to_string()],
            Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
            _ => f.values.clone(),
        };
        if f.var.is_empty() {
            continue;
        }
        let mut field = Element::builder("field", ns::DATA).at("var", f.var.as_str());
        if f.kind != "text-single" {
            field = field.at("type", f.kind.as_str());
        }
        let mut field = field.build();
        // A text field answered with a single string that has several lines is several values only for text-multi.
        let values = if f.kind != "text-multi" { match answers.get(&f.var) { Some(Value::String(s)) => vec![s.clone()], _ => values } } else { values };
        for v in values {
            field.append_child(Element::builder("value", ns::DATA).append(v).build());
        }
        x.append_child(field);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    const ROOM_CONFIG: &str = "<x xmlns='jabber:x:data' type='form'><title>Room</title><instructions>Fill in</instructions><field var='FORM_TYPE' type='hidden'><value>http://jabber.org/protocol/muc#roomconfig</value></field><field var='muc#roomconfig_roomname' type='text-single' label='Name'><value>Old</value></field><field var='muc#roomconfig_persistentroom' type='boolean' label='Persistent'><value>0</value></field><field var='muc#roomconfig_whois' type='list-single' label='Who sees addresses'><value>moderators</value><option label='Moderators'><value>moderators</value></option><option label='Anyone'><value>anyone</value></option></field><field var='muc#roomconfig_roomdesc' type='text-multi'><value>a</value><value>b</value></field></x>";

    #[test]
    fn a_form_is_read_with_its_fields_options_and_defaults() {
        let f = parse(&Element::from_str(ROOM_CONFIG).unwrap());
        assert_eq!((f.kind.as_str(), f.title.as_str(), f.fields.len()), ("form", "Room", 5));
        assert_eq!(f.fields[0].values, vec!["http://jabber.org/protocol/muc#roomconfig"]);
        let whois = f.fields.iter().find(|x| x.var.ends_with("whois")).unwrap();
        assert_eq!((whois.options.len(), whois.options[1].value.as_str(), whois.values.clone()), (2, "anyone", vec!["moderators".to_string()]));
        assert_eq!(f.fields[4].values, vec!["a", "b"]);
    }

    #[test]
    fn answers_replace_what_they_name_and_everything_else_keeps_its_default() {
        let f = parse(&Element::from_str(ROOM_CONFIG).unwrap());
        let x = submit(&f, &json!({ "muc#roomconfig_roomname": "New", "muc#roomconfig_persistentroom": true }));
        assert_eq!(x.attr("type"), Some("submit"));
        let value = |var: &str| x.children().find(|c| c.attr("var") == Some(var)).map(|c| c.children().map(|v| v.text()).collect::<Vec<_>>());
        assert_eq!(value("muc#roomconfig_roomname"), Some(vec!["New".to_string()]));
        assert_eq!(value("muc#roomconfig_persistentroom"), Some(vec!["1".to_string()]));
        assert_eq!(value("muc#roomconfig_whois"), Some(vec!["moderators".to_string()]));
        assert_eq!(value("FORM_TYPE"), Some(vec!["http://jabber.org/protocol/muc#roomconfig".to_string()]));
    }

    #[test]
    fn a_multi_line_answer_is_several_values_only_for_a_multi_line_field() {
        let f = parse(&Element::from_str(ROOM_CONFIG).unwrap());
        let x = submit(&f, &json!({ "muc#roomconfig_roomdesc": "one\ntwo", "muc#roomconfig_roomname": "a\nb" }));
        let count = |var: &str| x.children().find(|c| c.attr("var") == Some(var)).map(|c| c.children().count());
        assert_eq!(count("muc#roomconfig_roomdesc"), Some(2));
        assert_eq!(count("muc#roomconfig_roomname"), Some(1));
    }

    #[test]
    fn a_form_with_a_picture_to_read_out_is_marked_so_it_is_not_answered_for_the_person() {
        let f = parse(&Element::from_str("<x xmlns='jabber:x:data' type='form'><field var='ocr' type='text-single'><media xmlns='urn:xmpp:media-element'><uri type='image/png'>cid:abc@example.org</uri></media></field></x>").unwrap());
        assert_eq!(f.media, vec!["cid:abc@example.org"]);
    }
}
