//! Reading a Jenkins junit test report: which cases failed, and a stable
//! identity for each across builds (port of `core/jenkins-testreport.js`).
//! Generic Jenkins/junit logic (the TestCafe and Playwright quirks it handles
//! are about the report, not about any one pipeline), shared by Diagnose build
//! and by a pack's build-history features. Pure functions over JSON.

use crate::js_number::js_number;
use crate::js_text::{js_to_string, js_trim, js_truthy};
use serde_json::Value;
use std::collections::HashMap;

/// junit `status` values that mean "this test did not pass". FAILED vs
/// REGRESSION IS the new-vs-existing distinction: the same one Jenkins' own
/// Tests tab reports.
pub const FAILING_STATUSES: [&str; 2] = ["FAILED", "REGRESSION"];

const SUFFIX_START: &str = "(screenshots:";

/// The test's name with any run-specific screenshot path stripped off.
///
/// The TestCafe suites append the screenshot directory for *this run* into the
/// test name on failure, e.g. `Test Monitor Actions (screenshots: /screenshots/2026-09-18_18-57-24/...)`,
/// once per browser. That matters far more than it looks: Jenkins decides a
/// test's `age` and `failedSince` by matching className+name against the
/// previous build, so a name that changes every run can never be matched and a
/// test that has been failing for weeks reports age 1 forever. Normalizing
/// gives it a stable identity. Playwright names are already stable.
///
/// Equivalent to replacing `/(?:\s*\(screenshots:[^)]*\))+\s*$/` and trimming;
/// a path can't contain ")", so scanning back from the end is exact.
pub fn normalize_test_name(name: &str) -> String {
    let mut s = js_trim(name);
    while let Some(inner) = s.strip_suffix(')') {
        let Some(i) = inner.rfind(SUFFIX_START) else { break };
        if inner[i + SUFFIX_START.len()..].contains(')') {
            break;
        }
        s = js_trim(&s[..i]);
    }
    s.to_string()
}

/// True when normalizing actually changed the name, i.e. Jenkins' own
/// age/failedSince for this case were computed against an identity that won't
/// recur, so they can't be trusted and the failure has to be dated by walking
/// previous builds instead.
pub fn is_volatile_test_name(name: &str) -> bool {
    normalize_test_name(name) != js_trim(name)
}

/// Stable identity for a test case across builds.
pub fn test_key(class_name: &str, name: &str) -> String {
    format!("{class_name}::{}", normalize_test_name(name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailingCase {
    pub key: String,
    pub class_name: String,
    pub name: String,
    pub normalized_name: String,
    pub suite_name: String,
    pub status: String,
    pub age: i64,
    pub failed_since: i64,
    pub error_details: String,
    pub volatile: bool,
}

/// `x || ""` for a JSON field, as text.
fn text_or_empty(v: Option<&Value>) -> String {
    if js_truthy(v) { js_to_string(v.unwrap()) } else { String::new() }
}

/// `Number(x) || 0`.
fn number_or_zero(v: Option<&Value>) -> i64 {
    let n = js_number(v);
    if n.is_nan() { 0 } else { n as i64 }
}

/// The failing cases of a `testReport/api/json` response, flattened out of its
/// suites and deduplicated by test key.
///
/// Dedup is required, not cosmetic: a Playwright project configured for
/// several browsers emits one case entry per browser with *identical*
/// className+name, so counting raw entries double-counts one broken test. When
/// duplicates disagree we keep the one with the oldest known failure, the
/// conservative choice, since it argues the failure is pre-existing.
pub fn extract_failing_cases(report: &Value) -> Vec<FailingCase> {
    let mut order: Vec<FailingCase> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let suites = report.get("suites").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
    for suite in suites {
        let cases = suite.get("cases").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]);
        for case in cases {
            if js_truthy(case.get("skipped")) {
                continue;
            }
            let Some(status) = case.get("status").and_then(Value::as_str).filter(|s| FAILING_STATUSES.contains(s)) else { continue };
            let class_name = text_or_empty(case.get("className"));
            let name = match case.get("name") {
                None | Some(Value::Null) => String::new(),
                Some(v) => js_to_string(v),
            };
            let entry = FailingCase {
                key: test_key(&class_name, &name),
                normalized_name: normalize_test_name(&name),
                volatile: is_volatile_test_name(&name),
                class_name,
                name,
                suite_name: text_or_empty(suite.get("name")),
                status: status.to_string(),
                age: number_or_zero(case.get("age")),
                failed_since: number_or_zero(case.get("failedSince")),
                error_details: text_or_empty(case.get("errorDetails")),
            };
            match index.get(&entry.key) {
                // `Map.set` on an existing key keeps its position.
                Some(&i) => {
                    if older_failure(&entry, &order[i]) {
                        order[i] = entry;
                    }
                }
                None => {
                    index.insert(entry.key.clone(), order.len());
                    order.push(entry);
                }
            }
        }
    }
    order
}

/// Whether `a` claims an older first-failure than `b`.
///
/// Ranks on `age` rather than `failedSince`, because age is the field that is
/// always populated on a failing case, and the two say the same thing
/// (failedSince === buildNumber - age + 1): ranking on age agrees with the
/// reported first failing build even when failedSince is missing, which
/// ranking on failedSince alone did not.
pub fn older_failure(a: &FailingCase, b: &FailingCase) -> bool {
    if a.age != b.age {
        return a.age > b.age;
    }
    if a.failed_since > 0 && b.failed_since <= 0 {
        return true;
    }
    if a.failed_since <= 0 {
        return false;
    }
    a.failed_since < b.failed_since
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_test_name_strips_run_specific_screenshot_suffixes() {
        for (input, want) in [
            ("Test Monitor Actions (screenshots: /screenshots/2026-09-18_18-57-24/a.png)", "Test Monitor Actions"),
            ("T (screenshots: /a/b) (screenshots: /c/d)", "T"),
            ("T (screenshots: /a/b)   ", "T"),
            ("T(screenshots:/a)", "T"),
            ("  spec.ts \u{203a} describe \u{203a} test  ", "spec.ts \u{203a} describe \u{203a} test"),
            ("T (screenshots: /a) in the middle", "T (screenshots: /a) in the middle"),
            ("(screenshots: x) junk (screenshots: y)", "(screenshots: x) junk"),
            ("T (screenshots: x)y)", "T (screenshots: x)y)"),
            ("T (screenshot: x)", "T (screenshot: x)"),
            ("", ""),
        ] {
            assert_eq!(normalize_test_name(input), want, "{input:?}");
        }
    }

    #[test]
    fn volatile_names_are_the_ones_normalizing_changed() {
        assert!(is_volatile_test_name("T (screenshots: /x)"));
        assert!(!is_volatile_test_name("T"));
        assert!(!is_volatile_test_name("  T  "), "plain trimming is not volatility");
    }

    #[test]
    fn test_key_is_class_and_normalized_name() {
        assert_eq!(test_key("Suite", "T (screenshots: /x)"), "Suite::T");
        assert_eq!(test_key("", "T"), "::T");
    }

    fn report(cases: Value) -> Value {
        json!({ "suites": [{ "name": "S1", "cases": cases }] })
    }

    #[test]
    fn extract_failing_cases_keeps_failed_and_regression_and_drops_passed_and_skipped() {
        let r = report(json!([
            { "className": "A", "name": "ok", "status": "PASSED" },
            { "className": "A", "name": "fixed", "status": "FIXED" },
            { "className": "A", "name": "skip", "status": "FAILED", "skipped": true },
            { "className": "A", "name": "new", "status": "FAILED", "age": 1, "failedSince": 50, "errorDetails": "boom" },
            { "className": "B", "name": "old", "status": "REGRESSION", "age": 4, "failedSince": 47 },
        ]));
        let got = extract_failing_cases(&r);
        let keys: Vec<&str> = got.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["A::new", "B::old"]);
        assert_eq!(got[0].error_details, "boom");
        assert_eq!((got[1].status.as_str(), got[1].age, got[1].failed_since, got[0].suite_name.as_str()), ("REGRESSION", 4, 47, "S1"));
    }

    #[test]
    fn duplicate_cases_collapse_keeping_the_oldest_failure_in_the_first_position() {
        let r = report(json!([
            { "className": "A", "name": "dup", "status": "FAILED", "age": 1, "failedSince": 50 },
            { "className": "A", "name": "other", "status": "FAILED", "age": 1, "failedSince": 50 },
            { "className": "A", "name": "dup", "status": "REGRESSION", "age": 6, "failedSince": 45 },
        ]));
        let got = extract_failing_cases(&r);
        assert_eq!(got.iter().map(|c| c.key.as_str()).collect::<Vec<_>>(), ["A::dup", "A::other"]);
        assert_eq!((got[0].age, got[0].status.as_str()), (6, "REGRESSION"));
    }

    #[test]
    fn testcafe_names_with_different_screenshot_paths_are_one_test_and_flagged_volatile() {
        let r = report(json!([
            { "className": "C", "name": "T (screenshots: /s/2026-09-18/a.png)", "status": "FAILED", "age": 1, "failedSince": 9 },
            { "className": "C", "name": "T (screenshots: /s/2026-09-19/b.png)", "status": "FAILED", "age": 1, "failedSince": 9 },
        ]));
        let got = extract_failing_cases(&r);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].normalized_name.as_str(), got[0].volatile), ("T", true));
        assert!(got[0].name.contains("screenshots"), "the raw name is kept");
    }

    #[test]
    fn missing_or_odd_fields_default_sensibly() {
        assert!(extract_failing_cases(&json!(null)).is_empty());
        assert!(extract_failing_cases(&json!({ "suites": "x" })).is_empty());
        assert!(extract_failing_cases(&json!({ "suites": [{}] })).is_empty());
        let got = extract_failing_cases(&report(json!([{ "status": "FAILED", "age": "abc", "failedSince": "7" }])));
        assert_eq!(got.len(), 1);
        let c = &got[0];
        assert_eq!((c.key.as_str(), c.class_name.as_str(), c.name.as_str(), c.age, c.failed_since), ("::", "", "", 0, 7));
    }

    fn case(age: i64, failed_since: i64) -> FailingCase {
        FailingCase {
            key: "k".into(),
            class_name: String::new(),
            name: String::new(),
            normalized_name: String::new(),
            suite_name: String::new(),
            status: "FAILED".into(),
            age,
            failed_since,
            error_details: String::new(),
            volatile: false,
        }
    }

    #[test]
    fn older_failure_ranks_on_age_then_on_a_known_earlier_first_failure() {
        assert!(older_failure(&case(5, 0), &case(2, 99)), "age decides first");
        assert!(!older_failure(&case(2, 99), &case(5, 0)));
        assert!(older_failure(&case(3, 40), &case(3, 0)), "a known failedSince beats an unknown one");
        assert!(!older_failure(&case(3, 0), &case(3, 40)));
        assert!(!older_failure(&case(3, 0), &case(3, 0)));
        assert!(older_failure(&case(3, 40), &case(3, 41)));
        assert!(!older_failure(&case(3, 41), &case(3, 40)));
        assert!(!older_failure(&case(3, 40), &case(3, 40)));
    }
}
