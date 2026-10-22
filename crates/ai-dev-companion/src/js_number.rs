//! JavaScript `Number(value)` coercion over JSON values, for the places the
//! Node code accepted a number or numeric text interchangeably.

use serde_json::Value;

/// `Number(x)`: numbers as is, numeric text (trimmed; empty is 0; hex, octal
/// and binary prefixes; `Infinity`), `null` is 0, booleans 0/1, anything else
/// (including `undefined`/absent, arrays and objects) NaN. Arrays and objects
/// are NaN here although JS would stringify them first; no caller passes them.
pub fn js_number(v: Option<&Value>) -> f64 {
    match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(f64::NAN),
        Some(Value::Null) => 0.0,
        Some(Value::Bool(b)) => f64::from(u8::from(*b)),
        Some(Value::String(s)) => parse_numeric_string(s),
        _ => f64::NAN,
    }
}

fn parse_numeric_string(s: &str) -> f64 {
    let t = crate::js_text::js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    for (prefix, radix) in [("0x", 16), ("0X", 16), ("0o", 8), ("0O", 8), ("0b", 2), ("0B", 2)] {
        if let Some(digits) = t.strip_prefix(prefix) {
            return match u64::from_str_radix(digits, radix) {
                Ok(n) if !digits.is_empty() => n as f64,
                _ => f64::NAN,
            };
        }
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust's parser accepts "inf", "nan" and "infinity" spellings JS doesn't.
    if t.chars().any(|c| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// A JS number as JSON: integral values print without a fraction, NaN and
/// infinities become `null` (as `JSON.stringify` does).
pub fn json_number(n: f64) -> Value {
    if !n.is_finite() {
        Value::Null
    } else if n.fract() == 0.0 && n.abs() < 9.0e15 {
        Value::from(n as i64)
    } else {
        serde_json::Number::from_f64(n).map_or(Value::Null, Value::Number)
    }
}

/// `Number.isSafeInteger`.
pub fn is_safe_integer(n: f64) -> bool {
    n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn coerces_like_js_number() {
        assert_eq!(js_number(Some(&json!(12))), 12.0);
        assert_eq!(js_number(Some(&json!("12"))), 12.0);
        assert_eq!(js_number(Some(&json!(" 3 "))), 3.0);
        assert_eq!(js_number(Some(&json!(""))), 0.0);
        assert_eq!(js_number(Some(&json!(null))), 0.0);
        assert_eq!(js_number(Some(&json!(true))), 1.0);
        assert_eq!(js_number(Some(&json!("0x10"))), 16.0);
        assert_eq!(js_number(Some(&json!("1e3"))), 1000.0);
        assert_eq!(js_number(Some(&json!("-Infinity"))), f64::NEG_INFINITY);
        for nan in [json!("abc"), json!("inf"), json!("nan"), json!("0x"), json!("1 2"), json!([]), json!({})] {
            assert!(js_number(Some(&nan)).is_nan(), "{nan}");
        }
        assert!(js_number(None).is_nan());
    }

    #[test]
    fn safe_integers() {
        assert!(is_safe_integer(5.0));
        assert!(is_safe_integer(-0.0));
        assert!(!is_safe_integer(1.5));
        assert!(!is_safe_integer(f64::NAN));
        assert!(!is_safe_integer(f64::INFINITY));
        assert!(!is_safe_integer(9_007_199_254_740_992.0));
    }
}
