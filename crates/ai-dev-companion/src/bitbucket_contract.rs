//! Pure logic behind `doctor --bitbucket-contract [PR url]` (port of
//! `core/bitbucket-contract.js`). Kept apart from the API client (which does
//! the network calls) so it can be unit-tested with no I/O.
//!
//! Four independent pieces:
//!  - [`parse_pr_url`]: a pasted Bitbucket PR URL -> project, repo, id.
//!  - [`missing_keys`] / [`missing_keys_across`]: diff a live raw response's
//!    keys against `bitbucket_normalize::EXPECTED_KEYS`, without duplicating
//!    that list.
//!  - [`follow_pages`]: follow `nextPageStart` until `isLastPage`, capped,
//!    with the actual fetch injected, so the client's real fetcher and the
//!    tests share one implementation.
//!  - [`compare_version`]: whether the live Bitbucket version differs from
//!    what was last recorded.

use regex::Regex;
use serde_json::Value;
use std::future::Future;
use std::sync::LazyLock;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrRef {
    pub project: String,
    pub repo: String,
    pub id: u64,
}

static PR_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/projects/([^/]+)/repos/([^/]+)/pull-requests/([0-9]+)(?:/|$)").unwrap());

/// Parses `https://host/projects/P/repos/R/pull-requests/N[/...]`. Anything
/// after the PR number (diff, overview, a `#comment-N` fragment) is ignored.
/// `None` for anything that doesn't parse as a URL or whose path doesn't have
/// this shape; never fails, since this is user-pasted input.
pub fn parse_pr_url(url: &str) -> Option<PrRef> {
    let parsed = Url::parse(url).ok()?;
    let m = PR_PATH.captures(parsed.path())?;
    Some(PrRef { project: m[1].to_string(), repo: m[2].to_string(), id: m[3].parse().ok()? })
}

/// Whether `dotted` (e.g. "fromRef.displayId") resolves inside `obj`. An
/// explicit `null` counts as present (the field exists, Bitbucket just has
/// nothing to say); only a genuinely absent key, or a missing or non-object
/// intermediate, counts as missing.
fn has_dotted_key(obj: &Value, dotted: &str) -> bool {
    let mut cur = obj;
    for segment in dotted.split('.') {
        match cur.as_object().and_then(|o| o.get(segment)) {
            Some(next) => cur = next,
            None => return false,
        }
    }
    true
}

/// Which of `expected` are missing from a single raw response. A non-object
/// `raw` (the endpoint returned nothing usable) reports every key missing.
pub fn missing_keys(raw: &Value, expected: &[&str]) -> Vec<String> {
    if !raw.is_object() {
        return expected.iter().map(|k| k.to_string()).collect();
    }
    expected.iter().filter(|k| !has_dotted_key(raw, k)).map(|k| k.to_string()).collect()
}

/// As [`missing_keys`], across several raw samples of the same shape (needed
/// for `activity`/`comment`/`commentAnchor`, which come from a *list*). A key
/// only counts as missing if it's absent from every sample: Bitbucket only sets
/// `commentAnchor` on comments that anchor to a diff line. `[]` for no samples:
/// "nothing to check", never "everything is missing".
pub fn missing_keys_across(samples: &[Value], expected: &[&str]) -> Vec<String> {
    if samples.is_empty() {
        return vec![];
    }
    expected.iter().filter(|k| !samples.iter().any(|s| has_dotted_key(s, k))).map(|k| k.to_string()).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pages {
    pub pages: Vec<Value>,
    /// The cap was hit before `isLastPage`.
    pub truncated: bool,
    /// A page couldn't be followed (not an object, or not last but with no
    /// numeric `nextPageStart`): the walk stopped with what was collected.
    pub malformed: bool,
}

pub const DEFAULT_MAX_PAGES: usize = 20;

/// Follows a paged Bitbucket endpoint (`{values, isLastPage, nextPageStart}`)
/// by calling `fetch_page(start)`, starting at 0, then at each page's own
/// `nextPageStart`, until a page reports `isLastPage` or `max_pages` pages
/// have been fetched. `fetch_page` is the only I/O. Hitting the cap is not an
/// error (`truncated`); a page it can't follow is reported through `warn` and
/// ends the walk with `malformed` rather than throwing or asking for
/// `start=undefined` (which Bitbucket reads as 0, the same page again).
pub async fn follow_pages<E, F, Fut>(mut fetch_page: F, max_pages: usize, mut warn: impl FnMut(&str)) -> Result<Pages, E>
where
    F: FnMut(i64) -> Fut,
    Fut: Future<Output = Result<Value, E>>,
{
    let mut pages: Vec<Value> = Vec::new();
    let mut start: i64 = 0;
    for i in 0..max_pages {
        let page = fetch_page(start).await?;
        if !page.is_object() {
            warn(&format!("[bitbucket] page {} (start={start}) was not an object \u{2014} stopping with {} page(s).", i + 1, pages.len()));
            return Ok(Pages { pages, truncated: false, malformed: true });
        }
        let last = page.get("isLastPage").is_some_and(|v| crate::js_text::js_truthy(Some(v)));
        let next = page.get("nextPageStart").and_then(Value::as_f64).filter(|n| n.is_finite());
        pages.push(page);
        if last {
            return Ok(Pages { pages, truncated: false, malformed: false });
        }
        let Some(next) = next else {
            warn(&format!(
                "[bitbucket] page {} (start={start}) isn't the last page but has no numeric nextPageStart \u{2014} stopping with {} page(s).",
                i + 1,
                pages.len()
            ));
            return Ok(Pages { pages, truncated: false, malformed: true });
        };
        start = next as i64;
    }
    Ok(Pages { pages, truncated: true, malformed: false })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionComparison {
    pub changed: bool,
    pub message: Option<String>,
}

/// Compares the previously recorded `{version, seenAt}` (or `None` on the
/// first run) against the live `version` string. `message` is a ready-to-print
/// line when the version moved, else `None`.
pub fn compare_version(recorded: Option<&Value>, live: &str) -> VersionComparison {
    let version = recorded.and_then(|r| r.get("version")).and_then(Value::as_str).filter(|v| !v.is_empty());
    match version {
        Some(v) if v != live => VersionComparison {
            changed: true,
            message: Some(format!("Bitbucket changed from {v} to {live} \u{2014} re-check the contract.")),
        },
        _ => VersionComparison { changed: false, message: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn pr(url: &str) -> Option<(String, String, u64)> {
        parse_pr_url(url).map(|p| (p.project, p.repo, p.id))
    }

    fn t(a: &str, b: &str, c: u64) -> Option<(String, String, u64)> {
        Some((a.into(), b.into(), c))
    }

    #[test]
    fn parse_pr_url_parses_project_repo_and_id_ignoring_anything_after_the_number() {
        let base = "https://bitbucket.example.com/projects/ACME/repos/sample-app/pull-requests/123";
        assert_eq!(pr(base), t("ACME", "sample-app", 123));
        assert_eq!(pr(&format!("{base}/diff#chg-foo.js")), t("ACME", "sample-app", 123));
        assert_eq!(pr(&format!("{base}/overview")), t("ACME", "sample-app", 123));
        assert_eq!(pr("https://bitbucket.example.com/projects/ACME-1/repos/web.ui_2/pull-requests/7"), t("ACME-1", "web.ui_2", 7));
        assert_eq!(pr("https://bb.example/projects/~JSMITH/repos/r/pull-requests/9"), t("~JSMITH", "r", 9));
        assert_eq!(pr("https://bb.example/context/projects/A/repos/r/pull-requests/9"), t("A", "r", 9), "an app context path is fine");
    }

    #[test]
    fn parse_pr_url_returns_none_for_other_shapes_and_unparseable_input() {
        for bad in [
            "https://bitbucket.example.com/projects/ACME/repos/sample-app",
            "https://bitbucket.example.com/dashboard",
            "https://bb.example/projects/A/repos/r/pull-requests/x",
            "https://bb.example/projects/A/repos/r/pull-requests/12x",
            "not a url",
            "",
        ] {
            assert_eq!(pr(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn missing_keys_reports_absent_top_level_and_nested_keys_and_treats_null_as_present() {
        assert!(missing_keys(&json!({ "id": 1, "fromRef": { "displayId": "feature/x" } }), &["id", "fromRef.displayId"]).is_empty());
        assert_eq!(missing_keys(&json!({ "id": 1, "fromRef": {} }), &["id", "title", "fromRef.displayId"]), ["title", "fromRef.displayId"]);
        assert!(missing_keys(&json!({ "state": null }), &["state"]).is_empty());
        for raw in [json!(null), json!([1, 2]), json!("x")] {
            assert_eq!(missing_keys(&raw, &["id", "title"]), ["id", "title"], "{raw}");
        }
        assert_eq!(missing_keys(&json!({ "id": 1 }), &["author.user.slug"]), ["author.user.slug"]);
        assert_eq!(missing_keys(&json!({ "author": null }), &["author.user"]), ["author.user"], "null parent has no children");
    }

    #[test]
    fn missing_keys_across_counts_a_key_present_if_any_sample_has_it() {
        let keys = ["action", "comment", "commentAnchor"];
        assert!(missing_keys_across(&[], &keys).is_empty(), "nothing to check, not everything missing");
        let both = [json!({ "action": "COMMENTED", "comment": {} }), json!({ "action": "COMMENTED", "comment": {}, "commentAnchor": {} })];
        assert!(missing_keys_across(&both, &keys).is_empty());
        let neither = [json!({ "action": "COMMENTED" }), json!({ "action": "COMMENTED" })];
        assert_eq!(missing_keys_across(&neither, &keys), ["comment", "commentAnchor"]);
        assert!(missing_keys_across(&[json!(null), json!(null), json!({ "action": "COMMENTED" })], &["action"]).is_empty());
    }

    async fn walk(pages: Vec<Value>, max: usize) -> (Pages, Vec<i64>, Vec<String>) {
        let calls = RefCell::new(Vec::new());
        let warnings = RefCell::new(Vec::new());
        let out = follow_pages::<(), _, _>(
            |start| {
                calls.borrow_mut().push(start);
                let page = pages.get(calls.borrow().len() - 1).cloned().unwrap_or(Value::Null);
                async move { Ok(page) }
            },
            max,
            |m| warnings.borrow_mut().push(m.to_string()),
        )
        .await
        .unwrap();
        (out, calls.into_inner(), warnings.into_inner())
    }

    #[tokio::test]
    async fn follow_pages_stops_at_the_first_is_last_page_and_passes_the_right_start_each_call() {
        let pages = vec![
            json!({ "values": [1], "isLastPage": false, "nextPageStart": 25 }),
            json!({ "values": [2], "isLastPage": false, "nextPageStart": 50 }),
            json!({ "values": [3], "isLastPage": true }),
        ];
        let (out, calls, warnings) = walk(pages.clone(), DEFAULT_MAX_PAGES).await;
        assert_eq!(calls, [0, 25, 50]);
        assert_eq!(out, Pages { pages, truncated: false, malformed: false });
        assert!(warnings.is_empty());
        let single = json!({ "values": [1], "isLastPage": true });
        let (out, calls, _) = walk(vec![single.clone()], DEFAULT_MAX_PAGES).await;
        assert_eq!((out.pages, calls), (vec![single], vec![0]));
    }

    #[tokio::test]
    async fn follow_pages_stops_at_the_cap_and_reports_truncated() {
        let endless = |n: usize| (0..n).map(|i| json!({ "values": [], "isLastPage": false, "nextPageStart": (i + 1) * 25 })).collect::<Vec<_>>();
        let (out, calls, _) = walk(endless(10), 3).await;
        assert_eq!((calls.len(), out.pages.len(), out.truncated, out.malformed), (3, 3, true, false));
        let (out, calls, _) = walk(endless(30), DEFAULT_MAX_PAGES).await;
        assert_eq!((calls.len(), out.truncated), (20, true), "the cap defaults to 20 pages");
    }

    #[tokio::test]
    async fn follow_pages_stops_with_a_warning_keeping_the_pages_so_far_when_a_page_isnt_an_object() {
        let first = json!({ "values": [1], "isLastPage": false, "nextPageStart": 25 });
        let (out, calls, warnings) = walk(vec![first.clone(), Value::Null], DEFAULT_MAX_PAGES).await;
        assert_eq!(calls.len(), 2);
        assert_eq!(out, Pages { pages: vec![first], truncated: false, malformed: true });
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("not an object"));
        assert!(warnings[0].starts_with("[bitbucket] page 2 (start=25)"));
    }

    #[tokio::test]
    async fn follow_pages_stops_when_a_non_last_page_has_no_numeric_next_page_start() {
        for bad in [json!(null), json!("25"), json!(true)] {
            let page = json!({ "values": [1], "isLastPage": false, "nextPageStart": bad });
            let (out, calls, warnings) = walk(vec![page.clone()], DEFAULT_MAX_PAGES).await;
            assert_eq!(calls.len(), 1, "{bad}");
            assert_eq!(out, Pages { pages: vec![page], truncated: false, malformed: true });
            assert!(warnings[0].contains("nextPageStart"), "{bad}");
        }
        let missing = json!({ "values": [1], "isLastPage": false });
        assert!(walk(vec![missing], DEFAULT_MAX_PAGES).await.0.malformed);
    }

    #[tokio::test]
    async fn follow_pages_passes_a_fetch_error_straight_through() {
        let r: Result<Pages, String> = follow_pages(|_| async { Err("boom".to_string()) }, 5, |_| {}).await;
        assert_eq!(r.unwrap_err(), "boom");
    }

    #[test]
    fn compare_version_reports_a_change_with_a_clear_message_and_nothing_else() {
        let none = VersionComparison { changed: false, message: None };
        assert_eq!(compare_version(None, "8.19.1"), none);
        assert_eq!(compare_version(Some(&json!(null)), "8.19.1"), none);
        assert_eq!(compare_version(Some(&json!({ "version": "8.19.1", "seenAt": "2026-01-01T00:00:00.000Z" })), "8.19.1"), none);
        assert_eq!(compare_version(Some(&json!({ "version": "" })), "8.19.1"), none);
        assert_eq!(
            compare_version(Some(&json!({ "version": "8.19.1" })), "8.20.0"),
            VersionComparison { changed: true, message: Some("Bitbucket changed from 8.19.1 to 8.20.0 \u{2014} re-check the contract.".into()) }
        );
    }
}
