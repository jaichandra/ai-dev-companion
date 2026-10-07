// Pre-push risk, consumer side: which files a push changes were in earlier
// regressions according to the shared risk-facts/v1 feed (core/risk-facts.js,
// produced by the separate risk-facts project), which tests those
// regressions broke, and whether those tests are known flaky. No LLM call.
// Every line cites its evidence, and with nothing to go on the answer is
// "not enough data", never a guess. Pure.
const MAX_FILES = 500;
const MAX_PATH = 500;
const MAX_FILE_LINES = 5;
const MAX_FLAKY_LINES = 5;
const STALE_AFTER_MS = 7 * 24 * 60 * 60 * 1000;

/** A push's changed files as repo-relative paths: `./` dropped, absolute,
 * `..` and control characters refused, duplicates removed. */
function normalizeChangedFiles(files) {
  if (!Array.isArray(files) || files.length > MAX_FILES) return { ok: false, reason: `files must be a list of at most ${MAX_FILES} paths` };
  const out = [];
  for (const f of files) {
    const p = typeof f === "string" ? f.trim().replace(/^(\.\/)+/, "") : "";
    if (!p || p.length > MAX_PATH || p.startsWith("/") || /(^|\/)\.\.(\/|$)/.test(p) || /[\u0000-\u001f\u007f]/.test(p)) {
      return { ok: false, reason: `"${String(f).slice(0, 60)}" isn't a repo-relative file path` };
    }
    if (!out.includes(p)) out.push(p);
  }
  return { ok: true, value: out };
}

const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

function listSome(items, max) {
  const shown = items.slice(0, max).join(", ");
  return items.length > max ? `${shown} and ${items.length - max} more` : shown;
}

/**
 * `{level, summary, lines, checked, known, withRegressions, generatedAt}`.
 * level: "high" (a file in 3+ regressions, or 2+ files in any), "medium"
 * (one file with 1-2), "low" (changed files are in the facts with no
 * regressions), or null — not enough data (no changed file is in them).
 * `facts` is validateFacts(...).facts: `{tests: Map, files: Map, generatedAt}`.
 */
function assessPushRisk({ files, facts, now = Date.now() }) {
  const known = files.filter((f) => facts.files.has(f));
  const risky = known
    .map((file) => ({ file, regressions: facts.files.get(file) }))
    .filter((r) => r.regressions.length > 0)
    .sort((a, b) => b.regressions.length - a.regressions.length || a.file.localeCompare(b.file));

  let level = null;
  if (known.length > 0) {
    if (risky.length >= 2 || risky.some((r) => r.regressions.length >= 3)) level = "high";
    else if (risky.length === 1) level = "medium";
    else level = "low";
  }

  const lines = [];
  for (const r of risky.slice(0, MAX_FILE_LINES)) {
    const builds = r.regressions.map((g) => g.build);
    const tests = [...new Set(r.regressions.flatMap((g) => g.tests))];
    lines.push({
      text: `${r.file} was in ${plural(r.regressions.length, "regression")}: ${plural(builds.length, "build")} ${listSome(builds, 5)}${
        tests.length ? ` (tests: ${listSome(tests, 3)})` : ""
      }`,
      evidence: { kind: "file", file: r.file, builds, tests },
    });
  }
  if (risky.length > MAX_FILE_LINES) {
    lines.push({ text: `…and ${plural(risky.length - MAX_FILE_LINES, "more file")} with regressions`, evidence: { kind: "more" } });
  }

  const flaky = [];
  for (const name of new Set(risky.flatMap((r) => r.regressions.flatMap((g) => g.tests)))) {
    const t = facts.tests.get(name.trim().toLowerCase());
    if (t && t.flaky) flaky.push(t);
  }
  for (const t of flaky.slice(0, MAX_FLAKY_LINES)) {
    lines.push({
      text: `"${t.name}" is known flaky (failed ${t.failed} of ${t.of} builds), so a failure there may not be yours`,
      evidence: { kind: "flaky", test: t.name, failed: t.failed, of: t.of },
    });
  }

  const generatedMs = facts.generatedAt ? Date.parse(facts.generatedAt) : NaN;
  if (Number.isFinite(generatedMs) && now - generatedMs > STALE_AFTER_MS) {
    lines.push({ text: `The shared facts are from ${facts.generatedAt.slice(0, 10)} and may be out of date`, evidence: { kind: "stale", generatedAt: facts.generatedAt } });
  }

  let summary;
  if (level === null) {
    summary = `Not enough data: none of the ${plural(files.length, "changed file")} ${files.length === 1 ? "is" : "are"} in the shared test history.`;
  } else if (level === "low") {
    summary = `Low risk: ${known.length} of ${plural(files.length, "changed file")} ${known.length === 1 ? "is" : "are"} in the shared test history, with no regressions recorded.`;
  } else {
    summary = `${level === "high" ? "High" : "Medium"} risk: ${risky.length} of ${plural(files.length, "changed file")} ${risky.length === 1 ? "was" : "were"} in earlier regressions.`;
  }
  return {
    level,
    summary,
    lines,
    checked: files.length,
    known: known.length,
    withRegressions: risky.length,
    generatedAt: facts.generatedAt || null,
  };
}

module.exports = { MAX_FILES, normalizeChangedFiles, assessPushRisk };
