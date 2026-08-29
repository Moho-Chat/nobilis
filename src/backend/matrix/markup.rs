//! Turning what somebody typed into the HTML Matrix carries alongside it.
//!
//! Matrix messages may carry a `formatted_body` next to the plain one, and
//! every other client sends one - so a message typed here with emphasis in it
//! arrived everywhere else as literal asterisks.
//!
//! This is a deliberately small subset of Markdown, not a Markdown parser.
//! The distinction matters: a half-built parser that silently disagrees with
//! what people expect is worse than none, so the rule here is that anything
//! not listed below is left exactly as typed. What is covered is what people
//! actually reach for in a chat line:
//!
//! - fenced code blocks, and `inline code`
//! - `**bold**`, `*italic*` and `_italic_`, `~~struck through~~`
//! - line breaks
//!
//! Deliberately absent: headings, lists, tables, images, reference links.
//! Those are document structure, and a chat line that turns into one because
//! it happened to start with a hyphen is a surprise nobody asked for.
//!
//! Escaping happens before any of it. The input is somebody's text, not
//! markup, and the output goes to other people's clients - so `<b>` typed
//! into the box has to arrive as those five characters rather than as a tag.

/// The HTML for this message, or `None` if it holds nothing to format.
///
/// `None` is the common case and the point of the return type: a plain line
/// should travel as a plain line, and sending a `formatted_body` that only
/// restates the body would put markup on every message for the benefit of
/// none of them.
pub fn to_html(body: &str) -> Option<String> {
    let html = render(body);
    // Only worth sending if the formatting says something the plain text
    // does not. Comparing against the escaped input rather than the input
    // catches the case where the only difference is escaping, which every
    // client already does for itself.
    if html == escape(body).replace('\n', "<br/>") {
        return None;
    }
    Some(html)
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn render(body: &str) -> String {
    let mut out = String::new();
    // Fences first and whole: everything between them is text somebody
    // wanted shown exactly, so no emphasis rule may reach inside.
    let mut fenced = body.split("```");
    let mut in_code = false;
    while let Some(part) = fenced.next() {
        if in_code {
            // A fence often carries a language on its opening line; it is not
            // part of the code and Matrix has nowhere to put it.
            let code = part.strip_prefix(|c: char| c != '\n').unwrap_or(part);
            let code = match part.find('\n') {
                Some(i) if !part[..i].contains(char::is_whitespace) => &part[i + 1..],
                _ => code,
            };
            out.push_str("<pre><code>");
            out.push_str(&escape(code.trim_end_matches('\n')));
            out.push_str("</code></pre>");
        } else {
            out.push_str(&inline(part));
        }
        in_code = !in_code;
    }
    out
}

/// Emphasis and inline code, on one stretch of prose.
fn inline(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while !rest.is_empty() {
        // Inline code before emphasis, for the same reason fences come
        // before everything: `**` inside backticks is two asterisks.
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                out.push_str("<code>");
                out.push_str(&escape(&after[..end]));
                out.push_str("</code>");
                rest = &after[end + 1..];
                continue;
            }
        }
        if let Some(rendered) = span(rest, "**", "strong").or_else(|| span(rest, "~~", "del")) {
            let (html, remaining) = rendered;
            out.push_str(&html);
            rest = remaining;
            continue;
        }
        // Single-character markers last, so ** is never read as two *.
        if let Some(rendered) = span(rest, "*", "em").or_else(|| span(rest, "_", "em")) {
            let (html, remaining) = rendered;
            out.push_str(&html);
            rest = remaining;
            continue;
        }
        let take = rest.chars().next().map(char::len_utf8).unwrap_or(1);
        out.push_str(&escape(&rest[..take]));
        rest = &rest[take..];
    }
    out.replace('\n', "<br/>")
}

/// One `MARKER…MARKER` run, rendered as `tag`, and whatever follows it.
///
/// An unclosed marker is not emphasis - it is an asterisk somebody typed -
/// and an empty one is not either, so `**` on its own stays as it was.
fn span<'a>(text: &'a str, marker: &str, tag: &str) -> Option<(String, &'a str)> {
    let after = text.strip_prefix(marker)?;
    let end = after.find(marker)?;
    if end == 0 {
        return None;
    }
    let inner = &after[..end];
    // No emphasis across a line: an asterisk at the start of one line and
    // another paragraphs later is two asterisks, not italics.
    if inner.contains('\n') {
        return None;
    }
    // Markers have to hug the text they emphasise. Without this "2 * 3 * 4"
    // is multiplication rendered as italics, which is the exact kind of
    // surprise this module exists to avoid.
    if inner.starts_with(char::is_whitespace) || inner.ends_with(char::is_whitespace) {
        return None;
    }
    Some((format!("<{tag}>{}</{tag}>", inline(inner)), &after[end + marker.len()..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_carries_no_formatting() {
        assert_eq!(to_html("just a message"), None);
        assert_eq!(to_html("two\nlines"), None);
        // Escaping alone is not formatting: every client escapes for itself.
        assert_eq!(to_html("a < b & c"), None);
    }

    #[test]
    fn emphasis_becomes_tags() {
        assert_eq!(to_html("**bold**").as_deref(), Some("<strong>bold</strong>"));
        assert_eq!(to_html("*it*").as_deref(), Some("<em>it</em>"));
        assert_eq!(to_html("_it_").as_deref(), Some("<em>it</em>"));
        assert_eq!(to_html("~~no~~").as_deref(), Some("<del>no</del>"));
        assert_eq!(to_html("a **b** c").as_deref(), Some("a <strong>b</strong> c"));
    }

    /// The input is somebody's text and the output goes to other people's
    /// clients, so a tag typed into the box must arrive as characters.
    #[test]
    fn typed_markup_is_escaped_not_honoured() {
        assert_eq!(to_html("**<b>hi</b>**").as_deref(), Some("<strong>&lt;b&gt;hi&lt;/b&gt;</strong>"));
        assert_eq!(to_html("`<script>`").as_deref(), Some("<code>&lt;script&gt;</code>"));
    }

    #[test]
    fn code_is_left_exactly_as_typed() {
        assert_eq!(to_html("`a*b*c`").as_deref(), Some("<code>a*b*c</code>"));
        assert_eq!(to_html("```\na*b*\n```").as_deref(), Some("<pre><code>a*b*</code></pre>"));
        // A language on the fence is not part of the code.
        assert_eq!(to_html("```rust\nlet x = 1;\n```").as_deref(), Some("<pre><code>let x = 1;</code></pre>"));
    }

    /// A stray marker is a character somebody typed, not the start of
    /// something. Getting this wrong swallows the rest of the message.
    #[test]
    fn an_unclosed_marker_stays_as_it_was() {
        assert_eq!(to_html("2 * 3 * 4"), None);
        assert_eq!(to_html("**"), None);
        assert_eq!(to_html("a * b"), None);
        assert_eq!(to_html("half **open"), None);
        // Nor across lines, where the two are unrelated.
        assert_eq!(to_html("*one\ntwo*"), None);
    }

    #[test]
    fn line_breaks_survive_alongside_formatting() {
        assert_eq!(to_html("**a**\nb").as_deref(), Some("<strong>a</strong><br/>b"));
    }
}
