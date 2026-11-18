//! A message's text with Signal's body ranges drawn: bold, italic, strikethrough, monospace, spoiler, and mentions.
//!
//! Signal counts positions in UTF-16 units (it is a JavaScript and Java world), so the text is walked that way. A
//! mention is one placeholder character (U+FFFC) in the text with a range over it naming the person; it is drawn as
//! `@name`, in the plain text as well as the html, since the plain text is what a notification or a search sees.

use presage::proto::{body_range::AssociatedValue, BodyRange};

/// The character Signal leaves in the text where a mention goes.
const PLACEHOLDER: u16 = 0xFFFC;

/// A message's text as the window draws it.
#[derive(Debug, Default, PartialEq)]
pub struct Rendered {
    /// The words, with each mention written out.
    pub body: String,
    /// The same with the styles as html, or `None` when there are none worth drawing.
    pub html: Option<String>,
    /// Who was mentioned (ACIs), in order.
    pub mentions: Vec<String>,
}

enum Kind {
    Style(&'static str, &'static str),
    Mention(String),
}

struct Span {
    from: usize,
    to: usize,
    kind: Kind,
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn spans(ranges: &[BodyRange], len: usize) -> Vec<Span> {
    let mut out: Vec<Span> = ranges
        .iter()
        .filter_map(|range| {
            let from = range.start? as usize;
            let to = (from + range.length? as usize).min(len);
            if from >= to {
                return None;
            }
            let kind = match range.associated_value.as_ref()? {
                AssociatedValue::MentionAci(aci) => Kind::Mention(aci.clone()),
                AssociatedValue::MentionAciBinary(bytes) => Kind::Mention(uuid::Uuid::from_slice(bytes).ok()?.to_string()),
                AssociatedValue::Style(style) => {
                    use presage::proto::body_range::Style;
                    match Style::try_from(*style).ok()? {
                        Style::Bold => Kind::Style("<b>", "</b>"),
                        Style::Italic => Kind::Style("<i>", "</i>"),
                        Style::Strikethrough => Kind::Style("<s>", "</s>"),
                        Style::Monospace => Kind::Style("<code>", "</code>"),
                        Style::Spoiler => Kind::Style("<span class=\"spoiler\">", "</span>"),
                        Style::None => return None,
                    }
                }
            };
            Some(Span { from, to, kind })
        })
        .collect();
    // Outer spans open first: by where they start, the longer before the shorter.
    out.sort_by(|a, b| a.from.cmp(&b.from).then(b.to.cmp(&a.to)));
    out
}

/// Draws `text` with `ranges`. `name_of` turns a mentioned ACI into the name to write after the `@`.
pub fn render(text: &str, ranges: &[BodyRange], name_of: impl Fn(&str) -> String) -> Rendered {
    let units: Vec<u16> = text.encode_utf16().collect();
    let spans = spans(ranges, units.len());
    if spans.is_empty() {
        return Rendered { body: text.to_string(), html: None, mentions: Vec::new() };
    }
    let mut body = String::new();
    let mut html = String::new();
    let mut mentions = Vec::new();
    let mut styled = false;
    let mut open: Vec<&'static str> = Vec::new();
    let mut ends: Vec<usize> = Vec::new();
    let mut next = 0;
    let mut i = 0;
    let mut run: Vec<u16> = Vec::new();
    let flush = |run: &mut Vec<u16>, body: &mut String, html: &mut String| {
        if !run.is_empty() {
            let s = String::from_utf16_lossy(run);
            body.push_str(&s);
            html.push_str(&escape(&s).replace('\n', "<br>"));
            run.clear();
        }
    };
    while i <= units.len() {
        // What ends here closes first, innermost first.
        while ends.last().is_some_and(|end| *end <= i) {
            flush(&mut run, &mut body, &mut html);
            ends.pop();
            html.push_str(open.pop().unwrap_or(""));
        }
        let mut skipped_to = None;
        while next < spans.len() && spans[next].from <= i {
            let span = &spans[next];
            next += 1;
            // A span that began inside one already drawn over (a mention inside a mention) is not drawn again.
            if span.from < i {
                continue;
            }
            match &span.kind {
                Kind::Style(open_tag, close_tag) => {
                    flush(&mut run, &mut body, &mut html);
                    html.push_str(open_tag);
                    styled = true;
                    open.push(close_tag);
                    ends.push(span.to);
                }
                Kind::Mention(aci) => {
                    flush(&mut run, &mut body, &mut html);
                    let shown = format!("@{}", name_of(aci));
                    body.push_str(&shown);
                    html.push_str(&format!("<span class=\"mention\">{}</span>", escape(&shown)));
                    mentions.push(aci.clone());
                    styled = true;
                    skipped_to = Some(span.to);
                }
            }
        }
        if let Some(to) = skipped_to {
            i = to;
            continue;
        }
        if i == units.len() {
            break;
        }
        let unit = units[i];
        // The placeholder of a mention that has no range over it is nobody; it is dropped rather than drawn as a box.
        if unit != PLACEHOLDER {
            run.push(unit);
        }
        i += 1;
    }
    flush(&mut run, &mut body, &mut html);
    while let Some(close) = open.pop() {
        html.push_str(close);
    }
    Rendered { body, html: styled.then_some(html), mentions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use presage::proto::body_range::Style;

    fn style(start: u32, length: u32, style: Style) -> BodyRange {
        BodyRange { start: Some(start), length: Some(length), associated_value: Some(AssociatedValue::Style(style as i32)) }
    }

    fn mention(start: u32, aci: &str) -> BodyRange {
        BodyRange { start: Some(start), length: Some(1), associated_value: Some(AssociatedValue::MentionAci(aci.to_string())) }
    }

    fn name(aci: &str) -> String {
        format!("<{}>", &aci[..2])
    }

    #[test]
    fn text_with_no_ranges_is_just_text() {
        let r = render("plain <b>", &[], name);
        assert_eq!((r.body.as_str(), r.html), ("plain <b>", None));
    }

    #[test]
    fn styles_become_tags_and_nest() {
        let r = render("a bc d", &[style(2, 2, Style::Bold), style(2, 1, Style::Italic)], name);
        assert_eq!(r.body, "a bc d");
        assert_eq!(r.html.as_deref(), Some("a <b><i>b</i>c</b> d"));
    }

    #[test]
    fn every_style_has_its_tag_and_text_is_escaped() {
        let r = render("x<y>", &[style(0, 4, Style::Monospace)], name);
        assert_eq!(r.html.as_deref(), Some("<code>x&lt;y&gt;</code>"));
        let s = render("hidden", &[style(0, 6, Style::Spoiler)], name);
        assert_eq!(s.html.as_deref(), Some("<span class=\"spoiler\">hidden</span>"));
        let t = render("gone", &[style(0, 4, Style::Strikethrough)], name);
        assert_eq!(t.html.as_deref(), Some("<s>gone</s>"));
        // A style of none is no style.
        assert_eq!(render("same", &[style(0, 4, Style::None)], name).html, None);
    }

    #[test]
    fn positions_count_utf16_units_not_characters() {
        // The emoji is two units, so the bold word starts at 3.
        let r = render("😀 bold", &[style(3, 4, Style::Bold)], name);
        assert_eq!(r.html.as_deref(), Some("😀 <b>bold</b>"));
    }

    #[test]
    fn a_mention_is_written_out_in_both_forms() {
        let r = render("hi \u{FFFC}!", &[mention(3, "22222222-2222-4222-8222-222222222222")], name);
        assert_eq!(r.body, "hi @<22>!");
        assert_eq!(r.html.as_deref(), Some("hi <span class=\"mention\">@&lt;22&gt;</span>!"));
        assert_eq!(r.mentions, vec!["22222222-2222-4222-8222-222222222222".to_string()]);
    }

    #[test]
    fn a_range_past_the_end_is_cut_and_an_empty_one_ignored() {
        let r = render("abc", &[style(1, 99, Style::Bold), style(0, 0, Style::Italic), style(50, 2, Style::Bold)], name);
        assert_eq!(r.html.as_deref(), Some("a<b>bc</b>"));
    }

    #[test]
    fn a_stray_placeholder_is_dropped() {
        let r = render("a\u{FFFC}b", &[style(0, 1, Style::Bold)], name);
        assert_eq!(r.body, "ab");
    }
}
