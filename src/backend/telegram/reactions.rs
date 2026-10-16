//! Reactions: the emoji people put on a message, and the account's own.
//!
//! Telegram sends a message with its reactions as a list of (reaction, count, whether the account chose it), and an
//! update when they change. A reaction is an ordinary emoji, a custom emoji (drawn from Telegram's sticker sets,
//! which moho does not draw yet, so it is counted under a star) or a paid star.

use super::send::{explain, peer_for};
use crate::model::{Buffer, Reaction};
use crate::state::AppState;
use grammers_client::message::InputReactions;
use grammers_client::tl;

/// What stands for a reaction moho cannot draw: custom emoji and paid stars.
const STAND_IN: &str = "⭐";

/// A message's reactions as moho's list.
pub fn from_tl(reactions: &tl::enums::MessageReactions) -> Vec<Reaction> {
    let tl::enums::MessageReactions::Reactions(r) = reactions;
    let mut out: Vec<Reaction> = Vec::new();
    for count in &r.results {
        let tl::enums::ReactionCount::Count(c) = count;
        let emoji = match &c.reaction {
            tl::enums::Reaction::Emoji(e) => e.emoticon.clone(),
            tl::enums::Reaction::CustomEmoji(_) | tl::enums::Reaction::Paid => STAND_IN.to_string(),
            tl::enums::Reaction::Empty => continue,
        };
        // Two kinds that share the stand-in are one pill.
        if let Some(existing) = out.iter_mut().find(|x| x.emoji == emoji) {
            existing.count += i64::from(c.count);
            existing.me |= c.chosen_order.is_some();
        } else {
            out.push(Reaction { emoji, count: i64::from(c.count), me: c.chosen_order.is_some(), animated: false });
        }
    }
    out
}

/// Records what a message's reactions are now, and says so.
pub fn apply(state: &AppState, buffer_id: &str, msg_id: &str, reactions: &[Reaction]) {
    match state.store.set_reactions(buffer_id, msg_id, reactions) {
        Ok(true) => state.events.emit("reactionsChanged", serde_json::json!({ "bufferId": buffer_id, "id": msg_id, "reactions": reactions })),
        Ok(false) => {}
        Err(e) => tracing::warn!("telegram: recording reactions: {e}"),
    }
}

/// The reactions the account itself has put on a message, as Telegram's own type.
fn chosen(reactions: &[Reaction]) -> Vec<tl::enums::Reaction> {
    reactions.iter().filter(|r| r.me && r.emoji != STAND_IN).map(|r| tl::enums::Reaction::Emoji(tl::types::ReactionEmoji { emoticon: r.emoji.clone() })).collect()
}

/// Puts an emoji on a message or takes the account's off. Telegram lets an ordinary account have one reaction on a
/// message, so adding replaces the account's earlier one.
pub async fn toggle(state: &AppState, buffer: &Buffer, msg_id: &str, emoji: &str, add: bool) -> Result<(), String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message cannot take reactions".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let stored = state.store.get_message(&buffer.id, msg_id).ok().flatten().map(|m| m.reactions).unwrap_or_default();
    let stored_again = stored.clone();
    let mut mine = chosen(&stored);
    let this = tl::enums::Reaction::Emoji(tl::types::ReactionEmoji { emoticon: emoji.to_string() });
    if add {
        mine = vec![this];
    } else {
        mine.retain(|r| *r != this);
    }
    let input: InputReactions = if mine.is_empty() { InputReactions::remove() } else { mine.into() };
    handle.client.send_reactions(peer, id, input).await.map_err(|e| explain(&e))?;
    // Telegram answers with the message's new state as an update, which is read below; this is the same fact said
    // at once so the pill does not wait on it.
    apply(state, &buffer.id, msg_id, &after_change(&stored_again, emoji, add));
    Ok(())
}

/// The reactions a message has once the account has put an emoji on it (replacing its earlier one) or taken it off.
pub fn after_change(stored: &[Reaction], emoji: &str, add: bool) -> Vec<Reaction> {
    let mut next: Vec<Reaction> = stored.to_vec();
    for r in next.iter_mut() {
        let own_ordinary = r.me && r.emoji != STAND_IN;
        if own_ordinary && (add || r.emoji == emoji) {
            r.me = false;
            r.count -= 1;
        }
    }
    next.retain(|r| r.count > 0);
    if add {
        match next.iter_mut().find(|r| r.emoji == emoji) {
            Some(r) => {
                r.count += 1;
                r.me = true;
            }
            None => next.push(Reaction { emoji: emoji.to_string(), count: 1, me: true, animated: false }),
        }
    }
    next
}

/// A reactions update from Telegram.
pub fn update(state: &AppState, account_id: &str, remote: &str, msg_id: i32, reactions: &tl::enums::MessageReactions) {
    let Some(buffer) = state.runtime.list_buffers().into_iter().find(|b| b.account_id == account_id && b.remote_id.as_deref() == Some(remote)) else { return };
    apply(state, &buffer.id, &msg_id.to_string(), &from_tl(reactions));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count(reaction: tl::enums::Reaction, n: i32, chosen: Option<i32>) -> tl::enums::ReactionCount {
        tl::enums::ReactionCount::Count(tl::types::ReactionCount { chosen_order: chosen, reaction, count: n })
    }

    fn emoji(e: &str) -> tl::enums::Reaction {
        tl::enums::Reaction::Emoji(tl::types::ReactionEmoji { emoticon: e.into() })
    }

    #[test]
    fn reactions_become_pills_with_counts_and_whether_the_account_chose_them() {
        let results = vec![count(emoji("👍"), 5, Some(0)), count(emoji("🔥"), 2, None), count(tl::enums::Reaction::CustomEmoji(tl::types::ReactionCustomEmoji { document_id: 9 }), 3, None), count(tl::enums::Reaction::Paid, 4, None)];
        let reactions = tl::enums::MessageReactions::Reactions(tl::types::MessageReactions { min: false, can_see_list: false, reactions_as_tags: false, results, recent_reactions: None, top_reactors: None });
        let got = from_tl(&reactions);
        assert_eq!(got.iter().map(|r| (r.emoji.as_str(), r.count, r.me)).collect::<Vec<_>>(), vec![("👍", 5, true), ("🔥", 2, false), ("⭐", 7, false)], "two kinds drawn the same are one pill");
    }

    #[test]
    fn choosing_an_emoji_replaces_the_accounts_earlier_one_and_taking_it_off_leaves_the_rest() {
        let r = |e: &str, n: i64, me: bool| Reaction { emoji: e.into(), count: n, me, animated: false };
        let stored = vec![r("👍", 5, true), r("🔥", 2, false)];
        assert_eq!(after_change(&stored, "🔥", true), vec![r("👍", 4, false), r("🔥", 3, true)]);
        assert_eq!(after_change(&stored, "👍", false), vec![r("👍", 4, false), r("🔥", 2, false)]);
        assert_eq!(after_change(&[r("👍", 1, true)], "👍", false), Vec::<Reaction>::new(), "a pill nobody has any more goes");
        assert_eq!(after_change(&[], "😂", true), vec![r("😂", 1, true)]);
    }

    #[test]
    fn only_the_accounts_own_ordinary_reactions_are_carried_into_a_change() {
        let stored = vec![Reaction { emoji: "👍".into(), count: 5, me: true, animated: false }, Reaction { emoji: "🔥".into(), count: 2, me: false, animated: false }, Reaction { emoji: STAND_IN.into(), count: 1, me: true, animated: false }];
        assert_eq!(chosen(&stored), vec![emoji("👍")]);
    }
}
