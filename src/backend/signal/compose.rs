//! What is typed, as Signal wants it: plain text with body ranges.
//!
//! Signal Desktop lets a person write `*bold*`, `_italic_`, `~strikethrough~`, `` `monospace` `` and `||spoiler||`; the
//! markers are taken out of the text and become ranges over it, counted in UTF-16 units. A marker only counts where it
//! can be told from the same character in ordinary writing: an opening one follows a space, a bracket or the start and is
//! followed by a word, a closing one follows a word and is followed by a space, a stop or the end. `snake_case_name` and
//! `2 * 3 * 4` are left alone.

use presage::proto::{body_range, BodyRange};

#[derive(Clone, Copy)]
struct Marker {
    text: &'static str,
    style: body_range::Style,
}

const MARKERS: [Marker; 5] = [
    Marker { text: "||", style: body_range::Style::Spoiler },
    Marker { text: "`", style: body_range::Style::Monospace },
    Marker { text: "*", style: body_range::Style::Bold },
    Marker { text: "_", style: body_range::Style::Italic },
    Marker { text: "~", style: body_range::Style::Strikethrough },
];

fn opens_here(chars: &[char], at: usize, len: usize) -> bool {
    let before_ok = at == 0 || chars[at - 1].is_whitespace() || "([{\"'>*_~|".contains(chars[at - 1]);
    let after = chars.get(at + len);
    before_ok && after.is_some_and(|c| !c.is_whitespace())
}

fn closes_here(chars: &[char], at: usize, len: usize) -> bool {
    let before = at.checked_sub(1).and_then(|i| chars.get(i));
    let after_ok = chars.get(at + len).is_none_or(|c| c.is_whitespace() || ".,;:!?)]}\"'<*_~|".contains(*c));
    before.is_some_and(|c| !c.is_whitespace()) && after_ok
}

fn matches_at(chars: &[char], at: usize, marker: &str) -> bool {
    marker.chars().enumerate().all(|(i, m)| chars.get(at + i) == Some(&m))
}

/// The text with its markers removed, and the ranges they made.
pub fn parse(text: &str) -> (String, Vec<BodyRange>) {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut units = 0usize;
    let mut ranges = Vec::new();
    run(&chars, 0, chars.len(), &mut out, &mut units, &mut ranges);
    ranges.sort_by_key(|r| (r.start, std::cmp::Reverse(r.length)));
    (out, ranges)
}

fn run(chars: &[char], from: usize, to: usize, out: &mut String, units: &mut usize, ranges: &mut Vec<BodyRange>) {
    let mut i = from;
    while i < to {
        let found = MARKERS.iter().find(|m| matches_at(chars, i, m.text) && i + m.text.len() <= to && opens_here(chars, i, m.text.chars().count()));
        if let Some(marker) = found {
            let width = marker.text.chars().count();
            // The first closing marker of the same kind that can close, inside this run.
            let close = (i + width..to).find(|&j| matches_at(chars, j, marker.text) && j + width <= to && j > i + width && closes_here(chars, j, width));
            if let Some(close) = close {
                let start = *units;
                if marker.style == body_range::Style::Monospace {
                    // Nothing inside code is a marker.
                    for c in &chars[i + width..close] {
                        out.push(*c);
                        *units += c.len_utf16();
                    }
                } else {
                    run(chars, i + width, close, out, units, ranges);
                }
                ranges.push(BodyRange { start: Some(start as u32), length: Some((*units - start) as u32), associated_value: Some(body_range::AssociatedValue::Style(marker.style as i32)) });
                i = close + width;
                continue;
            }
        }
        out.push(chars[i]);
        *units += chars[i].len_utf16();
        i += 1;
    }
}

/// The character Signal puts in a message's text where a mention is; the mention's range covers it.
pub const MENTION: char = '\u{FFFC}';

/// Finds `@Name` for the people who can be mentioned (`people`: their name and id) and puts the mention character in its
/// place. Answers the text and who is mentioned, in the order they appear. At each `@` the longest name that fits wins,
/// and a name must end where a word ends, so `@Sam` is not found in `@Samantha`.
pub fn mentions(text: &str, people: &[(String, uuid::Uuid)]) -> (String, Vec<uuid::Uuid>) {
    let mut sorted: Vec<&(String, uuid::Uuid)> = people.iter().filter(|(name, _)| !name.trim().is_empty()).collect();
    sorted.sort_by_key(|(name, _)| std::cmp::Reverse(name.chars().count()));
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut who = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts_word = i == 0 || !chars[i - 1].is_alphanumeric();
        if chars[i] == '@' && starts_word {
            let found = sorted.iter().find(|(name, _)| {
                let needle: Vec<char> = name.chars().collect();
                let end = i + 1 + needle.len();
                end <= chars.len() && chars[i + 1..end] == needle[..] && chars.get(end).is_none_or(|c| !c.is_alphanumeric() && *c != '_')
            });
            if let Some((name, id)) = found {
                out.push(MENTION);
                who.push(*id);
                i += 1 + name.chars().count();
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    (out, who)
}

/// The mention ranges for a text already stripped of its style markers: one per mention character, in order. A text
/// whose mention characters do not match the people found (somebody typed one) gets none.
pub fn mention_ranges(plain: &str, who: &[uuid::Uuid]) -> Vec<BodyRange> {
    let mut at = Vec::new();
    let mut units = 0u32;
    for c in plain.chars() {
        if c == MENTION {
            at.push(units);
        }
        units += c.len_utf16() as u32;
    }
    if at.len() != who.len() {
        return Vec::new();
    }
    at.into_iter()
        .zip(who)
        .map(|(start, id)| BodyRange { start: Some(start), length: Some(1), associated_value: Some(body_range::AssociatedValue::MentionAci(id.to_string())) })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use body_range::Style;

    fn styles(ranges: &[BodyRange]) -> Vec<(u32, u32, Style)> {
        ranges
            .iter()
            .map(|r| match r.associated_value {
                Some(body_range::AssociatedValue::Style(s)) => (r.start.unwrap(), r.length.unwrap(), Style::try_from(s).unwrap()),
                _ => panic!("not a style"),
            })
            .collect()
    }

    #[test]
    fn each_marker_makes_its_range_and_leaves_the_words() {
        let (text, ranges) = parse("a *bold* b _it_ ~gone~ `code` ||secret||");
        assert_eq!(text, "a bold b it gone code secret");
        assert_eq!(
            styles(&ranges),
            vec![(2, 4, Style::Bold), (9, 2, Style::Italic), (12, 4, Style::Strikethrough), (17, 4, Style::Monospace), (22, 6, Style::Spoiler)]
        );
    }

    #[test]
    fn markers_nest_and_code_is_left_alone() {
        let (text, ranges) = parse("*bold _both_* `*not*`");
        assert_eq!(text, "bold both *not*");
        assert_eq!(styles(&ranges), vec![(0, 9, Style::Bold), (5, 4, Style::Italic), (10, 5, Style::Monospace)]);
    }

    #[test]
    fn ordinary_writing_is_not_taken_for_markup() {
        for plain in ["snake_case_name", "2 * 3 * 4", "a * b", "tilde~in~word", "5 > 3 || 2 < 1", "price: 5*", "lone _ underscore", "**", "||"] {
            let (text, ranges) = parse(plain);
            assert_eq!((text.as_str(), ranges.len()), (plain, 0), "{plain}");
        }
    }

    #[test]
    fn positions_count_utf16_units() {
        let (text, ranges) = parse("😀 *wow*");
        assert_eq!(text, "😀 wow");
        assert_eq!(styles(&ranges), vec![(3, 3, Style::Bold)]);
    }

    #[test]
    fn an_unclosed_marker_stays_as_typed() {
        let (text, ranges) = parse("*never closed");
        assert_eq!((text.as_str(), ranges.len()), ("*never closed", 0));
    }

    #[test]
    fn what_is_written_comes_back_the_same_through_the_drawing() {
        let (text, ranges) = parse("hi *there* and `x`");
        let drawn = super::super::ranges::render(&text, &ranges, |_| String::new());
        assert_eq!(drawn.body, "hi there and x");
        assert_eq!(drawn.html.as_deref(), Some("hi <b>there</b> and <code>x</code>"));
    }

    #[test]
    fn a_member_named_after_an_at_sign_becomes_a_mention() {
        let sam = uuid::Uuid::from_bytes([1; 16]);
        let samantha = uuid::Uuid::from_bytes([2; 16]);
        let people = vec![("Sam".to_string(), sam), ("Samantha Lee".to_string(), samantha)];
        let (text, who) = mentions("@Samantha Lee and @Sam, *look* at @Samuel and me@Sam", &people);
        assert_eq!(text, "\u{FFFC} and \u{FFFC}, *look* at @Samuel and me@Sam");
        assert_eq!(who, vec![samantha, sam]);
        let (plain, _) = parse(&text);
        let ranges = mention_ranges(&plain, &who);
        assert_eq!(ranges.iter().map(|r| r.start.unwrap()).collect::<Vec<_>>(), vec![0, 6]);
        assert!(matches!(&ranges[1].associated_value, Some(body_range::AssociatedValue::MentionAci(id)) if *id == sam.to_string()));
    }

    #[test]
    fn a_stray_mention_character_spoils_nothing() {
        assert!(mention_ranges("a \u{FFFC} b", &[]).is_empty());
        let who = vec![uuid::Uuid::from_bytes([1; 16])];
        assert!(mention_ranges("no mention here", &who).is_empty());
    }
}
