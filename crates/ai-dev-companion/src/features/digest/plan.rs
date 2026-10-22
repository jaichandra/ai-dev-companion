//! The morning digest (port of `features/digest/plan.js`): your PRs (conflicts,
//! builds, approvals and needs-work), PRs waiting on your review, your open
//! tickets, and what the watchers pre-warmed. Pure: the feature collects the
//! inputs (every read is best effort; a failed one becomes a "Couldn't check"
//! line) and the panel, `companion digest`, the `get_digest` MCP tool and the
//! scheduled weekday digest all show what [`build_digest`] returns.

use crate::cli_format::one_line;
use crate::js_date::to_iso_string;
use crate::js_text::{js_to_string, js_truthy, slice_utf16, utf16_len};
use serde::Serialize;
use serde_json::{Value, json};
use url::Url;

pub const MAX_ITEMS: usize = 10;
/// Items past the cap that still travel with the section, for the panel's "Show more".
const MAX_MORE: usize = 100;
const TEXT_MAX: usize = 200;
const URGENT_PRIORITIES: [&str; 4] = ["Blocker", "Highest", "Critical", "High"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tone {
    Bad,
    Warn,
    Busy,
    Ok,
    Neutral,
}

impl Tone {
    fn rank(self) -> u8 {
        match self {
            Tone::Bad => 0,
            Tone::Warn => 1,
            Tone::Busy => 2,
            Tone::Ok => 3,
            Tone::Neutral => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Item {
    pub text: String,
    pub tone: Tone,
    pub url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Section {
    pub id: String,
    pub title: String,
    pub items: Vec<Item>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub more: Vec<Item>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Counts {
    pub conflicts: usize,
    pub failing_builds: usize,
    pub needs_work: usize,
    pub approved: usize,
    pub to_review: usize,
    pub tickets: usize,
    /// Ready to act on: a failed background run is reported separately.
    pub ready: usize,
    pub failed_runs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Digest {
    pub generated_at: String,
    pub headline: String,
    pub counts: Counts,
    pub sections: Vec<Section>,
}

type V<'a> = Option<&'a Value>;

fn field<'a>(v: &'a Value, k: &str) -> V<'a> {
    v.get(k)
}

/// Text from PRs and tickets: control, C1, zero-width and bidi characters go,
/// whitespace collapses, and it is cut to `max` with an ellipsis.
fn one(text: V<'_>, max: usize) -> String {
    let clean = one_line(text);
    if utf16_len(&clean) > max { format!("{}\u{2026}", slice_utf16(&clean, max - 1)) } else { clean }
}

fn one_str(text: &str, max: usize) -> String {
    one(Some(&Value::String(text.to_string())), max)
}

fn https_url(v: V<'_>) -> Option<String> {
    let s = v?.as_str()?;
    (Url::parse(s).ok()?.scheme() == "https").then(|| s.to_string())
}

/// `${x}` for a possibly-absent field: JS prints "undefined" for a missing one.
fn interp(v: V<'_>) -> String {
    v.map_or_else(|| "undefined".to_string(), js_to_string)
}

fn pr_label(pr: &Value) -> String {
    one_str(&format!("{}/{} #{}", interp(field(pr, "project")), interp(field(pr, "repo")), interp(field(pr, "id"))), 100)
}

fn plural(n: usize, one: &str, many: Option<&str>) -> String {
    let many = many.map_or_else(|| format!("{one}s"), str::to_string);
    format!("{n} {}", if n == 1 { one } else { &many })
}

struct MyPrItem {
    item: Item,
    conflicted: bool,
    failing: bool,
    needs_work: bool,
    approved: bool,
}

fn my_pr_item(entry: &Value) -> MyPrItem {
    let pr = entry.get("pr").cloned().unwrap_or(Value::Null);
    let conflicted = entry.get("conflicted") == Some(&Value::Bool(true));
    let build_state = entry.get("build").and_then(|b| b.get("state")).and_then(Value::as_str);
    let mut tags: Vec<String> = Vec::new();
    let mut tone = Tone::Neutral;
    if conflicted {
        tags.push("conflicts".into());
        tone = Tone::Bad;
    }
    match build_state {
        Some("FAILED") => {
            tags.push("build failing".into());
            tone = Tone::Bad;
        }
        Some("INPROGRESS") => {
            tags.push("build running".into());
            if tone == Tone::Neutral {
                tone = Tone::Busy;
            }
        }
        _ => {}
    }
    let needs_work: Vec<String> = pr
        .get("reviewers")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter(|r| r.get("status").and_then(Value::as_str) == Some("NEEDS_WORK"))
        .map(|r| one(r.get("name"), 40))
        .collect();
    if !needs_work.is_empty() {
        tags.push(format!("needs work ({})", needs_work.iter().take(3).cloned().collect::<Vec<_>>().join(", ")));
        if tone != Tone::Bad {
            tone = Tone::Warn;
        }
    }
    let approvals = pr.get("approvals").and_then(Value::as_f64).unwrap_or(0.0);
    if approvals > 0.0 {
        tags.push(format!("approved by {}", js_to_string(&pr["approvals"])));
        if tone == Tone::Neutral && (entry.get("build").is_none_or(|b| !js_truthy(Some(b))) || build_state == Some("SUCCESSFUL")) {
            tone = Tone::Ok;
        }
    }
    let suffix = if tags.is_empty() { String::new() } else { format!(" \u{2014} {}", tags.join(", ")) };
    MyPrItem {
        item: Item { text: format!("{} {}{suffix}", pr_label(&pr), one(field(&pr, "title"), 120)), tone, url: https_url(field(&pr, "url")) },
        conflicted,
        failing: build_state == Some("FAILED"),
        needs_work: !needs_work.is_empty(),
        approved: approvals > 0.0,
    }
}

fn strip_scheme(scope: &str) -> &str {
    for p in ["bitbucket:", "github:", "jira:"] {
        if let Some(rest) = scope.strip_prefix(p) {
            return rest;
        }
    }
    scope
}

fn ready_item(job: &Value) -> Item {
    let where_ = if js_truthy(field(job, "scopeKey")) { one_str(strip_scheme(&js_to_string(&job["scopeKey"])), 100) } else { String::new() };
    let feature_id = one(field(job, "featureId"), 60);
    if job.get("status").and_then(Value::as_str) == Some("failed") {
        let error = if js_truthy(field(job, "error")) { field(job, "error").cloned() } else { Some(json!("no details")) };
        return Item { text: format!("A background {feature_id} run for {where_} failed: {}", one(error.as_ref(), 120)), tone: Tone::Bad, url: https_url(field(job, "url")) };
    }
    let what = match job.get("featureId").and_then(Value::as_str) {
        Some("resolve-conflict") => "Conflict resolution ready to review".to_string(),
        Some("analyze-issue") => "Analysis ready".to_string(),
        _ => format!("{feature_id} ready"),
    };
    let summary = if js_truthy(field(job, "summary")) { format!(" \u{2014} {}", one(field(job, "summary"), 100)) } else { String::new() };
    Item { text: format!("{what}: {where_}{summary}"), tone: Tone::Ok, url: https_url(field(job, "url")) }
}

fn section(id: &str, title: &str, items: Vec<Item>) -> Section {
    let mut sorted = items;
    sorted.sort_by_key(|i| i.tone.rank());
    let mut shown: Vec<Item> = sorted.iter().take(MAX_ITEMS).cloned().collect();
    // The rest travel with the section (the panel's "Show N more"); the closing
    // "...and N more" row stays for readers that only print items.
    let more: Vec<Item> = sorted.iter().skip(MAX_ITEMS).take(MAX_MORE).cloned().collect();
    if sorted.len() > MAX_ITEMS {
        shown.push(Item { text: format!("\u{2026}and {} more", sorted.len() - MAX_ITEMS), tone: Tone::Neutral, url: None });
    }
    Section { id: id.into(), title: title.into(), items: shown, more }
}

#[derive(Debug, Clone, Default)]
pub struct DigestInput {
    /// Epoch milliseconds; a non-finite value means "now".
    pub now: f64,
    /// `{ pr, conflicted?, build? }` entries.
    pub my_prs: Vec<Value>,
    pub review_prs: Vec<Value>,
    pub tickets: Vec<Value>,
    pub prewarmed: Vec<Value>,
    pub notes: Vec<String>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

pub fn build_digest(input: &DigestInput) -> Digest {
    let mine: Vec<MyPrItem> = input.my_prs.iter().map(my_pr_item).collect();
    let count = |f: fn(&MyPrItem) -> bool| mine.iter().filter(|i| f(i)).count();
    let failed_runs = input.prewarmed.iter().filter(|j| j.get("status").and_then(Value::as_str) == Some("failed")).count();
    let counts = Counts {
        conflicts: count(|i| i.conflicted),
        failing_builds: count(|i| i.failing),
        needs_work: count(|i| i.needs_work),
        approved: count(|i| i.approved),
        to_review: input.review_prs.len(),
        tickets: input.tickets.len(),
        ready: input.prewarmed.len() - failed_runs,
        failed_runs,
    };
    let review_items = input
        .review_prs
        .iter()
        .map(|pr| {
            let by = if js_truthy(field(pr, "authorSlug")) { format!(" \u{2014} by {}", one(field(pr, "authorSlug"), 40)) } else { String::new() };
            Item { text: format!("{} {}{by}", pr_label(pr), one(field(pr, "title"), 120)), tone: Tone::Neutral, url: https_url(field(pr, "url")) }
        })
        .collect();
    let ticket_items = input
        .tickets
        .iter()
        .map(|t| {
            let status = if js_truthy(field(t, "status")) { format!(" ({})", one(field(t, "status"), 30)) } else { String::new() };
            let urgent = field(t, "priority").and_then(Value::as_str).is_some_and(|p| URGENT_PRIORITIES.contains(&p));
            Item {
                text: format!("{} {}{status}", one(field(t, "key"), 40), one(field(t, "summary"), 120)),
                tone: if urgent { Tone::Warn } else { Tone::Neutral },
                url: https_url(field(t, "url")),
            }
        })
        .collect();
    let sections: Vec<Section> = [
        section("my-prs", "Your pull requests", mine.into_iter().map(|m| m.item).collect()),
        section("to-review", "Waiting on your review", review_items),
        section("tickets", "Your open tickets", ticket_items),
        section("ready", "Ready for you", input.prewarmed.iter().map(ready_item).collect()),
        section("notes", "Couldn't check", input.notes.iter().map(|n| Item { text: one_str(n, TEXT_MAX), tone: Tone::Warn, url: None }).collect()),
    ]
    .into_iter()
    .filter(|s| !s.items.is_empty())
    .collect();

    let mut needs: Vec<String> = Vec::new();
    if counts.conflicts > 0 {
        needs.push(plural(counts.conflicts, "conflict", None));
    }
    if counts.failing_builds > 0 {
        needs.push(plural(counts.failing_builds, "failing build", None));
    }
    if counts.needs_work > 0 {
        needs.push(plural(counts.needs_work, "PR needing work", Some("PRs needing work")));
    }
    if counts.to_review > 0 {
        needs.push(plural(counts.to_review, "PR to review", Some("PRs to review")));
    }
    if counts.ready > 0 {
        needs.push(format!("{} ready for you", counts.ready));
    }
    if counts.failed_runs > 0 {
        needs.push(plural(counts.failed_runs, "background run failed", Some("background runs failed")));
    }
    // With sources that couldn't be read, "nothing needs you" would be a guess.
    let headline = if !needs.is_empty() {
        format!("Needs you: {}.", needs.join(", "))
    } else if !input.notes.is_empty() {
        "Some sources couldn't be checked \u{2014} see below.".to_string()
    } else {
        "Nothing needs you right now.".to_string()
    };
    let at = if input.now.is_finite() { input.now as i64 } else { now_ms() };
    Digest { generated_at: to_iso_string(at).unwrap_or_default(), headline, counts, sections }
}

/// The inbox item the scheduled weekday digest posts, as the JSON the inbox
/// store takes. The scheduled run has no browser session, only a cached cookie
/// or a saved token, so its failed reads are usually "no sign-in"; the button
/// relays the page's session.
pub fn digest_notification(digest: &Digest, day_key: &str) -> Value {
    let unread = digest.sections.iter().any(|s| s.id == "notes");
    let note_lines: Vec<String> = digest.sections.iter().filter(|s| s.id == "notes").flat_map(|s| s.items.iter().map(|i| i.text.clone())).take(3).collect();
    let lines: Vec<String> = digest.sections.iter().filter(|s| s.id != "notes").flat_map(|s| s.items.iter().take(2).map(|i| i.text.clone())).take(3).collect();
    let title = if unread && lines.is_empty() {
        "Morning digest \u{2014} couldn't read some sources in the background".to_string()
    } else {
        format!("Morning digest \u{2014} {}", digest.headline)
    };
    // Keep the reasons a source couldn't be read, so the inbox item says what failed.
    let body = if !lines.is_empty() {
        lines.iter().chain(note_lines.iter()).cloned().collect::<Vec<_>>().join("\n")
    } else if unread {
        "Open \u{2728} Morning digest to read them with your browser sign-in, or save tokens in \u{2699} Settings so the background run can sign in.".to_string()
    } else {
        "Open \u{2728} Morning digest for the details.".to_string()
    };
    json!({ "key": format!("digest:{day_key}"), "kind": "digest", "title": title, "body": body, "url": null, "urgent": false })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_790_670_600_000.0; // 2026-09-29T08:30:00Z

    fn pr(id: i64, over: Value) -> Value {
        let mut v = json!({
            "id": id, "title": format!("PR {id}"), "project": "CI", "repo": "sample-app",
            "url": format!("https://bb.example/projects/CI/repos/sample-app/pull-requests/{id}"), "approvals": 0, "reviewers": [],
        });
        for (k, x) in over.as_object().unwrap() {
            v[k] = x.clone();
        }
        v
    }

    fn input() -> DigestInput {
        DigestInput { now: NOW, ..Default::default() }
    }

    #[test]
    fn an_empty_digest_says_nothing_needs_you_and_has_no_sections() {
        let d = build_digest(&input());
        assert_eq!(d.headline, "Nothing needs you right now.");
        assert!(d.sections.is_empty());
        assert_eq!(d.generated_at, "2026-09-29T08:30:00.000Z");
    }

    #[test]
    fn your_prs_show_conflicts_builds_needs_work_and_approvals_worst_first() {
        let d = build_digest(&DigestInput {
            my_prs: vec![
                json!({ "pr": pr(1, json!({ "approvals": 2 })), "conflicted": false, "build": { "state": "SUCCESSFUL" } }),
                json!({ "pr": pr(2, json!({})), "conflicted": true, "build": { "state": "FAILED" } }),
                json!({ "pr": pr(3, json!({ "reviewers": [{ "name": "ann", "status": "NEEDS_WORK" }] })), "conflicted": false, "build": null }),
            ],
            ..input()
        });
        assert_eq!(d.headline, "Needs you: 1 conflict, 1 failing build, 1 PR needing work.");
        assert_eq!(serde_json::to_value(&d.counts).unwrap(), json!({ "conflicts": 1, "failingBuilds": 1, "needsWork": 1, "approved": 1, "toReview": 0, "tickets": 0, "ready": 0, "failedRuns": 0 }));
        let items = &d.sections[0].items;
        assert_eq!(d.sections[0].id, "my-prs");
        assert_eq!(items.iter().map(|i| i.tone).collect::<Vec<_>>(), [Tone::Bad, Tone::Warn, Tone::Ok]);
        assert_eq!(items[0].text, "CI/sample-app #2 PR 2 \u{2014} conflicts, build failing");
        assert_eq!(items[1].text, "CI/sample-app #3 PR 3 \u{2014} needs work (ann)");
        assert_eq!(items[2].text, "CI/sample-app #1 PR 1 \u{2014} approved by 2");
        assert_eq!(items[0].url.as_deref(), Some("https://bb.example/projects/CI/repos/sample-app/pull-requests/2"));
        assert!(serde_json::to_value(&items[0]).unwrap().get("flags").is_none(), "internal flags aren't handed out");
    }

    #[test]
    fn reviews_tickets_prewarmed_results_and_notes_each_get_a_section_only_https_links_survive() {
        let d = build_digest(&DigestInput {
            review_prs: vec![pr(9, json!({ "authorSlug": "bob", "url": "javascript:alert(1)" }))],
            tickets: vec![
                json!({ "key": "PROJ-7", "summary": "Login broken", "status": "Open", "priority": "High", "url": "https://jira.example/browse/PROJ-7" }),
                json!({ "key": "PROJ-8", "summary": "Typo", "status": "In Progress", "priority": "Low" }),
            ],
            prewarmed: vec![
                json!({ "featureId": "resolve-conflict", "status": "awaiting-approval", "scopeKey": "bitbucket:CI/sample-app#2", "summary": "Resolved 2 files" }),
                json!({ "featureId": "analyze-issue", "status": "failed", "scopeKey": "jira:PROJ-7", "error": "Claude exited" }),
            ],
            notes: vec!["Couldn't read your review queue: HTTP 500".into()],
            ..input()
        });
        assert_eq!(d.sections.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["to-review", "tickets", "ready", "notes"]);
        assert_eq!(d.sections[0].items[0].text, "CI/sample-app #9 PR 9 \u{2014} by bob");
        assert_eq!(d.sections[0].items[0].url, None);
        assert_eq!(d.sections[1].items.iter().map(|i| i.tone).collect::<Vec<_>>(), [Tone::Warn, Tone::Neutral]);
        assert_eq!(d.sections[1].items[0].text, "PROJ-7 Login broken (Open)");
        assert_eq!(d.sections[2].items[0].tone, Tone::Bad);
        assert!(d.sections[2].items[0].text.contains("background analyze-issue run for PROJ-7 failed: Claude exited"));
        assert_eq!(d.sections[2].items[1].text, "Conflict resolution ready to review: CI/sample-app#2 \u{2014} Resolved 2 files");
        assert_eq!(d.headline, "Needs you: 1 PR to review, 1 ready for you, 1 background run failed.");
        assert_eq!((d.counts.ready, d.counts.failed_runs), (1, 1), "a failed run is not \"ready\"");
    }

    #[test]
    fn long_lists_are_capped_and_untrusted_text_is_flattened() {
        let many: Vec<Value> = (0..MAX_ITEMS + 3).map(|i| pr(i as i64 + 1, json!({ "title": format!("line\nbreak {i}") }))).collect();
        let d = build_digest(&DigestInput { review_prs: many, ..input() });
        assert_eq!(d.sections[0].items.len(), MAX_ITEMS + 1);
        assert_eq!(d.sections[0].items.last().unwrap().text, "\u{2026}and 3 more");
        assert!(!d.sections[0].items[0].text.contains('\n'));
        assert_eq!(d.sections[0].more.len(), 3, "the hidden items travel with the section");
        let few = build_digest(&DigestInput { review_prs: vec![pr(1, json!({}))], ..input() });
        assert!(serde_json::to_value(&few.sections[0]).unwrap().get("more").is_none());
    }

    #[test]
    fn digest_notification_makes_one_inbox_item_per_day() {
        let d = build_digest(&DigestInput { review_prs: vec![pr(9, json!({}))], ..input() });
        assert_eq!(
            digest_notification(&d, "2026-09-29"),
            json!({ "key": "digest:2026-09-29", "kind": "digest", "title": "Morning digest \u{2014} Needs you: 1 PR to review.", "body": "CI/sample-app #9 PR 9", "url": null, "urgent": false })
        );
        assert!(digest_notification(&build_digest(&input()), "d")["body"].as_str().unwrap().contains("Open \u{2728} Morning digest"));
    }

    #[test]
    fn digest_notification_says_a_background_run_couldnt_read_its_sources_and_points_to_the_button() {
        let d = build_digest(&DigestInput { notes: vec!["Couldn't read your pull requests: no sign-in (open the site in Chrome, or save a token in \u{2699} Settings).".into()], ..input() });
        let n = digest_notification(&d, "2026-09-29");
        assert_eq!(n["title"], "Morning digest \u{2014} couldn't read some sources in the background");
        let body = n["body"].as_str().unwrap();
        assert!(body.contains("Open \u{2728} Morning digest") && body.contains("sign-in") && !body.contains("Couldn't read your pull requests"));
    }

    #[test]
    fn digest_notification_keeps_the_reasons_sources_couldnt_be_read_next_to_the_lines_it_does_show() {
        let d = build_digest(&DigestInput { tickets: vec![json!({ "key": "PROJ-1", "summary": "s" })], notes: vec!["Couldn't read your pull requests: HTTP 401.".into()], ..input() });
        let n = digest_notification(&d, "2026-09-29");
        assert_eq!(n["body"].as_str().unwrap().split('\n').collect::<Vec<_>>(), ["PROJ-1 s", "Couldn't read your pull requests: HTTP 401."]);
    }

    #[test]
    fn every_label_is_one_clean_line_control_invisible_and_bidi_characters_go_markup_stays_literal() {
        let nasty = "x\u{1b}[2Jy\u{202e}z\u{200b}w";
        let d = build_digest(&DigestInput {
            my_prs: vec![json!({ "pr": pr(1, json!({ "project": nasty, "repo": nasty, "title": nasty, "reviewers": [{ "name": nasty, "status": "NEEDS_WORK" }] })), "conflicted": true, "build": null })],
            review_prs: vec![pr(2, json!({ "project": nasty, "authorSlug": nasty }))],
            tickets: vec![json!({ "key": nasty, "summary": "<script>alert(1)</script>", "status": nasty, "priority": "High" })],
            prewarmed: vec![
                json!({ "featureId": nasty, "status": "awaiting-approval", "scopeKey": format!("bitbucket:{nasty}"), "summary": nasty }),
                json!({ "featureId": nasty, "status": "failed", "scopeKey": format!("jira:{nasty}"), "error": nasty }),
            ],
            notes: vec![nasty.into()],
            ..input()
        });
        let all = serde_json::to_string(&d).unwrap();
        assert!(!all.contains("\\u001b") && !all.contains('\u{202e}') && !all.contains('\u{200b}') && !all.contains('\u{85}'), "{all}");
        let tickets = &d.sections.iter().find(|s| s.id == "tickets").unwrap().items[0].text;
        assert!(tickets.contains("<script>alert(1)</script>"), "markup is data; the panel renders it with textContent");
    }

    #[test]
    fn long_titles_are_clipped_in_progress_builds_show_as_busy_an_approved_pr_with_a_running_build_is_not_ok() {
        let d = build_digest(&DigestInput {
            my_prs: vec![
                json!({ "pr": pr(1, json!({ "title": "t".repeat(500) })), "conflicted": false, "build": { "state": "INPROGRESS" } }),
                json!({ "pr": pr(2, json!({ "approvals": 1 })), "conflicted": false, "build": { "state": "INPROGRESS" } }),
                json!({ "pr": pr(3, json!({ "approvals": 1 })), "conflicted": false, "build": { "state": "SUCCESSFUL" } }),
            ],
            ..input()
        });
        let items = &d.sections[0].items;
        assert_eq!(items.iter().map(|i| i.tone).collect::<Vec<_>>(), [Tone::Busy, Tone::Busy, Tone::Ok]);
        assert!(utf16_len(&items[0].text) < 160);
        assert!(items[0].text.contains("build running"));
    }

    #[test]
    fn notes_come_last_and_a_digest_with_only_notes_doesnt_claim_nothing_needs_you() {
        let with = build_digest(&DigestInput { review_prs: vec![pr(9, json!({}))], notes: vec!["Couldn't read your Jira tickets: HTTP 500.".into()], ..input() });
        assert_eq!(with.sections.last().unwrap().id, "notes");
        let only = build_digest(&DigestInput { notes: vec!["Couldn't read your pull requests: HTTP 500.".into()], ..input() });
        assert_eq!(only.sections.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["notes"]);
        assert!(!only.headline.contains("Nothing needs you") && only.headline.contains("couldn't be checked"));
    }

    #[test]
    fn digest_notification_shows_at_most_3_lines_and_survives_a_non_finite_clock() {
        let d = build_digest(&DigestInput {
            review_prs: (1..=5).map(|i| pr(i, json!({}))).collect(),
            tickets: vec![json!({ "key": "PROJ-1", "summary": "s" }), json!({ "key": "PROJ-2", "summary": "t" })],
            ..input()
        });
        assert_eq!(digest_notification(&d, "2026-09-29")["body"].as_str().unwrap().split('\n').count(), 3);
        let iso = regex::Regex::new(r"^\d{4}-\d\d-\d\dT").unwrap();
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let x = build_digest(&DigestInput { now: bad, ..Default::default() });
            assert!(iso.is_match(&x.generated_at), "{bad}");
        }
    }

    fn input_from(v: &Value) -> DigestInput {
        let list = |k: &str| v.get(k).and_then(Value::as_array).cloned().unwrap_or_default();
        DigestInput {
            now: v["now"].as_f64().unwrap(),
            my_prs: list("myPrs"),
            review_prs: list("reviewPrs"),
            tickets: list("tickets"),
            prewarmed: list("prewarmed"),
            notes: list("notes").iter().map(|n| n.as_str().unwrap().to_string()).collect(),
        }
    }

    /// The Node implementation's digest and inbox item for four scenarios (a
    /// full one, hostile text, empty, only notes), compared as whole JSON.
    #[test]
    fn output_matches_what_the_node_implementation_produced() {
        let golden: Value = serde_json::from_str(include_str!("../../../tests/fixtures/digest/golden.json")).unwrap();
        for (name, case) in golden.as_object().unwrap() {
            let digest = build_digest(&input_from(&case["input"]));
            assert_eq!(serde_json::to_value(&digest).unwrap(), case["digest"], "digest {name}");
            assert_eq!(digest_notification(&digest, "2026-09-29"), case["notification"], "notification {name}");
        }
    }
}
