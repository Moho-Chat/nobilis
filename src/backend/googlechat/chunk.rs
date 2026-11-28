//! BrowserChannel's framing: each chunk is `<length>\n<payload>`, where the length counts UTF-16 code units - what
//! JavaScript's `string.length` would say - not bytes and not characters.
//!
//! The bytes are kept as bytes until a whole chunk is there: a character split across two reads must not be decoded
//! half at a time, or the counting goes wrong for the rest of the stream.

#[derive(Default)]
pub struct ChunkParser {
    buf: Vec<u8>,
}

impl ChunkParser {
    /// Takes what was read and returns every chunk now complete; the rest waits for the next read.
    pub fn feed(&mut self, data: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(data);
        let mut chunks = Vec::new();
        loop {
            let digits = self.buf.iter().take_while(|b| b.is_ascii_digit()).count();
            if digits == self.buf.len() {
                // Only digits so far, or nothing: the length line is not finished yet.
                return chunks;
            }
            if digits == 0 || self.buf[digits] != b'\n' {
                // Not a length at all. Drop a byte and look again, so a stream that went wrong recovers rather than
                // sticking.
                self.buf.remove(0);
                continue;
            }
            let Ok(want) = std::str::from_utf8(&self.buf[..digits]).unwrap_or("").parse::<usize>() else {
                self.buf.drain(..=digits);
                continue;
            };
            let body = &self.buf[digits + 1..];
            let Some(len) = utf16_prefix(body, want) else { return chunks };
            chunks.push(String::from_utf8_lossy(&body[..len]).into_owned());
            self.buf.drain(..digits + 1 + len);
        }
    }
}

/// How many bytes of `bytes` make `want` UTF-16 units, or `None` if there are not that many yet. A byte that is not
/// UTF-8 counts as one unit, as a browser's replacement character would.
fn utf16_prefix(bytes: &[u8], want: usize) -> Option<usize> {
    let (mut units, mut i) = (0usize, 0usize);
    while units < want {
        if i >= bytes.len() {
            return None;
        }
        let lead = bytes[i];
        let width = match lead {
            0x00..=0x7f => 1,
            0xc0..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf7 => 4,
            _ => 1,
        };
        if i + width > bytes.len() {
            return None;
        }
        match std::str::from_utf8(&bytes[i..i + width]) {
            Ok(s) => {
                units += s.chars().map(char::len_utf16).sum::<usize>();
                i += width;
            }
            Err(_) => {
                units += 1;
                i += 1;
            }
        }
    }
    Some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_chunks_counted_in_utf16_units() {
        let mut p = ChunkParser::default();
        // "é" is one unit and two bytes; "😀" is two units and four bytes.
        let a = "[1,\"é\"]";
        let b = "[2,\"😀\"]";
        let stream = format!("{}\n{}{}\n{}", a.encode_utf16().count(), a, b.encode_utf16().count(), b);
        assert_eq!(p.feed(stream.as_bytes()), vec![a.to_string(), b.to_string()]);
    }

    #[test]
    fn waits_for_a_character_split_across_reads() {
        let mut p = ChunkParser::default();
        let payload = "[\"😀😀\"]";
        let stream = format!("{}\n{}", payload.encode_utf16().count(), payload);
        let bytes = stream.as_bytes();
        let cut = stream.find('😀').unwrap() + 2;
        assert!(p.feed(&bytes[..cut]).is_empty());
        assert_eq!(p.feed(&bytes[cut..]), vec![payload.to_string()]);
    }

    #[test]
    fn waits_for_the_length_line_and_recovers_from_junk() {
        let mut p = ChunkParser::default();
        assert!(p.feed(b"1").is_empty());
        assert_eq!(p.feed(b"0\n[[\"noop\"]]"), vec!["[[\"noop\"]]".to_string()]);
        assert_eq!(p.feed(b"xx2\n[]"), vec!["[]".to_string()]);
    }
}
