//! Plain-text formatting for the `companion` command (port of
//! `core/cli-format.js`). Pure. Inputs are the service's JSON answers, read
//! with the same coercions the JS code relied on (`String(x ?? "")`, `||`
//! fallbacks, `now - updatedAt`).
//!
//! Everything the service (or a Jira/Bitbucket author) wrote is untrusted
//! text for a terminal: whitespace collapses, and control characters (ESC, C1,
//! DEL) and invisible/bidi characters are dropped so it can't move the cursor,
//! clear the screen or hide text.

use crate::js_number::js_number;
use crate::js_text::{is_js_space, js_to_string, js_truthy, slice_utf16, utf16_len};
use serde_json::Value;

type V<'a> = Option<&'a Value>;

fn get<'a>(v: &'a Value, key: &str) -> V<'a> {
    v.as_object().and_then(|o| o.get(key))
}

fn items(v: V<'_>) -> &[Value] {
    v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// `Math.round`: halves go up.
fn js_round(x: f64) -> f64 {
    (x + 0.5).floor()
}

pub fn ago(ms: f64) -> String {
    let s = (js_round(ms / 1000.0)).max(0.0);
    if ms.is_nan() {
        return "NaNd ago".into();
    }
    if s < 60.0 {
        format!("{s}s ago")
    } else if s < 3600.0 {
        format!("{}m ago", (s / 60.0).floor())
    } else if s < 86400.0 {
        format!("{}h ago", (s / 3600.0).floor())
    } else {
        format!("{}d ago", (s / 86400.0).floor())
    }
}

/// `None` (null/undefined) is "-".
pub fn duration(ms: Option<f64>) -> String {
    let Some(ms) = ms else { return "-".into() };
    let s = js_round(ms / 1000.0);
    if s < 60.0 {
        format!("{s}s")
    } else if s < 3600.0 {
        format!("{}m {}s", (s / 60.0).floor(), s % 60.0)
    } else {
        format!("{}h {}m", (s / 3600.0).floor(), ((s % 3600.0) / 60.0).floor())
    }
}

fn cut(text: &str, n: usize) -> String {
    if utf16_len(text) > n { format!("{}\u{2026}", slice_utf16(text, n - 1)) } else { text.to_string() }
}

fn is_invisible(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{feff}')
}

/// `String(text ?? "")` with whitespace collapsed, then control and invisible
/// characters dropped, then trimmed. (The order matters: collapsing first
/// turns tabs and newlines into one space; dropping afterwards can leave two.)
pub fn one_line_str(text: &str) -> String {
    let mut out = String::new();
    // `\s+` -> " ": a run of whitespace is one space, including at the ends.
    let mut prev_space = false;
    for ch in text.chars() {
        if is_js_space(ch) {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(ch);
            prev_space = false;
        }
    }
    let cleaned: String = out.chars().filter(|c| !is_invisible(*c)).collect();
    cleaned.trim_matches(is_js_space).to_string()
}

pub fn one_line(v: V<'_>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(v) => one_line_str(&js_to_string(v)),
    }
}

fn pad_end(s: &str, n: usize) -> String {
    format!("{s}{}", " ".repeat(n.saturating_sub(utf16_len(s))))
}

fn pad_start(s: &str, n: usize) -> String {
    format!("{}{s}", " ".repeat(n.saturating_sub(utf16_len(s))))
}

fn num(v: V<'_>) -> f64 {
    js_number(v)
}

/// `a || b || c || ""` over JSON fields.
fn first_truthy<'a>(vs: &[V<'a>]) -> V<'a> {
    vs.iter().copied().find(|v| js_truthy(*v)).flatten()
}

pub fn format_jobs(jobs: &Value, now: f64) -> String {
    let list = items(Some(jobs));
    if list.is_empty() {
        return "No jobs.".into();
    }
    list.iter()
        .map(|j| {
            let summary = first_truthy(&[get(j, "summary"), get(j, "error"), get(j, "progress")]);
            let scope = if js_truthy(get(j, "scopeKey")) { get(j, "scopeKey") } else { None };
            [
                slice_utf16(&one_line(get(j, "id")), 8),
                pad_end(&one_line(get(j, "featureId")), 22),
                pad_end(&one_line(get(j, "status")), 17),
                pad_end(&ago(now - num(get(j, "updatedAt"))), 8),
                pad_end(&one_line(scope), 24),
                cut(&one_line(summary), 60),
            ]
            .join(" ")
            .trim_end_matches(is_js_space)
            .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn format_status(jobs: &Value, now: f64, base_url: &str) -> String {
    let list = items(Some(jobs));
    let mut counts: Vec<(String, usize)> = Vec::new();
    for j in list {
        let status = one_line(get(j, "status"));
        match counts.iter_mut().find(|(s, _)| *s == status) {
            Some((_, n)) => *n += 1,
            None => counts.push((status, 1)),
        }
    }
    let parts: Vec<String> = counts.iter().map(|(s, n)| format!("{n} {s}")).collect();
    let tail = if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) };
    let head = format!("Companion is running at {} \u{2014} {} jobs{tail}.", one_line_str(base_url), list.len());
    let active: Vec<Value> = list
        .iter()
        .filter(|j| matches!(get(j, "status").and_then(Value::as_str), Some("running" | "approving" | "rejecting")))
        .cloned()
        .collect();
    if active.is_empty() { head } else { format!("{head}\n\n{}", format_jobs(&Value::Array(active), now)) }
}

pub fn format_history(detail: &Value, now: f64) -> String {
    let item = get(detail, "item").cloned().unwrap_or(Value::Null);
    let title_part = if js_truthy(get(&item, "title")) { format!("  {}", cut(&one_line(get(&item, "title")), 200)) } else { String::new() };
    let mut lines = vec![format!("{} ({}){title_part}", one_line(get(&item, "key")), one_line(get(&item, "kind")))];
    let repo = if js_truthy(get(&item, "repo")) { format!(" \u{b7} repo {}", one_line(get(&item, "repo"))) } else { String::new() };
    lines.push(format!("  updated {}{repo}", ago(now - num(get(&item, "updatedAt")))));
    if js_truthy(get(&item, "excerpt")) {
        lines.push(format!("  {}", cut(&one_line(get(&item, "excerpt")), 200)));
    }
    let edges = items(get(detail, "edges"));
    if !edges.is_empty() {
        lines.push("Links:".into());
        for e in edges {
            let arrow = if get(e, "direction").and_then(Value::as_str) == Some("out") { "\u{2192}" } else { "\u{2190}" };
            let title = if js_truthy(get(e, "title")) { format!("  {}", cut(&one_line(get(e, "title")), 60)) } else { String::new() };
            lines.push(format!("  {arrow} {} {}{title}", pad_end(&one_line(get(e, "rel")), 10), one_line(get(e, "key"))));
        }
    }
    let facts = items(get(detail, "facts"));
    if !facts.is_empty() {
        lines.push("Facts:".into());
        for f in facts {
            let value = get(f, "value").map_or_else(String::new, Value::to_string);
            lines.push(format!("  {} ({}): {}", one_line(get(f, "kind")), one_line(get(f, "provenance")), cut(&one_line_str(&value), 120)));
        }
    }
    lines.join("\n")
}

pub fn format_metrics(r: &Value) -> String {
    let mut lines = vec![format!("How long jobs took, last {} days:", one_line(get(r, "days")))];
    let metrics = items(get(r, "metrics"));
    if metrics.is_empty() {
        lines.push("  (nothing recorded yet)".into());
    }
    for m in metrics {
        let ms = |k: &str| match get(m, k) {
            None | Some(Value::Null) => None,
            v => Some(num(v)),
        };
        lines.push(format!(
            "  {} {} {}  {} {}",
            pad_end(&one_line(get(m, "featureId")), 24),
            pad_end(&one_line(get(m, "outcome")), 10),
            pad_start(&one_line(get(m, "count")), 3),
            pad_end(&duration(ms("medianMs")), 8),
            duration(ms("avgMs")),
        ));
    }
    let prewarmed = items(get(r, "prewarmed"));
    if !prewarmed.is_empty() {
        lines.push(String::new());
        lines.push("Pre-warmed by the background watchers (used = approved, or opened from the inbox):".into());
        for p in prewarmed {
            let pct = match get(p, "usedFraction") {
                Some(Value::Null) => "-".to_string(),
                v => format!("{}%", js_round(num(v) * 100.0)),
            };
            lines.push(format!(
                "  {} {} run(s)  {} used  {}",
                pad_end(&one_line(get(p, "watcher")), 16),
                pad_start(&one_line(get(p, "runs")), 3),
                pad_start(&one_line(get(p, "used")), 3),
                pad_start(&pct, 4),
            ));
        }
    }
    let matches = items(get(r, "matches"));
    if !matches.is_empty() {
        lines.push(String::new());
        lines.push("Matches:".into());
        for m in matches {
            let title = if js_truthy(get(m, "title")) { get(m, "title") } else { None };
            lines.push(format!("  {} {}", pad_end(&one_line(get(m, "key")), 30), cut(&one_line(title), 60)));
        }
    }
    lines.join("\n")
}

pub fn format_inbox(r: &Value, now: f64) -> String {
    let unseen = get(r, "unseen");
    let list = items(get(r, "items"));
    if list.is_empty() {
        return if js_truthy(unseen) { format!("{} unseen.", one_line(unseen)) } else { "Nothing new.".into() };
    }
    let mut lines = vec![format!("{} unseen:", one_line(unseen))];
    for i in list {
        let mark = if js_truthy(get(i, "urgent")) { "!" } else { "\u{2022}" };
        let seen = if js_truthy(get(i, "seen")) { ", seen" } else { "" };
        lines.push(format!("{mark} {}  ({}{seen})", cut(&one_line(get(i, "title")), 100), ago(now - num(get(i, "createdAt")))));
        if js_truthy(get(i, "body")) {
            lines.push(format!("    {}", cut(&one_line(get(i, "body")), 120)));
        }
        if js_truthy(get(i, "url")) {
            lines.push(format!("    {}", one_line(get(i, "url"))));
        }
    }
    lines.join("\n")
}

pub fn format_digest(digest: &Value) -> String {
    let mut lines = vec![one_line(get(digest, "headline"))];
    for s in items(get(digest, "sections")) {
        lines.push(String::new());
        lines.push(format!("{}:", one_line(get(s, "title"))));
        for item in items(get(s, "items")) {
            let mark = match get(item, "tone").and_then(Value::as_str) {
                Some("bad") => "\u{2717}",
                Some("ok") => "\u{2713}",
                _ => "\u{2022}",
            };
            lines.push(format!("  {mark} {}", cut(&one_line(get(item, "text")), 140)));
        }
    }
    lines.join("\n")
}

/// One pre-push assessment, as `companion precheck` prints it.
pub fn format_risk(r: &Value) -> String {
    if !r.is_object() {
        return "companion precheck: skipped \u{2014} no usable answer".into();
    }
    if js_truthy(get(r, "skipped")) {
        return format!("companion precheck: skipped \u{2014} {}", cut(&one_line(get(r, "reason")), 200));
    }
    let mut lines = vec![format!("companion precheck: {}", cut(&one_line(get(r, "summary")), 300))];
    for l in items(get(r, "lines")) {
        lines.push(format!("  \u{2022} {}", cut(&one_line(get(l, "text")), 300)));
    }
    lines.join("\n")
}

/// `companion inbox --brief`: one line when something is unseen, else "". The
/// Claude Code plugin's SessionStart hook prints it into the session's
/// context, so it carries counts and fixed labels only: never a title, which
/// other people write.
pub fn format_inbox_brief(r: &Value) -> String {
    let unseen = get(r, "unseen").and_then(Value::as_f64).filter(|f| f.fract() == 0.0).unwrap_or(0.0) as i64;
    if unseen <= 0 {
        return String::new();
    }
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for i in items(get(r, "items")) {
        let label = match get(i, "kind").and_then(Value::as_str) {
            Some("conflict") => "conflict resolution",
            Some("analysis") => "background analysis",
            Some("review-request") => "review request",
            Some("digest") => "digest",
            _ => continue,
        };
        match counts.iter_mut().find(|(l, _)| *l == label) {
            Some((_, n)) => *n += 1,
            None => counts.push((label, 1)),
        }
    }
    let parts: Vec<String> = counts.iter().map(|(l, n)| format!("{l}: {n}")).collect();
    let tail = if parts.is_empty() { String::new() } else { format!(" ({})", parts.join(", ")) };
    format!("AI companion: {unseen} new in your inbox{tail}. Run `companion inbox` to see them.")
}

/// `YYYY-MM-DD` of an epoch-millisecond time (`toISOString().slice(0, 10)`),
/// `None` outside the range JS dates cover.
fn iso_day(ms: f64) -> Option<String> {
    if !ms.is_finite() {
        return None;
    }
    let iso = crate::js_date::to_iso_string(ms.floor() as i64)?;
    Some(slice_utf16(&iso, iso.find('T')?))
}

/// `companion similar`: past tickets and PRs like one, each with where its text came from.
pub fn format_similar(r: &Value, label: &str) -> String {
    if !r.is_object() || get(r, "mode").and_then(Value::as_str) == Some("off") {
        return "Similar items are turned off (\u{2699} Settings \u{2192} Local history).".into();
    }
    let list = items(get(r, "items"));
    if list.is_empty() {
        return format!("Nothing similar to {} in the local history yet.", one_line_str(label));
    }
    let how = if get(r, "mode").and_then(Value::as_str) == Some("vector+text") { "by meaning and shared words" } else { "by shared words (no LLM)" };
    let mut lines = vec![format!("Similar to {} ({how}):", one_line_str(label))];
    for i in list {
        let day = get(i, "updatedAt").and_then(Value::as_f64).and_then(iso_day).unwrap_or_default();
        let title = if js_truthy(get(i, "title")) { one_line(get(i, "title")) } else { one_line_str("(no title)") };
        let when = if day.is_empty() { String::new() } else { format!("  ({}, {day})", one_line(get(i, "kind"))) };
        lines.push(format!("  {} {}{when}", pad_end(&one_line(get(i, "key")), 18), cut(&title, 80)));
        if js_truthy(get(i, "analysis")) {
            lines.push(format!("  {} earlier AI output: {}", " ".repeat(18), cut(&one_line(get(i, "analysis")), 140)));
        }
    }
    lines.push(String::new());
    lines.push("Titles are written by people and analyses are earlier AI output \u{2014} check before relying on them.".into());
    lines.join("\n")
}

/// `companion worktrees`: one block per repo, one line per worktree.
pub fn format_worktrees(rows: &Value, now: f64) -> String {
    let list = items(Some(rows));
    if list.is_empty() {
        return "No worktrees: no repositories are set up, or none could be read.".into();
    }
    let truthy = |r: &Value, k: &str| js_truthy(get(r, k));
    let mut out: Vec<String> = Vec::new();
    let mut repo: Option<Value> = None;
    for r in list {
        let key = get(r, "repoKey").cloned().unwrap_or(Value::Null);
        if repo.as_ref() != Some(&key) {
            if repo.is_some() {
                out.push(String::new());
            }
            out.push(one_line(Some(&key)));
            repo = Some(key);
        }
        let who = if truthy(r, "isMainClone") { "clone" } else if truthy(r, "owned") { "tool" } else { "yours" };
        let state = if !truthy(r, "exists") {
            "folder gone".to_string()
        } else {
            let mut parts: Vec<String> = Vec::new();
            parts.push(if truthy(r, "dirty") { format!("{} changed", one_line(get(r, "changed"))) } else { "clean".into() });
            if truthy(r, "inProgress") {
                parts.push(js_to_string(get(r, "inProgress").unwrap()));
            }
            if num(get(r, "unpushed")) > 0.0 {
                parts.push(format!("{} unpushed", js_to_string(get(r, "unpushed").unwrap())));
            }
            if truthy(r, "merged") {
                parts.push("merged".into());
            }
            if truthy(r, "upstreamGone") {
                parts.push("branch gone".into());
            }
            parts.join(", ")
        };
        let sessions = num(get(r, "sessions"));
        let mut links: Vec<String> = Vec::new();
        for k in ["issueKey", "prKey"] {
            if truthy(r, k) {
                links.push(js_to_string(get(r, k).unwrap()));
            }
        }
        if sessions > 0.0 {
            links.push(format!("{} session{}", js_to_string(get(r, "sessions").unwrap()), if sessions == 1.0 { "" } else { "s" }));
        }
        let branch = if truthy(r, "branch") { get(r, "branch") } else { None };
        let branch = if branch.is_some() { one_line(branch) } else { "(detached)".into() };
        let last = if truthy(r, "lastUsed") { ago(now - num(get(r, "lastUsed"))) } else { String::new() };
        out.push(
            format!(
                "  {} {} {} {} {}",
                pad_end(who, 5),
                pad_end(&cut(&branch, 34), 34),
                pad_end(&state, 30),
                pad_end(&last, 8),
                cut(&one_line_str(&links.join(" ")), 40)
            )
            .trim_end_matches(is_js_space)
            .to_string(),
        );
        let prunable = if truthy(r, "prunable") { "   <- can be pruned" } else { "" };
        out.push(format!("        {}{prunable}", one_line(get(r, "dir"))));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use regex::Regex;
    use serde_json::json;

    const NOW: f64 = 10_000_000.0;

    fn jobs() -> Value {
        json!([
            { "id": "11111111-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "featureId": "resolve-conflict", "status": "awaiting-approval", "scopeKey": "bitbucket:CI/sample-app#12", "updatedAt": NOW - 5.0 * 60.0 * 1000.0, "summary": "Merged master in" },
            { "id": "22222222-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "featureId": "analyze-issue", "status": "failed", "scopeKey": "jira:PROJ-7", "updatedAt": NOW - 3.0 * 3600.0 * 1000.0, "error": "Jira said no" },
        ])
    }

    fn re(p: &str, s: &str) -> bool {
        Regex::new(p).unwrap().is_match(s)
    }

    #[test]
    fn format_jobs_prints_one_line_per_job_with_a_short_id_age_and_the_summary_or_error() {
        let out = format_jobs(&jobs(), NOW);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines.len(), 2);
        assert!(re(r"^11111111 +resolve-conflict +awaiting-approval +5m ago +bitbucket:CI/sample-app#12 +Merged master in$", lines[0]), "{}", lines[0]);
        assert!(re(r"^22222222 +analyze-issue +failed +3h ago +jira:PROJ-7 +Jira said no$", lines[1]), "{}", lines[1]);
        assert_eq!(format_jobs(&json!([]), NOW), "No jobs.");
        assert_eq!(format_jobs(&json!(null), NOW), "No jobs.");
    }

    #[test]
    fn format_status_counts_jobs_by_status() {
        let out = format_status(&jobs(), NOW, "http://127.0.0.1:8787");
        assert!(out.starts_with("Companion is running at http://127.0.0.1:8787 \u{2014} 2 jobs (1 awaiting-approval, 1 failed)."), "{out}");
        let running = json!([{ "id": "abcdefgh1", "featureId": "f", "status": "running", "updatedAt": NOW, "progress": "Reading a.ts" }]);
        let out = format_status(&running, NOW, "u");
        assert!(out.contains("\n\nabcdefgh f") && out.ends_with("Reading a.ts"), "{out}");
        assert_eq!(format_status(&json!([]), NOW, "u"), "Companion is running at u \u{2014} 0 jobs.");
    }

    #[test]
    fn format_history_shows_the_item_its_links_in_both_directions_and_its_facts() {
        let out = format_history(
            &json!({
                "item": { "kind": "ticket", "key": "jira:PROJ-7", "title": "Login fails", "repo": "CI/sample-app", "updatedAt": NOW - 2.0 * 86_400_000.0, "excerpt": "It times out" },
                "edges": [
                    { "direction": "out", "rel": "analyzed", "key": "analysis:jira:PROJ-7", "title": "Analysis of PROJ-7" },
                    { "direction": "in", "rel": "links", "key": "job:1111", "title": "analyze-issue awaiting-approval" },
                ],
                "facts": [{ "kind": "note", "value": { "severity": "high" }, "provenance": "user" }],
            }),
            NOW,
        );
        for p in [r"jira:PROJ-7 \(ticket\)", "Login fails", "2d ago", "\u{2192} analyzed +analysis:jira:PROJ-7", "\u{2190} links +job:1111", r#"note \(user\): \{"severity":"high"\}"#] {
            assert!(re(p, &out), "{p} in {out}");
        }
        assert!(out.contains("repo CI/sample-app") && out.contains("It times out"));
    }

    #[test]
    fn format_metrics_prints_a_table_of_counts_and_humanized_durations() {
        let out = format_metrics(&json!({
            "days": 30,
            "metrics": [{ "featureId": "resolve-conflict", "outcome": "approved", "count": 3, "medianMs": 125000, "avgMs": 3600000 }],
            "matches": [{ "kind": "ticket", "key": "jira:PROJ-7", "title": "Login fails" }],
        }));
        assert!(out.contains("last 30 days"));
        assert!(re(r"resolve-conflict +approved +3 +2m 5s +1h 0m", &out), "{out}");
        assert!(re(r"jira:PROJ-7 +Login fails", &out));
        let empty = format_metrics(&json!({ "days": 7, "metrics": [] }));
        assert_eq!(empty, "How long jobs took, last 7 days:\n  (nothing recorded yet)");
        let pre = format_metrics(&json!({ "days": 7, "metrics": [], "prewarmed": [{ "watcher": "conflicts", "runs": 4, "used": 3, "usedFraction": 0.75 }, { "watcher": "reviews", "runs": 0, "used": 0, "usedFraction": null }] }));
        assert!(re(r"conflicts +4 run\(s\) +3 used +75%", &pre) && re(r"reviews +0 run\(s\) +0 used +-", &pre), "{pre}");
    }

    #[test]
    fn one_line_drops_control_c1_zero_width_and_bidi_characters_every_formatter_uses_it() {
        assert_eq!(one_line_str("a\u{1b}[2Jb\u{202e}c\u{200b}d\u{85}e\u{7f} f\n g"), "a[2Jbcde f g");
        // Collapsing runs before dropping, so dropping can leave two spaces.
        assert_eq!(one_line_str("a \u{200b} b"), "a  b");
        assert_eq!(one_line(None), "");
        assert_eq!(one_line(Some(&json!(null))), "");
        assert_eq!(one_line(Some(&json!(42))), "42");
        let nasty = "x\u{1b}[2Jy\u{202e}";
        let out = [
            format_jobs(&json!([{ "id": nasty, "featureId": nasty, "status": nasty, "scopeKey": nasty, "updatedAt": NOW, "summary": nasty }]), NOW),
            format_status(&json!([{ "status": nasty }]), NOW, nasty),
            format_history(&json!({ "item": { "key": nasty, "kind": nasty, "title": nasty, "repo": nasty, "updatedAt": NOW, "excerpt": nasty }, "edges": [{ "direction": "out", "rel": nasty, "key": nasty, "title": nasty }], "facts": [{ "kind": nasty, "provenance": nasty, "value": nasty }] }), NOW),
            format_metrics(&json!({ "days": nasty, "metrics": [{ "featureId": nasty, "outcome": nasty, "count": nasty, "medianMs": 1, "avgMs": 1 }], "prewarmed": [{ "watcher": nasty, "runs": nasty, "used": nasty, "usedFraction": 0.5 }], "matches": [{ "key": nasty, "title": nasty }] })),
            format_inbox(&json!({ "unseen": nasty, "items": [{ "title": nasty, "body": nasty, "url": nasty, "createdAt": NOW }] }), NOW),
            format_digest(&json!({ "headline": nasty, "sections": [{ "title": nasty, "items": [{ "text": nasty }] }] })),
            format_risk(&json!({ "skipped": true, "reason": nasty })),
            format_risk(&json!({ "summary": nasty, "lines": [{ "text": nasty }] })),
            format_similar(&json!({ "mode": "text", "items": [{ "key": nasty, "title": nasty, "kind": nasty, "updatedAt": NOW, "analysis": nasty }] }), nasty),
            format_worktrees(&json!([{ "repoKey": nasty, "branch": nasty, "dir": nasty, "exists": true, "issueKey": nasty }]), NOW),
        ]
        .join("\n");
        assert!(!out.chars().any(|c| is_invisible(c) && c != '\n'), "{out:?}");
        assert_eq!(format_risk(&json!(null)), "companion precheck: skipped \u{2014} no usable answer");
    }

    #[test]
    fn ago_and_duration_humanize_like_node() {
        for (ms, want) in [(0.0, "0s ago"), (499.0, "0s ago"), (500.0, "1s ago"), (59_499.0, "59s ago"), (60_000.0, "1m ago"), (3_599_000.0, "59m ago"), (3_600_000.0, "1h ago"), (86_400_000.0, "1d ago"), (-5000.0, "0s ago"), (f64::NAN, "NaNd ago")] {
            assert_eq!(ago(ms), want, "{ms}");
        }
        for (ms, want) in [(None, "-"), (Some(0.0), "0s"), (Some(59_400.0), "59s"), (Some(125_000.0), "2m 5s"), (Some(3_600_000.0), "1h 0m"), (Some(5_400_000.0), "1h 30m")] {
            assert_eq!(duration(ms), want, "{ms:?}");
        }
    }

    #[test]
    fn inbox_digest_risk_and_brief() {
        assert_eq!(format_inbox(&json!({ "unseen": 0, "items": [] }), NOW), "Nothing new.");
        assert_eq!(format_inbox(&json!({ "unseen": 3, "items": [] }), NOW), "3 unseen.");
        let out = format_inbox(
            &json!({ "unseen": 2, "items": [
                { "title": "Conflict on #12", "body": "Ready.", "url": "https://bb/x", "createdAt": NOW - 60_000.0, "urgent": true },
                { "title": "Digest", "createdAt": NOW - 7_200_000.0, "seen": true },
            ] }),
            NOW,
        );
        assert_eq!(out, "2 unseen:\n! Conflict on #12  (1m ago)\n    Ready.\n    https://bb/x\n\u{2022} Digest  (2h ago, seen)");
        let digest = format_digest(&json!({ "headline": "Morning", "sections": [{ "title": "PRs", "items": [{ "text": "a", "tone": "bad" }, { "text": "b", "tone": "ok" }, { "text": "c" }] }] }));
        assert_eq!(digest, "Morning\n\nPRs:\n  \u{2717} a\n  \u{2713} b\n  \u{2022} c");
        assert_eq!(format_risk(&json!({ "summary": "Low risk", "lines": [{ "text": "x" }, {}] })), "companion precheck: Low risk\n  \u{2022} x\n  \u{2022} ");
        assert_eq!(format_inbox_brief(&json!({ "unseen": 0 })), "");
        assert_eq!(format_inbox_brief(&json!({ "unseen": 1.5 })), "");
        assert_eq!(
            format_inbox_brief(&json!({ "unseen": 3, "items": [{ "kind": "conflict", "title": "SECRET" }, { "kind": "conflict" }, { "kind": "digest" }, { "kind": "weird" }] })),
            "AI companion: 3 new in your inbox (conflict resolution: 2, digest: 1). Run `companion inbox` to see them."
        );
        assert_eq!(format_inbox_brief(&json!({ "unseen": 2 })), "AI companion: 2 new in your inbox. Run `companion inbox` to see them.");
    }

    #[test]
    fn similar_and_worktrees() {
        assert_eq!(format_similar(&json!({ "mode": "off" }), "x"), "Similar items are turned off (\u{2699} Settings \u{2192} Local history).");
        assert_eq!(format_similar(&json!({ "mode": "text", "items": [] }), "PROJ-7"), "Nothing similar to PROJ-7 in the local history yet.");
        let day = 1_790_668_800_000.0; // 2026-09-29
        let out = format_similar(&json!({ "mode": "vector+text", "items": [{ "key": "jira:PROJ-1", "title": "Login", "kind": "ticket", "updatedAt": day, "analysis": "It was the token" }, { "key": "x" }] }), "PROJ-7");
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines[0], "Similar to PROJ-7 (by meaning and shared words):");
        assert_eq!(lines[1], "  jira:PROJ-1        Login  (ticket, 2026-09-29)");
        assert_eq!(lines[2], format!("  {} earlier AI output: It was the token", " ".repeat(18)));
        assert_eq!(lines[3], "  x                  (no title)");
        assert!(out.ends_with("check before relying on them."));
        assert!(format_similar(&json!({ "mode": "text", "items": [{ "key": "k" }] }), "q").contains("by shared words (no LLM)"));
        assert_eq!(iso_day(0.0).as_deref(), Some("1970-01-01"));
        assert_eq!(iso_day(day).as_deref(), Some("2026-09-29"));
        assert_eq!(iso_day(-86_400_000.0).as_deref(), Some("1969-12-31"));
        assert_eq!(iso_day(f64::NAN), None);

        assert_eq!(format_worktrees(&json!([]), NOW), "No worktrees: no repositories are set up, or none could be read.");
        let rows = json!([
            { "repoKey": "CI/app", "isMainClone": true, "exists": true, "branch": "master", "dirty": false, "dir": "/r/app", "lastUsed": NOW - 3_600_000.0 },
            { "repoKey": "CI/app", "owned": true, "exists": true, "branch": "fix/login", "dirty": true, "changed": 3, "inProgress": "merge", "unpushed": 2, "merged": true, "upstreamGone": true, "issueKey": "PROJ-7", "prKey": "CI/app#12", "sessions": 2, "dir": "/w/x", "prunable": true },
            { "repoKey": "CI/other", "exists": false, "dir": "/gone" },
        ]);
        let out = format_worktrees(&rows, NOW);
        let lines: Vec<&str> = out.split('\n').collect();
        assert_eq!(lines[0], "CI/app");
        // Checked against the Node implementation: each row is trimmed at the end.
        assert_eq!(lines[1], "  clone master                             clean                          1h ago");
        assert_eq!(lines[2], "        /r/app");
        assert!(lines[3].starts_with("  tool  fix/login") && lines[3].contains("3 changed, merge, 2 unpushed, merged, branch gone") && lines[3].ends_with("PROJ-7 CI/app#12 2 sessions"), "{}", lines[3]);
        assert_eq!(lines[4], "        /w/x   <- can be pruned");
        assert_eq!(lines[5], "");
        assert_eq!(lines[6], "CI/other");
        assert!(lines[7].starts_with("  yours (detached)") && lines[7].contains("folder gone"), "{}", lines[7]);
    }
}
