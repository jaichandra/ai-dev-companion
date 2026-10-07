// Reading a Jenkins junit test report: which cases failed, and a stable identity for
// each across builds. Generic Jenkins/junit logic (the TestCafe and Playwright quirks
// it handles are about the report, not about any one pipeline), shared by Diagnose
// build and by a pack's build-history features. Pure functions over JSON.

/** junit `status` values that mean "this test did not pass". FAILED vs
 * REGRESSION IS the new-vs-existing distinction — the same one Jenkins'
 * own Tests tab reports — see classifyFailures. */
const FAILING_STATUSES = new Set(["FAILED", "REGRESSION"]);

// ---- Test identity ----

// The TestCafe suites append the screenshot directory for *this run* into
// the test name on failure, e.g.
//   "Test Monitor Actions (screenshots: /screenshots/2026-09-18_18-57-24/...)"
// A path can't contain ")", so [^)]* is both sufficient and safe from
// catastrophic backtracking. Repeated because a test that failed on
// several browsers gets one suffix per browser.
const SCREENSHOT_SUFFIX = /(?:\s*\(screenshots:[^)]*\))+\s*$/;

/**
 * The test's name with any run-specific screenshot path stripped off.
 *
 * This matters far more than it looks. Jenkins decides a test's `age` and
 * `failedSince` by matching className+name against the previous build's
 * results — so a name that changes every run can never be matched, and a
 * TestCafe test that has been failing for weeks reports age:1 forever,
 * i.e. it always looks like a brand-new failure. Normalizing gives those
 * tests a stable identity so firstRegressionBuildFromSets can trace their
 * history across builds at all. Playwright suites (className = spec
 * path, name = "describe › test") are already stable and pass through
 * unchanged.
 */
function normalizeTestName(name) {
  return String(name == null ? "" : name)
    .replace(SCREENSHOT_SUFFIX, "")
    .trim();
}

/** True when normalizing actually changed the name — i.e. Jenkins' own
 * age/failedSince for this case were computed against an identity that
 * won't recur, so they can't be trusted and the failure has to be dated by
 * walking previous builds instead. */
function isVolatileTestName(name) {
  return normalizeTestName(name) !== String(name == null ? "" : name).trim();
}

/** Stable identity for a test case across builds. */
function testKey(testCase) {
  return `${testCase.className || ""}::${normalizeTestName(testCase.name)}`;
}

// ---- Reading a test report ----

/**
 * The failing cases of a `testReport/api/json` response, flattened out of
 * its suites and deduplicated by testKey.
 *
 * Dedup is required, not cosmetic: a Playwright project configured for
 * several browsers emits one case entry per browser with *identical*
 * className+name (the "[chromium]"/"[webkit]" marker only appears in the
 * console, not in the junit XML), so counting raw entries double-counts
 * one broken test. When duplicates disagree we keep the one with the
 * oldest known failure — the conservative choice, since it argues the
 * failure is pre-existing rather than new.
 */
function extractFailingCases(report) {
  const byKey = new Map();
  for (const suite of (report && report.suites) || []) {
    for (const testCase of suite.cases || []) {
      if (testCase.skipped) continue;
      if (!FAILING_STATUSES.has(testCase.status)) continue;
      const key = testKey(testCase);
      const entry = {
        key,
        className: testCase.className || "",
        name: String(testCase.name == null ? "" : testCase.name),
        normalizedName: normalizeTestName(testCase.name),
        suiteName: suite.name || "",
        status: testCase.status,
        age: Number(testCase.age) || 0,
        failedSince: Number(testCase.failedSince) || 0,
        errorDetails: testCase.errorDetails || "",
        volatile: isVolatileTestName(testCase.name),
      };
      const existing = byKey.get(key);
      if (!existing || olderFailure(entry, existing)) byKey.set(key, entry);
    }
  }
  return [...byKey.values()];
}

/** Whether `a` claims an older first-failure than `b`.
 *
 * Ranks on `age` rather than `failedSince`, because age is the field that
 * is always populated on a failing case, and the two say the same thing
 * (failedSince === buildNumber - age + 1) — so ranking on age agrees with
 * reportedFirstFailingBuild even when failedSince is missing, which
 * ranking on failedSince alone did not. */
function olderFailure(a, b) {
  if (a.age !== b.age) return a.age > b.age;
  if (a.failedSince > 0 && b.failedSince <= 0) return true;
  if (a.failedSince <= 0) return false;
  return a.failedSince < b.failedSince;
}

module.exports = {
  FAILING_STATUSES,
  normalizeTestName,
  isVolatileTestName,
  testKey,
  extractFailingCases,
  olderFailure,
};
