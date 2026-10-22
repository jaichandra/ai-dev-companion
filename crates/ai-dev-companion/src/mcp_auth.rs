//! The /mcp route's own trust boundary (port of `core/mcp-auth.js`): a bearer
//! token of its own (`mcp.token` in the credential store), never the
//! extension's shared secret, so a leaked `~/.claude.json` (where Claude Code
//! keeps the token it was registered with) can drive the MCP tools but none of
//! the extension routes.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use subtle::ConstantTimeEq;

const TOKEN_BYTES: usize = 32;

/// A fresh MCP token: 32 random bytes as base64url (43 chars, no padding),
/// safe to paste into a JSON config or an HTTP header as-is.
pub fn generate_mcp_token() -> String {
    generate_mcp_token_with(|buf| getrandom::fill(buf).expect("the OS random source is unavailable"))
}

/// As [`generate_mcp_token`], drawing the bytes from `fill` (for tests).
pub fn generate_mcp_token_with(fill: impl FnOnce(&mut [u8])) -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `^Bearer ([^ ]+)$` with the scheme case-insensitive: exactly one space, then
/// a token of any characters but a space.
fn bearer_token(header: &str) -> Option<&str> {
    let (scheme, rest) = header.split_at_checked(7)?;
    let scheme = scheme.as_bytes();
    if !scheme[..6].eq_ignore_ascii_case(b"bearer") || scheme[6] != b' ' {
        return None;
    }
    (!rest.is_empty() && !rest.contains(' ')).then_some(rest)
}

/// Whether an Authorization header carries exactly `Bearer <expected_token>`
/// (scheme case-insensitive, exactly one space). Compared in constant time;
/// the length is checked first (leaking only the length, which is fixed
/// anyway). Never panics, whatever the header; false whenever no token is
/// expected, so an unset token can never be "matched" by an empty one.
pub fn bearer_matches(authorization_header: Option<&str>, expected_token: Option<&str>) -> bool {
    let (Some(header), Some(expected)) = (authorization_header, expected_token) else { return false };
    if expected.is_empty() {
        return false;
    }
    let Some(given) = bearer_token(header) else { return false };
    given.len() == expected.len() && bool::from(given.as_bytes().ct_eq(expected.as_bytes()))
}

/// DNS-rebinding guard: the service only listens on 127.0.0.1, but a web page
/// can still point its own hostname at that address, and its requests then
/// carry that hostname in Host. Only the two names a local MCP client actually
/// uses, on this service's own port, are let through.
pub fn host_allowed(host_header: Option<&str>, port: u16) -> bool {
    match host_header {
        Some(h) => h == format!("127.0.0.1:{port}") || h == format!("localhost:{port}"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQ";
    fn matches(h: &str, t: &str) -> bool {
        bearer_matches(Some(h), Some(t))
    }

    #[test]
    fn bearer_matches_accepts_the_exact_token_and_a_lowercase_scheme() {
        assert!(matches(&format!("Bearer {TOKEN}"), TOKEN));
        assert!(matches(&format!("bearer {TOKEN}"), TOKEN));
        assert!(matches(&format!("BEARER {TOKEN}"), TOKEN));
    }

    #[test]
    fn bearer_matches_rejects_a_wrong_token_of_the_same_length_and_a_different_length() {
        let wrong = format!("{}X", &TOKEN[..TOKEN.len() - 1]);
        assert_eq!(wrong.len(), TOKEN.len());
        assert!(!matches(&format!("Bearer {wrong}"), TOKEN));
        assert!(!matches(&format!("Bearer {TOKEN}x"), TOKEN));
        assert!(!matches(&format!("Bearer {}", &TOKEN[1..]), TOKEN));
    }

    #[test]
    fn bearer_matches_rejects_two_spaces_another_scheme_and_a_missing_header() {
        assert!(!matches("bearer  x", "x"));
        assert!(!matches(&format!("Bearer  {TOKEN}"), TOKEN));
        assert!(!matches("Basic x", "x"));
        assert!(!bearer_matches(None, Some(TOKEN)));
        assert!(!matches("Bearer", "x"));
        assert!(!matches("Bearerx x", "x"));
    }

    #[test]
    fn bearer_matches_is_false_whenever_no_token_is_expected() {
        assert!(!matches("Bearer ", ""));
        assert!(!matches("Bearer x", ""));
        assert!(!bearer_matches(Some("Bearer undefined"), None));
        assert!(!bearer_matches(None, None));
    }

    #[test]
    fn bearer_matches_never_panics_on_a_10_kb_header_or_multibyte_text() {
        let huge = format!("Bearer {}", "a".repeat(10 * 1024));
        assert!(!matches(&huge, TOKEN));
        assert!(!matches(&"x".repeat(10 * 1024), TOKEN));
        // A multi-byte character straddling the 7-byte scheme boundary.
        assert!(!matches("Bear\u{e9}r x", "x"));
        assert!(!matches("\u{1F600}\u{1F600}", "x"));
    }

    #[test]
    fn host_allowed_accepts_127_0_0_1_and_localhost_on_the_services_own_port() {
        assert!(host_allowed(Some("127.0.0.1:8787"), 8787));
        assert!(host_allowed(Some("localhost:8787"), 8787));
    }

    #[test]
    fn host_allowed_rejects_another_port_host_no_port_no_header_and_a_suffixed_host() {
        assert!(!host_allowed(Some("127.0.0.1:9999"), 8787));
        assert!(!host_allowed(Some("evil.test:8787"), 8787));
        assert!(!host_allowed(Some("127.0.0.1"), 8787));
        assert!(!host_allowed(None, 8787));
        assert!(!host_allowed(Some("127.0.0.1:8787.evil.test"), 8787));
    }

    #[test]
    fn generate_mcp_token_returns_43_base64url_characters_different_each_call() {
        let (a, b) = (generate_mcp_token(), generate_mcp_token());
        for t in [&a, &b] {
            assert_eq!(t.len(), 43);
            assert!(t.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-'), "{t}");
        }
        assert_ne!(a, b);
    }

    #[test]
    fn generate_mcp_token_draws_its_32_bytes_from_the_injected_source() {
        let mut asked = 0;
        let token = generate_mcp_token_with(|buf| {
            asked = buf.len();
            buf.fill(0xff);
        });
        assert_eq!(asked, 32);
        assert_eq!(token, URL_SAFE_NO_PAD.encode([0xffu8; 32]));
    }
}
