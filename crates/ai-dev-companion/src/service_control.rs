//! Health-check helpers shared by setup (which starts and verifies the service
//! at the end of every run) and doctor (which diagnoses a service that isn't
//! working); port of the runtime half of `core/service-control.js`. The Node
//! build also compiled the TypeScript here (`buildServer`); there is nothing to
//! build any more, so that function and its install-layout constant are gone.

use crate::http::{HttpRequest, HttpTransport};
use serde::Serialize;
use serde_json::Value;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthState {
    /// Nothing answering on the port.
    Down,
    /// Something answers, but rejects this secret (the extension would get the
    /// same 403).
    Unauthorized,
    /// Reachable and the secret works.
    Ok,
    /// Reachable but answered unexpectedly.
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Health {
    pub state: HealthState,
    pub message: String,
    /// What's enabled, when the state is `Ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub features: Option<Vec<String>>,
}

fn request(port: u16, path: &str, secret: &str) -> HttpRequest {
    HttpRequest::new("GET", &format!("http://127.0.0.1:{port}{path}"), Duration::from_secs(2)).header("X-Companion-Secret", secret)
}

/// Probes the running service.
pub async fn check_health<T: HttpTransport>(transport: &T, port: u16, secret: &str) -> Health {
    let base = format!("http://127.0.0.1:{port}");
    let Ok(res) = transport.send(request(port, "/features", secret)).await else {
        return Health { state: HealthState::Down, message: format!("Nothing is answering on {base}."), features: None };
    };
    if res.status == 403 {
        return Health {
            state: HealthState::Unauthorized,
            message: format!("{base} is answering but rejects the configured secret \u{2014} it's an old copy of the service, or another program."),
            features: None,
        };
    }
    if !res.ok() {
        return Health { state: HealthState::Error, message: format!("{base}/features returned HTTP {}.", res.status), features: None };
    }
    let features: Vec<String> = serde_json::from_slice::<Value>(&res.body)
        .ok()
        .and_then(|v| v.as_array().map(|a| a.iter().filter_map(|f| f.get("id").and_then(Value::as_str).map(str::to_string)).collect()))
        .unwrap_or_default();
    let listed = if features.is_empty() { "none".to_string() } else { features.join(", ") };
    Health { state: HealthState::Ok, message: format!("Running on {base} (features: {listed})."), features: Some(features) }
}

/// Polls [`check_health`] until it reports `Ok` or `timeout` passes; returns the
/// last result either way.
pub async fn wait_for_healthy<T: HttpTransport>(transport: &T, port: u16, secret: &str, timeout: Duration) -> Health {
    let deadline = Instant::now() + timeout;
    let mut result = check_health(transport, port, secret).await;
    while result.state != HealthState::Ok && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(500)).await;
        result = check_health(transport, port, secret).await;
    }
    result
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionStatus {
    pub connected: bool,
    pub version: Option<String>,
}

/// Whether the Chrome extension has said hello to the running service (see
/// `POST /extension/hello`). Any failure counts as not connected.
pub async fn check_extension_connected<T: HttpTransport>(transport: &T, port: u16, secret: &str) -> ExtensionStatus {
    let none = ExtensionStatus { connected: false, version: None };
    let Ok(res) = transport.send(request(port, "/extension/hello", secret)).await else { return none };
    if !res.ok() {
        return none;
    }
    let Ok(body) = serde_json::from_slice::<Value>(&res.body) else { return none };
    ExtensionStatus {
        connected: body.get("connected") == Some(&Value::Bool(true)),
        version: body.get("version").and_then(Value::as_str).filter(|v| !v.is_empty()).map(str::to_string),
    }
}

/// The last `lines` non-empty lines of a log file, or "" if it doesn't exist.
pub fn tail_file(path: &std::path::Path, lines: usize) -> String {
    let Ok(bytes) = std::fs::read(path) else { return String::new() };
    let text = String::from_utf8_lossy(&bytes);
    let all: Vec<&str> = text.split('\n').filter(|l| !l.is_empty()).collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::TransportError;
    use crate::http::testing::FakeTransport;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn a_healthy_service_lists_its_features_and_the_request_carries_the_secret() {
        let t = FakeTransport::replying(200, r#"[{"id":"digest"},{"id":"resolve-conflict"},{"nope":1}]"#, "application/json");
        let h = check_health(&t, 8787, "s3cret").await;
        assert_eq!(h.state, HealthState::Ok);
        assert_eq!(h.features.as_deref(), Some(&["digest".to_string(), "resolve-conflict".to_string()][..]));
        assert_eq!(h.message, "Running on http://127.0.0.1:8787 (features: digest, resolve-conflict).");
        let seen = t.seen.lock().unwrap();
        assert_eq!(seen[0].url, "http://127.0.0.1:8787/features");
        assert_eq!(seen[0].headers, [("X-Companion-Secret".to_string(), "s3cret".to_string())]);
        assert_eq!(seen[0].timeout, Duration::from_secs(2));
    }

    #[tokio::test]
    async fn down_unauthorized_and_error_each_say_what_is_wrong() {
        let down = check_health(&FakeTransport::new(|_| Err(TransportError::ConnectionRefused)), 8787, "s").await;
        assert_eq!((down.state, down.message.as_str()), (HealthState::Down, "Nothing is answering on http://127.0.0.1:8787."));
        let denied = check_health(&FakeTransport::replying(403, "no", "text/plain"), 8787, "s").await;
        assert_eq!(denied.state, HealthState::Unauthorized);
        assert!(denied.message.contains("rejects the configured secret"));
        let broken = check_health(&FakeTransport::replying(500, "boom", "text/plain"), 8787, "s").await;
        assert_eq!((broken.state, broken.message.as_str()), (HealthState::Error, "http://127.0.0.1:8787/features returned HTTP 500."));
        let empty = check_health(&FakeTransport::replying(200, "not json", "text/plain"), 8787, "s").await;
        assert_eq!((empty.state, empty.message.as_str()), (HealthState::Ok, "Running on http://127.0.0.1:8787 (features: none)."));
        let json = serde_json::to_value(&down).unwrap();
        assert_eq!(json["state"], "down");
        assert!(json.get("features").is_none());
    }

    #[tokio::test]
    async fn wait_for_healthy_polls_until_ok_or_the_deadline() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let t = FakeTransport::new(move |_| {
            if c.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(TransportError::ConnectionRefused)
            } else {
                Ok(crate::http::HttpResponse { status: 200, headers: vec![], body: b"[]".to_vec() })
            }
        });
        let started = Instant::now();
        let h = wait_for_healthy(&t, 1, "s", Duration::from_secs(15)).await;
        assert_eq!(h.state, HealthState::Ok);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert!(started.elapsed() >= Duration::from_millis(900), "two 500ms waits");
        let down = FakeTransport::new(|_| Err(TransportError::ConnectionRefused));
        assert_eq!(wait_for_healthy(&down, 1, "s", Duration::from_millis(100)).await.state, HealthState::Down);
    }

    #[tokio::test]
    async fn the_extension_status_comes_from_the_hello_route_and_any_failure_is_not_connected() {
        let ok = FakeTransport::replying(200, r#"{"connected":true,"version":"0.18.0"}"#, "application/json");
        assert_eq!(check_extension_connected(&ok, 8787, "s").await, ExtensionStatus { connected: true, version: Some("0.18.0".into()) });
        assert_eq!(ok.seen.lock().unwrap()[0].url, "http://127.0.0.1:8787/extension/hello");
        let none = ExtensionStatus { connected: false, version: None };
        assert_eq!(check_extension_connected(&FakeTransport::replying(200, r#"{"connected":false}"#, "application/json"), 1, "s").await, none);
        assert_eq!(check_extension_connected(&FakeTransport::replying(403, "no", "text/plain"), 1, "s").await, none);
        assert_eq!(check_extension_connected(&FakeTransport::replying(200, "junk", "text/plain"), 1, "s").await, none);
        assert_eq!(check_extension_connected(&FakeTransport::new(|_| Err(TransportError::Timeout)), 1, "s").await, none);
    }

    #[test]
    fn tail_file_returns_the_last_non_empty_lines_or_nothing_for_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("log");
        std::fs::write(&f, "a\n\nb\nc\n\nd\n").unwrap();
        assert_eq!(tail_file(&f, 2), "c\nd");
        assert_eq!(tail_file(&f, 15), "a\nb\nc\nd");
        assert_eq!(tail_file(&dir.path().join("none"), 15), "");
    }
}
