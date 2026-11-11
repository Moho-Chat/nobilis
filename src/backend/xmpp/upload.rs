//! Sending files (XEP-0363): ask the server's upload service for a place to put one, put it there over HTTPS, and send the
//! address in a message, in the forms the common clients read (the plain address as the body, XEP-0066 for older clients,
//! XEP-0447 with XEP-0446 for those that show what the file is before fetching it).

use super::dataform;
use super::session::Handle;
use super::xml::Attr;
use super::ns;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;

/// The media type of a file from its name, for what the upload says it is and the message describes.
pub fn content_type_of(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "avif" => "image/avif",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mkv" => "video/x-matroska",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// A slot: where to put a file, and where it will then be.
#[derive(Debug, Clone, PartialEq)]
pub struct Slot {
    pub put: String,
    pub get: String,
    /// The headers the service wants on the PUT. Only the ones the specification allows are kept.
    pub headers: Vec<(String, String)>,
}

pub fn parse_slot(slot: &Element) -> Option<Slot> {
    let put = slot.children().find(|c| c.name() == "put")?;
    let get = slot.children().find(|c| c.name() == "get")?;
    let headers = put
        .children()
        .filter(|h| h.name() == "header")
        .filter_map(|h| Some((h.attr("name")?.to_string(), h.text().replace(['\r', '\n'], ""))))
        .filter(|(n, _)| matches!(n.to_ascii_lowercase().as_str(), "authorization" | "cookie" | "expires"))
        .collect();
    Some(Slot { put: put.attr("url")?.to_string(), get: get.attr("url")?.to_string(), headers })
}

/// Finds the account's upload service, once.
pub async fn service(handle: &Handle) -> Result<(Jid, Option<u64>), String> {
    if let Some(found) = handle.session().upload.clone() {
        return Ok(found);
    }
    let domain: Jid = handle.own().domain().as_str().parse().map_err(|_| "the server's address is not usable".to_string())?;
    let items = handle.request(Some(domain), false, Element::builder("query", ns::DISCO_ITEMS).build()).await.map_err(|e| e.words())?;
    for item in items.iter().flat_map(|q| q.children()).filter(|c| c.name() == "item") {
        let Some(jid) = item.attr("jid").and_then(|j| j.parse::<Jid>().ok()) else { continue };
        let Ok(Some(info)) = handle.request(Some(jid.clone()), false, Element::builder("query", ns::DISCO_INFO).build()).await else { continue };
        if !info.children().any(|c| c.name() == "feature" && c.attr("var") == Some(ns::HTTP_UPLOAD)) {
            continue;
        }
        let max = info.children().find(|c| c.is("x", ns::DATA)).map(dataform::parse).and_then(|f| f.fields.into_iter().find(|f| f.var == "max-file-size")).and_then(|f| f.values.first().and_then(|v| v.parse::<u64>().ok()));
        handle.session().upload = Some((jid.clone(), max));
        return Ok((jid, max));
    }
    Err("this server has no place to put files".to_string())
}

/// Asks for a slot for a file.
pub async fn request_slot(handle: &Handle, service: &Jid, name: &str, size: u64, content_type: &str) -> Result<Slot, String> {
    let request = Element::builder("request", ns::HTTP_UPLOAD).at("filename", name).at("size", size.to_string()).at("content-type", content_type).build();
    let answer = handle.request(Some(service.clone()), false, request).await.map_err(|e| match e.condition.as_str() {
        "NotAcceptable" | "not-acceptable" => format!("The server will not take a file this big{}", if e.text.is_empty() { String::new() } else { format!(": {}", e.text) }),
        _ => e.words(),
    })?;
    answer.as_ref().and_then(parse_slot).ok_or_else(|| "the server's answer about where to put the file was not understood".to_string())
}

/// The elements that describe a shared file in a message: XEP-0066 for old clients and XEP-0447 for the rest.
pub fn file_payloads(url: &str, name: &str, content_type: &str, size: u64, dims: Option<(u32, u32)>, description: Option<&str>) -> Vec<Element> {
    let mut file = Element::builder("file", ns::FILE_METADATA).build();
    file.append_child(Element::builder("media-type", ns::FILE_METADATA).append(content_type).build());
    file.append_child(Element::builder("name", ns::FILE_METADATA).append(name).build());
    file.append_child(Element::builder("size", ns::FILE_METADATA).append(size.to_string()).build());
    if let Some((w, h)) = dims {
        file.append_child(Element::builder("dimensions", ns::FILE_METADATA).at("width", w.to_string()).at("height", h.to_string()).build());
    }
    if let Some(d) = description {
        file.append_child(Element::builder("desc", ns::FILE_METADATA).append(d).build());
    }
    let sources = Element::builder("sources", ns::SFS).append(Element::builder("url-data", ns::URL_DATA).at("target", url).build()).build();
    let sharing = Element::builder("file-sharing", ns::SFS).at("disposition", "inline").append(file).append(sources).build();
    let oob = Element::builder("x", ns::OOB).append(Element::builder("url", ns::OOB).append(url).build()).build();
    vec![oob, sharing]
}

/// Puts a file at a slot.
pub async fn put(account_id: &str, ca_pem: &str, slot: &Slot, path: &str, content_type: &str, size: u64) -> Result<(), String> {
    if !slot.put.starts_with("https://") {
        return Err("the server asked for the file to be sent without encryption, which moho does not do".to_string());
    }
    let file = tokio::fs::File::open(path).await.map_err(|e| format!("{path}: {e}"))?;
    let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
    let client = super::media::http_client(account_id, ca_pem);
    let mut request = client.put(&slot.put).header("Content-Type", content_type).header("Content-Length", size).body(body);
    for (n, v) in &slot.headers {
        request = request.header(n.as_str(), v.as_str());
    }
    let response = request.timeout(std::time::Duration::from_secs(3600)).send().await.map_err(|e| format!("sending the file: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("the server refused the file ({})", response.status()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn a_slot_names_where_to_put_and_where_to_get_and_keeps_only_the_headers_that_may_be_sent() {
        let slot = Element::from_str("<slot xmlns='urn:xmpp:http:upload:0'><put url='https://up.example.org/put/abc/cat.png'><header name='Authorization'>Basic xyz</header><header name='Cookie'>a=b</header><header name='X-Evil'>1</header><header name='Expires'>never\r\nInjected: 1</header></put><get url='https://up.example.org/get/abc/cat.png'/></slot>").unwrap();
        let s = parse_slot(&slot).unwrap();
        assert_eq!((s.put.as_str(), s.get.as_str()), ("https://up.example.org/put/abc/cat.png", "https://up.example.org/get/abc/cat.png"));
        let names: Vec<&str> = s.headers.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["Authorization", "Cookie", "Expires"]);
        assert!(!s.headers[2].1.contains('\n') && !s.headers[2].1.contains('\r'), "a header value cannot carry another header");
    }

    #[test]
    fn a_slot_without_both_addresses_is_not_one() {
        assert!(parse_slot(&Element::from_str("<slot xmlns='urn:xmpp:http:upload:0'><get url='https://x'/></slot>").unwrap()).is_none());
    }

    #[test]
    fn a_shared_file_is_described_the_way_other_clients_read() {
        let p = file_payloads("https://up.example.org/a.png", "a.png", "image/png", 12, Some((3, 4)), None);
        assert!(p[0].is("x", ns::OOB));
        let sharing = &p[1];
        assert!(sharing.is("file-sharing", ns::SFS));
        let file = sharing.children().find(|c| c.name() == "file").unwrap();
        assert_eq!(file.children().find(|c| c.name() == "dimensions").and_then(|d| d.attr("width")), Some("3"));
    }

    #[test]
    fn a_files_type_comes_from_its_name() {
        assert_eq!(content_type_of("A.PNG"), "image/png");
        assert_eq!(content_type_of("x.opus"), "audio/ogg");
        assert_eq!(content_type_of("noext"), "application/octet-stream");
    }
}
