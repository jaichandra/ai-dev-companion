//! The Host-header check for the whole app (port of `core/host-guard.js`). A
//! page that rebinds its own DNS name to 127.0.0.1 still sends its own name in
//! Host, so requiring `127.0.0.1:<port>` / `localhost:<port>` stops that class
//! of attack before any route (or the shared-secret check) runs. The web layer
//! calls [`check_host`] first and answers the error as a 403.

use crate::mcp_auth::host_allowed;
use serde_json::{Value, json};

pub const FORBIDDEN_MESSAGE: &str = "This service only answers requests addressed to 127.0.0.1 or localhost on its own port.";

/// `Ok` to let the request through, else the 403 body.
pub fn check_host(host_header: Option<&str>, port: u16) -> Result<(), Value> {
    if host_allowed(host_header, port) { Ok(()) } else { Err(json!({ "error": FORBIDDEN_MESSAGE })) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lets_requests_addressed_to_127_0_0_1_or_localhost_on_our_port_through() {
        assert!(check_host(Some("127.0.0.1:8787"), 8787).is_ok());
        assert!(check_host(Some("localhost:8787"), 8787).is_ok());
    }

    #[test]
    fn refuses_any_other_host_header_with_the_403_body_before_the_route_runs() {
        for host in [Some("evil.example.com:8787"), Some("127.0.0.1:9999"), Some("127.0.0.1"), Some(""), None, Some("127.0.0.1:8787.evil.com")] {
            let body = check_host(host, 8787).unwrap_err();
            let msg = body["error"].as_str().unwrap();
            assert!(msg.contains("127.0.0.1") || msg.contains("localhost"), "{host:?}");
        }
    }

    #[test]
    fn follows_the_current_port() {
        assert!(check_host(Some("127.0.0.1:9000"), 9000).is_ok());
        assert!(check_host(Some("127.0.0.1:8787"), 9000).is_err());
    }
}
