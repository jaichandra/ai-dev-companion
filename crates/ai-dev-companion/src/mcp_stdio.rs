//! `companion mcp-stdio`: the companion's MCP server over stdin/stdout, for the
//! Claude Code plugin (port of `core/mcp-stdio.js`). Each JSON-RPC line from
//! Claude Code is POSTed to the running service's own `/mcp` route with the MCP
//! token from `credentials.enc` (the plugin itself never holds a token) and
//! every answer is written back as one line. When the service is down, each
//! request gets a JSON-RPC error saying "companion service not running", so
//! Claude Code shows why instead of failing silently. Requests are handled one
//! at a time, in order (a slow answer holds the next request up for at most the
//! 60 s request timeout); nothing here ever panics, and every request id gets
//! exactly one answer.

use crate::history_record::redact_secrets;
use crate::http::{HttpRequest, HttpTransport, Redirect, TransportError};
use crate::js_text::{js_trim, utf16_len};
use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

const SERVER_ERROR: i64 = -32000;
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

fn parse_body(text: &str, content_type: Option<&str>) -> Result<Vec<Value>, serde_json::Error> {
    if js_trim(text).is_empty() {
        return Ok(vec![]);
    }
    if content_type.unwrap_or("").contains("text/event-stream") {
        return text.split('\n').filter_map(|l| l.strip_prefix("data:")).map(|l| serde_json::from_str(js_trim(l))).collect();
    }
    Ok(match serde_json::from_str::<Value>(text)? {
        Value::Array(a) => a,
        v => vec![v],
    })
}

/// The ids of the requests in one JSON-RPC message or batch (notifications have none).
pub fn request_ids(msg: &Value) -> Vec<Value> {
    let list: Vec<&Value> = match msg {
        Value::Array(a) => a.iter().collect(),
        v => vec![v],
    };
    list.into_iter().filter(|m| m["method"].is_string() && !m["id"].is_null() && m.get("id").is_some()).map(|m| m["id"].clone()).collect()
}

/// The ids answered by a list of JSON-RPC responses (messages with an id and no method).
fn answered_ids(messages: &[Value]) -> Vec<Value> {
    messages.iter().filter(|m| m.is_object() && m.get("method").is_none() && !m["id"].is_null()).map(|m| m["id"].clone()).collect()
}

fn error(id: &Value, message: impl Into<String>) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": SERVER_ERROR, "message": message.into() } })
}

pub struct StdioBridge<T: HttpTransport> {
    url: String,
    get_token: Box<dyn Fn() -> Option<String> + Send + Sync>,
    transport: T,
    timeout: Duration,
    max_bytes: usize,
}

impl<T: HttpTransport> StdioBridge<T> {
    pub fn new(transport: T, url: &str, get_token: impl Fn() -> Option<String> + Send + Sync + 'static) -> Self {
        Self { url: url.to_string(), get_token: Box::new(get_token), transport, timeout: REQUEST_TIMEOUT, max_bytes: MAX_RESPONSE_BYTES }
    }

    pub fn with_limits(mut self, timeout: Duration, max_bytes: usize) -> Self {
        self.timeout = timeout;
        self.max_bytes = max_bytes;
        self
    }

    fn fail(ids: &[Value], message: &str) -> Vec<Value> {
        ids.iter().map(|id| error(id, message)).collect()
    }

    /// Handles one line from Claude Code and returns the messages to write back.
    /// Never fails.
    pub async fn handle_line(&self, line: &str) -> Vec<Value> {
        let text = js_trim(line);
        if text.is_empty() {
            return vec![];
        }
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            return vec![json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } })];
        };
        let ids = request_ids(&msg);
        let token = (self.get_token)().filter(|t| !t.is_empty());
        let Some(token) = token else {
            return Self::fail(&ids, "No MCP token is saved yet: run `companion setup` (or `companion doctor`).");
        };
        match self.forward(text, &ids, &token).await {
            Ok(out) => out,
            Err(message) => Self::fail(&ids, &format!("The MCP bridge hit an error: {}", redact_secrets(&message).replace(&token, "***"))),
        }
    }

    async fn forward(&self, text: &str, ids: &[Value], token: &str) -> Result<Vec<Value>, String> {
        static PORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r":(\d+)/").unwrap());
        let req = HttpRequest::new("POST", &self.url, self.timeout)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("authorization", &format!("Bearer {token}"))
            .body(text.as_bytes().to_vec())
            .redirect(Redirect::Error);
        let res = match self.transport.send(req).await {
            Ok(r) => r,
            Err(e) => {
                let port = PORT.captures(&self.url).map(|m| m[1].to_string());
                let message = match e {
                    TransportError::Timeout => format!("The companion didn't answer within {} seconds.", self.timeout.as_secs()),
                    TransportError::ConnectionRefused | TransportError::ConnectionReset => format!(
                        "companion service not running{}. Start it, or run `companion doctor`.",
                        port.map(|p| format!(" (nothing is listening on port {p})")).unwrap_or_default()
                    ),
                    other => format!("Couldn't reach the companion service: {other}"),
                };
                // The error text can echo the request line, header included.
                return Ok(Self::fail(ids, &redact_secrets(&message).replace(token, "***")));
            }
        };
        if res.status == 401 || res.status == 403 {
            return Ok(Self::fail(ids, "The companion refused the MCP token. Run `companion doctor` \u{2014} it may have been rotated."));
        }
        let declared = res.header("content-length").and_then(|c| c.trim().parse::<f64>().ok());
        let body = res.text();
        if declared.is_some_and(|d| d > self.max_bytes as f64) || utf16_len(&body) > self.max_bytes {
            return Ok(Self::fail(ids, "The companion's answer is too large to pass on."));
        }
        let content_type = res.header("content-type");
        if !res.ok() {
            // A JSON-RPC error body (for example an invalid request) is passed through.
            let passed: Vec<Value> = parse_body(&body, content_type).unwrap_or_default().into_iter().filter(|m| m["jsonrpc"] == "2.0" && !m["error"].is_null()).collect();
            let answered = answered_ids(&passed);
            let pending: Vec<Value> = ids.iter().filter(|id| !answered.contains(id)).cloned().collect();
            let mut out = passed;
            out.extend(Self::fail(&pending, &format!("The companion answered HTTP {}.", res.status)));
            return Ok(out);
        }
        let Ok(messages) = parse_body(&body, content_type) else {
            return Ok(Self::fail(ids, "The companion sent an answer that isn't JSON-RPC."));
        };
        let answered = answered_ids(&messages);
        let pending: Vec<Value> = ids.iter().filter(|id| !answered.contains(id)).cloned().collect();
        let mut out = messages;
        out.extend(Self::fail(&pending, "The companion sent no answer to this request."));
        Ok(out)
    }
}

/// Reads JSON-RPC lines from `stdin`, answers each through `bridge` on
/// `stdout`, one at a time. Returns 0 once stdin has ended and the last request
/// is answered, or as soon as stdout stops listening (EPIPE): a closed reader
/// is a normal way for a stdio server to end, not an error. Only JSON-RPC ever
/// goes to `stdout`; diagnostics go to `stderr`.
pub async fn run_stdio<T: HttpTransport>(stdin: impl AsyncRead + Unpin, mut stdout: impl AsyncWrite + Unpin, bridge: &StdioBridge<T>) -> i32 {
    let mut lines = BufReader::new(stdin).lines();
    // An unreadable stdin ends the session like a closed one.
    while let Ok(Some(line)) = lines.next_line().await {
        if js_trim(&line).is_empty() {
            continue;
        }
        for msg in bridge.handle_line(&line).await {
            let mut text = msg.to_string();
            text.push('\n');
            if stdout.write_all(text.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                return 0;
            }
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::FakeTransport;
    use crate::http::HttpResponse;
    use std::sync::Mutex;

    const URL: &str = "http://127.0.0.1:8787/mcp";
    const REQ: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;

    fn reply(status: u16, body: &str, content_type: &str) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse { status, headers: vec![("content-type".into(), content_type.into())], body: body.as_bytes().to_vec() })
    }

    fn json_reply(status: u16, body: &str) -> Result<HttpResponse, TransportError> {
        reply(status, body, "application/json")
    }

    fn bridge(answer: Result<HttpResponse, TransportError>, token: Option<&str>) -> StdioBridge<FakeTransport> {
        let cell = Mutex::new(answer);
        let t = FakeTransport::new(move |_| cell.lock().unwrap().clone());
        let token = token.map(String::from);
        StdioBridge::new(t, URL, move || token.clone())
    }

    fn msg(v: Value) -> String {
        v.to_string()
    }

    #[tokio::test]
    async fn forwards_a_request_with_the_bearer_token_and_returns_the_answer() {
        let b = bridge(json_reply(200, &msg(json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } }))), Some("tok"));
        assert_eq!(b.handle_line(REQ).await, [json!({ "jsonrpc": "2.0", "id": 1, "result": { "tools": [] } })]);
        let seen = b.transport.seen.lock().unwrap();
        assert_eq!((seen[0].url.as_str(), seen[0].method.as_str()), (URL, "POST"));
        assert!(seen[0].headers.contains(&("authorization".into(), "Bearer tok".into())));
        assert_eq!(seen[0].body.as_deref(), Some(REQ.as_bytes()));
        assert_eq!(seen[0].redirect, Redirect::Error);
    }

    #[tokio::test]
    async fn an_event_stream_answer_and_a_notifications_empty_202_both_work() {
        let sse = bridge(reply(200, "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n", "text/event-stream"), Some("tok"));
        assert_eq!(sse.handle_line(REQ).await, [json!({ "jsonrpc": "2.0", "id": 1, "result": {} })]);
        let note = bridge(json_reply(202, ""), Some("tok"));
        assert!(note.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await.is_empty());
    }

    #[tokio::test]
    async fn the_service_being_down_answers_each_request_with_companion_service_not_running() {
        let down = bridge(Err(TransportError::ConnectionRefused), Some("tok"));
        let out = down.handle_line(REQ).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 1);
        assert!(out[0]["error"]["message"].as_str().unwrap().starts_with("companion service not running (nothing is listening on port 8787)"));
        assert!(down.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).await.is_empty(), "a notification gets no answer, even an error");
    }

    #[tokio::test]
    async fn no_token_a_refused_token_an_http_error_junk_in_and_junk_out() {
        let none = bridge(json_reply(200, "{}"), Some(""));
        assert!(none.handle_line(REQ).await[0]["error"]["message"].as_str().unwrap().contains("No MCP token"));
        assert!(none.transport.seen.lock().unwrap().is_empty(), "nothing is sent without a token");
        assert!(bridge(json_reply(401, ""), Some("tok")).handle_line(REQ).await[0]["error"]["message"].as_str().unwrap().contains("refused the MCP token"));
        assert!(bridge(json_reply(403, ""), Some("tok")).handle_line(REQ).await[0]["error"]["message"].as_str().unwrap().contains("refused the MCP token"));
        assert!(bridge(json_reply(500, "oops"), Some("tok")).handle_line(REQ).await[0]["error"]["message"].as_str().unwrap().contains("HTTP 500"));
        let junk_in = bridge(json_reply(200, "{}"), Some("tok"));
        assert_eq!(junk_in.handle_line("not json").await, [json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "Parse error" } })]);
        assert!(junk_in.handle_line("   ").await.is_empty(), "a blank line is ignored");
        assert!(bridge(json_reply(200, "<html>"), Some("tok")).handle_line(REQ).await[0]["error"]["message"].as_str().unwrap().contains("isn't JSON-RPC"));
    }

    #[test]
    fn request_ids_finds_the_requests_in_a_message_or_a_batch() {
        assert_eq!(request_ids(&json!({ "jsonrpc": "2.0", "id": 3, "method": "x" })), [json!(3)]);
        assert_eq!(request_ids(&json!([{ "id": 1, "method": "a" }, { "method": "n" }, { "id": 2, "result": {} }])), [json!(1)]);
        assert!(request_ids(&Value::Null).is_empty());
        assert_eq!(request_ids(&json!({ "id": "abc", "method": "x" })), [json!("abc")]);
    }

    #[tokio::test]
    async fn a_transport_failure_answers_the_request_with_an_error_without_the_token() {
        let t = bridge(Err(TransportError::Other("socket hang up Bearer tok".into())), Some("tok"));
        let out = t.handle_line(REQ).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[0]["error"]["code"], -32000);
        let m = out[0]["error"]["message"].as_str().unwrap();
        assert!(m.contains("socket hang up") && !m.contains("Bearer tok"), "{m}");
    }

    #[tokio::test]
    async fn a_request_the_service_didnt_answer_gets_an_error_and_a_json_rpc_error_body_is_passed_through() {
        let empty = bridge(json_reply(202, ""), Some("tok"));
        let out = empty.handle_line(REQ).await;
        assert_eq!((out.len(), out[0]["id"].clone()), (1, json!(1)));
        assert!(out[0]["error"]["message"].as_str().unwrap().contains("no answer"));
        let rpc = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "Invalid Request: bad session" } });
        let passed = bridge(json_reply(400, &msg(rpc.clone())), Some("tok")).handle_line(REQ).await;
        assert_eq!(passed[0], rpc);
        assert_eq!(passed[1]["id"], 1, "the request itself is still answered");
        let batch = bridge(json_reply(200, &msg(json!([{ "jsonrpc": "2.0", "id": 1, "result": {} }]))), Some("tok"));
        let out = batch.handle_line(&msg(json!([{ "jsonrpc": "2.0", "id": 1, "method": "a" }, { "jsonrpc": "2.0", "id": 2, "method": "b" }]))).await;
        assert_eq!(out.iter().map(|m| (m["id"].clone(), !m["error"].is_null())).collect::<Vec<_>>(), [(json!(1), false), (json!(2), true)]);
    }

    #[tokio::test]
    async fn an_answer_over_the_size_cap_is_refused_declared_or_not() {
        let big = bridge(json_reply(200, r#"{"jsonrpc":"2.0","id":1,"result":{}}"#), Some("tok")).with_limits(REQUEST_TIMEOUT, 10);
        assert!(big.handle_line(REQ).await[0]["error"]["message"].as_str().unwrap().contains("too large"));
        let declared = Ok(HttpResponse { status: 200, headers: vec![("content-length".into(), "999".into()), ("content-type".into(), "application/json".into())], body: b"{}".to_vec() });
        let d = bridge(declared, Some("tok")).with_limits(REQUEST_TIMEOUT, 10);
        assert!(d.handle_line(REQ).await[0]["error"]["message"].as_str().unwrap().contains("too large"));
    }

    #[tokio::test]
    async fn a_timeout_is_named_as_such_and_the_default_request_timeout_is_60_s() {
        assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(60));
        let t = bridge(Err(TransportError::Timeout), Some("tok"));
        assert!(t.handle_line(REQ).await[0]["error"]["message"].as_str().unwrap().contains("didn't answer within 60 seconds"));
        assert_eq!(t.transport.seen.lock().unwrap()[0].timeout, Duration::from_secs(60));
    }

    fn echo_bridge() -> StdioBridge<FakeTransport> {
        let t = FakeTransport::new(|req| {
            let m: Value = serde_json::from_slice(req.body.as_deref().unwrap()).unwrap();
            json_reply(200, &msg(json!({ "jsonrpc": "2.0", "id": m["id"], "result": { "echoed": m["method"] } })))
        });
        StdioBridge::new(t, URL, || Some("tok".into()))
    }

    async fn run(input: &str, b: &StdioBridge<FakeTransport>) -> (i32, Vec<Value>) {
        let mut out: Vec<u8> = vec![];
        let code = run_stdio(input.as_bytes(), &mut out, b).await;
        (code, String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect())
    }

    #[tokio::test]
    async fn run_stdio_handles_several_lines_crlf_blank_lines_and_a_last_line_without_a_newline_in_order() {
        let (code, out) = run("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"b\"}\r\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"c\"}", &echo_bridge()).await;
        assert_eq!(code, 0);
        assert_eq!(out.iter().map(|m| m["result"]["echoed"].as_str().unwrap()).collect::<Vec<_>>(), ["a", "b", "c"]);
    }

    #[tokio::test]
    async fn run_stdio_keeps_going_after_a_request_that_fails() {
        let t = FakeTransport::new(|req| {
            let m: Value = serde_json::from_slice(req.body.as_deref().unwrap()).unwrap();
            if m["id"] == 1 { Err(TransportError::Other("reset".into())) } else { json_reply(200, &msg(json!({ "jsonrpc": "2.0", "id": m["id"], "result": { "echoed": m["method"] } }))) }
        });
        let b = StdioBridge::new(t, URL, || Some("tok".into()));
        let (code, out) = run("{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"a\"}\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"b\"}\n", &b).await;
        assert_eq!(code, 0);
        assert_eq!((out[0]["error"]["code"].clone(), out[1]["result"]["echoed"].clone()), (json!(-32000), json!("b")));
    }

    #[tokio::test]
    async fn run_stdio_exits_cleanly_with_zero_when_stdout_closes_and_a_parse_error_goes_to_stdout() {
        struct Broken;
        impl AsyncWrite for Broken {
            fn poll_write(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, _: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
                std::task::Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
            }
            fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
            fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Ok(()))
            }
        }
        let b = echo_bridge();
        assert_eq!(run_stdio(format!("{REQ}\n{REQ}\n").as_bytes(), Broken, &b).await, 0);
        assert_eq!(b.transport.seen.lock().unwrap().len(), 1, "after the pipe closed, no further request is handled");
        let (_, out) = run("nonsense\n", &b).await;
        assert_eq!(out[0]["error"]["code"], -32700);
    }
}
