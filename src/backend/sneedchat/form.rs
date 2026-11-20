//! Reading attributes off the site's markup.
//!
//! What is left of the form scraping the password sign-in used (ported from
//! sneedchat-rs's `auth/form.rs`): one attribute from one tag, which the
//! upload host's page is read with (see postimg.rs).

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

/// Undo HTML escaping applied to attribute values, so a value round-trips.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&amp;", "&").replace("&quot;", "\"").replace("&#039;", "'").replace("&lt;", "<").replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribute_values_are_entity_decoded() {
        assert_eq!(attr(r#"<input name="r" value="https://h/?a=1&amp;b=2" />"#, "value").as_deref(), Some("https://h/?a=1&b=2"));
    }

    #[test]
    fn handles_single_quotes_and_unquoted_values() {
        assert_eq!(attr("<input name='a' value='1'>", "value").as_deref(), Some("1"));
        assert_eq!(attr("<input name=b value=2>", "value").as_deref(), Some("2"));
    }

    #[test]
    fn a_name_inside_another_is_not_it() {
        assert_eq!(attr(r#"<div data-name="x" name="y">"#, "name").as_deref(), Some("y"));
    }
}
