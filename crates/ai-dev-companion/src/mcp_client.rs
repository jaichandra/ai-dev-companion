//! A minimal client for the companion's own /mcp route (port of
//! `core/mcp-client.js`): one stateless JSON-RPC `tools/call` per invocation.
//! Used by the `companion` command so it needs neither an MCP SDK nor any
//! route besides /mcp.

use crate::http::{HttpRequest, HttpTransport, TransportError};
use serde_json::{Value, json};
use std::time::Duration;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The last `data:` line of an SSE-framed answer, or the whole text as JSON.
fn parse_rpc_body(text: &str, content_type: &str) -> Result<Value, ()> {
    if content_type.contains("text/event-stream") {
        let data = text.split('\n').rfind(|l| l.starts_with("data:")).ok_or(())?;
        return serde_json::from_str(data[5..].trim_matches(crate::js_text::is_js_space)).map_err(|_| ());
    }
    serde_json::from_str(text).map_err(|_| ())
}

/// The port out of a `:<digits>/` in the URL, like `http://127.0.0.1:8787/mcp`.
fn port_in(url: &str) -> Option<&str> {
    let bytes = url.as_bytes();
    (0..bytes.len()).find_map(|i| {
        if bytes[i] != b':' {
            return None;
        }
        let digits = bytes[i + 1..].iter().take_while(|b| b.is_ascii_digit()).count();
        (digits > 0 && bytes.get(i + 1 + digits) == Some(&b'/')).then(|| &url[i + 1..i + 1 + digits])
    })
}

pub async fn call_tool<T: HttpTransport>(
    transport: &T,
    url: &str,
    token: &str,
    name: &str,
    args: Value,
    timeout: Duration,
) -> Result<Value, String> {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": name, "arguments": args } });
    let req = HttpRequest::new("POST", url, timeout)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("authorization", &format!("Bearer {token}"))
        .body(body.to_string());
    let res = match transport.send(req).await {
        Ok(res) => res,
        Err(TransportError::ConnectionRefused | TransportError::ConnectionReset) => {
            return Err(match port_in(url) {
                Some(p) => format!(
                    "The companion service isn't running (nothing is listening on port {p}). Try `companion doctor`."
                ),
                None => "The companion service isn't running. Try `companion doctor`.".into(),
            });
        }
        Err(e) => return Err(format!("Couldn't reach the companion service: {e}")),
    };
    if res.status == 401 || res.status == 403 {
        return Err("The companion refused the token. Run `companion doctor` \u{2014} the MCP token may have been rotated.".into());
    }
    let text = res.text();
    if !res.ok() {
        return Err(format!("The companion answered HTTP {}.", res.status));
    }
    let parsed = parse_rpc_body(&text, res.header("content-type").unwrap_or(""))
        .ok()
        .filter(|v| !v.is_null())
        .ok_or("The companion sent an answer this command can't read.")?;
    if let Some(err) = parsed.get("error").filter(|e| !e.is_null() && **e != json!(false) && **e != json!("")) {
        let msg = err.get("message").and_then(Value::as_str).filter(|m| !m.is_empty());
        return Err(msg.unwrap_or("The companion returned an error.").to_string());
    }
    let result = parsed.get("result").filter(|r| !r.is_null()).cloned().unwrap_or_else(|| json!({}));
    let text_out = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|items| items.iter().find(|c| c.get("type").and_then(Value::as_str) == Some("text")))
        .and_then(|c| c.get("text").and_then(Value::as_str))
        .unwrap_or("")
        .to_string();
    if result.get("isError").is_some_and(|v| *v == json!(true)) {
        return Err(if text_out.is_empty() { "The tool failed.".into() } else { text_out });
    }
    Ok(serde_json::from_str(&text_out).unwrap_or(Value::String(text_out)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::FakeTransport;

    fn tool_result(payload: &str, is_error: bool) -> String {
        json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": payload }], "isError": is_error } })
            .to_string()
    }

    async fn call(t: &FakeTransport, url: &str) -> Result<Value, String> {
        call_tool(t, url, "t", "n", json!({}), DEFAULT_TIMEOUT).await
    }

    #[tokio::test]
    async fn sends_a_json_rpc_tools_call_with_the_bearer_token_and_returns_the_parsed_payload() {
        let t = FakeTransport::replying(200, &tool_result(r#"[{"id":"j1"}]"#, false), "application/json");
        let out = call_tool(&t, "http://127.0.0.1:8787/mcp", "tok", "list_jobs", json!({ "a": 1 }), DEFAULT_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(out, json!([{ "id": "j1" }]));
        let seen = t.seen.lock().unwrap();
        let req = &seen[0];
        assert_eq!(req.url, "http://127.0.0.1:8787/mcp");
        assert_eq!(req.method, "POST");
        let header = |k: &str| req.headers.iter().find(|(h, _)| h == k).map(|(_, v)| v.as_str());
        assert_eq!(header("authorization"), Some("Bearer tok"));
        assert!(header("accept").unwrap().contains("application/json"));
        let body: Value = serde_json::from_slice(req.body.as_ref().unwrap()).unwrap();
        assert_eq!(body["method"], "tools/call");
        assert_eq!(body["params"], json!({ "name": "list_jobs", "arguments": { "a": 1 } }));
    }

    #[tokio::test]
    async fn returns_the_text_as_is_when_the_answer_isnt_json_and_reads_an_sse_framed_answer() {
        let t = FakeTransport::replying(200, &tool_result("plain words", false), "application/json");
        assert_eq!(call(&t, "u").await.unwrap(), json!("plain words"));
        let sse = format!("event: message\ndata: {}\n\n", tool_result(r#"{"ok":1}"#, false));
        let t = FakeTransport::replying(200, &sse, "text/event-stream");
        assert_eq!(call(&t, "u").await.unwrap(), json!({ "ok": 1 }));
    }

    #[tokio::test]
    async fn a_tool_error_a_refused_token_and_a_down_service_each_give_a_plain_message() {
        let t = FakeTransport::replying(200, &tool_result("Nothing is recorded under that key.", true), "application/json");
        assert!(call(&t, "u").await.unwrap_err().contains("Nothing is recorded"));
        let t = FakeTransport::replying(401, "no", "text/plain");
        assert!(call(&t, "u").await.unwrap_err().contains("refused the token"));
        let t = FakeTransport::new(|_| Err(TransportError::ConnectionRefused));
        assert_eq!(
            call(&t, "http://127.0.0.1:8787/mcp").await.unwrap_err(),
            "The companion service isn't running (nothing is listening on port 8787). Try `companion doctor`."
        );
        assert!(call(&t, "u").await.unwrap_err().contains("isn't running"));
    }

    #[tokio::test]
    async fn other_failures_are_reported_plainly() {
        let t = FakeTransport::new(|_| Err(TransportError::Other("dns".into())));
        assert_eq!(call(&t, "u").await.unwrap_err(), "Couldn't reach the companion service: dns");
        let t = FakeTransport::replying(500, "x", "text/plain");
        assert_eq!(call(&t, "u").await.unwrap_err(), "The companion answered HTTP 500.");
        for (body, ct) in [("not json", "application/json"), ("event: x\n", "text/event-stream"), ("null", "application/json")] {
            let t = FakeTransport::replying(200, body, ct);
            assert_eq!(call(&t, "u").await.unwrap_err(), "The companion sent an answer this command can't read.", "{body}");
        }
        let t = FakeTransport::replying(200, r#"{"error":{"message":"bad"}}"#, "application/json");
        assert_eq!(call(&t, "u").await.unwrap_err(), "bad");
        let t = FakeTransport::replying(200, r#"{"error":{}}"#, "application/json");
        assert_eq!(call(&t, "u").await.unwrap_err(), "The companion returned an error.");
        let t = FakeTransport::replying(200, r#"{"result":{"isError":true,"content":[]}}"#, "application/json");
        assert_eq!(call(&t, "u").await.unwrap_err(), "The tool failed.");
        let t = FakeTransport::replying(200, r#"{"result":{}}"#, "application/json");
        assert_eq!(call(&t, "u").await.unwrap(), json!(""));
    }

    #[test]
    fn finds_the_port_in_the_url() {
        assert_eq!(port_in("http://127.0.0.1:8787/mcp"), Some("8787"));
        assert_eq!(port_in("http://localhost/mcp"), None);
        assert_eq!(port_in("u"), None);
        assert_eq!(port_in("http://h:80"), None);
    }
}
