//! Google Chat's text: plain words plus annotation spans, counted in UTF-16 units, that may overlap and nest.
//!
//! Formatting (bold, italic, strike, code, a code block, underline), links laid over text, and mentions are spans over
//! the text; a chip such as an uploaded file is an annotation of its own that stands for something outside the text.

use super::pb;
use super::pb::annotation::Metadata;
use super::pb::format_metadata::FormatType;

/// What a message says, for the store: the text, and HTML when there is formatting the text alone would lose.
pub struct Rendered {
    pub body: String,
    pub html: Option<String>,
    /// Whether the message mentions `own` (or everybody).
    pub mentions_own: bool,
}

struct Span {
    start: usize,
    end: usize,
    open: String,
    close: &'static str,
}

pub fn render(message: &pb::Message, own: &str) -> Rendered {
    let text = message.text_body();
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut mentions_own = false;
    let mut extra: Vec<String> = Vec::new();

    for annotation in &message.annotations {
        let start = annotation.start_index().max(0) as usize;
        let end = (start + annotation.length().max(0) as usize).min(units.len());
        match &annotation.metadata {
            Some(Metadata::FormatMetadata(format)) if end > start => {
                let (open, close) = match format.format_type() {
                    FormatType::Bold => ("<strong>", "</strong>"),
                    FormatType::Italic => ("<em>", "</em>"),
                    FormatType::Strike => ("<del>", "</del>"),
                    FormatType::Underline => ("<u>", "</u>"),
                    FormatType::SourceCode | FormatType::Monospace => ("<code>", "</code>"),
                    FormatType::MonospaceBlock => ("<pre><code>", "</code></pre>"),
                    _ => continue,
                };
                spans.push(Span { start, end, open: open.to_string(), close });
            }
            Some(Metadata::UrlMetadata(url)) => {
                let href = url.url.as_ref().map(|u| u.url().to_string()).unwrap_or_default();
                if !(href.starts_with("https://") || href.starts_with("http://")) {
                    continue;
                }
                if end > start {
                    spans.push(Span { start, end, open: format!("<a href=\"{}\">", escape(&href)), close: "</a>" });
                } else if !text.contains(&href) {
                    // A link the text does not show: said after it, so the message is not missing what it carried.
                    extra.push(href);
                }
            }
            Some(Metadata::UserMentionMetadata(mention)) => {
                let everybody = mention.r#type() == pb::user_mention_metadata::Type::MentionAll;
                let mentioned = mention.id.as_ref().map(|u| u.id()).unwrap_or("");
                if everybody || (!own.is_empty() && mentioned == own) {
                    mentions_own = true;
                }
                if end > start {
                    spans.push(Span { start, end, open: "<strong>".into(), close: "</strong>" });
                }
            }
            Some(Metadata::UploadMetadata(upload)) => {
                let name = upload.content_name();
                extra.push(if name.is_empty() { "[attachment]".to_string() } else { format!("[attachment: {name}]") });
            }
            Some(Metadata::DriveMetadata(drive)) => {
                let title = drive.title();
                extra.push(if title.is_empty() { "[Google Drive file]".to_string() } else { format!("[Google Drive: {title}]") });
            }
            Some(Metadata::VideoCallMetadata(_)) => extra.push("[Google Meet call]".to_string()),
            _ => {}
        }
    }
    // Attachments that come as attachments rather than as annotations (app cards, mostly).
    if !message.attachments.is_empty() && extra.is_empty() && text.is_empty() {
        extra.push("[attachment]".to_string());
    }

    let mut body = text.to_string();
    for line in &extra {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(line);
    }
    let html = if spans.is_empty() {
        None
    } else {
        let mut html = to_html(&units, &spans);
        for line in &extra {
            html.push_str("<br>");
            html.push_str(&escape(line));
        }
        Some(html)
    };
    Rendered { body, html, mentions_own }
}

/// The text with the spans laid over it, properly nested: where spans cross, the inner one is closed and opened again.
fn to_html(units: &[u16], spans: &[Span]) -> String {
    let mut out = String::new();
    let mut open: Vec<usize> = Vec::new();
    let mut boundaries: Vec<usize> = spans.iter().flat_map(|s| [s.start, s.end]).chain([0, units.len()]).collect();
    boundaries.sort_unstable();
    boundaries.dedup();
    for window in boundaries.windows(2) {
        let (from, to) = (window[0], window[1]);
        // What should be open over [from, to): longest first, so the outer ones are outside.
        let mut wanted: Vec<usize> = (0..spans.len()).filter(|&i| spans[i].start <= from && spans[i].end >= to).collect();
        wanted.sort_by_key(|&i| (std::cmp::Reverse(spans[i].end - spans[i].start), i));
        let keep = open.iter().zip(&wanted).take_while(|(a, b)| a == b).count();
        while open.len() > keep {
            let i = open.pop().unwrap_or_default();
            out.push_str(spans[i].close);
        }
        for &i in &wanted[keep..] {
            out.push_str(&spans[i].open);
            open.push(i);
        }
        out.push_str(&escape(&String::from_utf16_lossy(&units[from..to])).replace('\n', "<br>"));
    }
    while let Some(i) = open.pop() {
        out.push_str(spans[i].close);
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format(start: i32, length: i32, kind: FormatType) -> pb::Annotation {
        pb::Annotation {
            start_index: Some(start),
            length: Some(length),
            metadata: Some(Metadata::FormatMetadata(pb::FormatMetadata { format_type: Some(kind as i32), ..Default::default() })),
            ..Default::default()
        }
    }

    #[test]
    fn plain_text_has_no_html() {
        let m = pb::Message { text_body: Some("hello <you>".into()), ..Default::default() };
        let r = render(&m, "1");
        assert_eq!(r.body, "hello <you>");
        assert!(r.html.is_none());
    }

    #[test]
    fn spans_are_counted_in_utf16_and_nest() {
        // "😀 bold it" - the emoji is two units, so "bold" starts at 3.
        let m = pb::Message {
            text_body: Some("😀 bold it".into()),
            annotations: vec![format(3, 7, FormatType::Bold), format(8, 2, FormatType::Italic)],
            ..Default::default()
        };
        assert_eq!(render(&m, "").html.as_deref(), Some("😀 <strong>bold <em>it</em></strong>"));
    }

    #[test]
    fn crossing_spans_are_reopened() {
        let m = pb::Message { text_body: Some("abcd".into()), annotations: vec![format(0, 3, FormatType::Bold), format(1, 3, FormatType::Italic)], ..Default::default() };
        assert_eq!(render(&m, "").html.as_deref(), Some("<strong>a<em>bc</em></strong><em>d</em>"));
    }

    #[test]
    fn mentions_of_me_and_everybody_highlight() {
        let mention = |id: &str, kind: pb::user_mention_metadata::Type| pb::Annotation {
            start_index: Some(0),
            length: Some(4),
            metadata: Some(Metadata::UserMentionMetadata(pb::UserMentionMetadata { id: Some(super::super::user_id(id)), r#type: Some(kind as i32), ..Default::default() })),
            ..Default::default()
        };
        let m = pb::Message { text_body: Some("@Bob hi".into()), annotations: vec![mention("7", pb::user_mention_metadata::Type::Mention)], ..Default::default() };
        assert!(render(&m, "7").mentions_own);
        assert!(!render(&m, "8").mentions_own);
        let all = pb::Message { text_body: Some("@all hi".into()), annotations: vec![mention("", pb::user_mention_metadata::Type::MentionAll)], ..Default::default() };
        assert!(render(&all, "8").mentions_own);
    }
}
