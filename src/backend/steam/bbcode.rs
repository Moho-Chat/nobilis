//! Steam's BBCode, read into the markup the rest of moho already draws.
//!
//! A message on Steam is text with tags in it: `[b]bold[/b]`, `[url=...]`,
//! `[emoticon]steamhappy[/emoticon]`. The window has no BBCode reader for
//! Steam and does not need one: it draws Discord's markdown, spoilers and
//! quotes, and a handful of stand-ins for pictures. So what is stored is the
//! message as that dialect - `**bold**`, `[text](url)`, `[steam:emote:steamhappy]`
//! - and everything after this point treats it like any other body.
//!
//! The text is the sender's and is never trusted to be well formed. An
//! unknown tag is text; a tag that is never closed is text; a close with
//! nothing open is text. Nothing here produces markup the window does not
//! already treat as untrusted, and nothing is lost for being misread - what is
//! unreadable is shown as it was written.

/// A bracket that is only a bracket: followed by a zero-width space, which is
/// what stops anything downstream taking `[b]` for formatting.
pub const LITERAL_BRACKET: &str = "[\u{200b}";

/// What a tag becomes, given what was inside it already read.
fn render(tag: &str, attr: Option<&str>, inner: &str) -> String {
    let wrapped = |mark: &str| if inner.trim().is_empty() { inner.to_string() } else { format!("{mark}{inner}{mark}") };
    match tag {
        "b" | "h1" | "h2" | "h3" | "h4" | "h5" => wrapped("**"),
        "i" => wrapped("*"),
        // The window reads `[u]` itself; Markdown's `__` it draws as bold.
        "u" if inner.trim().is_empty() => inner.to_string(),
        "u" => format!("[u]{inner}[/u]"),
        "strike" | "s" => wrapped("~~"),
        "spoiler" => wrapped("||"),
        "quote" | "pullquote" => {
            let quoted: Vec<String> = inner.trim().lines().map(|l| format!("> {l}")).collect();
            format!("\n{}\n", quoted.join("\n"))
        }
        // Steam writes extra attributes after the address (`fromclient=1`), which
        // are not part of it.
        "url" => match attr.and_then(|a| a.split_whitespace().next()).filter(|u| is_web(u)) {
            Some(url) if inner.trim().is_empty() || inner.trim() == url => url.to_string(),
            Some(url) => format!("[{}]({url})", inner.replace(']', "\\]")),
            // `[url]address[/url]`: the address is what was written.
            None => inner.to_string(),
        },
        "emoticon" => picture("emote", inner).unwrap_or_else(|| inner.to_string()),
        "sticker" => picture("sticker", inner).unwrap_or_else(|| inner.to_string()),
        // A picture is its address, which is what the link previews read.
        "img" | "docimg" => format!("\n{}\n", inner.trim()),
        "p" => format!("{inner}\n"),
        // Tags that only lay out or colour: what is inside is the message.
        "center" | "c" | "color" | "bgcolor" | "size" | "smalltext" | "list" | "olist" | "dynamiclink" | "doclink" | "expand" | "table" | "tr" | "td" | "th" => inner.to_string(),
        // Effects are an animation over the window, with nothing to say.
        "roomeffect" => String::new(),
        _ => inner.to_string(),
    }
}

/// Every tag that is read; anything else stays as the text it was typed as.
const KNOWN: &[&str] = &[
    "b", "i", "u", "strike", "s", "spoiler", "h1", "h2", "h3", "h4", "h5", "quote", "pullquote", "url", "emoticon", "sticker", "img", "docimg", "p", "center", "c", "color", "bgcolor", "size",
    "video", "looping_media", "smalltext", "list", "olist", "dynamiclink", "doclink", "expand", "table", "tr", "td", "th", "roomeffect", "code", "noparse", "hr", "*",
];

/// Whose contents are not read: code is code, and noparse says so.
const RAW: &[&str] = &["code", "noparse"];

/// An attribute written as `name="value"`, `name=value`, or among others
/// (`src="..." width="..."`): the value, unquoted.
fn attribute<'a>(attrs: &'a str, name: &str) -> Option<&'a str> {
    let lower = attrs.to_ascii_lowercase();
    let mut from = 0;
    while let Some(at) = lower[from..].find(&format!("{name}=")) {
        let start = from + at;
        let boundary = start == 0 || lower.as_bytes()[start - 1].is_ascii_whitespace();
        let value = &attrs[start + name.len() + 1..];
        if boundary {
            return Some(match value.strip_prefix('"') {
                Some(quoted) => quoted.split('"').next().unwrap_or_default(),
                None => value.split_whitespace().next().unwrap_or_default(),
            });
        }
        from = start + 1;
    }
    None
}

/// A picture or video written with its address as an attribute - what Steam
/// makes of a file sent through its website - read as that address on a line
/// of its own, which is what the link previews read.
fn media_address(tag: &Tag<'_>) -> Option<String> {
    matches!(tag.name.as_str(), "img" | "video" | "looping_media")
        .then(|| tag.attr.and_then(|a| attribute(a, "src")))
        .flatten()
        .filter(|u| is_web(u))
        .map(str::to_string)
}

fn is_web(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    (lower.starts_with("http://") || lower.starts_with("https://")) && !url.contains(char::is_whitespace)
}

/// The stand-in for a picture: an emoticon or a sticker by name. A name is
/// letters, digits, underscore and hyphen, with the colons somebody typed
/// dropped; anything else is not a picture's name.
fn picture(kind: &str, name: &str) -> Option<String> {
    let name = name.trim().trim_matches(':');
    (!name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')).then(|| format!("[steam:{kind}:{name}]"))
}

/// A tag read at the start of `s`: its name, its `=value` or attributes, whether it closes, and how
/// much text it took.
struct Tag<'a> {
    name: String,
    attr: Option<&'a str>,
    closing: bool,
    len: usize,
}

fn read_tag(s: &str) -> Option<Tag<'_>> {
    let rest = s.strip_prefix('[')?;
    let end = rest.find(']')?;
    // Long enough for a picture's tag, which carries its address twice and
    // its size.
    if end > 1000 {
        return None;
    }
    let inside = &rest[..end];
    let (closing, inside) = match inside.strip_prefix('/') {
        Some(i) => (true, i),
        None => (false, inside),
    };
    let name_len = inside.find(|c: char| !(c.is_ascii_alphanumeric() || c == '*')).unwrap_or(inside.len());
    let name = inside[..name_len].to_ascii_lowercase();
    if name.is_empty() || !KNOWN.contains(&name.as_str()) {
        return None;
    }
    let after = &inside[name_len..];
    let attr = match after.chars().next() {
        None => None,
        Some('=') => Some(after[1..].trim_matches('"')),
        // `[sticker type="x" limit="1"]`: attributes, which are not read.
        Some(c) if c.is_whitespace() => Some(after.trim()),
        Some(_) => return None,
    };
    Some(Tag { name, attr, closing, len: end + 2 })
}

/// The text of a message as moho draws it.
pub fn to_markup(bbcode: &str) -> String {
    struct Open {
        tag: String,
        attr: Option<String>,
        literal: String,
        out: String,
    }
    let mut stack: Vec<Open> = Vec::new();
    let mut top = String::new();
    let mut i = 0;
    let bytes = bbcode;

    macro_rules! out {
        () => {
            stack.last_mut().map(|f| &mut f.out).unwrap_or(&mut top)
        };
    }

    while i < bytes.len() {
        let rest = &bytes[i..];
        // `\[` is a bracket somebody typed, which Steam escapes so it is not a tag.
        // Drawn as a bracket with nothing visible after it, so that the window,
        // which reads `[b]` for itself, does not read it as a tag either.
        if let Some(r) = rest.strip_prefix("\\[") {
            out!().push_str(LITERAL_BRACKET);
            i = bytes.len() - r.len();
            continue;
        }
        if rest.starts_with('[') {
            if let Some(tag) = read_tag(rest) {
                if !tag.closing && RAW.contains(&tag.name.as_str()) {
                    // Everything up to the matching close, unread.
                    let lower = rest.to_ascii_lowercase();
                    let close = format!("[/{}]", tag.name);
                    if let Some(at) = lower[tag.len..].find(&close) {
                        let inner = &rest[tag.len..tag.len + at];
                        let drawn = if tag.name == "code" {
                            code(inner)
                        } else {
                            inner.to_string()
                        };
                        out!().push_str(&drawn);
                        i += tag.len + at + close.len();
                        continue;
                    }
                } else if !tag.closing && tag.name == "hr" {
                    out!().push('\n');
                    i += tag.len;
                    continue;
                } else if !tag.closing && tag.name == "*" {
                    out!().push_str("\n• ");
                    i += tag.len;
                    continue;
                } else if let (false, Some(address)) = (tag.closing, media_address(&tag)) {
                    out!().push_str(&format!("\n{address}\n"));
                    i += tag.len;
                    // Steam writes the address again after the tag, as the
                    // text the tag stands in for.
                    if bytes[i..].starts_with(address.as_str()) {
                        i += address.len();
                    }
                    // Its own close, if it was written with one, says nothing more.
                    let close = format!("[/{}]", tag.name);
                    if bytes[i..].to_ascii_lowercase().starts_with(&close) {
                        i += close.len();
                    }
                    continue;
                } else if !tag.closing && !RAW.contains(&tag.name.as_str()) {
                    stack.push(Open { tag: tag.name, attr: tag.attr.map(str::to_string), literal: rest[..tag.len].to_string(), out: String::new() });
                    i += tag.len;
                    continue;
                } else if tag.closing {
                    if let Some(pos) = stack.iter().rposition(|f| f.tag == tag.name) {
                        // Whatever was left open inside it is closed with it:
                        // shown as the tag text it was, with what it held.
                        while stack.len() > pos + 1 {
                            let f = stack.pop().unwrap();
                            let text = format!("{}{}", f.literal, f.out);
                            out!().push_str(&text);
                        }
                        let f = stack.pop().unwrap();
                        let drawn = render(&f.tag, f.attr.as_deref(), &f.out);
                        out!().push_str(&drawn);
                        i += tag.len;
                        continue;
                    }
                }
            }
        }
        let c = rest.chars().next().unwrap();
        out!().push(c);
        i += c.len_utf8();
    }
    // Opened and never closed: the tag as typed, then what followed it.
    while let Some(f) = stack.pop() {
        let text = format!("{}{}", f.literal, f.out);
        out!().push_str(&text);
    }
    top.trim().to_string()
}

/// The longest run of one character in some text.
fn longest_run(text: &str, of: char) -> usize {
    text.split(|c| c != of).map(str::len).max().unwrap_or(0)
}

/// Code, as the backticks the window reads: inline for a line, a block for
/// several, and a fence longer than anything inside it so it cannot end early.
fn code(inner: &str) -> String {
    let longest = longest_run(inner, '`');
    if inner.contains('\n') {
        let fence = "`".repeat(longest.max(2) + 1);
        format!("\n{fence}\n{}\n{fence}\n", inner.trim_matches('\n'))
    } else {
        let fence = "`".repeat(longest + 1);
        let pad = if inner.starts_with('`') || inner.ends_with('`') { " " } else { "" };
        format!("{fence}{pad}{inner}{pad}{fence}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_inline_formats_become_the_marks_the_window_draws() {
        assert_eq!(to_markup("[b]bold[/b] and [i]italic[/i] and [u]under[/u] and [strike]struck[/strike]"), "**bold** and *italic* and [u]under[/u] and ~~struck~~");
        assert_eq!(to_markup("a [spoiler]secret[/spoiler] b"), "a ||secret|| b");
        assert_eq!(to_markup("[B]shouted[/B]"), "**shouted**", "tags are not case sensitive");
        assert_eq!(to_markup("[h1]Title[/h1]"), "**Title**");
    }

    #[test]
    fn they_nest_and_an_empty_one_says_nothing() {
        assert_eq!(to_markup("[b][i]both[/i][/b]"), "***both***");
        assert_eq!(to_markup("[b][/b]x"), "x");
        assert_eq!(to_markup("a[b]   [/b]b"), "a   b");
    }

    #[test]
    fn links_are_markdown_links_or_the_address_itself() {
        assert_eq!(to_markup("[url=https://example.com/a?b=1]the site[/url]"), "[the site](https://example.com/a?b=1)");
        assert_eq!(to_markup("see [url]https://example.com[/url] now"), "see https://example.com now");
        assert_eq!(to_markup("[url=https://example.com][/url]"), "https://example.com");
        // Not a web address, so not a link: what was said is shown.
        assert_eq!(to_markup("[url=javascript:alert(1)]click[/url]"), "click");
        // Steam's own writing of a link, with its extra attribute; the address is the first word.
        assert_eq!(to_markup("[url=https://example.com/x fromclient=1]a link[/url]"), "[a link](https://example.com/x)");
        assert_eq!(to_markup("see [url=https://example.com/x fromclient=1]https://example.com/x[/url]!"), "see https://example.com/x!", "a link that is its own label is just the address");
        assert_eq!(to_markup("[url=\"https://example.com\"]quoted[/url]"), "[quoted](https://example.com)");
        assert_eq!(to_markup("[url=https://e.com]a ] b[/url]"), "[a \\] b](https://e.com)", "a bracket in the label cannot end it early");
    }

    #[test]
    fn emoticons_and_stickers_are_named_pictures() {
        assert_eq!(to_markup("hi [emoticon]steamhappy[/emoticon]!"), "hi [steam:emote:steamhappy]!");
        assert_eq!(to_markup("[emoticon]:steamhappy:[/emoticon]"), "[steam:emote:steamhappy]", "colons are not part of the name");
        assert_eq!(to_markup("[sticker type=\"Cats\" limit=\"1\"]CatSurprise[/sticker]"), "[steam:sticker:CatSurprise]");
        // Anything that is not a name is not a picture, and is shown as it was.
        assert_eq!(to_markup("[emoticon]a b[/emoticon]"), "a b");
        assert_eq!(to_markup("[emoticon]x]y[/emoticon]"), "x]y");
        assert_eq!(to_markup("[emoticon][/emoticon]"), "");
    }

    #[test]
    fn quotes_are_lines_of_their_own() {
        assert_eq!(to_markup("before [quote]one\ntwo[/quote] after"), "before \n> one\n> two\n after");
        assert_eq!(to_markup("[quote=somebody]said this[/quote]"), "> said this");
    }

    #[test]
    fn code_and_noparse_are_not_read() {
        assert_eq!(to_markup("a [code][b]not bold[/b][/code] b"), "a `[b]not bold[/b]` b");
        assert_eq!(to_markup("[noparse][b]literally[/b][/noparse]"), "[b]literally[/b]");
        assert_eq!(to_markup("[code]line one\nline two[/code]"), "```\nline one\nline two\n```");
        // A backtick inside code cannot end it: the fence is longer than any run.
        assert_eq!(to_markup("[code]a `tick` b[/code]"), "``a `tick` b``");
        // An unclosed code is text, and what follows is still read.
        assert_eq!(to_markup("[code]open [b]x[/b]"), "[code]open **x**");
    }

    #[test]
    fn what_cannot_be_read_is_shown_as_it_was_written() {
        assert_eq!(to_markup("[nosuchtag]x[/nosuchtag]"), "[nosuchtag]x[/nosuchtag]");
        assert_eq!(to_markup("[b]never closed"), "[b]never closed");
        assert_eq!(to_markup("closed [/b] with nothing open"), "closed [/b] with nothing open");
        assert_eq!(to_markup("[b]wrong [i]order[/b] here[/i]"), "**wrong [i]order** here[/i]", "closing an outer tag closes what was left open inside it");
        assert_eq!(to_markup("a [ lone bracket and [b"), "a [ lone bracket and [b");
        assert_eq!(to_markup(""), "");
    }

    #[test]
    fn an_escaped_bracket_is_a_bracket_somebody_typed() {
        assert_eq!(to_markup("not \\[b]bold\\[/b]"), "not [\u{200b}b]bold[\u{200b}/b]", "a bracket the window will not take for a tag");
        assert_eq!(to_markup("\\[emoticon]x\\[/emoticon]"), "[\u{200b}emoticon]x[\u{200b}/emoticon]");
        assert_eq!(to_markup("\\[steam:emote:x]"), "[\u{200b}steam:emote:x]", "nor for a picture");
    }

    #[test]
    fn lists_effects_and_layout_do_not_get_in_the_way() {
        assert_eq!(to_markup("[list][*]one[*]two[/list]"), "• one\n• two");
        assert_eq!(to_markup("[color=red]coloured[/color] [size=5]big[/size]"), "coloured big");
        assert_eq!(to_markup("[roomeffect]fireworks[/roomeffect]hello"), "hello");
        assert_eq!(to_markup("a[hr]b"), "a\nb");
        assert_eq!(to_markup("[img]https://example.com/a.png[/img]"), "https://example.com/a.png");
        assert_eq!(to_markup("[img src=\"https://x.test/ugc/1/\" width=\"96\" height=\"64\"][/img]"), "https://x.test/ugc/1/");
        assert_eq!(to_markup("[img src=\"https://x.test/ugc/1/\" width=\"96\" /]"), "https://x.test/ugc/1/", "with no close at all");
        assert_eq!(to_markup("[spoiler][img src=\"https://x.test/ugc/1/\"][/img][/spoiler]"), "||\nhttps://x.test/ugc/1/\n||");
        let sent = "[img src=https://images.steamusercontent.com/ugc/1269778906054997137/522C3130A7C6AE1FC52CB49820609316A34ECD01/ thumbnail_src=https://images.steamusercontent.com/ugc/1269778906054997137/522C3130A7C6AE1FC52CB49820609316A34ECD01/ srcset=\"\" width=479 height=450]https://images.steamusercontent.com/ugc/1269778906054997137/522C3130A7C6AE1FC52CB49820609316A34ECD01/";
        assert_eq!(to_markup(sent), "https://images.steamusercontent.com/ugc/1269778906054997137/522C3130A7C6AE1FC52CB49820609316A34ECD01/", "as Steam wrote a real one: unquoted, long, and its address again after it");
        assert_eq!(to_markup(&format!("{sent}[/img]")), to_markup(sent));
        assert_eq!(to_markup(&format!("[spoiler]{sent}[/spoiler]")), format!("||\n{}\n||", to_markup(sent)));
        assert_eq!(to_markup("[video src=\"https://x.test/v.webm\" controls=\"1\"][/video]"), "https://x.test/v.webm");
    }

    #[test]
    fn plain_text_passes_through_untouched_including_what_is_not_ascii() {
        assert_eq!(to_markup("héllo wörld 日本語 😀 [not a tag"), "héllo wörld 日本語 😀 [not a tag");
        assert_eq!(to_markup("2 < 3 && <b>html</b> is text"), "2 < 3 && <b>html</b> is text");
    }

    #[test]
    fn a_hostile_message_cannot_loop_or_blow_the_stack() {
        let nested = format!("{}x{}", "[b]".repeat(5000), "[/b]".repeat(5000));
        let drawn = to_markup(&nested);
        assert!(drawn.contains('x'));
        let unclosed = "[b][i][u]".repeat(2000);
        let _ = to_markup(&unclosed);
        let brackets = format!("{}{}", "[".repeat(10000), "]".repeat(10000));
        assert_eq!(to_markup(&brackets).len(), 20000);
    }
}
