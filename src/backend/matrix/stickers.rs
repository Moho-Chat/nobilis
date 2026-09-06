//! Sticker packs, and the places Matrix keeps them.
//!
//! Matrix has no sticker service. A sticker is an `m.sticker` event carrying
//! an image, and where the images *come from* is a convention rather than a
//! specification: MSC2545 image packs, which is what Element's own picker
//! ends up reading and what every pack bot writes.
//!
//! A pack lives in one of two places, and both are read here because people
//! have them in both:
//!
//! - `im.ponies.user_emotes` in the account's own data, which is the pack
//!   somebody assembled for themselves and carries between clients.
//! - `im.ponies.room_emotes` in a room's state, which is the pack a room
//!   shares with everybody in it - keyed by state key, so a room may have
//!   several.
//!
//! An image in a pack says what it is for: `usage: ["sticker"]`, `["emoticon"]`
//! or neither, which means both. Only stickers are offered here; sending an
//! emoticon as a sticker posts a 20-pixel image as a message of its own, which
//! is not what anybody meant by it.

use serde_json::Value;

/// One image out of a pack, in the shape the picker draws and sends.
#[derive(Clone, Debug, PartialEq)]
pub struct Sticker {
    /// The shortcode, which is also what a client with no images shows.
    pub name: String,
    /// The pack it came from, for grouping in the picker.
    pub pack: String,
    /// The image itself, still as an mxc URI - resolving it needs the
    /// account's token, so that happens where the token is.
    pub mxc: String,
    /// What it is called in a client that cannot show it, and the fallback
    /// body of the event that sends it.
    pub body: String,
}

/// Whether this image is a sticker, as its pack describes it.
///
/// Absent usage means "both", per the convention: a pack that says nothing is
/// a pack of images usable either way, and dropping those would empty most of
/// the packs people actually have.
fn is_sticker(image: &Value, pack_usage: &Value) -> bool {
    let usage = image.get("usage").filter(|u| u.is_array()).unwrap_or(pack_usage);
    match usage.as_array() {
        None => true,
        Some(list) if list.is_empty() => true,
        Some(list) => list.iter().any(|u| u.as_str() == Some("sticker")),
    }
}

/// Reads one `im.ponies.*` pack into the stickers it offers.
///
/// Tolerant on purpose: these are written by half a dozen different bots and
/// clients, and one malformed image in a pack should cost that image rather
/// than the pack.
pub fn read_pack(content: &Value, fallback_name: &str) -> Vec<Sticker> {
    let pack_name = content["pack"]["display_name"]
        .as_str()
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(fallback_name)
        .to_string();
    let pack_usage = content["pack"]["usage"].clone();
    let Some(images) = content["images"].as_object() else {
        return Vec::new();
    };
    images
        .iter()
        .filter(|(_, image)| is_sticker(image, &pack_usage))
        .filter_map(|(shortcode, image)| {
            let mxc = image["url"].as_str()?;
            if !mxc.starts_with("mxc://") {
                return None;
            }
            Some(Sticker {
                name: shortcode.clone(),
                pack: pack_name.clone(),
                mxc: mxc.to_string(),
                body: image["body"]
                    .as_str()
                    .filter(|b| !b.trim().is_empty())
                    .unwrap_or(shortcode)
                    .to_string(),
            })
        })
        .collect()
}

/// The content of an `m.sticker` event for one of these.
///
/// `info` is left to the caller: a sticker's dimensions are what a client
/// draws it at, and this module does not fetch the image to measure it.
pub fn sticker_event(sticker: &Sticker) -> Value {
    serde_json::json!({
        "body": sticker.body,
        "url": sticker.mxc,
        "info": {},
    })
}

/// A place, as `m.location` spells one.
///
/// The `geo:` URI is the machine-readable half and the body is what a client
/// with no map shows - which is most of them, this one included, so the body
/// is written to be read rather than to be parsed.
pub fn location_event(latitude: f64, longitude: f64, label: &str) -> Value {
    let geo = format!("geo:{latitude},{longitude}");
    let body = if label.trim().is_empty() {
        format!("{latitude}, {longitude}")
    } else {
        format!("{}, {latitude}, {longitude}", label.trim())
    };
    serde_json::json!({
        "msgtype": "m.location",
        "body": body,
        "geo_uri": geo,
        "org.matrix.msc3488.location": { "uri": geo, "description": label.trim() },
        "org.matrix.msc3488.asset": { "type": "m.pin" },
    })
}

/// Coordinates out of whatever somebody pasted.
///
/// People do not type `geo:` URIs. They paste a map link, or they type two
/// numbers with a comma between them, and both of those are what this
/// accepts - along with the `geo:` form, since a location received here can
/// then be sent on.
pub fn parse_place(text: &str) -> Option<(f64, f64)> {
    let text = text.trim();
    // A pair of numbers, however spaced: "51.5, -0.12".
    let plain = text.trim_start_matches("geo:");
    if let Some((lat, lon)) = plain.split_once(',') {
        if let (Ok(lat), Ok(lon)) = (lat.trim().parse::<f64>(), lon.trim().split(';').next().unwrap_or("").trim().parse::<f64>()) {
            if valid(lat, lon) {
                return Some((lat, lon));
            }
        }
    }
    // A map link, in the two shapes people paste. Read by their own layout
    // rather than by scanning for any two numbers: OpenStreetMap puts the
    // zoom level first, and "the first two numbers in this URL" reads that
    // zoom as a latitude.
    let pair = |rest: &str, separators: [char; 2], skip: usize| -> Option<(f64, f64)> {
        let mut parts = rest.split(separators).skip(skip);
        let lat = parts.next()?.trim().parse::<f64>().ok()?;
        let lon = parts.next()?.trim().parse::<f64>().ok()?;
        valid(lat, lon).then_some((lat, lon))
    };
    // openstreetmap.org/#map=15/51.5/-0.12
    if let Some(rest) = text.split("#map=").nth(1) {
        if let Some(found) = pair(rest, ['/', '&'], 1) {
            return Some(found);
        }
    }
    // google.com/maps/@51.5,-0.12,15z
    if let Some(rest) = text.split('@').nth(1) {
        if let Some(found) = pair(rest, [',', '/'], 0) {
            return Some(found);
        }
    }
    // Anything with the place in a query parameter.
    for marker in ["?q=", "&q=", "query=", "?mlat=", "ll="] {
        if let Some(rest) = text.split(marker).nth(1) {
            if let Some(found) = pair(rest, [',', '&'], 0) {
                return Some(found);
            }
        }
    }
    None
}

/// Splits "51.5, -0.12 the pub" into the place and what it is called.
///
/// The place is however many words still parse as one, which is the only way
/// to tell the two apart: coordinates are written with a space after the comma
/// as often as not, so "the first word" is half a place, and "everything" is a
/// place with a name stuck to it. Longest wins, and what is left is the name.
pub fn split_place_and_label(text: &str) -> Option<(String, String)> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let mut best = 0;
    for take in 1..=words.len() {
        if parse_place(&words[..take].join(" ")).is_some() {
            best = take;
        }
    }
    (best > 0).then(|| (words[..best].join(" "), words[best..].join(" ")))
}

/// Somewhere on Earth, rather than two numbers that happened to parse.
fn valid(latitude: f64, longitude: f64) -> bool {
    (-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack() -> Value {
        serde_json::json!({
            "pack": { "display_name": "Cats" },
            "images": {
                "sleepy": { "url": "mxc://example.org/a", "usage": ["sticker"] },
                "wave": { "url": "mxc://example.org/b", "body": "waving cat" },
                "tiny": { "url": "mxc://example.org/c", "usage": ["emoticon"] },
                "broken": { "body": "no url at all" },
                "elsewhere": { "url": "https://example.org/not-matrix.png" }
            }
        })
    }

    #[test]
    fn a_pack_offers_its_stickers_and_not_its_emoticons() {
        let mut read = read_pack(&pack(), "fallback");
        read.sort_by(|a, b| a.name.cmp(&b.name));
        let names: Vec<&str> = read.iter().map(|s| s.name.as_str()).collect();
        // "wave" says nothing about usage, which means both - dropping those
        // would empty most real packs.
        assert_eq!(names, vec!["sleepy", "wave"]);
        assert_eq!(read[0].pack, "Cats");
        assert_eq!(read[1].body, "waving cat");
    }

    #[test]
    fn a_pack_with_no_name_borrows_the_one_it_was_filed_under() {
        let content = serde_json::json!({ "images": { "a": { "url": "mxc://example.org/a" } } });
        assert_eq!(read_pack(&content, "this room").first().unwrap().pack, "this room");
    }

    #[test]
    fn nothing_at_all_is_not_an_error() {
        assert!(read_pack(&serde_json::json!({}), "x").is_empty());
        assert!(read_pack(&serde_json::json!({ "images": [] }), "x").is_empty());
    }

    #[test]
    fn a_place_is_read_from_what_people_actually_paste() {
        assert_eq!(parse_place("51.5074, -0.1278"), Some((51.5074, -0.1278)));
        assert_eq!(parse_place("geo:51.5074,-0.1278;u=35"), Some((51.5074, -0.1278)));
        assert_eq!(
            parse_place("https://www.openstreetmap.org/#map=15/51.5074/-0.1278"),
            Some((51.5074, -0.1278))
        );
        assert_eq!(
            parse_place("https://www.google.com/maps/@51.5074,-0.1278,15z"),
            Some((51.5074, -0.1278))
        );
    }

    #[test]
    fn two_numbers_that_are_not_a_place_are_refused() {
        // A latitude past the pole is somebody's phone number, not a place.
        assert_eq!(parse_place("910, 20"), None);
        assert_eq!(parse_place("hello"), None);
        assert_eq!(parse_place(""), None);
    }

    #[test]
    fn a_place_and_its_name_come_apart() {
        // The comma-space form is how people write coordinates, so the place
        // is two words and the name is the rest.
        assert_eq!(
            split_place_and_label("51.5074, -0.1278 the pub"),
            Some(("51.5074, -0.1278".to_string(), "the pub".to_string()))
        );
        assert_eq!(
            split_place_and_label("51.5074,-0.1278"),
            Some(("51.5074,-0.1278".to_string(), String::new()))
        );
        assert_eq!(
            split_place_and_label("https://www.openstreetmap.org/#map=15/51.5074/-0.1278 home"),
            Some(("https://www.openstreetmap.org/#map=15/51.5074/-0.1278".to_string(), "home".to_string()))
        );
        assert_eq!(split_place_and_label("where are you"), None);
    }

    #[test]
    fn a_location_event_carries_both_halves() {
        let event = location_event(51.5074, -0.1278, "the pub");
        assert_eq!(event["geo_uri"], "geo:51.5074,-0.1278");
        assert_eq!(event["msgtype"], "m.location");
        // The body is what a client with no map shows, so the label leads.
        assert_eq!(event["body"], "the pub, 51.5074, -0.1278");
    }
}
