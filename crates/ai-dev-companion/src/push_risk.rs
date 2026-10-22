//! Pre-push risk, consumer side (port of `core/push-risk.js`): which files a
//! push changes were in earlier regressions according to the shared
//! risk-facts/v1 feed, which tests those regressions broke, and whether those
//! tests are known flaky. No LLM call. Every line cites its evidence, and with
//! nothing to go on the answer is "not enough data", never a guess. Pure.

use crate::js_text::{js_to_string, js_trim, locale_compare, slice_utf16, utf16_len};
use crate::risk_facts::Facts;
use serde::Serialize;
use serde_json::Value;

pub const MAX_FILES: usize = 500;
const MAX_PATH: usize = 500;
const MAX_FILE_LINES: usize = 5;
const MAX_FLAKY_LINES: usize = 5;
const STALE_AFTER_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// A push's changed files as repo-relative paths: `./` dropped, absolute, `..`
/// and control characters refused, duplicates removed.
pub fn normalize_changed_files(files: &Value) -> Result<Vec<String>, String> {
    let list = match files {
        Value::Array(a) if a.len() <= MAX_FILES => a,
        _ => return Err(format!("files must be a list of at most {MAX_FILES} paths")),
    };
    let mut out: Vec<String> = Vec::new();
    for f in list {
        let mut p = f.as_str().map_or("", js_trim);
        while let Some(rest) = p.strip_prefix("./") {
            p = rest;
        }
        let bad = p.is_empty()
            || utf16_len(p) > MAX_PATH
            || p.starts_with('/')
            || p.split('/').any(|s| s == "..")
            || p.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}');
        if bad {
            return Err(format!("\"{}\" isn't a repo-relative file path", slice_utf16(&js_to_string(f), 60)));
        }
        if !out.iter().any(|o| o == p) {
            out.push(p.to_string());
        }
    }
    Ok(out)
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {}", if n == 1 { one.to_string() } else { format!("{one}s") })
}

fn list_some<T: ToString>(items: &[T], max: usize) -> String {
    let shown = items.iter().take(max).map(ToString::to_string).collect::<Vec<_>>().join(", ");
    if items.len() > max { format!("{shown} and {} more", items.len() - max) } else { shown }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Evidence {
    File { file: String, builds: Vec<i64>, tests: Vec<String> },
    More,
    Flaky { test: String, failed: i64, of: i64 },
    Stale {
        #[serde(rename = "generatedAt")]
        generated_at: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Line {
    pub text: String,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushRisk {
    pub level: Option<RiskLevel>,
    pub summary: String,
    pub lines: Vec<Line>,
    pub checked: usize,
    pub known: usize,
    pub with_regressions: usize,
    pub generated_at: Option<String>,
}

/// `level`: "high" (a file in 3+ regressions, or 2+ files in any), "medium"
/// (one file with 1-2), "low" (changed files are in the facts with no
/// regressions), or `None`: not enough data (no changed file is in them).
pub fn assess_push_risk(files: &[String], facts: &Facts, now: i64) -> PushRisk {
    let known: Vec<&String> = files.iter().filter(|f| facts.files.contains_key(*f)).collect();
    let mut risky: Vec<(&String, &Vec<crate::risk_facts::Regression>)> =
        known.iter().map(|f| (*f, &facts.files[*f])).filter(|(_, r)| !r.is_empty()).collect();
    risky.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then_with(|| locale_compare(a.0, b.0)));

    let level = if known.is_empty() {
        None
    } else if risky.len() >= 2 || risky.iter().any(|(_, r)| r.len() >= 3) {
        Some(RiskLevel::High)
    } else if risky.len() == 1 {
        Some(RiskLevel::Medium)
    } else {
        Some(RiskLevel::Low)
    };

    let mut lines = Vec::new();
    for (file, regs) in risky.iter().take(MAX_FILE_LINES) {
        let builds: Vec<i64> = regs.iter().map(|g| g.build).collect();
        let mut tests: Vec<String> = Vec::new();
        for t in regs.iter().flat_map(|g| g.tests.iter()) {
            if !tests.contains(t) {
                tests.push(t.clone());
            }
        }
        let tail = if tests.is_empty() { String::new() } else { format!(" (tests: {})", list_some(&tests, 3)) };
        lines.push(Line {
            text: format!("{file} was in {}: {} {}{tail}", plural(regs.len(), "regression"), plural(builds.len(), "build"), list_some(&builds, 5)),
            evidence: Evidence::File { file: (*file).clone(), builds, tests },
        });
    }
    if risky.len() > MAX_FILE_LINES {
        lines.push(Line {
            text: format!("\u{2026}and {} with regressions", plural(risky.len() - MAX_FILE_LINES, "more file")),
            evidence: Evidence::More,
        });
    }

    let mut names: Vec<&String> = Vec::new();
    for n in risky.iter().flat_map(|(_, r)| r.iter().flat_map(|g| g.tests.iter())) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    let flaky: Vec<_> = names.iter().filter_map(|n| facts.tests.get(&js_trim(n).to_lowercase())).filter(|t| t.flaky).collect();
    for t in flaky.iter().take(MAX_FLAKY_LINES) {
        lines.push(Line {
            text: format!("\"{}\" is known flaky (failed {} of {} builds), so a failure there may not be yours", t.name, t.failed, t.of),
            evidence: Evidence::Flaky { test: t.name.clone(), failed: t.failed, of: t.of },
        });
    }

    if let Some(g) = &facts.generated_at {
        if crate::js_date::parse_date_ms(g).is_some_and(|ms| now - ms > STALE_AFTER_MS) {
            lines.push(Line {
                text: format!("The shared facts are from {} and may be out of date", slice_utf16(g, 10)),
                evidence: Evidence::Stale { generated_at: g.clone() },
            });
        }
    }

    let changed = plural(files.len(), "changed file");
    let (is, was) = (if files.len() == 1 { "is" } else { "are" }, if risky.len() == 1 { "was" } else { "were" });
    let summary = match level {
        None => format!("Not enough data: none of the {changed} {is} in the shared test history."),
        Some(RiskLevel::Low) => format!(
            "Low risk: {} of {changed} {} in the shared test history, with no regressions recorded.",
            known.len(),
            if known.len() == 1 { "is" } else { "are" }
        ),
        Some(l) => format!(
            "{} risk: {} of {changed} {was} in earlier regressions.",
            if l == RiskLevel::High { "High" } else { "Medium" },
            risky.len()
        ),
    };
    PushRisk {
        level,
        summary,
        lines,
        checked: files.len(),
        known: known.len(),
        with_regressions: risky.len(),
        generated_at: facts.generated_at.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::risk_facts::validate_facts;
    use serde_json::json;

    const NOW: i64 = 1_790_672_400_000; // 2026-09-29T09:00:00Z

    fn facts() -> Facts {
        validate_facts(&json!({
            "schema": "risk-facts/v1",
            "generatedAt": "2026-09-29T02:00:00Z",
            "tests": [
                { "name": "Login with SSO", "flaky": true, "failed": 8, "of": 30 },
                { "name": "Billing totals", "flaky": false, "failed": 1, "of": 30 },
            ],
            "files": [
                { "path": "src/app/login.ts", "regressions": [{ "build": 812, "tests": ["Login with SSO"] }, { "build": 815, "tests": ["Login with SSO"] }] },
                { "path": "src/app/billing.ts", "regressions": [{ "build": 801, "tests": ["Billing totals"] }, { "build": 806, "tests": [] }, { "build": 809, "tests": [] }] },
                { "path": "src/app/quiet.ts", "regressions": [] },
            ],
        }))
        .unwrap()
    }

    fn files(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn many(n: usize, test_names: &[String], tests: Vec<Value>) -> Facts {
        validate_facts(&json!({
            "schema": "risk-facts/v1",
            "generatedAt": "2026-09-29T02:00:00Z",
            "tests": tests,
            "files": (0..n).map(|i| json!({ "path": format!("src/f{i}.ts"), "regressions": [{ "build": 100 + i, "tests": test_names }] })).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn normalize_changed_files_keeps_repo_relative_paths_only() {
        assert_eq!(normalize_changed_files(&json!(["./a.ts", "a.ts", " b/c.ts "])).unwrap(), ["a.ts", "b/c.ts"]);
        assert_eq!(normalize_changed_files(&json!(["././a.ts"])).unwrap(), ["a.ts"]);
        for bad in ["/etc/passwd", "../x", "a/../../x", "a\nb", "", "a/.."] {
            assert!(normalize_changed_files(&json!([bad])).is_err(), "{bad:?}");
        }
        assert!(normalize_changed_files(&json!([5])).is_err());
        assert!(normalize_changed_files(&json!("a.ts")).is_err());
        assert!(normalize_changed_files(&Value::Array(vec![json!("a"); 501])).is_err());
        assert!(normalize_changed_files(&json!(["a..b/c"])).is_ok(), "`..` inside a name is fine");
        assert_eq!(normalize_changed_files(&json!(["a\nb"])).unwrap_err(), "\"a\nb\" isn't a repo-relative file path");
    }

    #[test]
    fn no_changed_file_in_the_facts_is_not_enough_data_never_a_guess() {
        let r = assess_push_risk(&files(&["README.md", "docs/x.md"]), &facts(), NOW);
        assert_eq!(r.level, None);
        assert_eq!(r.summary, "Not enough data: none of the 2 changed files are in the shared test history.");
        assert!(r.lines.is_empty());
        assert_eq!(assess_push_risk(&files(&["README.md"]), &facts(), NOW).summary, "Not enough data: none of the 1 changed file is in the shared test history.");
    }

    #[test]
    fn known_files_with_no_regressions_are_low() {
        let r = assess_push_risk(&files(&["src/app/quiet.ts", "README.md"]), &facts(), NOW);
        assert_eq!(r.level, Some(RiskLevel::Low));
        assert!(r.summary.starts_with("Low risk: 1 of 2 changed files is in the shared test history"));
    }

    #[test]
    fn one_file_in_one_or_two_regressions_is_medium_citing_builds_tests_and_the_flaky_test() {
        let r = assess_push_risk(&files(&["src/app/login.ts"]), &facts(), NOW);
        assert_eq!(r.level, Some(RiskLevel::Medium));
        assert_eq!(r.summary, "Medium risk: 1 of 1 changed file was in earlier regressions.");
        let texts: Vec<&str> = r.lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "src/app/login.ts was in 2 regressions: 2 builds 812, 815 (tests: Login with SSO)",
                "\"Login with SSO\" is known flaky (failed 8 of 30 builds), so a failure there may not be yours",
            ]
        );
        assert_eq!(
            r.lines[0].evidence,
            Evidence::File { file: "src/app/login.ts".into(), builds: vec![812, 815], tests: vec!["Login with SSO".into()] }
        );
    }

    #[test]
    fn a_file_in_3_plus_regressions_or_2_plus_risky_files_is_high_and_old_facts_are_flagged() {
        assert_eq!(assess_push_risk(&files(&["src/app/billing.ts"]), &facts(), NOW).level, Some(RiskLevel::High));
        let both = assess_push_risk(&files(&["src/app/login.ts", "src/app/billing.ts"]), &facts(), NOW);
        assert_eq!(both.level, Some(RiskLevel::High));
        assert!(matches!(&both.lines[0].evidence, Evidence::File { file, .. } if file == "src/app/billing.ts"), "most regressions first");
        assert_eq!(both.with_regressions, 2);
        let late = assess_push_risk(&files(&["src/app/login.ts"]), &facts(), NOW + 8 * 24 * 3600 * 1000);
        assert!(late.lines.last().unwrap().text.contains("from 2026-09-29 and may be out of date"));
    }

    #[test]
    fn two_risky_files_each_with_one_regression_are_already_high() {
        let f = many(2, &[], vec![]);
        let r = assess_push_risk(&files(&["src/f0.ts", "src/f1.ts"]), &f, NOW);
        assert_eq!((r.level, r.with_regressions), (Some(RiskLevel::High), 2));
        assert_eq!(assess_push_risk(&files(&["src/f0.ts"]), &f, NOW).level, Some(RiskLevel::Medium));
    }

    #[test]
    fn more_than_5_risky_files_give_5_lines_then_a_more_line_with_the_rest_counted() {
        let f = many(8, &[], vec![]);
        let list: Vec<String> = (0..8).map(|i| format!("src/f{i}.ts")).collect();
        let r = assess_push_risk(&list, &f, NOW);
        assert_eq!(r.lines.iter().filter(|l| matches!(l.evidence, Evidence::File { .. })).count(), 5);
        let more = r.lines.iter().find(|l| l.evidence == Evidence::More).unwrap();
        assert_eq!(more.text, "\u{2026}and 3 more files with regressions");
    }

    #[test]
    fn flaky_lines_are_capped_at_5() {
        let names: Vec<String> = (0..8).map(|i| format!("Flaky {i}")).collect();
        let tests = names.iter().map(|n| json!({ "name": n, "flaky": true, "failed": 3, "of": 10 })).collect();
        let f = many(1, &names, tests);
        let r = assess_push_risk(&files(&["src/f0.ts"]), &f, NOW);
        assert_eq!(r.lines.iter().filter(|l| matches!(l.evidence, Evidence::Flaky { .. })).count(), 5);
    }

    #[test]
    fn facts_without_a_generated_at_are_never_called_stale_and_report_none() {
        let mut f = many(1, &[], vec![]);
        f.generated_at = None;
        let r = assess_push_risk(&files(&["src/f0.ts"]), &f, NOW + 400 * 24 * 3600 * 1000);
        assert_eq!(r.generated_at, None);
        assert!(!r.lines.iter().any(|l| matches!(l.evidence, Evidence::Stale { .. })));
        f.generated_at = Some("not a date".into());
        assert!(!assess_push_risk(&files(&["src/f0.ts"]), &f, NOW).lines.iter().any(|l| matches!(l.evidence, Evidence::Stale { .. })));
    }

    #[test]
    fn the_result_serializes_in_the_shape_the_node_service_returned() {
        let r = assess_push_risk(&files(&["src/app/login.ts"]), &facts(), NOW);
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["level"], "medium");
        assert_eq!(v["withRegressions"], 1);
        assert_eq!(v["generatedAt"], "2026-09-29T02:00:00Z");
        assert_eq!(v["lines"][0]["evidence"], json!({ "kind": "file", "file": "src/app/login.ts", "builds": [812, 815], "tests": ["Login with SSO"] }));
        assert_eq!(v["lines"][1]["evidence"], json!({ "kind": "flaky", "test": "Login with SSO", "failed": 8, "of": 30 }));
        let none = serde_json::to_value(assess_push_risk(&files(&["x"]), &facts(), NOW)).unwrap();
        assert_eq!(none["level"], Value::Null);
        let stale = serde_json::to_value(assess_push_risk(&files(&["src/app/login.ts"]), &facts(), NOW + 8 * 24 * 3600 * 1000)).unwrap();
        assert_eq!(stale["lines"][2]["evidence"], json!({ "kind": "stale", "generatedAt": "2026-09-29T02:00:00Z" }));
    }
}
