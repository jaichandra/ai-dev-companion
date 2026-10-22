//! `encodeURIComponent`: percent-encodes everything except the unreserved
//! set `A-Z a-z 0-9 - _ . ! ~ * ' ( )`.

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

const KEEP: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'_').remove(b'.').remove(b'!').remove(b'~').remove(b'*').remove(b'\'').remove(b'(').remove(b')');

pub fn encode_uri_component(s: &str) -> String {
    utf8_percent_encode(s, KEEP).to_string()
}

/// `encodeURI`: as `encodeURIComponent`, but the characters that structure a URI
/// (`; , / ? : @ & = + $ #`) are kept too.
const KEEP_URI: &AsciiSet = &KEEP.remove(b';').remove(b',').remove(b'/').remove(b'?').remove(b':').remove(b'@').remove(b'&').remove(b'=').remove(b'+').remove(b'$').remove(b'#');

pub fn encode_uri(s: &str) -> String {
    utf8_percent_encode(s, KEEP_URI).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_encode_uri_component() {
        assert_eq!(encode_uri_component("a b#c"), "a%20b%23c");
        assert_eq!(encode_uri_component("Review & don't edit"), "Review%20%26%20don't%20edit");
        assert_eq!(encode_uri_component("-_.!~*'()"), "-_.!~*'()");
        assert_eq!(encode_uri_component("/?:@&=+$,;"), "%2F%3F%3A%40%26%3D%2B%24%2C%3B");
        assert_eq!(encode_uri_component("\u{e9}\u{1F600}"), "%C3%A9%F0%9F%98%80");
        assert_eq!(encode_uri_component(""), "");
    }

    #[test]
    fn encode_uri_keeps_the_structure_of_a_uri() {
        assert_eq!(super::encode_uri("/my repos/pr-review?a=b&c#d"), "/my%20repos/pr-review?a=b&c#d");
        assert_eq!(super::encode_uri(";,/?:@&=+$#"), ";,/?:@&=+$#");
        assert_eq!(super::encode_uri("\u{e9}[]"), "%C3%A9%5B%5D");
    }
}

/// `decodeURIComponent`: strict, unlike lenient percent-decoding. A `%` not
/// followed by two hex digits, or bytes that aren't valid UTF-8, gives `None`.
pub fn decode_uri_component(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = b.get(i + 1..i + 3)?;
            let byte = u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?;
            out.push(byte);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod decode_tests {
    use super::*;

    #[test]
    fn decodes_strictly() {
        assert_eq!(decode_uri_component("a%20b%C3%A9").unwrap(), "a b\u{e9}");
        assert_eq!(decode_uri_component("plain").unwrap(), "plain");
        assert_eq!(decode_uri_component("a+b").unwrap(), "a+b");
        for bad in ["%", "%2", "%zz", "%C3", "%FF", "100%"] {
            assert!(decode_uri_component(bad).is_none(), "{bad}");
        }
    }
}
