//! What a room has hung on its wall.
//!
//! A Matrix room can carry widgets: a jitsi conference, an etherpad, a
//! whiteboard, a dashboard somebody wrote. They are state events, so they are
//! part of what the room *is* rather than something said in it - and a room
//! that keeps one showed nothing of it here at all, which made the room look
//! emptier than it was.
//!
//! This reads them and says what is there. It does not draw them, and that is
//! a decision rather than an omission: a widget is somebody else's web page,
//! and the client's whole security posture is that a message body never
//! reaches a renderer that can navigate (see the sanitiser in richtext, and
//! the captcha window's own comment on why *that* one gets a window of its
//! own). Opening one belongs in a browser, where a page from a room's
//! integration manager is somebody else's page in somebody else's sandbox.
//!
//! The url is templated. Element fills `$matrix_user_id`, `$matrix_room_id`
//! and friends before opening one, and a link handed over with the braces
//! still in it is a link that 404s - so the substitutions a widget can
//! actually be opened with are done here, and the ones that need a scalar
//! token are deliberately not.

use serde_json::{json, Value};

/// One widget, as a client needs it.
///
/// Kept as JSON rather than a struct because this crosses the wire almost
/// unchanged and has no behaviour - and because a widget's `data` is whatever
/// its author put there.
pub(super) fn read(id: &str, content: &Value) -> Option<Value> {
    // A widget is taken down by replacing its content with `{}` - state
    // events cannot be deleted - so an empty one is a removal, not a widget
    // with nothing in it.
    let url = content["url"].as_str().filter(|s| !s.is_empty())?;
    let kind = content["type"].as_str().unwrap_or("m.custom");
    Some(json!({
        "id": id,
        "kind": kind,
        // What to call it in a list. Widgets are often unnamed, and "Etherpad"
        // is a better answer than an empty row.
        "name": content["name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| pretty_kind(kind)),
        "url": url,
        "creator": content["creatorUserId"].as_str().unwrap_or(""),
        // Whether a link can actually be opened, which is not the same as
        // whether there is a url - see `fill`.
        "openable": !url.contains("$matrix_") || fillable(url),
        "data": content["data"].clone(),
    }))
}

/// A name for a widget that did not bring one.
fn pretty_kind(kind: &str) -> String {
    match kind {
        "jitsi" | "m.jitsi" => "Jitsi call".to_string(),
        "etherpad" | "m.etherpad" => "Etherpad".to_string(),
        "whiteboard" | "m.whiteboard" => "Whiteboard".to_string(),
        "grafana" => "Grafana".to_string(),
        "video" | "m.video" => "Video".to_string(),
        "m.custom" | "" => "Widget".to_string(),
        other => other.to_string(),
    }
}

/// The variables a widget url can carry, and what this client can put in them.
///
/// Element substitutes a longer list, several of which need a scalar token
/// from an integration manager this client does not have. Those are left
/// alone and the widget is reported as not openable, which is honest: a link
/// with `$matrix_display_name` still in it does not open, and offering it
/// anyway is offering a broken link.
const FILLABLE: [&str; 3] = ["$matrix_user_id", "$matrix_room_id", "$matrix_widget_id"];

fn fillable(url: &str) -> bool {
    // Every variable the url mentions has to be one we can fill.
    url.match_indices("$matrix_")
        .all(|(at, _)| FILLABLE.iter().any(|v| url[at..].starts_with(v)))
}

/// Puts this account and room into a widget's url.
pub fn fill(url: &str, user_id: &str, room_id: &str, widget_id: &str) -> String {
    url.replace("$matrix_user_id", &encode(user_id))
        .replace("$matrix_room_id", &encode(room_id))
        .replace("$matrix_widget_id", &encode(widget_id))
}

fn encode(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_widget_is_read_with_a_name_it_can_be_listed_under() {
        let w = read(
            "widget_1",
            &json!({ "type": "etherpad", "url": "https://pad.example/p/notes", "creatorUserId": "@a:b" }),
        )
        .expect("a widget");
        assert_eq!(w["kind"], "etherpad");
        // Unnamed, so named for what it is rather than left blank.
        assert_eq!(w["name"], "Etherpad");
        assert_eq!(w["creator"], "@a:b");
        assert_eq!(w["openable"], true);

        let named = read("w2", &json!({ "type": "m.custom", "url": "https://x/y", "name": "Rota" })).unwrap();
        assert_eq!(named["name"], "Rota");
    }

    /// A widget is taken down by emptying its content, because state events
    /// cannot be deleted. Reading that back as a widget would leave a removed
    /// one on screen for ever.
    #[test]
    fn an_emptied_widget_is_a_removed_widget() {
        assert!(read("w", &json!({})).is_none());
        assert!(read("w", &json!({ "type": "etherpad" })).is_none());
        assert!(read("w", &json!({ "url": "" })).is_none());
    }

    /// A url still holding a variable this client cannot fill does not open,
    /// and saying it does is offering a broken link.
    #[test]
    fn a_url_is_only_openable_when_every_variable_can_be_filled() {
        let ours = read("w", &json!({ "url": "https://x/?room=$matrix_room_id&me=$matrix_user_id" })).unwrap();
        assert_eq!(ours["openable"], true);

        let theirs = read("w", &json!({ "url": "https://x/?t=$matrix_display_name" })).unwrap();
        assert_eq!(theirs["openable"], false, "a variable we cannot fill must not be called openable");
    }

    /// The path a widget actually takes: a state event is read, kept by id,
    /// replaced when it changes, and removed when its content is emptied -
    /// with the list coming back in a stable order, because a list that
    /// reshuffles itself whenever a room's state is re-read is one nobody can
    /// point at.
    #[test]
    fn a_room_keeps_its_wall_in_order() {
        let runtime = crate::runtime::Runtime::new();
        let (account, room) = ("matrix:@salastil:poa.st", "!abc:poa.st");

        for (id, content) in [
            ("b_pad", json!({ "type": "etherpad", "url": "https://pad/x", "name": "Notes" })),
            ("a_call", json!({ "type": "jitsi", "url": "https://jitsi/y" })),
        ] {
            runtime.set_matrix_widget(account, room, id, read(id, &content));
        }
        let wall = runtime.matrix_widgets(account, room);
        assert_eq!(wall.len(), 2);
        // By id, and the same order every time.
        assert_eq!(wall[0]["id"], "a_call");
        assert_eq!(wall[0]["name"], "Jitsi call");
        assert_eq!(wall[1]["name"], "Notes");

        // Changed in place rather than added twice.
        runtime.set_matrix_widget(account, room, "b_pad", read("b_pad", &json!({ "type": "etherpad", "url": "https://pad/x", "name": "Minutes" })));
        let wall = runtime.matrix_widgets(account, room);
        assert_eq!(wall.len(), 2);
        assert_eq!(wall[1]["name"], "Minutes");

        // Taken down: the event arrives with empty content, `read` gives
        // nothing, and the widget goes.
        runtime.set_matrix_widget(account, room, "b_pad", read("b_pad", &json!({})));
        let wall = runtime.matrix_widgets(account, room);
        assert_eq!(wall.len(), 1);
        assert_eq!(wall[0]["id"], "a_call");

        // And a room nobody hung anything in has an empty wall, not an error.
        assert!(runtime.matrix_widgets(account, "!other:poa.st").is_empty());
    }

    #[test]
    fn filling_a_url_escapes_what_it_puts_in() {
        let filled = fill(
            "https://pad.example/?room=$matrix_room_id&me=$matrix_user_id&w=$matrix_widget_id",
            "@salastil:poa.st",
            "!abc:example.org",
            "widget 1",
        );
        assert_eq!(
            filled,
            "https://pad.example/?room=%21abc%3Aexample.org&me=%40salastil%3Apoa.st&w=widget+1"
        );
        // Nothing to fill is not an error.
        assert_eq!(fill("https://x/y", "@a:b", "!c:d", "w"), "https://x/y");
    }
}
