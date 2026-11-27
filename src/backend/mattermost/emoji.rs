//! Mattermost names its emoji: a post says `:smile:`, a reaction is stored as `smile` or `+1`, and a server can add its
//! own pictures under names of their own. The window shows a standard emoji as itself and a custom one as its picture,
//! and its picker hands back the character; this is the table between the two.
//!
//! The names are the ones Mattermost's app uses (the emoji-data set, MIT), kept in `emoji_names.tsv`.

use super::media;
use super::Handle;
use crate::state::AppState;
use regex::Regex;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Characters by name, and names by character.
type Tables = (HashMap<&'static str, &'static str>, HashMap<&'static str, &'static str>);

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut by_name = HashMap::new();
        let mut by_char = HashMap::new();
        for line in include_str!("emoji_names.tsv").lines() {
            if let Some((name, ch)) = line.split_once('\t') {
                by_name.insert(name, ch);
                // The first name listed for a character is its own; the rest are aliases.
                by_char.entry(ch).or_insert(name);
                by_char.entry(ch.trim_end_matches('\u{fe0f}')).or_insert(name);
            }
        }
        (by_name, by_char)
    })
}

/// The character a standard name stands for.
pub fn char_of(name: &str) -> Option<&'static str> {
    tables().0.get(name).copied()
}

/// The name Mattermost knows a character by.
pub fn name_of(ch: &str) -> Option<&'static str> {
    let by_char = &tables().1;
    by_char.get(ch).or_else(|| by_char.get(ch.trim_end_matches('\u{fe0f}'))).copied()
}

/// How a reaction is shown: a standard emoji as its character, a custom one as its picture in the form the window
/// draws (`[mattermost:emoji:name:file]`), anything unknown by its name between colons.
pub fn shown(handle: &Handle, name: &str) -> String {
    if let Some(ch) = char_of(name) {
        return ch.to_string();
    }
    let id = handle.world.lock().unwrap_or_else(|e| e.into_inner()).emoji.get(name).cloned();
    match id {
        Some(id) => format!("[mattermost:emoji:{name}:{}]", media::emoji(&handle.account_id, &id)),
        None => format!(":{name}:"),
    }
}

/// The name a reaction the window chose goes out as: a character by its name, a custom emoji's picture or `:name:` by the
/// name inside.
pub fn wire_name(shown: &str) -> String {
    if let Some(rest) = shown.strip_prefix("[mattermost:emoji:") {
        return rest.split(':').next().unwrap_or("").to_string();
    }
    let trimmed = shown.trim();
    if trimmed.starts_with(':') && trimmed.ends_with(':') && trimmed.len() > 2 {
        return trimmed.trim_matches(':').to_string();
    }
    name_of(trimmed).map(str::to_string).unwrap_or_else(|| trimmed.to_string())
}

/// A post's `:name:` words drawn: a standard emoji as its character, a custom one as its picture. A name that is neither
/// (and the colons of a time, `12:30:45`) is left as it is.
pub fn words(handle: &Handle, text: &str) -> String {
    static NAMED: OnceLock<Regex> = OnceLock::new();
    let named = NAMED.get_or_init(|| Regex::new(r":([a-z0-9_+\-]{1,64}):").expect("a fixed pattern parses"));
    let custom = handle.world.lock().unwrap_or_else(|e| e.into_inner()).emoji.clone();
    named
        .replace_all(text, |c: &regex::Captures| {
            let name = &c[1];
            if let Some(ch) = char_of(name) {
                return ch.to_string();
            }
            match custom.get(name) {
                Some(id) => format!("[mattermost:emoji:{name}:{}]", media::emoji(&handle.account_id, id)),
                None => c[0].to_string(),
            }
        })
        .into_owned()
}

/// Learns the server's custom emoji and gives them to the picker.
pub async fn load(state: &AppState, handle: &Handle) {
    if !handle.server.custom_emoji {
        return;
    }
    let mut all: Vec<(String, String)> = Vec::new();
    for page in 0..20 {
        let Ok(list) = handle.api.get("/api/v4/emoji", &[("page", page.to_string()), ("per_page", "200".into()), ("sort", "name".into())]).await else { break };
        let got: Vec<(String, String)> = list.as_array().into_iter().flatten().filter_map(|e| Some((e["name"].as_str()?.to_string(), e["id"].as_str()?.to_string()))).collect();
        let done = got.len() < 200;
        all.extend(got);
        if done {
            break;
        }
    }
    {
        let mut world = handle.world.lock().unwrap_or_else(|e| e.into_inner());
        world.emoji = all.iter().cloned().collect();
    }
    publish(state, handle);
}

/// Gives the picker the server's own emoji, usable anywhere on the server.
pub fn publish(state: &AppState, handle: &Handle) {
    let mut list: Vec<(String, String)> = handle.world.lock().unwrap_or_else(|e| e.into_inner()).emoji.iter().map(|(n, i)| (n.clone(), i.clone())).collect();
    list.sort();
    let account_id = &handle.account_id;
    state.runtime.clear_emoji_sources(account_id, "mattermost");
    if list.is_empty() {
        return;
    }
    let buffers: Vec<String> = state.runtime.list_buffers().into_iter().filter(|b| b.account_id == *account_id).map(|b| b.id).collect();
    state.runtime.set_emoji_source(
        account_id,
        crate::model::EmojiSource {
            id: format!("mattermost:{}", handle.server.site_name),
            name: if handle.server.site_name.is_empty() { "This server".to_string() } else { handle.server.site_name.clone() },
            service: "mattermost".to_string(),
            kind: "text".to_string(),
            icon_url: None,
            buffers,
            sendable_anywhere: true,
            emoji: list.into_iter().map(|(name, id)| crate::model::EmojiEntry { url: Some(media::emoji(account_id, &id)), id, name, animated: false, locked: false }).collect(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standard_name_is_its_character_and_back() {
        assert_eq!(char_of("+1"), Some("👍"));
        assert_eq!(char_of("thumbsup"), Some("👍"));
        assert_eq!(name_of("👍"), Some("+1"));
        assert_eq!(name_of("❤️"), Some("heart"));
        assert_eq!(name_of("❤"), Some("heart"), "with or without the variation selector");
        assert_eq!(char_of("not_an_emoji_name"), None);
    }

    #[test]
    fn what_the_window_chose_goes_out_by_name() {
        assert_eq!(wire_name("👍"), "+1");
        assert_eq!(wire_name(":party_parrot:"), "party_parrot");
        assert_eq!(wire_name("[mattermost:emoji:party_parrot:file:///x/y.png]"), "party_parrot");
    }
}
