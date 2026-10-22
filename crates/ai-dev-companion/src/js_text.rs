//! JavaScript-compatible text helpers. The guards were written against JS
//! semantics (`String.prototype.trim`, `\s`), and the two differ from Rust's
//! `str::trim` / `char::is_whitespace` (JS counts U+FEFF and not U+0085), so
//! the exact JS set is reproduced here to keep every allow/deny decision the
//! same.

/// True for exactly the characters JS treats as WhiteSpace or LineTerminator
/// (what `trim()` strips and what `\s` matches).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'
            | '\u{000A}'
            | '\u{000B}'
            | '\u{000C}'
            | '\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_like_js() {
        assert_eq!(js_trim("  a b \t\n"), "a b");
        assert_eq!(js_trim("\u{FEFF}x\u{FEFF}"), "x");
        assert_eq!(js_trim("\u{3000}x\u{00A0}"), "x");
    }

    #[test]
    fn next_line_is_not_js_space() {
        assert_eq!(js_trim("\u{0085}x"), "\u{0085}x");
        assert!(!is_js_space('\u{0085}'));
        assert!(!is_js_space('\u{200B}'));
    }
}

/// JS truthiness of a JSON value (`undefined`/absent is falsy).
pub fn js_truthy(v: Option<&serde_json::Value>) -> bool {
    use serde_json::Value::*;
    match v {
        None | Some(Null) | Some(Bool(false)) => false,
        Some(String(s)) => !s.is_empty(),
        Some(Number(n)) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Some(_) => true,
    }
}

/// `JSON.stringify(v)` as it appears inside messages: `undefined` for absent.
pub fn js_stringify(v: Option<&serde_json::Value>) -> String {
    v.map_or_else(|| "undefined".to_string(), serde_json::Value::to_string)
}

#[cfg(test)]
mod value_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truthiness_and_stringify_match_js() {
        for falsy in [json!(null), json!(false), json!(""), json!(0), json!(0.0)] {
            assert!(!js_truthy(Some(&falsy)), "{falsy}");
        }
        for truthy in [json!(true), json!("a"), json!(1), json!([]), json!({})] {
            assert!(js_truthy(Some(&truthy)), "{truthy}");
        }
        assert!(!js_truthy(None));
        assert_eq!(js_stringify(None), "undefined");
        assert_eq!(js_stringify(Some(&json!("x"))), "\"x\"");
        assert_eq!(js_stringify(Some(&json!(null))), "null");
    }
}

/// `String.prototype.length`: UTF-16 code units.
pub fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

/// `s.slice(0, n)` on UTF-16 units. A cut through a surrogate pair drops the
/// half pair instead of producing an invalid string.
pub fn slice_utf16(s: &str, n: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > n {
            break;
        }
        out.push(c);
    }
    out
}

/// `s.slice(-n)` on UTF-16 units: the last `n`. A cut through a surrogate pair drops the half pair.
pub fn slice_utf16_end(s: &str, n: usize) -> String {
    let total = utf16_len(s);
    if total <= n {
        return s.to_string();
    }
    let mut skipped = 0;
    let mut out = String::new();
    for c in s.chars() {
        if skipped >= total - n {
            out.push(c);
        } else {
            skipped += c.len_utf16();
        }
    }
    out
}

/// `String(value)` for a JSON value (arrays join with commas and objects are
/// `[object Object]`, as in JS; numbers print without a trailing `.0`).
pub fn js_to_string(v: &serde_json::Value) -> String {
    use serde_json::Value::*;
    match v {
        Null => "null".into(),
        Bool(b) => b.to_string(),
        String(s) => s.clone(),
        Number(n) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f.abs() < 1e21 => format!("{}", f as i128),
            Some(f) => f.to_string(),
            None => n.to_string(),
        },
        Array(a) => a.iter().map(|x| if x.is_null() { std::string::String::new() } else { js_to_string(x) }).collect::<Vec<_>>().join(","),
        Object(_) => "[object Object]".into(),
    }
}

/// Primary collation weight of an ASCII character in ICU's root collation
/// (what `localeCompare` uses): whitespace, then punctuation in this order,
/// then digits, then letters (case-insensitive at this level).
fn primary_weight(c: char) -> u32 {
    const PUNCT: &str = "_-,;:!?.'\"()[]{}@*/\\&#%`^+<=>|~$";
    if c.is_whitespace() {
        1
    } else if let Some(i) = PUNCT.find(c) {
        10 + i as u32
    } else if c.is_ascii_digit() {
        100 + (c as u32 - '0' as u32)
    } else if c.is_ascii_alphabetic() {
        200 + (c.to_ascii_lowercase() as u32 - 'a' as u32)
    } else {
        1000 + c as u32
    }
}

/// `a.localeCompare(b)` for the text this service sorts (paths, names): ASCII
/// follows ICU's root collation (punctuation < digits < letters, letters
/// compared ignoring case first, then lowercase before uppercase); anything
/// else falls back to code point order after the ASCII weights.
pub fn locale_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let primary = a.chars().map(primary_weight).cmp(b.chars().map(primary_weight));
    if primary != std::cmp::Ordering::Equal {
        return primary;
    }
    // Same letters ignoring case: lowercase sorts before uppercase.
    a.chars().zip(b.chars()).find(|(x, y)| x != y).map_or(std::cmp::Ordering::Equal, |(x, _)| {
        if x.is_lowercase() { std::cmp::Ordering::Less } else { std::cmp::Ordering::Greater }
    })
}

#[cfg(test)]
mod collation_tests {
    use super::*;
    use serde_json::json;
    use std::cmp::Ordering::{Equal, Greater, Less};

    #[test]
    fn locale_compare_follows_icu_for_ascii() {
        for (a, b, want) in [
            ("a", "B", Less),
            ("a", "A", Less),
            ("A", "b", Less),
            ("a_b", "ab", Less),
            ("a_b", "a-b", Less),
            ("a.ts", "a/b", Less),
            ("file10", "file2", Less),
            ("src/app/billing.ts", "src/app/login.ts", Less),
            ("x", "x", Equal),
            ("abc", "ab", Greater),
            ("1", "a", Less),
        ] {
            assert_eq!(locale_compare(a, b), want, "{a} vs {b}");
        }
    }

    #[test]
    fn utf16_helpers() {
        assert_eq!(utf16_len("a\u{1F600}"), 3);
        assert_eq!(slice_utf16("abcdef", 3), "abc");
        assert_eq!(slice_utf16("a\u{1F600}b", 2), "a", "a half pair is dropped");
        assert_eq!(slice_utf16("a\u{1F600}b", 3), "a\u{1F600}");
    }

    #[test]
    fn js_to_string_matches_js() {
        assert_eq!(js_to_string(&json!(42)), "42");
        assert_eq!(js_to_string(&json!(1.5)), "1.5");
        assert_eq!(js_to_string(&json!(null)), "null");
        assert_eq!(js_to_string(&json!([1, null, "a"])), "1,,a");
        assert_eq!(js_to_string(&json!({})), "[object Object]");
        assert_eq!(js_to_string(&json!(true)), "true");
    }
}

/// The JS `\s` set as the inside of a regex character class, for patterns
/// that must treat whitespace exactly as the Node code did (U+FEFF is JS
/// whitespace and U+0085 is not, the opposite of Rust's `\s`).
pub const JS_WS_CLASS: &str = r"\t\n\x0B\f\r \x{A0}\x{1680}\x{2000}-\x{200A}\x{2028}\x{2029}\x{202F}\x{205F}\x{3000}\x{FEFF}";

#[cfg(test)]
mod slice_end_tests {
    use super::*;

    #[test]
    fn slice_utf16_end_keeps_the_last_units_and_drops_a_split_pair() {
        assert_eq!(slice_utf16_end("abcdef", 3), "def");
        assert_eq!(slice_utf16_end("abc", 10), "abc");
        assert_eq!(slice_utf16_end("", 3), "");
        assert_eq!(slice_utf16_end("a\u{1F600}b", 2), "b", "the cut falls inside the emoji");
        assert_eq!(slice_utf16_end("a\u{1F600}b", 3), "\u{1F600}b");
    }
}
