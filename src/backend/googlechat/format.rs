//! Google Chat's text: plain words plus annotation spans, counted in UTF-16 units, that may overlap and nest.
//!
//! Inbound: formatting (`bold`, `italic`, `strike`, `code`, `code block`, `underline`), links laid over text, and
//! mentions become HTML spans; `UploadMetadata`, inline `UrlMetadata` images/GIFs, Drive files, and Meet calls become
//! attachments and embeds.
//!
//! Outbound: Markdown (`**bold**`, `_italic_`, `~~strike~~`, `` `code` ``, ```` ```block``` ````) and `@mentions`
//! (`@all` or `@Display Name`) are converted into plain text plus UTF-16 annotation spans.

use super::{media, pb};
use super::pb::annotation::Metadata;
use super::pb::format_metadata::FormatType;
use crate::model::{Attachment, Embed, EmbedLink, Reaction};

/// What a message says, for the store: the text, HTML when formatted, attachments, embeds, and reactions.
pub struct Rendered {
    pub body: String,
    pub html: Option<String>,
    /// Whether the message mentions `own` (or everybody).
    pub mentions_own: bool,
    pub attachments: Vec<Attachment>,
    pub embeds: Vec<Embed>,
    pub reactions: Vec<Reaction>,
}

struct Span {
    start: usize,
    end: usize,
    open: String,
    close: &'static str,
}

fn kind_from_mime(mime: &str) -> String {
    match mime.split('/').next().unwrap_or("") {
        "image" => "image",
        "video" => "video",
        "audio" => "audio",
        _ => "file",
    }
    .to_string()
}

/// Whether a `UrlMetadata` annotation carries an inlineable image/GIF supplied by Google (`image_url`).
fn is_inlineable_url_media(ann: &pb::Annotation, meta: &pb::UrlMetadata) -> bool {
    if meta.should_not_render() || meta.image_url().is_empty() {
        return false;
    }
    if ann.r#type() == pb::AnnotationType::Image {
        return true;
    }
    let mt = meta.mime_type();
    if mt.starts_with("image/") || mt.starts_with("video/") {
        return true;
    }
    ann.length() == 0
}

pub fn render(message: &pb::Message, own: &str, account_id: &str) -> Rendered {
    let text = message.text_body();
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut spans: Vec<Span> = Vec::new();
    let mut mentions_own = false;
    let mut extra: Vec<String> = Vec::new();
    let mut attachments: Vec<Attachment> = Vec::new();
    let mut embeds: Vec<Embed> = Vec::new();

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
                let valid_href = href.starts_with("https://") || href.starts_with("http://");
                if valid_href {
                    if end > start {
                        spans.push(Span { start, end, open: format!("<a href=\"{}\">", escape(&href)), close: "</a>" });
                    } else if !text.contains(&href) && !is_inlineable_url_media(annotation, url) {
                        extra.push(href.clone());
                    }
                }
                if is_inlineable_url_media(annotation, url) && !account_id.is_empty() {
                    let img_url = url.image_url();
                    let ct = (!url.mime_type().is_empty()).then_some(url.mime_type());
                    if let Some(path) = media::cached_external(account_id, img_url, ct) {
                        let kind = ct.map(kind_from_mime).unwrap_or_else(|| "image".to_string());
                        attachments.push(Attachment {
                            kind,
                            mimetype: ct.map(str::to_string),
                            filename: None,
                            size: None,
                            width: (url.int_image_width() > 0).then_some(url.int_image_width() as u32),
                            height: (url.int_image_height() > 0).then_some(url.int_image_height() as u32),
                            path: Some(path),
                            url: valid_href.then_some(href),
                            ..Default::default()
                        });
                    }
                } else if valid_href && !url.should_not_render() && (!url.title().is_empty() || !url.snippet().is_empty()) {
                    let image_url = (!url.image_url().is_empty() && !account_id.is_empty())
                        .then(|| media::cached_external(account_id, url.image_url(), None))
                        .flatten();
                    embeds.push(Embed {
                        title: (!url.title().is_empty()).then(|| url.title().to_string()),
                        description: (!url.snippet().is_empty()).then(|| url.snippet().to_string()),
                        url: Some(href),
                        image_url,
                        ..Default::default()
                    });
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
                let ct = upload.content_type();
                let cached = if !account_id.is_empty() { media::cached_attachment(account_id, upload) } else { None };
                if let Some(path) = cached {
                    attachments.push(Attachment {
                        kind: kind_from_mime(ct),
                        mimetype: (!ct.is_empty()).then(|| ct.to_string()),
                        filename: (!name.is_empty()).then(|| name.to_string()),
                        size: None,
                        width: None,
                        height: None,
                        path: Some(path),
                        url: media::attachment_url(upload),
                        ..Default::default()
                    });
                } else {
                    extra.push(if name.is_empty() { "[attachment]".to_string() } else { format!("[attachment: {name}]") });
                }
            }
            Some(Metadata::DriveMetadata(drive)) => {
                let title = drive.title();
                let id = drive.id();
                let url = (!id.is_empty()).then(|| format!("https://drive.google.com/file/d/{id}/view"));
                embeds.push(Embed {
                    title: Some(if title.is_empty() { "Google Drive file".to_string() } else { title.to_string() }),
                    provider: Some("Google Drive".to_string()),
                    url,
                    ..Default::default()
                });
            }
            Some(Metadata::VideoCallMetadata(call)) => {
                let url = call.meeting_space.as_ref().map(|m| m.meeting_url().to_string()).filter(|s| !s.is_empty());
                let code = call.meeting_space.as_ref().map(|m| m.meeting_code().to_string()).filter(|s| !s.is_empty());
                embeds.push(Embed {
                    kind: Some("meet".to_string()),
                    color: Some(0x00897b),
                    icon: Some("videocam".to_string()),
                    title: Some("Google Meet".to_string()),
                    description: code.map(|c| format!("meet.google.com/{c}")),
                    provider: Some("Google Meet".to_string()),
                    cta: url.clone().map(|u| EmbedLink { label: "Join".to_string(), url: Some(u) }),
                    url,
                    ..Default::default()
                });
            }
            _ => {}
        }
    }

    // Rich App / Bot Cards (`JAddOnsContextualAddOn`) rendered read-only as safe Embed cards.
    for att in &message.attachments {
        let app_name = att.app_user.as_ref().map(|u| u.name().to_string()).filter(|s| !s.is_empty());
        match &att.r#type {
            Some(pb::attachment::Type::AddOnData(addon)) => {
                for card in &addon.cards {
                    let title = card
                        .header
                        .as_ref()
                        .and_then(|h| h.title.as_ref())
                        .map(|t| t.original_text().to_string())
                        .filter(|s| !s.is_empty())
                        .or_else(|| card.name.clone().filter(|s| !s.is_empty()));
                    let mut lines: Vec<String> = Vec::new();
                    if let Some(sub) = card
                        .header
                        .as_ref()
                        .and_then(|h| h.subtitle.as_ref())
                        .map(|t| t.original_text().trim().to_string())
                        .filter(|s| !s.is_empty())
                    {
                        lines.push(sub);
                    }
                    for section in &card.sections {
                        if let Some(h) = section.header.as_ref().map(|t| t.original_text().trim()).filter(|s| !s.is_empty()) {
                            lines.push(h.to_string());
                        }
                        for widget in &section.widgets {
                            match &widget.data {
                                Some(pb::j_add_ons_widget::Data::TextParagraph(tp)) => {
                                    if let Some(t) = tp.text.as_ref().map(|x| x.original_text().trim()).filter(|s| !s.is_empty()) {
                                        lines.push(t.to_string());
                                    }
                                }
                                Some(pb::j_add_ons_widget::Data::TextKeyValue(kv)) => {
                                    let k = kv.key.as_ref().map(|x| x.original_text().trim()).unwrap_or("");
                                    let v = kv.text.as_ref().map(|x| x.original_text().trim()).unwrap_or("");
                                    if !k.is_empty() && !v.is_empty() {
                                        lines.push(format!("{k}: {v}"));
                                    } else if !v.is_empty() {
                                        lines.push(v.to_string());
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    let description = (!lines.is_empty()).then(|| lines.join("\n"));
                    if title.is_some() || description.is_some() {
                        embeds.push(Embed {
                            title,
                            description,
                            provider: app_name.clone().or_else(|| Some("Google Chat App".to_string())),
                            ..Default::default()
                        });
                    }
                }
            }
            Some(pb::attachment::Type::Html(_)) => {
                embeds.push(Embed {
                    title: Some("App Card".to_string()),
                    provider: app_name.or_else(|| Some("Google Chat App".to_string())),
                    ..Default::default()
                });
            }
            None => {}
        }
    }

    if !message.attachments.is_empty() && attachments.is_empty() && embeds.is_empty() && extra.is_empty() && text.is_empty() {
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

    let reactions: Vec<Reaction> = message
        .reactions
        .iter()
        .filter_map(|r| {
            let emoji = match r.emoji.as_ref()?.content.as_ref()? {
                pb::emoji::Content::Unicode(u) if !u.is_empty() => u.replace('\u{fe0f}', ""),
                _ => return None,
            };
            let count = r.count().max(1) as i64;
            Some(Reaction { emoji, count, me: r.current_user_participated(), animated: false })
        })
        .collect();

    Rendered { body, html, mentions_own, attachments, embeds, reactions }
}

/// Parses outbound Markdown formatting and `@mentions` into Google Chat's plain text + UTF-16 annotations.
///
/// Supported syntax:
/// - ```` ```code block``` ```` -> `MonospaceBlock`
/// - `` `inline code` `` -> `Monospace`
/// - `**bold**` -> `Bold`
/// - `_italic_` -> `Italic`
/// - `~~strike~~` -> `Strike`
/// - `@all` -> `MentionAll`
/// - `@Name` (matched against `members` `(gaia_id, display_name)`) -> `Mention`
pub fn parse_outbound(input: &str, members: &[(String, String)]) -> (String, Vec<pb::Annotation>) {
    let mut out = String::with_capacity(input.len());
    let mut annotations: Vec<pb::Annotation> = Vec::new();
    let mut utf16_pos: usize = 0;

    // Sort member names longest-first so "@Ada Lovelace" matches before "@Ada".
    let mut sorted_members: Vec<(&str, &str)> = members
        .iter()
        .filter(|(id, name)| !id.is_empty() && !name.trim().is_empty())
        .map(|(id, name)| (id.as_str(), name.trim()))
        .collect();
    sorted_members.sort_by_key(|(_, name)| std::cmp::Reverse(name.len()));

    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Fenced code block: ```...```
        if input[i..].starts_with("```") {
            let after_open = i + 3;
            // Skip optional language tag up to first newline
            let content_start = if let Some(nl) = input[after_open..].find('\n') {
                let lang = &input[after_open..after_open + nl];
                if lang.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '+') {
                    after_open + nl + 1
                } else {
                    after_open
                }
            } else {
                after_open
            };
            if let Some(close_rel) = input[content_start..].find("```") {
                let inner = input[content_start..content_start + close_rel].trim_end_matches('\n');
                let len16 = inner.encode_utf16().count();
                if len16 > 0 {
                    annotations.push(format_annotation(utf16_pos, len16, FormatType::MonospaceBlock));
                    out.push_str(inner);
                    utf16_pos += len16;
                }
                i = content_start + close_rel + 3;
                continue;
            }
        }

        // Inline code: `...`
        if bytes[i] == b'`' {
            if let Some(close_rel) = input[i + 1..].find('`') {
                if close_rel > 0 && !input[i + 1..i + 1 + close_rel].contains('\n') {
                    let inner = &input[i + 1..i + 1 + close_rel];
                    let len16 = inner.encode_utf16().count();
                    annotations.push(format_annotation(utf16_pos, len16, FormatType::Monospace));
                    out.push_str(inner);
                    utf16_pos += len16;
                    i = i + 1 + close_rel + 1;
                    continue;
                }
            }
        }

        // Bold: **...**
        if input[i..].starts_with("**") {
            if let Some(close_rel) = input[i + 2..].find("**") {
                if close_rel > 0 && !input[i + 2..i + 2 + close_rel].contains('\n') {
                    let inner = &input[i + 2..i + 2 + close_rel];
                    let len16 = inner.encode_utf16().count();
                    annotations.push(format_annotation(utf16_pos, len16, FormatType::Bold));
                    out.push_str(inner);
                    utf16_pos += len16;
                    i = i + 2 + close_rel + 2;
                    continue;
                }
            }
        }

        // Strikethrough: ~~...~~
        if input[i..].starts_with("~~") {
            if let Some(close_rel) = input[i + 2..].find("~~") {
                if close_rel > 0 && !input[i + 2..i + 2 + close_rel].contains('\n') {
                    let inner = &input[i + 2..i + 2 + close_rel];
                    let len16 = inner.encode_utf16().count();
                    annotations.push(format_annotation(utf16_pos, len16, FormatType::Strike));
                    out.push_str(inner);
                    utf16_pos += len16;
                    i = i + 2 + close_rel + 2;
                    continue;
                }
            }
        }

        // Italic: _..._ (only at word boundary so snake_case is left alone)
        if bytes[i] == b'_' && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric()) {
            if let Some(close_rel) = input[i + 1..].find('_') {
                let close_end = i + 1 + close_rel + 1;
                let after_ok = close_end >= bytes.len() || !bytes[close_end].is_ascii_alphanumeric();
                if close_rel > 0 && after_ok && !input[i + 1..i + 1 + close_rel].contains('\n') {
                    let inner = &input[i + 1..i + 1 + close_rel];
                    let len16 = inner.encode_utf16().count();
                    annotations.push(format_annotation(utf16_pos, len16, FormatType::Italic));
                    out.push_str(inner);
                    utf16_pos += len16;
                    i = close_end;
                    continue;
                }
            }
        }

        // Mentions: @all or @Display Name (at start or after whitespace)
        if bytes[i] == b'@' && (i == 0 || bytes[i - 1].is_ascii_whitespace()) {
            let rest = &input[i + 1..];
            if rest.len() >= 3
                && rest[..3].eq_ignore_ascii_case("all")
                && rest[3..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_')
            {
                let token = &input[i..i + 4];
                let len16 = token.encode_utf16().count();
                annotations.push(pb::Annotation {
                    r#type: Some(pb::AnnotationType::UserMention as i32),
                    start_index: Some(utf16_pos as i32),
                    length: Some(len16 as i32),
                    metadata: Some(Metadata::UserMentionMetadata(pb::UserMentionMetadata {
                        id: None,
                        display_name: Some("all".into()),
                        r#type: Some(pb::user_mention_metadata::Type::MentionAll as i32),
                        ..Default::default()
                    })),
                    ..Default::default()
                });
                out.push_str(token);
                utf16_pos += len16;
                i += 4;
                continue;
            }
            let mut matched = false;
            for &(gaia, name) in &sorted_members {
                if rest.len() >= name.len()
                    && rest.is_char_boundary(name.len())
                    && rest[..name.len()].eq_ignore_ascii_case(name)
                    && rest[name.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_')
                {
                    let token = format!("@{name}");
                    let len16 = token.encode_utf16().count();
                    annotations.push(pb::Annotation {
                        r#type: Some(pb::AnnotationType::UserMention as i32),
                        start_index: Some(utf16_pos as i32),
                        length: Some(len16 as i32),
                        metadata: Some(Metadata::UserMentionMetadata(pb::UserMentionMetadata {
                            id: Some(super::user_id(gaia)),
                            display_name: Some(name.to_string()),
                            r#type: Some(pb::user_mention_metadata::Type::Mention as i32),
                            ..Default::default()
                        })),
                        ..Default::default()
                    });
                    out.push_str(&token);
                    utf16_pos += len16;
                    i += 1 + name.len();
                    matched = true;
                    break;
                }
            }
            if matched {
                continue;
            }
        }

        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        utf16_pos += ch.len_utf16();
        i += ch.len_utf8();
    }

    (out, annotations)
}

fn format_annotation(start: usize, length: usize, kind: FormatType) -> pb::Annotation {
    pb::Annotation {
        r#type: Some(pb::AnnotationType::FormatData as i32),
        start_index: Some(start as i32),
        length: Some(length as i32),
        metadata: Some(Metadata::FormatMetadata(pb::FormatMetadata {
            format_type: Some(kind as i32),
            ..Default::default()
        })),
        ..Default::default()
    }
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
        let r = render(&m, "1", "");
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
        assert_eq!(render(&m, "", "").html.as_deref(), Some("😀 <strong>bold <em>it</em></strong>"));
    }

    #[test]
    fn crossing_spans_are_reopened() {
        let m = pb::Message { text_body: Some("abcd".into()), annotations: vec![format(0, 3, FormatType::Bold), format(1, 3, FormatType::Italic)], ..Default::default() };
        assert_eq!(render(&m, "", "").html.as_deref(), Some("<strong>a<em>bc</em></strong><em>d</em>"));
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
        assert!(render(&m, "7", "").mentions_own);
        assert!(!render(&m, "8", "").mentions_own);
        let all = pb::Message { text_body: Some("@all hi".into()), annotations: vec![mention("", pb::user_mention_metadata::Type::MentionAll)], ..Default::default() };
        assert!(render(&all, "8", "").mentions_own);
    }

    #[test]
    fn outbound_markdown_and_mentions_use_utf16_offsets() {
        let members = vec![("42".to_string(), "Ada Lovelace".to_string())];
        let (text, anns) = parse_outbound("😀 **hi** _you_ ~~old~~ `x` @Ada Lovelace @all", &members);
        assert_eq!(text, "😀 hi you old x @Ada Lovelace @all");
        assert_eq!(anns.len(), 6);
        // "😀 " is 3 UTF-16 code units (2 + 1), so "hi" is at 3..5.
        assert_eq!(anns[0].start_index, Some(3));
        assert_eq!(anns[0].length, Some(2));
        // "@Ada Lovelace" is 13 UTF-16 units.
        assert!(matches!(&anns[4].metadata, Some(Metadata::UserMentionMetadata(m)) if m.id.as_ref().and_then(|u| u.id.as_deref()) == Some("42")));
        assert!(matches!(&anns[5].metadata, Some(Metadata::UserMentionMetadata(m)) if m.r#type() == pb::user_mention_metadata::Type::MentionAll));
    }
}
