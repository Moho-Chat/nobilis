//! Minimal HTML form scraping.
//!
//! Ported from sockchat-rs's `auth/form.rs`
//! (<https://gitgud.io/jcmoon/sockchat-rs>). XenForo forms carry CSRF
//! tokens and bookkeeping fields that must be echoed back verbatim. Rather
//! than hardcode a field list - which would break the moment the forum
//! software is upgraded - the form is read and resubmitted with only the
//! fields we own overridden.

/// Extract the slice of `html` between a `<form>` whose tag contains
/// `marker` and its closing tag.
pub fn form_section<'a>(html: &'a str, marker: &str) -> Option<&'a str> {
    let mut search_from = 0;
    while let Some(rel) = html[search_from..].find("<form") {
        let start = search_from + rel;
        let tag_end = html[start..].find('>').map(|i| start + i)?;
        let tag = &html[start..tag_end];
        search_from = tag_end;

        if !tag.contains(marker) {
            continue;
        }
        // Forms don't nest, so the next closing tag is ours.
        let end = html[tag_end..].find("</form>").map(|i| tag_end + i).unwrap_or(html.len());
        return Some(&html[start..end]);
    }
    None
}

/// Read every `name`/`value` pair from the `<input>` elements in `html`.
///
/// Unchecked checkboxes and radios are skipped, matching what a browser
/// would submit; submit buttons are skipped since XenForo doesn't need them.
pub fn inputs(html: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = html;

    while let Some(i) = rest.find("<input") {
        let after = &rest[i..];
        let end = match after.find('>') {
            Some(e) => e,
            None => break,
        };
        let tag = &after[..end];
        rest = &after[end..];

        let Some(name) = attr(tag, "name") else { continue };
        let kind = attr(tag, "type").unwrap_or_else(|| "text".into());
        match kind.as_str() {
            "submit" | "button" | "image" | "file" | "reset" => continue,
            // A browser only submits these when they're checked.
            "checkbox" | "radio" if !tag.contains("checked") => continue,
            _ => {}
        }
        out.push((name, attr(tag, "value").unwrap_or_default()));
    }
    out
}

/// Read one attribute from a single tag, handling both quote styles.
pub fn attr(tag: &str, name: &str) -> Option<String> {
    let mut offset = 0;
    while let Some(rel) = tag[offset..].find(name) {
        let start = offset + rel;
        let end = start + name.len();
        offset = end;

        let clean_start = tag[..start].chars().next_back().is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_');
        if !clean_start {
            continue;
        }
        let Some(value) = tag[end..].trim_start().strip_prefix('=') else { continue };
        let value = value.trim_start();
        let extracted = if let Some(v) = value.strip_prefix('"') {
            v.split('"').next()
        } else if let Some(v) = value.strip_prefix('\'') {
            v.split('\'').next()
        } else {
            value.split([' ', '>', '\n', '\t', '\r', '/']).next()
        };
        return extracted.map(decode_entities);
    }
    None
}

/// Undo HTML escaping applied to attribute values, so a token round-trips.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#039;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

/// Replace or append a field, preserving the original ordering where
/// possible.
pub fn set(fields: &mut Vec<(String, String)>, name: &str, value: impl Into<String>) {
    let value = value.into();
    if let Some(slot) = fields.iter_mut().find(|(n, _)| n == name) {
        slot.1 = value;
    } else {
        fields.push((name.to_string(), value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN_PAGE: &str = r#"
<form action="/search/search" method="post" data-xf-init="quick-search">
  <input type="hidden" name="order" value="replies" />
  <input type="hidden" name="search_type" value="post" />
  <input type="hidden" name="_xfToken" value="1785255424,474390e89b0fcdcc197c32b55a7fd30c" />
</form>
<form action="/login/login" method="post" class="block"
    data-xf-init="">
  <input type="hidden" name="_xfToken" value="1785255424,474390e89b0fcdcc197c32b55a7fd30c" />
  <input type="text" class="input" name="login" autofocus="autofocus" autocomplete="username" id="_xfUid-1" />
  <input type="password" name="password" value="" />
  <label><input type="checkbox" name="remember" value="1" checked="checked" /></label>
  <input type="hidden" name="_xfRedirect" value="https://example.onion/" />
  <input type="submit" name="go" value="Log in" />
</form>
"#;

    #[test]
    fn picks_the_right_form_out_of_several() {
        let section = form_section(LOGIN_PAGE, "/login/login").unwrap();
        assert!(section.contains("name=\"login\""));
        assert!(!section.contains("search_type"));

        let fields = inputs(section);
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["_xfToken", "login", "password", "remember", "_xfRedirect"]);
    }

    #[test]
    fn reads_the_csrf_token_verbatim() {
        let fields = inputs(form_section(LOGIN_PAGE, "/login/login").unwrap());
        let token = &fields.iter().find(|(n, _)| n == "_xfToken").unwrap().1;
        assert_eq!(token, "1785255424,474390e89b0fcdcc197c32b55a7fd30c");
    }

    #[test]
    fn checked_boxes_are_submitted_and_unchecked_ones_are_not() {
        let html = r#"<form action="/x">
            <input type="checkbox" name="on" value="1" checked="checked" />
            <input type="checkbox" name="off" value="1" />
            <input type="radio" name="pick" value="a" checked />
        </form>"#;
        let fields = inputs(form_section(html, "/x").unwrap());
        let names: Vec<&str> = fields.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["on", "pick"]);
    }

    #[test]
    fn missing_form_is_reported_rather_than_guessed() {
        assert!(form_section(LOGIN_PAGE, "/login/two-step").is_none());
        assert!(form_section("", "/anything").is_none());
    }

    #[test]
    fn attribute_values_are_entity_decoded() {
        let html = r#"<form action="/x"><input name="r" value="https://h/?a=1&amp;b=2" /></form>"#;
        let fields = inputs(form_section(html, "/x").unwrap());
        assert_eq!(fields[0].1, "https://h/?a=1&b=2");
    }

    #[test]
    fn handles_single_quotes_and_unquoted_values() {
        let html = "<form action='/x'><input name='a' value='1'><input name=b value=2></form>";
        let fields = inputs(form_section(html, "/x").unwrap());
        assert_eq!(fields, vec![("a".into(), "1".into()), ("b".into(), "2".into())]);
    }

    #[test]
    fn set_overrides_in_place_and_appends_when_absent() {
        let mut fields = vec![("a".to_string(), "1".to_string())];
        set(&mut fields, "a", "2");
        set(&mut fields, "b", "3");
        assert_eq!(fields, vec![("a".into(), "2".into()), ("b".into(), "3".into())]);
    }

    #[test]
    fn value_less_inputs_become_empty_strings() {
        let html = r#"<form action="/x"><input type="password" name="password" /></form>"#;
        let fields = inputs(form_section(html, "/x").unwrap());
        assert_eq!(fields, vec![("password".to_string(), String::new())]);
    }
}
