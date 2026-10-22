//! The handlers behind two core MCP tools (port of
//! `core/mcp-background-tools.js`):
//!  - `list_notifications`: the inbox, for `companion inbox` and terminal Claude.
//!  - `get_change_risk`: the pre-push check against the shared risk-facts/v1
//!    feed, for `companion precheck` and terminal Claude / Cursor. With no feed
//!    configured it says so and checks nothing.

use crate::http::HttpTransport;
use crate::notifications::NotificationStore;
use crate::push_risk::{assess_push_risk, normalize_changed_files};
use crate::risk_facts::{RiskFactsClient, RiskFactsResult};
use serde_json::{Map, Value, json};

pub fn list_notifications(inbox: Option<&mut NotificationStore>, args: &Value) -> Result<Value, String> {
    let Some(inbox) = inbox else { return Err("The inbox isn't available on this copy of the companion.".into()) };
    let include_seen = args.get("all") == Some(&Value::Bool(true));
    let items: Vec<Value> = inbox
        .list(include_seen, 50)
        .into_iter()
        .map(|i| {
            let mut m = Map::new();
            m.insert("id".into(), json!(i.id));
            m.insert("kind".into(), json!(i.kind));
            m.insert("title".into(), json!(i.title));
            m.insert("body".into(), json!(i.body));
            m.insert("url".into(), json!(i.url));
            m.insert("urgent".into(), json!(i.urgent));
            // Left out when absent, as `undefined` is by JSON.stringify.
            if let Some(s) = i.scope_key {
                m.insert("scopeKey".into(), json!(s));
            }
            if let Some(j) = i.job_id {
                m.insert("jobId".into(), json!(j));
            }
            m.insert("createdAt".into(), json!(i.created_at));
            m.insert("seen".into(), json!(i.seen_at.is_some_and(|t| t != 0)));
            Value::Object(m)
        })
        .collect();
    Ok(json!({ "unseen": inbox.unseen_count(), "items": items }))
}

/// The pre-push check. `risk_url` is the configured feed (`None` or blank:
/// nothing to check against); `risk_facts` is the shared client.
pub async fn assess_push_risk_tool<T: HttpTransport + 'static>(
    args: &Value,
    risk_url: Option<&str>,
    risk_facts: Option<&RiskFactsClient<T>>,
    now: i64,
) -> Result<Value, String> {
    let files = normalize_changed_files(args.get("files").unwrap_or(&Value::Null))?;
    let repo = args.get("repo").and_then(Value::as_str);
    let skipped = |reason: String| json!({ "skipped": true, "repo": repo, "reason": reason });
    let Some(url) = risk_url.filter(|u| !crate::js_text::js_trim(u).is_empty()) else {
        return Ok(skipped("The shared test history (\u{2699} Settings \u{2192} Shared test history) isn't set up, so there is nothing to check against.".into()));
    };
    let Some(client) = risk_facts else {
        return Ok(skipped("The shared test history can't be read right now (the client isn't available).".into()));
    };
    match client.get(url).await {
        RiskFactsResult::Err { reason } => Ok(skipped(format!("The shared test history can't be read right now ({reason})."))),
        RiskFactsResult::Ok { facts, stale, .. } => {
            let assessment = serde_json::to_value(assess_push_risk(&files, &facts, now)).map_err(|e| e.to_string())?;
            let mut out = Map::new();
            out.insert("skipped".into(), json!(false));
            out.insert("repo".into(), json!(repo));
            out.insert("stale".into(), json!(stale));
            if let Value::Object(a) = assessment {
                out.extend(a);
            }
            Ok(Value::Object(out))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::testing::FakeTransport;
    use crate::notifications::StdFs;

    fn facts_body() -> String {
        json!({
            "schema": "risk-facts/v1", "generatedAt": "2026-09-29T02:00:00Z", "tests": [],
            "files": [{ "path": "src/a.ts", "regressions": [{ "build": 7, "tests": ["T"] }] }],
        })
        .to_string()
    }

    const NOW: i64 = 1_790_672_400_000; // 2026-09-29T09:00:00Z

    #[tokio::test]
    async fn skipped_without_a_feed_or_when_it_cannot_be_read_else_the_cited_assessment() {
        let none = assess_push_risk_tool::<FakeTransport>(&json!({ "files": ["src/a.ts"] }), None, None, NOW).await.unwrap();
        assert_eq!(none["skipped"], true);
        assert!(none["reason"].as_str().unwrap().contains("isn't set up"));
        let blank = assess_push_risk_tool::<FakeTransport>(&json!({ "files": ["src/a.ts"] }), Some("   "), None, NOW).await.unwrap();
        assert_eq!(blank["skipped"], true);

        let down = RiskFactsClient::new(FakeTransport::replying(503, "no", "text/plain"));
        let d = assess_push_risk_tool(&json!({ "files": ["src/a.ts"] }), Some("https://facts/x"), Some(&down), NOW).await.unwrap();
        assert!(d["reason"].as_str().unwrap().contains("can't be read right now (unreachable: HTTP 503)"), "{d}");

        let up = RiskFactsClient::new(FakeTransport::replying(200, &facts_body(), "application/json"));
        let r = assess_push_risk_tool(&json!({ "files": ["src/a.ts"], "repo": "CI/sample-app" }), Some("https://facts/x"), Some(&up), NOW).await.unwrap();
        assert_eq!((r["skipped"].clone(), r["level"].clone(), r["repo"].clone(), r["stale"].clone()), (json!(false), json!("medium"), json!("CI/sample-app"), json!(false)));
        assert!(r["lines"][0]["text"].as_str().unwrap().contains("src/a.ts was in 1 regression: 1 build 7"), "{r}");
        for k in ["summary", "checked", "known", "withRegressions", "generatedAt"] {
            assert!(r.get(k).is_some(), "{k}");
        }

        let err = assess_push_risk_tool::<FakeTransport>(&json!({ "files": ["/abs"] }), Some("x"), None, NOW).await.unwrap_err();
        assert!(err.contains("repo-relative"));
        let nonstring_repo = assess_push_risk_tool::<FakeTransport>(&json!({ "files": ["a"], "repo": 5 }), None, None, NOW).await.unwrap();
        assert_eq!(nonstring_repo["repo"], Value::Null);
    }

    #[test]
    fn list_notifications_returns_the_inboxs_items_without_internal_fields() {
        let mut inbox = NotificationStore::with(None, || 1, || "id-1".into(), Box::new(StdFs), |_| {});
        inbox.add(&json!({ "key": "k", "kind": "review-request", "title": "Review requested: CI/sample-app #3" })).unwrap();
        let out = list_notifications(Some(&mut inbox), &json!({})).unwrap();
        assert_eq!(out["unseen"], 1);
        let mut keys: Vec<&str> = out["items"][0].as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        // `scopeKey` and `jobId` are absent here, so JSON.stringify would have dropped them too.
        assert_eq!(keys, ["body", "createdAt", "id", "kind", "seen", "title", "urgent", "url"]);
        assert_eq!(out["items"][0]["seen"], false);
        assert!(list_notifications(None, &json!({})).unwrap_err().contains("isn't available"));
        let mut with_ids = NotificationStore::with(None, || 1, || "id-2".into(), Box::new(StdFs), |_| {});
        with_ids.add(&json!({ "key": "k", "kind": "analysis", "title": "t", "scopeKey": "jira:P-1", "jobId": "3f2b8c1e-1111-4222-8333-444455556666" })).unwrap();
        let o = list_notifications(Some(&mut with_ids), &json!({})).unwrap();
        assert_eq!((o["items"][0]["scopeKey"].clone(), o["items"][0]["jobId"].clone()), (json!("jira:P-1"), json!("3f2b8c1e-1111-4222-8333-444455556666")));
        // `all: true` includes the seen ones.
        with_ids.mark_seen(crate::notifications::SeenIds::All);
        assert_eq!(list_notifications(Some(&mut with_ids), &json!({})).unwrap()["items"].as_array().unwrap().len(), 0);
        assert_eq!(list_notifications(Some(&mut with_ids), &json!({ "all": true })).unwrap()["items"][0]["seen"], true);
    }
}
