//! What the message box holds, as Telegram reads it: plain text and a list of entities.
//!
//! The window writes formatting as Markdown, the way Discord reads it (`**bold**`, `*italic*`, `__underline__`,
//! `~~strike~~`, `||spoiler||`, `` `code` ``, fenced code, `> quote`, `[text](url)`), and Telegram wants the text with
//! the marks taken out and a list saying where each kind starts and how long it runs, counted in UTF-16 code units.
//! A mark with no partner is the character it looks like.

use grammers_client::tl;

#[derive(Debug, PartialEq, Clone, Copy)]
enum Kind {
    Bold,
    Italic,
    Underline,
    Strike,
    Spoiler,
    Code,
    Pre,
    Quote,
}

#[derive(Debug, PartialEq)]
struct Span {
    kind: Kind,
    offset: usize,
    length: usize,
    /// For a link, where it goes.
    url: Option<String>,
    /// For a fenced block, its language.
    language: Option<String>,
}

#[derive(Default)]
struct Out {
    text: String,
    units: usize,
    spans: Vec<Span>,
}

impl Out {
    fn push(&mut self, c: char) {
        self.text.push(c);
        self.units += c.len_utf16();
    }
    fn push_str(&mut self, s: &str) {
        for c in s.chars() {
            self.push(c);
        }
    }
}

/// The text with its marks taken out, and the entities that say what they marked.
pub fn parse(source: &str) -> (String, Vec<tl::enums::MessageEntity>) {
    let mut out = Out::default();
    let lines: Vec<&str> = source.split('\n').collect();
    let mut i = 0;
    let mut first = true;
    while i < lines.len() {
        if !first {
            out.push('\n');
        }
        first = false;
        let line = lines[i];
        if let Some(fence) = line.strip_prefix("```") {
            // A fenced block runs to the next fence, or to the end.
            if let Some(end) = lines[i + 1..].iter().position(|l| l.trim_end() == "```") {
                let language = fence.trim().to_string();
                let body = lines[i + 1..i + 1 + end].join("\n");
                let offset = out.units;
                out.push_str(&body);
                out.spans.push(Span { kind: Kind::Pre, offset, length: out.units - offset, url: None, language: (!language.is_empty()).then_some(language) });
                i += end + 2;
                continue;
            }
        }
        if line.starts_with("> ") || line == ">" {
            // Consecutive quoted lines are one quote.
            let start = out.units;
            let mut quoted = vec![line.strip_prefix("> ").unwrap_or("")];
            while i + 1 < lines.len() && (lines[i + 1].starts_with("> ") || lines[i + 1] == ">") {
                i += 1;
                quoted.push(lines[i].strip_prefix("> ").unwrap_or(""));
            }
            for (n, q) in quoted.iter().enumerate() {
                if n > 0 {
                    out.push('\n');
                }
                inline(&q.chars().collect::<Vec<_>>(), &mut out);
            }
            out.spans.push(Span { kind: Kind::Quote, offset: start, length: out.units - start, url: None, language: None });
            i += 1;
            continue;
        }
        inline(&line.chars().collect::<Vec<_>>(), &mut out);
        i += 1;
    }
    let entities = out.spans.iter().filter(|s| s.length > 0).map(entity).collect();
    (out.text, entities)
}

fn entity(s: &Span) -> tl::enums::MessageEntity {
    use tl::enums::MessageEntity as E;
    use tl::types as t;
    let (offset, length) = (s.offset as i32, s.length as i32);
    if let Some(url) = &s.url {
        return E::TextUrl(t::MessageEntityTextUrl { offset, length, url: url.clone() });
    }
    match s.kind {
        Kind::Bold => E::Bold(t::MessageEntityBold { offset, length }),
        Kind::Italic => E::Italic(t::MessageEntityItalic { offset, length }),
        Kind::Underline => E::Underline(t::MessageEntityUnderline { offset, length }),
        Kind::Strike => E::Strike(t::MessageEntityStrike { offset, length }),
        Kind::Spoiler => E::Spoiler(t::MessageEntitySpoiler { offset, length }),
        Kind::Code => E::Code(t::MessageEntityCode { offset, length }),
        Kind::Pre => E::Pre(t::MessageEntityPre { offset, length, language: s.language.clone().unwrap_or_default() }),
        Kind::Quote => E::Blockquote(t::MessageEntityBlockquote { collapsed: false, offset, length }),
    }
}

fn find(chars: &[char], from: usize, marker: &[char]) -> Option<usize> {
    (from..chars.len().saturating_sub(marker.len() - 1)).find(|&j| chars[j..].starts_with(marker))
}

/// A single `*` or `_` only opens at the start of a word and closes at the end of one, so `snake_case_name` and `2*3*4`
/// stay as written.
fn word_edge(chars: &[char], at: usize, opening: bool) -> bool {
    let alnum = |c: char| c.is_alphanumeric();
    if opening {
        (at == 0 || !alnum(chars[at - 1])) && chars.get(at + 1).is_some_and(|c| !c.is_whitespace())
    } else {
        at > 0 && !chars[at - 1].is_whitespace() && chars.get(at + 1).is_none_or(|c| !alnum(*c))
    }
}

fn inline(chars: &[char], out: &mut Out) {
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' && i + 1 < chars.len() && "\\*_~|`[]()>".contains(chars[i + 1]) {
            out.push(chars[i + 1]);
            i += 2;
            continue;
        }
        // Code takes its contents as they are.
        if c == '`' {
            if let Some(end) = find(chars, i + 1, &['`']).filter(|&e| e > i + 1) {
                let offset = out.units;
                out.push_str(&chars[i + 1..end].iter().collect::<String>());
                out.spans.push(Span { kind: Kind::Code, offset, length: out.units - offset, url: None, language: None });
                i = end + 1;
                continue;
            }
        }
        // [text](url)
        if c == '[' {
            if let Some(close) = find(chars, i + 1, &[']']) {
                if chars.get(close + 1) == Some(&'(') {
                    if let Some(end) = find(chars, close + 2, &[')']) {
                        let url: String = chars[close + 2..end].iter().collect();
                        if ["http://", "https://", "tg://", "mailto:"].iter().any(|p| url.starts_with(p)) && close > i + 1 {
                            let offset = out.units;
                            inline(&chars[i + 1..close], out);
                            out.spans.push(Span { kind: Kind::Bold, offset, length: out.units - offset, url: Some(url), language: None });
                            i = end + 1;
                            continue;
                        }
                    }
                }
            }
        }
        let two = |m: &str| m.chars().collect::<Vec<_>>();
        let pairs: [(&str, Kind); 4] = [("**", Kind::Bold), ("__", Kind::Underline), ("~~", Kind::Strike), ("||", Kind::Spoiler)];
        let mut done = false;
        for (marker, kind) in pairs {
            let m = two(marker);
            if chars[i..].starts_with(&m) {
                if let Some(end) = find(chars, i + 2, &m).filter(|&e| e > i + 2) {
                    let offset = out.units;
                    inline(&chars[i + 2..end], out);
                    out.spans.push(Span { kind, offset, length: out.units - offset, url: None, language: None });
                    i = end + 2;
                    done = true;
                    break;
                }
            }
        }
        if done {
            continue;
        }
        if (c == '*' || c == '_') && word_edge(chars, i, true) {
            // The closing one: the next of the same mark that ends a word.
            let mut j = i + 1;
            let mut found = None;
            while let Some(e) = find(chars, j, &[c]) {
                if e > i + 1 && word_edge(chars, e, false) {
                    found = Some(e);
                    break;
                }
                j = e + 1;
            }
            if let Some(end) = found {
                let offset = out.units;
                inline(&chars[i + 1..end], out);
                out.spans.push(Span { kind: Kind::Italic, offset, length: out.units - offset, url: None, language: None });
                i = end + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tl::enums::MessageEntity as E;

    fn kinds(entities: &[E]) -> Vec<(&'static str, i32, i32)> {
        entities
            .iter()
            .map(|e| match e {
                E::Bold(x) => ("bold", x.offset, x.length),
                E::Italic(x) => ("italic", x.offset, x.length),
                E::Underline(x) => ("underline", x.offset, x.length),
                E::Strike(x) => ("strike", x.offset, x.length),
                E::Spoiler(x) => ("spoiler", x.offset, x.length),
                E::Code(x) => ("code", x.offset, x.length),
                E::Pre(x) => ("pre", x.offset, x.length),
                E::Blockquote(x) => ("quote", x.offset, x.length),
                E::TextUrl(x) => ("link", x.offset, x.length),
                _ => ("other", 0, 0),
            })
            .collect()
    }

    #[test]
    fn marks_are_taken_out_and_the_entities_say_where_they_were() {
        let (text, entities) = parse("a **bold** and *italic* and ~~gone~~ and ||secret|| and __under__");
        assert_eq!(text, "a bold and italic and gone and secret and under");
        let got = kinds(&entities);
        assert!(got.contains(&("bold", 2, 4)) && got.contains(&("italic", 11, 6)) && got.contains(&("strike", 22, 4)) && got.contains(&("spoiler", 31, 6)) && got.contains(&("underline", 42, 5)), "{got:?}");
    }

    #[test]
    fn marks_nest_and_a_mark_with_no_partner_is_just_the_character() {
        let (text, entities) = parse("**a *b* c**");
        assert_eq!(text, "a b c");
        let got = kinds(&entities);
        assert!(got.contains(&("bold", 0, 5)) && got.contains(&("italic", 2, 1)), "{got:?}");
        let (text, entities) = parse("2 * 3 and snake_case_name and **unclosed");
        assert_eq!(text, "2 * 3 and snake_case_name and **unclosed");
        assert!(entities.is_empty());
    }

    #[test]
    fn code_is_taken_as_written_and_a_fence_is_a_block_with_its_language() {
        let (text, entities) = parse("see `**not bold**` ok");
        assert_eq!(text, "see **not bold** ok");
        assert_eq!(kinds(&entities), vec![("code", 4, 12)]);
        let (text, entities) = parse("```rust\nlet x = 1;\n```\nafter");
        assert_eq!(text, "let x = 1;\nafter");
        assert!(matches!(&entities[0], E::Pre(p) if p.language == "rust" && p.offset == 0 && p.length == 10));
    }

    #[test]
    fn a_link_and_a_quote_and_an_escape() {
        let (text, entities) = parse("[here](https://example.com/a?b=1) \\*not\\*");
        assert_eq!(text, "here *not*");
        assert!(matches!(&entities[0], E::TextUrl(u) if u.url == "https://example.com/a?b=1" && u.offset == 0 && u.length == 4));
        let (text, entities) = parse("> one\n> two\nthree");
        assert_eq!(text, "one\ntwo\nthree");
        assert_eq!(kinds(&entities), vec![("quote", 0, 7)]);
        // A link to somewhere that is not a web address is left as it was typed.
        assert_eq!(parse("[x](javascript:alert(1))").0, "[x](javascript:alert(1))");
    }

    #[test]
    fn offsets_count_utf16_units_so_an_emoji_before_a_mark_moves_it_by_two() {
        let (text, entities) = parse("😀 **hi**");
        assert_eq!(text, "😀 hi");
        assert_eq!(kinds(&entities), vec![("bold", 3, 2)]);
    }
}
