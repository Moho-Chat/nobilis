use libsignal_service::sender::AttachmentSpec;
use mime::Mime;
use regex::Regex;
use reqwest::header::CONTENT_TYPE;
use reqwest::IntoUrl;
use scraper::{Html, Selector};
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;
use url::{Host, Url};

// same limits as Signal-Android
const MAX_HTML_SIZE: usize = 2 * 1024 * 1024;
const MAX_IMAGE_SIZE: usize = 2 * 1024 * 1024;

// Signal clients don't preview links to these
const RESERVED_DOMAINS: &[&str] = &[
    "example",
    "example.com",
    "example.net",
    "example.org",
    "i2p",
    "invalid",
    "localhost",
    "onion",
    "test",
];

#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum Error {
    #[error("Reqwest error: {0}")]
    ReqwestError(#[from] reqwest::Error),
    #[error("The response does not contain a Content-Type header")]
    MissingContentTypeHeadersError,
    #[error("Unsupported mime type: {0}")]
    UnsupportedMimeTypeError(String),
    #[error("Downloaded file does not look like an image")]
    NonImageBytesError,
    #[error("ToStr error: {0}")]
    ToStrError(#[from] reqwest::header::ToStrError),
    #[error("FromStr error: {0}")]
    FromStrError(#[from] mime::FromStrError),
    #[error("The page has no title")]
    MissingTitleError,
    #[error("The page is larger than {MAX_HTML_SIZE} bytes")]
    HtmlTooLargeError,
    #[error("The image is larger than {MAX_IMAGE_SIZE} bytes")]
    ImageTooLargeError,
    #[error("Redirect to a url that can't be previewed")]
    RedirectError,
    #[error("The url can't be previewed")]
    NotPreviewableError,
}

#[derive(Clone, PartialEq, Debug)]
pub struct PreviewContent {
    pub url: Option<String>,
    pub title: Option<String>,
    pub image: Option<Vec<u8>>,
    /// The MIME (e.g. `image/jpeg`) of `image`, detected from the response's
    /// `Content-Type` header or, failing that, sniffed from the image bytes themselves:
    pub image_content_type: Option<String>,
    pub description: Option<String>,
    pub date: Option<u64>,
}

/// Client for fetching previews: https only, no internal hosts, and a timeout.
pub fn client_builder() -> reqwest::ClientBuilder {
    // moho: through the same route as the account's other connections (Tor or a SOCKS5 proxy), never around it.
    let network = libsignal_service::push_service::network::for_this_thread();
    let builder = match network.proxy.as_deref() {
        Some(proxy) => reqwest::Client::builder().no_proxy().proxy(
            reqwest::Proxy::all(proxy).unwrap_or_else(|_| {
                reqwest::Proxy::all(libsignal_service::push_service::network::NOWHERE).expect("a fixed proxy address")
            }),
        ),
        None => reqwest::Client::builder(),
    };
    builder
        .user_agent("WhatsApp/2")
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if is_previewable(attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
}

/// Whether Signal clients accept a preview for `url`: https, and a public domain name.
fn is_previewable(url: &Url) -> bool {
    let Some(Host::Domain(domain)) = url.host() else {
        return false;
    };
    // "localhost." is localhost
    let domain = domain.trim_end_matches('.');
    url.scheme() == "https"
        && !RESERVED_DOMAINS
            .iter()
            .any(|reserved| domain == *reserved || domain.ends_with(&format!(".{reserved}")))
}

pub async fn generate_preview_from_url<T: IntoUrl + AsRef<str>>(
    url: T,
    client: &reqwest::Client,
) -> Result<PreviewContent, Error> {
    // receivers drop the preview if its url is not exactly as in the message body
    let link = url.as_ref().to_owned();
    let url = url.into_url()?;
    if !is_previewable(&url) {
        return Err(Error::NotPreviewableError);
    }

    let mut preview: PreviewContent = {
        let mut response = client.get(url.clone()).send().await?.error_for_status()?;
        // the client stopped at a redirect to an invalid url
        if response.status().is_redirection() {
            return Err(Error::RedirectError);
        }

        let headers = &response.headers();

        match headers.get(CONTENT_TYPE) {
            None => return Err(Error::MissingContentTypeHeadersError),
            Some(content_type) => {
                let content_type = content_type.to_str()?;
                let content_type = Mime::from_str(content_type)?;
                match (content_type.type_(), content_type.subtype()) {
                    (mime::TEXT, mime::HTML) => {
                        let mut html = Vec::new();
                        while let Some(chunk) = response.chunk().await? {
                            if html.len() + chunk.len() > MAX_HTML_SIZE {
                                return Err(Error::HtmlTooLargeError);
                            }
                            html.extend_from_slice(&chunk);
                        }
                        let html = String::from_utf8_lossy(&html);
                        fetch_preview_from_html(&html, &url, client).await
                    }
                    (mime::IMAGE, _) => {
                        // like Signal-Android, use the file name as title
                        let title = response
                            .url()
                            .path_segments()
                            .and_then(|mut segments| segments.next_back())
                            .filter(|name| !name.is_empty())
                            .map(|name| {
                                percent_encoding::percent_decode_str(name)
                                    .decode_utf8_lossy()
                                    .into_owned()
                            });
                        let (image, image_content_type) =
                            match fetch_image_from_response(response).await {
                                Ok((bytes, content_type)) => (Some(bytes), Some(content_type)),
                                Err(_) => (None, None),
                            };
                        PreviewContent {
                            url: None,
                            title,
                            image,
                            image_content_type,
                            description: None,
                            date: None,
                        }
                    }
                    _ => return Err(Error::UnsupportedMimeTypeError(content_type.to_string())),
                }
            }
        }
    };

    if preview.title.as_deref().is_none_or(str::is_empty) {
        return Err(Error::MissingTitleError);
    }
    preview.url = Some(link);
    Ok(preview)
}

/// The function builds a Preview primarily from the html's OG tags.
/// If some of these are unavailable, it resorts to other tags to fill the preview's fields.
/// The title is taken from the <title> tag (if available).
/// The description is taken from the  <meta name="description"> tag (if available).
pub async fn fetch_preview_from_html(
    html_doc: &str,
    url: &Url,
    client: &reqwest::Client,
) -> PreviewContent {
    let document = Html::parse_document(html_doc);
    let selector = Selector::parse("meta").unwrap();
    let mut preview = PreviewContent {
        url: None,
        title: None,
        image: None,
        image_content_type: None,
        description: None,
        date: None,
    };

    // like Signal-Android, the first date found in this order
    let date_properties = [
        "og:published_time",
        "article:published_time",
        "og:modified_time",
        "article:modified_time",
    ];
    let mut dates = [None; 4];

    for element in document.select(&selector) {
        // get description from <meta name="description">
        let name = element
            .value()
            .attr("name")
            .unwrap_or("")
            .trim()
            .to_string();

        if (name == "description") && (preview.description.is_none()) {
            if let Some(c) = element.value().attr("content") {
                preview.description = Some(c.to_string())
            }
        }

        // get fields from <meta property="og:"
        let property = element
            .value()
            .attr("property")
            .unwrap_or("")
            .to_ascii_lowercase();
        if let Some(i) = date_properties.iter().position(|p| *p == property) {
            dates[i] = timestamp_from_iso(element.value().attr("content").unwrap_or(""));
        }
        if let Some(capture) = property.strip_prefix("og:") {
            let field = capture.trim();
            let content = element.value().attr("content").unwrap_or("").to_string();
            match field {
                "title" => preview.title = Some(content),
                "image" if preview.image.is_none() => {
                    // og:image may be relative to the page
                    let image_url = url.join(&content).ok().filter(is_previewable);
                    if let Some(image_url) = image_url {
                        if let Ok((bytes, content_type)) =
                            fetch_image_from_url(image_url, client).await
                        {
                            preview.image = Some(bytes);
                            preview.image_content_type = Some(content_type);
                        }
                    }
                }
                "description" => preview.description = Some(content),
                _ => (),
            }
        }
    }
    preview.date = dates.into_iter().flatten().next();

    // in case no og:title was found, get the value from <title>
    if preview.title.is_none() | (preview.title == Some("".to_string())) {
        let selector = Selector::parse("title").unwrap();
        let title = document
            .select(&selector)
            .next()
            .map(|x| x.text().collect::<String>().trim().to_owned());
        preview.title = title;
    }
    preview
}

// Characters that end a link, as in Signal-Android's `Linkifier`
const LINK_END: &str = r#"\s\x{85}\x{A0}\x{1680}\x{2000}-\x{200D}\x{2028}\x{2029}\x{202A}-\x{202F}\x{205F}\x{2066}-\x{2069}\x{3000}\x{FEFF}<>"`|\\"#;

/// Bidi controls and zero-width characters, used to spoof links.
fn is_format_char(c: char) -> bool {
    matches!(c, '\u{200B}'..='\u{200D}' | '\u{FEFF}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

/// Finds links the way Signal-Android's `Linkifier` does: receivers drop a preview whose url
/// isn't exactly one of the links they find in the message body.
pub fn extract_urls_from_text_block(message: &str) -> Vec<String> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        // scheme, host (which can't contain an apostrophe), then path, query or fragment
        Regex::new(&format!(
            r"(?i)https?://[^{LINK_END}'/?#]+(?:[/?#][^{LINK_END}]*)?"
        ))
        .unwrap()
    });

    let mut urls = Vec::new();

    for m in RE.find_iter(message) {
        let before = message[..m.start()].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '@' || is_format_char(c))
        {
            continue;
        }

        // a comma or semicolon right before another link ends this one
        let mut candidate = m.as_str();
        if let Some((i, _)) = candidate.match_indices([',', ';']).find(|(i, _)| {
            let next = candidate[i + 1..].to_ascii_lowercase();
            ["http://", "https://", "www."]
                .iter()
                .any(|p| next.starts_with(p))
        }) {
            candidate = &candidate[..i];
        }

        // trim punctuation, and closing brackets without an opening one
        while let Some(last) = candidate.chars().next_back() {
            let rest = &candidate[..candidate.len() - last.len_utf8()];
            let opener = match last {
                '.' | ',' | '!' | '?' | ';' | ':' | '\'' | '"' | '`' | '|' | '<' | '>' => {
                    candidate = rest;
                    continue;
                }
                ')' => '(',
                ']' => '[',
                '}' => '{',
                _ => break,
            };
            if rest.matches(last).count() >= rest.matches(opener).count() {
                candidate = rest;
            } else {
                break;
            }
        }

        let after = message[m.start() + candidate.len()..].chars().next();
        if after.is_some_and(is_format_char) {
            continue;
        }

        if Url::parse(candidate).is_ok_and(|url| is_previewable(&url)) {
            urls.push(candidate.to_owned());
        }
    }
    urls
}

/// Returns `None` if `message` has no link to preview.
pub async fn generate_preview_from_message(
    message: &str,
    client: &reqwest::Client,
) -> Result<Option<PreviewContent>, Error> {
    // like Signal clients, only preview the first link
    let Some(url) = extract_urls_from_text_block(message).into_iter().next() else {
        return Ok(None);
    };

    generate_preview_from_url(url, client).await.map(Some)
}

/// Builds the `AttachmentSpec` and raw bytes needed to upload `content`'s image as an
/// attachment, e.g. via `Manager::upload_attachment`. Returns `None` if `content` has no image.
pub fn build_preview_image(content: &PreviewContent) -> Option<(AttachmentSpec, Vec<u8>)> {
    let image = content.image.as_ref()?;
    let content_type = content.image_content_type.as_ref()?;

    Some((
        AttachmentSpec {
            content_type: content_type.clone(),
            length: image.len(),
            file_name: None,
            preview: None,
            voice_note: None,
            borderless: None,
            width: None,
            height: None,
            caption: None,
            blur_hash: None,
        },
        image.clone(),
    ))
}

async fn fetch_image_from_url<T: IntoUrl>(
    url: T,
    client: &reqwest::Client,
) -> Result<(Vec<u8>, String), Error> {
    let response: reqwest::Response = client.get(url).send().await?.error_for_status()?;

    fetch_image_from_response(response).await
}

/// Fetches the body of `response`, returning its bytes along with the detected image MIME
/// (e.g. `image/jpeg`), preferring the response's own `Content-Type` header and falling
/// back to sniffing the file signature when that header is missing or not an image type.
async fn fetch_image_from_response(
    mut response: reqwest::Response,
) -> Result<(Vec<u8>, String), Error> {
    let header_content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Mime::from_str(value).ok())
        .filter(|mime| mime.type_() == mime::IMAGE)
        .map(|mime| mime.essence_str().to_string());

    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if bytes.len() + chunk.len() > MAX_IMAGE_SIZE {
            return Err(Error::ImageTooLargeError);
        }
        bytes.extend_from_slice(&chunk);
    }

    let Some(content_type) =
        header_content_type.or_else(|| sniff_image_mime_type(&bytes).map(str::to_string))
    else {
        return Err(Error::NonImageBytesError);
    };

    Ok((bytes, content_type))
}

fn timestamp_from_iso(datetime_str: &str) -> Option<u64> {
    let datetime = chrono::DateTime::parse_from_rfc3339(datetime_str).ok()?;
    let timestamp = chrono::DateTime::timestamp_millis(&datetime);
    u64::try_from(timestamp)
        .ok()
        .filter(|&timestamp| timestamp > 0)
}

/// Detects the image MIME essence (e.g. `image/jpeg`) from a file's magic bytes.
fn sniff_image_mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}
