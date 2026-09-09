//! Uploading a picture somewhere the forum will accept a link to.
//!
//! The site's own uploader takes images only and refuses anything else, so
//! sharing a file means putting it on a host that will take it and posting
//! the link. This is that host, and it is deliberately the only one.

use super::*;

/// postimg.cc's own anonymous upload endpoint - undocumented (its
/// advertised "official API" at api.postimages.org requires registration
/// and returns an empty body when probed anonymously), reverse-engineered
/// from a real browser session's HAR capture instead. The plain form POST
/// a browser submits (`postimages.org/json`, the same fields the site's own
/// upload page sends - see the `fields` array below) 403s with "Automated
/// uploads are not allowed via the website" unless `Origin`/`Referer` also
/// look like they came from the site itself (confirmed live: adding just
/// those two headers, nothing else, is what turns the 403 into a real
/// upload) - see `post_multipart`'s `extra_headers`.
pub(super) const POSTIMG_UPLOAD_URL: &str = "https://postimages.org/json";

pub(super) const POSTIMG_ORIGIN: &str = "https://postimages.org";

/// postimg.cc's own free-tier cap, read directly out of its upload page's
/// JS init (`maxFilesize:33554432`) - checked here for the same "fail fast
/// with a clear reason" rationale qu.ax's old cap had.
pub(super) const POSTIMG_MAX_UPLOAD_BYTES: usize = 32 * 1024 * 1024;

#[derive(serde::Deserialize)]
pub(super) struct PostimgUploadResponse {
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    error: Option<PostimgErrorBody>,
}

#[derive(serde::Deserialize)]
pub(super) struct PostimgErrorBody {
    message: String,
}

/// postimg.cc only accepts images (its own `accept` list is image formats
/// plus PDF/postscript/raw-camera formats, no video) - unlike qu.ax, which
/// hosted anything. `None` for a file extension this project's own
/// attachment picker would otherwise let through (see ATTACHMENT_EXTS)
/// means "ask for an image instead" rather than guessing a content type
/// postimg.cc would reject anyway.
/// Every extension postimg.cc will take, and what to send it as.
///
/// One table rather than a match, because two things need it: this, to fill
/// in the multipart content type, and the host listing a client draws its
/// menu from. They were separate before, and disagreed - the listing counted
/// avif an image while this refused it, so an .avif routed to postimg was
/// accepted by the menu and rejected by the site.
pub const POSTIMG_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
];

pub fn guess_postimg_content_type(file_name: &str) -> Option<&'static str> {
    let ext = file_name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    POSTIMG_TYPES.iter().find(|(e, _)| *e == ext).map(|(_, t)| *t)
}

/// Matches the site's own `new Date().getTime()+Math.random().toString().
/// substring(1)` (read out of its upload page's JS) closely enough to pass
/// as a real one - a millisecond timestamp directly followed by a
/// "0."-stripped random fraction, e.g. "1786561259146.7777739930052086".
/// Nothing observed depends on the exact shape (it reads as a per-upload
/// de-dup/session key, not a validated token), but there's no reason to
/// diverge from it either.
pub(super) fn postimg_upload_session() -> String {
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
    let frac = format!("{:.16}", rand::random::<f64>());
    format!("{millis}{}", frac.trim_start_matches('0'))
}

/// postimg.cc's result page (`https://postimg.cc/<slug>/<hash>`) embeds a
/// `<div class="col" data-image="..." data-hash="..." data-hotlink="..."
/// data-name="..." data-ext="...">` for the just-uploaded image - `data-
/// name`/`data-ext` are the server's own (possibly sanitized - e.g. an
/// underscore in the original filename comes back as a dash) stored
/// filename, needed to build the `i.postimg.cc/<slug>/<name>.<ext>`
/// "thumbnail" link (see send_attachment's own doc comment for why this
/// variant over the page's other "hotlink"/direct-link slug). Reusing
/// form::attr rather than a one-off parser here since the shape (name/
/// value pairs on a single tag) is exactly what it already handles.
///
/// `data-image=` isn't unique on the page - confirmed live, the page's
/// own outer `<div class="container mb-5" data-image="...">` wrapper
/// carries it too, with none of the other `data-*` attributes alongside
/// it, and appears first in the HTML. Taking the very first match found
/// nothing there and returned early instead of continuing on to the real
/// `class="col"` one right after it, so every candidate tag is checked in
/// order instead of stopping at the first.
pub(super) fn extract_postimg_thumb_name(html: &str) -> Option<(String, String)> {
    let mut search_from = 0;
    while let Some(rel) = html[search_from..].find("data-image=") {
        let start = search_from + rel;
        let tag_start = html[..start].rfind('<')?;
        let tag_end = tag_start + html[tag_start..].find('>')?;
        let tag = &html[tag_start..tag_end];
        search_from = tag_end;

        if let (Some(name), Some(ext)) = (form::attr(tag, "data-name"), form::attr(tag, "data-ext")) {
            return Some((name, ext));
        }
    }
    None
}

/// Both links postimg.cc gives back for one upload.
pub struct PostimgLinks {
    /// The page about the image, for a click-through.
    pub page: String,
    /// The image itself, for anywhere that renders a bare URL.
    pub direct: String,
}

/// Puts an image on postimg.cc and returns where it landed.
///
/// Shared rather than private to Sneedchat, because the two callers want
/// different halves of the same answer: Sneedchat wraps both in the BBCode
/// the site renders, and IRC sends the direct link on its own, having no
/// markup to wrap anything in. The mechanics below are the same either way
/// and were hard enough won that a second copy of them would be a liability.
pub async fn upload_to_postimg(file_path: &str) -> Result<PostimgLinks> {
    let bytes = tokio::fs::read(file_path).await.with_context(|| format!("reading {file_path}"))?;
    if bytes.is_empty() {
        bail!("file is empty");
    }
    if bytes.len() > POSTIMG_MAX_UPLOAD_BYTES {
        bail!("file is over postimg.cc's {}MB upload limit", POSTIMG_MAX_UPLOAD_BYTES / 1024 / 1024);
    }

    let file_name = std::path::Path::new(file_path).file_name().and_then(|n| n.to_str()).unwrap_or("upload").to_string();
    let Some(content_type) = guess_postimg_content_type(&file_name) else {
        bail!("postimg.cc only accepts images (png/jpg/gif/webp/bmp), not \"{file_name}\"");
    };

    let http = http::HttpClient::new(Transport::Direct, http::CookieJar::new(), DEFAULT_USER_AGENT.to_string());
    let session = postimg_upload_session();
    let bytes = Bytes::from(bytes);
    let fields = [
        http::MultipartField::Text { name: "gallery", value: "" },
        http::MultipartField::Text { name: "optsize", value: "0" },
        http::MultipartField::Text { name: "expire", value: "604800" },
        http::MultipartField::Text { name: "numfiles", value: "1" },
        http::MultipartField::Text { name: "upload_session", value: &session },
        http::MultipartField::File { name: "file", file_name: &file_name, content_type, bytes: &bytes },
    ];
    let extra_headers = [("Origin", POSTIMG_ORIGIN), ("Referer", &format!("{POSTIMG_ORIGIN}/"))];

    let (status, resp_bytes) = http.post_multipart(POSTIMG_UPLOAD_URL, &fields, &extra_headers).await.context("uploading to postimg.cc")?;
    if !(200..300).contains(&status) {
        let snippet = String::from_utf8_lossy(&resp_bytes[..resp_bytes.len().min(300)]);
        bail!("postimg.cc upload failed with HTTP {status}: {snippet}");
    }
    let parsed: PostimgUploadResponse = serde_json::from_slice(&resp_bytes).context("parsing postimg.cc response")?;
    if let Some(err) = parsed.error {
        bail!("postimg.cc upload failed: {}", err.message);
    }
    // The upload response's own url carries a delete-hash suffix
    // (`https://postimg.cc/<slug>/<hash>`) that the result page needs to
    // actually render (the bare `/<slug>` alone serves something else -
    // confirmed live, scraping came back empty without it). The outer
    // `[url=]` wrapper below uses the slug-only form instead, matching
    // postimg.cc's own "Thumbnail for forums" BBCode preset shown on that
    // page - the hash is a one-time delete credential, not part of the
    // link anyone else needs to view the image.
    let full_page_url = parsed.url.ok_or_else(|| anyhow!("postimg.cc response had no url"))?;
    let slug = full_page_url.strip_prefix("https://postimg.cc/").and_then(|rest| rest.split('/').next()).ok_or_else(|| anyhow!("unexpected postimg.cc url shape: {full_page_url}"))?;
    let short_page_url = format!("https://postimg.cc/{slug}");

    let page = http.get(&full_page_url).await.context("fetching postimg.cc result page")?;
    let (name, ext) = extract_postimg_thumb_name(&page.body).ok_or_else(|| anyhow!("couldn't find the uploaded image's filename on {full_page_url}"))?;
    let direct_url = format!("https://i.postimg.cc/{slug}/{name}.{ext}");


    Ok(PostimgLinks { page: short_page_url, direct: direct_url })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_postimg_thumb_name_from_a_real_result_page() {
        use super::extract_postimg_thumb_name;
        // The page's own outer wrapper carries a bare data-image= with none
        // of the other data-* attributes, appearing before the real
        // class="col" one - confirmed live to trip up a "stop at the first
        // match" parser (see extract_postimg_thumb_name's own doc comment).
        let html = r#"<div class="container mb-5" data-image="PNzTLBBt">
    <div class="row g-4">
    <div class="col-12 col-md-4" style="max-width: 240px;">
    <div class="col" data-image="PNzTLBBt" data-hash="ea20f286" data-hotlink="05hQ4vnJ" data-name="2026-08-09-14-45-58" data-ext="png" data-homepage="0"><div class="card h-100">"#;
        let (name, ext) = extract_postimg_thumb_name(html).expect("should find the data-image div");
        assert_eq!(name, "2026-08-09-14-45-58");
        assert_eq!(ext, "png");
    }

    #[test]
    fn postimg_thumb_name_is_none_without_a_matching_tag() {
        use super::extract_postimg_thumb_name;
        assert!(extract_postimg_thumb_name("<html><body>nothing here</body></html>").is_none());
    }

    #[test]
    fn postimg_accepts_common_image_extensions_only() {
        use super::guess_postimg_content_type;
        assert_eq!(guess_postimg_content_type("photo.PNG"), Some("image/png"));
        assert_eq!(guess_postimg_content_type("photo.jpeg"), Some("image/jpeg"));
        assert_eq!(guess_postimg_content_type("clip.mp4"), None);
        assert_eq!(guess_postimg_content_type("noext"), None);
    }
}
