//! Instant View: the article a link's preview carries, drawn as a page of its own.
//!
//! A message that links to a page Telegram has made an Instant View of carries the page's blocks (headings,
//! paragraphs with their styling, pictures, lists, quotes, tables) as a tree. The window opens it in a window of its
//! own, so this turns the tree into one self-contained HTML document: everything in it is escaped here, pictures are
//! fetched into the cache and named by their local file, and links are limited to the kinds a page may reasonably have.
//! What it cannot draw (embedded players, maps, other channels' posts) is left out, with the page's own link on the
//! card to open it in a browser instead.

use super::media::{download_thumb, download_to, media_name, pick, Variant, MEDIA};
use super::send::{explain, peer_for};
use crate::model::Buffer;
use crate::state::AppState;
use grammers_client::media::{Document, Media, Photo};
use grammers_client::tl;
use std::collections::HashMap;
use std::path::PathBuf;
use tl::enums::{PageBlock as B, RichText as R};

/// A finished article: its title, for the window, and the document.
pub struct Article {
    pub title: String,
    pub url: String,
    pub html: String,
}

/// What a page needs fetched: a picture, or a video's poster.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Want {
    Photo(i64),
    Poster(i64),
}

struct Cx {
    account: i64,
    remote: String,
    message: i32,
    photos: HashMap<i64, tl::enums::Photo>,
    documents: HashMap<i64, tl::enums::Document>,
    wanted: Vec<Want>,
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// An address a page's link may point to: web pages, mail, phones and Telegram's own, or an anchor in the page.
fn safe_url(url: &str) -> String {
    let lower = url.trim().to_ascii_lowercase();
    let ok = ["http://", "https://", "mailto:", "tel:", "tg://", "#"].iter().any(|p| lower.starts_with(p));
    if ok { escape(url.trim()) } else { "#".to_string() }
}

fn percent(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

impl Cx {
    fn path(&self, want: Want) -> PathBuf {
        let (kind, id) = match want {
            Want::Photo(id) => ("iv", id),
            Want::Poster(id) => ("ivp", id),
        };
        let name = media_name(self.account, &self.remote, self.message, Variant::Full, "jpg").replace("_full.jpg", &format!("_{kind}{id}.jpg"));
        crate::media_cache::dir(MEDIA).join(name)
    }

    /// The address of a picture the page uses, once it is fetched; `None` when the page does not carry it.
    fn picture(&mut self, want: Want) -> Option<String> {
        let known = match want {
            Want::Photo(id) => self.photos.contains_key(&id),
            Want::Poster(id) => self.documents.contains_key(&id),
        };
        if !known {
            return None;
        }
        if !self.wanted.contains(&want) {
            self.wanted.push(want);
        }
        Some(format!("moho-media://file/?p={}", percent(&self.path(want).display().to_string())))
    }
}

fn rich(t: &R, cx: &mut Cx) -> String {
    let wrap = |tag: &str, inner: &R, cx: &mut Cx| format!("<{tag}>{}</{tag}>", rich(inner, cx));
    match t {
        R::TextEmpty => String::new(),
        R::TextPlain(p) => escape(&p.text).replace('\n', "<br>"),
        R::TextBold(b) => wrap("b", &b.text, cx),
        R::TextItalic(i) => wrap("i", &i.text, cx),
        R::TextUnderline(u) => wrap("u", &u.text, cx),
        R::TextStrike(s) => wrap("s", &s.text, cx),
        R::TextFixed(f) => wrap("code", &f.text, cx),
        R::TextSubscript(s) => wrap("sub", &s.text, cx),
        R::TextSuperscript(s) => wrap("sup", &s.text, cx),
        R::TextMarked(m) => wrap("mark", &m.text, cx),
        R::TextUrl(u) => format!("<a href=\"{}\">{}</a>", safe_url(&u.url), rich(&u.text, cx)),
        R::TextEmail(e) => format!("<a href=\"mailto:{}\">{}</a>", safe_url(&e.email).trim_start_matches("mailto:"), rich(&e.text, cx)),
        R::TextPhone(p) => format!("<a href=\"tel:{}\">{}</a>", escape(&p.phone), rich(&p.text, cx)),
        R::TextConcat(c) => c.texts.iter().map(|t| rich(t, cx)).collect(),
        R::TextAnchor(a) => format!("<span id=\"{}\">{}</span>", escape(&a.name), rich(&a.text, cx)),
        R::TextSpoiler(s) => rich(&s.text, cx),
        R::TextMention(m) => rich(&m.text, cx),
        R::TextHashtag(m) => rich(&m.text, cx),
        R::TextBotCommand(m) => rich(&m.text, cx),
        R::TextCashtag(m) => rich(&m.text, cx),
        R::TextBankCard(m) => rich(&m.text, cx),
        R::TextAutoUrl(m) => rich(&m.text, cx),
        R::TextAutoEmail(m) => rich(&m.text, cx),
        R::TextAutoPhone(m) => rich(&m.text, cx),
        _ => String::new(),
    }
}

fn plain(t: &R) -> String {
    let mut cx = Cx { account: 0, remote: String::new(), message: 0, photos: HashMap::new(), documents: HashMap::new(), wanted: Vec::new() };
    let html = rich(t, &mut cx);
    // Only for a title: the tags taken out again.
    let mut out = String::new();
    let mut inside = false;
    for c in html.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            c if !inside => out.push(c),
            _ => {}
        }
    }
    out.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&#39;", "'").trim().to_string()
}

fn is_empty(t: &R) -> bool {
    matches!(t, R::TextEmpty) || plain(t).is_empty()
}

fn caption(c: &tl::enums::PageCaption, cx: &mut Cx) -> String {
    let tl::enums::PageCaption::Caption(c) = c;
    let (text, credit) = (rich(&c.text, cx), rich(&c.credit, cx));
    if text.is_empty() && credit.is_empty() {
        return String::new();
    }
    format!("<figcaption>{text}{}</figcaption>", if credit.is_empty() { String::new() } else { format!("<cite>{credit}</cite>") })
}

fn blocks(list: &[B], cx: &mut Cx) -> String {
    list.iter().map(|b| block(b, cx)).collect()
}

fn heading(tag: &str, t: &R, cx: &mut Cx) -> String {
    format!("<{tag}>{}</{tag}>", rich(t, cx))
}

fn block(b: &B, cx: &mut Cx) -> String {
    match b {
        B::Title(t) => heading("h1", &t.text, cx),
        B::Subtitle(t) => format!("<p class=\"subtitle\">{}</p>", rich(&t.text, cx)),
        B::Kicker(t) => format!("<p class=\"kicker\">{}</p>", rich(&t.text, cx)),
        B::AuthorDate(a) => {
            let who = rich(&a.author, cx);
            let when = chrono::DateTime::from_timestamp(i64::from(a.published_date), 0).filter(|_| a.published_date > 0).map(|d| d.format("%B %-d, %Y").to_string());
            let parts: Vec<String> = [Some(who).filter(|w| !w.is_empty()), when].into_iter().flatten().collect();
            if parts.is_empty() { String::new() } else { format!("<p class=\"byline\">{}</p>", parts.join(" · ")) }
        }
        B::Header(h) => heading("h2", &h.text, cx),
        B::Heading1(h) => heading("h2", &h.text, cx),
        B::Heading2(h) => heading("h3", &h.text, cx),
        B::Heading3(h) => heading("h4", &h.text, cx),
        B::Heading4(h) => heading("h5", &h.text, cx),
        B::Heading5(h) => heading("h6", &h.text, cx),
        B::Heading6(h) => heading("h6", &h.text, cx),
        B::Subheader(t) => heading("h3", &t.text, cx),
        B::Paragraph(p) => {
            let inner = rich(&p.text, cx);
            if inner.is_empty() { String::new() } else { format!("<p>{inner}</p>") }
        }
        B::Preformatted(p) => format!("<pre><code>{}</code></pre>", rich(&p.text, cx)),
        B::Footer(f) => format!("<p class=\"footer\">{}</p>", rich(&f.text, cx)),
        B::Divider => "<hr>".to_string(),
        B::Anchor(a) => format!("<a id=\"{}\"></a>", escape(&a.name)),
        B::List(l) => {
            let items: String = l
                .items
                .iter()
                .map(|i| match i {
                    tl::enums::PageListItem::Text(t) => format!("<li>{}</li>", rich(&t.text, cx)),
                    tl::enums::PageListItem::Blocks(b) => format!("<li>{}</li>", blocks(&b.blocks, cx)),
                })
                .collect();
            format!("<ul>{items}</ul>")
        }
        B::OrderedList(l) => {
            let items: String = l
                .items
                .iter()
                .map(|i| match i {
                    tl::enums::PageListOrderedItem::Text(t) => format!("<li>{}</li>", rich(&t.text, cx)),
                    tl::enums::PageListOrderedItem::Blocks(b) => format!("<li>{}</li>", blocks(&b.blocks, cx)),
                })
                .collect();
            let start = l.start.map(|s| format!(" start=\"{s}\"")).unwrap_or_default();
            format!("<ol{start}{}>{items}</ol>", if l.reversed { " reversed" } else { "" })
        }
        B::Blockquote(q) => {
            let who = rich(&q.caption, cx);
            format!("<blockquote><p>{}</p>{}</blockquote>", rich(&q.text, cx), if who.is_empty() { String::new() } else { format!("<cite>{who}</cite>") })
        }
        B::BlockquoteBlocks(q) => format!("<blockquote>{}</blockquote>", blocks(&q.blocks, cx)),
        B::Pullquote(q) => {
            let who = rich(&q.caption, cx);
            format!("<blockquote class=\"pull\"><p>{}</p>{}</blockquote>", rich(&q.text, cx), if who.is_empty() { String::new() } else { format!("<cite>{who}</cite>") })
        }
        B::Photo(p) => {
            let Some(src) = cx.picture(Want::Photo(p.photo_id)) else { return String::new() };
            let img = format!("<img src=\"{src}\" alt=\"\">");
            let img = match &p.url {
                Some(url) => format!("<a href=\"{}\">{img}</a>", safe_url(url)),
                None => img,
            };
            format!("<figure>{img}{}</figure>", caption(&p.caption, cx))
        }
        B::Video(v) => {
            let poster = cx.picture(Want::Poster(v.video_id)).map(|src| format!("<img src=\"{src}\" alt=\"\">")).unwrap_or_default();
            format!("<figure class=\"video\">{poster}<span class=\"note\">Video: open the link above to watch it</span>{}</figure>", caption(&v.caption, cx))
        }
        B::Cover(c) => format!("<div class=\"cover\">{}</div>", block(&c.cover, cx)),
        B::Collage(c) => format!("<figure class=\"collage\"><div>{}</div>{}</figure>", blocks(&c.items, cx), caption(&c.caption, cx)),
        B::Slideshow(c) => format!("<figure class=\"collage\"><div>{}</div>{}</figure>", blocks(&c.items, cx), caption(&c.caption, cx)),
        B::Embed(e) => {
            let poster = e.poster_photo_id.and_then(|id| cx.picture(Want::Photo(id))).map(|src| format!("<img src=\"{src}\" alt=\"\">")).unwrap_or_default();
            let link = e.url.as_deref().map(|u| format!("<a class=\"note\" href=\"{}\">Open the embedded content</a>", safe_url(u))).unwrap_or_default();
            format!("<figure>{poster}{link}{}</figure>", caption(&e.caption, cx))
        }
        B::Table(t) => {
            let rows: String = t
                .rows
                .iter()
                .map(|row| {
                    let tl::enums::PageTableRow::Row(row) = row;
                    let cells: String = row
                        .cells
                        .iter()
                        .map(|cell| {
                            let tl::enums::PageTableCell::Cell(c) = cell;
                            let tag = if c.header { "th" } else { "td" };
                            let span = format!("{}{}", c.colspan.map(|n| format!(" colspan=\"{n}\"")).unwrap_or_default(), c.rowspan.map(|n| format!(" rowspan=\"{n}\"")).unwrap_or_default());
                            let align = if c.align_center { " class=\"c\"" } else if c.align_right { " class=\"r\"" } else { "" };
                            format!("<{tag}{span}{align}>{}</{tag}>", c.text.as_ref().map(|t| rich(t, cx)).unwrap_or_default())
                        })
                        .collect();
                    format!("<tr>{cells}</tr>")
                })
                .collect();
            let title = rich(&t.title, cx);
            format!("<div class=\"table\"><table>{}{rows}</table></div>", if title.is_empty() { String::new() } else { format!("<caption>{title}</caption>") })
        }
        B::Details(d) => format!("<details{}><summary>{}</summary>{}</details>", if d.open { " open" } else { "" }, rich(&d.title, cx), blocks(&d.blocks, cx)),
        B::Math(m) => format!("<pre><code>{}</code></pre>", escape(&m.source)),
        _ => String::new(),
    }
}

const STYLE: &str = "
:root{color-scheme:light dark;--bg:#fff;--fg:#1c2733;--muted:#6b7a89;--line:#d8dee4;--link:#2b7bd3;--soft:#f2f5f8}
@media (prefers-color-scheme:dark){:root{--bg:#0e1621;--fg:#e4e9ee;--muted:#8a99a8;--line:#26323f;--link:#6ab3f3;--soft:#17212b}}
html,body{margin:0;background:var(--bg);color:var(--fg)}
body{font:17px/1.65 system-ui,-apple-system,'Segoe UI','Noto Sans',sans-serif}
main{max-width:740px;margin:0 auto;padding:0 20px 80px}
a{color:var(--link);text-decoration:none}a:hover{text-decoration:underline}
h1{font-size:2rem;line-height:1.2;margin:.8em 0 .4em}h2{font-size:1.5rem;margin:1.4em 0 .4em}h3{font-size:1.25rem;margin:1.3em 0 .3em}h4,h5,h6{margin:1.2em 0 .3em}
p{margin:.9em 0}.kicker{color:var(--muted);text-transform:uppercase;letter-spacing:.06em;font-size:.8rem;margin:1.6em 0 0}
.subtitle{color:var(--muted);font-size:1.15rem}.byline,.footer{color:var(--muted);font-size:.9rem}
figure{margin:1.4em -20px}figure img,.cover img{display:block;width:100%;height:auto}figcaption{padding:.5em 20px 0;color:var(--muted);font-size:.9rem}figcaption cite{display:block;font-style:normal;opacity:.8}
.cover{margin:0 -20px}.cover figure{margin:0}.collage div{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:4px}.collage figure{margin:0}
.note{display:block;padding:.6em 20px;color:var(--muted);font-size:.9rem}
blockquote{margin:1.2em 0;padding:.1em 1em;border-left:3px solid var(--link);color:var(--fg)}blockquote.pull{font-size:1.25rem;border-left:0;text-align:center;font-style:italic}cite{display:block;color:var(--muted);font-style:normal;font-size:.9rem}
pre{background:var(--soft);padding:12px 14px;overflow:auto;border-radius:6px;font-size:.9rem}code{font-family:ui-monospace,Menlo,Consolas,monospace}mark{background:#ffe08a55;color:inherit}
hr{border:0;border-top:1px solid var(--line);margin:2em 0}ul,ol{padding-left:1.4em}li{margin:.3em 0}
.table{overflow:auto;margin:1.2em 0}table{border-collapse:collapse;width:100%}td,th{border:1px solid var(--line);padding:6px 10px;vertical-align:top}th{background:var(--soft)}.c{text-align:center}.r{text-align:right}caption{caption-side:top;text-align:left;font-weight:600;padding-bottom:.4em}
details{margin:1em 0;border:1px solid var(--line);border-radius:6px;padding:.4em .9em}summary{cursor:pointer;font-weight:600}
";

/// The article a message's link preview carries, with its pictures fetched.
pub async fn article(state: &AppState, buffer: &Buffer, msg_id: &str) -> Result<Article, String> {
    let id: i32 = msg_id.parse().map_err(|_| "that message has no article".to_string())?;
    let (handle, peer) = peer_for(state, buffer).await?;
    let found = handle.client.get_messages_by_id(peer, &[id]).await.map_err(|e| explain(&e))?;
    let Some(Some(message)) = found.into_iter().next() else { return Err("that message is no longer there".to_string()) };
    let Some(Media::WebPage(media)) = message.media() else { return Err("that message links to no page".to_string()) };
    let tl::enums::WebPage::Page(page) = &media.raw.webpage else { return Err("Telegram has no preview of that page".to_string()) };
    let url = page.url.clone();
    let title = page.title.clone().unwrap_or_default();
    let mut cached = page.cached_page.clone();
    // A message may carry only the start of the page: the whole of it is asked for.
    if matches!(&cached, Some(tl::enums::Page::Page(p)) if p.part) {
        if let Ok(tl::enums::messages::WebPage::Page(full)) = handle.client.invoke(&tl::functions::messages::GetWebPage { url: url.clone(), hash: 0 }).await {
            if let tl::enums::WebPage::Page(whole) = full.webpage {
                cached = whole.cached_page.or(cached);
            }
        }
    }
    let Some(tl::enums::Page::Page(doc)) = cached else { return Err("Telegram has no Instant View of that page".to_string()) };
    let remote = buffer.remote_id.clone().unwrap_or_default();
    let mut cx = Cx {
        account: handle.own.user_id,
        remote,
        message: id,
        photos: doc.photos.iter().filter_map(|p| match p { tl::enums::Photo::Photo(x) => Some((x.id, p.clone())), _ => None }).collect(),
        documents: doc.documents.iter().filter_map(|d| match d { tl::enums::Document::Document(x) => Some((x.id, d.clone())), _ => None }).collect(),
        wanted: Vec::new(),
    };
    let body = blocks(&doc.blocks, &mut cx);
    // The page's own title, from its first heading when the preview gave none.
    let title = if title.is_empty() { doc.blocks.iter().find_map(|b| if let B::Title(t) = b { Some(plain(&t.text)).filter(|t| !t.is_empty()) } else { None }).unwrap_or_default() } else { title };
    let has_title = doc.blocks.iter().any(|b| matches!(b, B::Title(t) if !is_empty(&t.text)));
    let heading = if has_title || title.is_empty() { String::new() } else { format!("<h1>{}</h1>", escape(&title)) };

    // The pictures, fetched together; one that will not come leaves a gap where it was.
    let mut jobs = tokio::task::JoinSet::new();
    for want in cx.wanted.clone() {
        let path = cx.path(want);
        if path.exists() {
            continue;
        }
        let client = handle.client.clone();
        let (photo, document) = match want {
            Want::Photo(id) => (cx.photos.get(&id).cloned(), None),
            Want::Poster(id) => (None, cx.documents.get(&id).cloned()),
        };
        jobs.spawn(async move {
            let done = if let Some(photo) = photo {
                let photo = Photo::from_raw(photo);
                match pick(&photo.thumbs(), 1280) {
                    Some(size) => download_to(&client, size, &path, false).await,
                    None => download_to(&client, &photo, &path, false).await,
                }
            } else if let Some(document) = document {
                let document = Document::from_raw_media(tl::types::MessageMediaDocument {
                    nopremium: false,
                    spoiler: false,
                    video: true,
                    round: false,
                    voice: false,
                    document: Some(document),
                    alt_documents: None,
                    video_cover: None,
                    video_timestamp: None,
                    ttl_seconds: None,
                });
                download_thumb(&client, &document, &path).await
            } else {
                Ok(())
            };
            if let Err(e) = done {
                tracing::debug!("telegram: a picture of an article: {e:#}");
            }
        });
    }
    while jobs.join_next().await.is_some() {}

    let rtl = if doc.rtl { " dir=\"rtl\"" } else { "" };
    let html = format!(
        "<!doctype html><html lang=\"en\"{rtl}><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src moho-media: data:; style-src 'unsafe-inline'\">\
         <title>{}</title><style>{STYLE}</style></head><body><main>{heading}{body}<p class=\"footer\"><a href=\"{}\">Open the original page</a></p></main></body></html>",
        escape(&title),
        safe_url(&url)
    );
    Ok(Article { title, url, html })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx() -> Cx {
        Cx { account: 1, remote: "-100123".into(), message: 7, photos: HashMap::new(), documents: HashMap::new(), wanted: Vec::new() }
    }

    fn plain_text(t: &str) -> R {
        R::TextPlain(tl::types::TextPlain { text: t.into() })
    }

    #[test]
    fn text_is_escaped_and_styled() {
        let mut c = cx();
        let t = R::TextConcat(tl::types::TextConcat {
            texts: vec![plain_text("a <b> & "), R::TextBold(Box::new(tl::types::TextBold { text: plain_text("bold") }))],
        });
        assert_eq!(rich(&t, &mut c), "a &lt;b&gt; &amp; <b>bold</b>");
    }

    #[test]
    fn a_link_may_only_go_where_a_page_would() {
        assert_eq!(safe_url("https://example.com/a?b=1&c=2"), "https://example.com/a?b=1&amp;c=2");
        assert_eq!(safe_url("javascript:alert(1)"), "#");
        assert_eq!(safe_url("data:text/html,x"), "#");
        assert_eq!(safe_url("#section"), "#section");
        let mut c = cx();
        let link = R::TextUrl(Box::new(tl::types::TextUrl { text: plain_text("x"), url: "javascript:alert(1)".into(), webpage_id: 0 }));
        assert_eq!(rich(&link, &mut c), "<a href=\"#\">x</a>");
    }

    #[test]
    fn a_picture_the_page_lacks_leaves_nothing_and_one_it_has_is_named_by_its_file() {
        let mut c = cx();
        let photo = B::Photo(tl::types::PageBlockPhoto {
            spoiler: false,
            photo_id: 9,
            caption: tl::enums::PageCaption::Caption(tl::types::PageCaption { text: plain_text("a caption"), credit: R::TextEmpty }),
            url: None,
            webpage_id: None,
        });
        assert_eq!(block(&photo, &mut c), "");
        c.photos.insert(9, tl::enums::Photo::Empty(tl::types::PhotoEmpty { id: 9 }));
        let html = block(&photo, &mut c);
        assert!(html.contains("moho-media://file/?p=") && html.contains("_iv9.jpg") && html.contains("a caption"), "{html}");
        assert_eq!(c.wanted, vec![Want::Photo(9)]);
    }
}
