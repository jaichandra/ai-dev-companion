// Pure helpers for "Diagnose a failed build": validating the build URL the
// browser sends, and building what Claude Code is started with. The URL comes
// from a web page, so it is re-validated here (same origin as the configured
// Jenkins, plain job path, numeric build) and rebuilt from its parts — only
// the rebuilt URL is ever put in a prompt.

const JOB_SEGMENT = /^[A-Za-z0-9][A-Za-z0-9._ -]{0,99}$/;
const MAX_FLAKY_LINES = 5;

function decode(segment) {
  try {
    return decodeURIComponent(segment);
  } catch {
    return null;
  }
}

// NOTE: chrome-extension/jenkins-urls.js (jenkinsBuildFromUrl, JOB_SEGMENT)
// holds a copy of this parser — keep the two in step. The test
// url-parser-twins.test.js runs both on one table of URLs.
//
// Known limits: a Blue Ocean multibranch build whose branch name equals the
// pipeline's last segment collapses into one job (the URL alone can't tell
// "team/repo" + branch "repo" from the single job "team/repo"); and a path
// prefix on the configured baseUrl (Jenkins served under /jenkins/) is
// ignored — only the origin is compared and rebuilt.
/**
 * Classic (/job/<a>/job/<b>/<n>/…) and Blue Ocean
 * (/blue/organizations/<org>/<pipeline>/detail/<pipeline-or-branch>/<n>/…)
 * build URLs of the configured Jenkins. Multibranch branch names containing
 * "/" are not supported and give null.
 * @returns {{ jobName: string, number: number, buildUrl: string } | null}
 */
function parseBuildUrl(input, baseUrl) {
  let url;
  let base;
  try {
    url = new URL(input);
    base = new URL(baseUrl);
  } catch {
    return null;
  }
  if (url.origin !== base.origin || url.username || url.password) return null;
  const segs = url.pathname.split("/").filter(Boolean);

  let names;
  let number;
  if (segs[0] === "blue" && segs[1] === "organizations" && segs[4] === "detail" && segs.length >= 7) {
    const pipeline = decode(segs[3]);
    const leaf = decode(segs[5]);
    if (pipeline === null || leaf === null) return null;
    names = pipeline.split("/");
    if (leaf !== names[names.length - 1]) names.push(leaf);
    number = segs[6];
  } else {
    names = [];
    let i = 0;
    while (i + 1 < segs.length && segs[i] === "job") {
      const name = decode(segs[i + 1]);
      if (name === null) return null;
      names.push(name);
      i += 2;
    }
    number = segs[i];
  }
  if (names.length === 0 || !/^[1-9]\d{0,8}$/.test(number || "")) return null;
  if (!names.every((n) => JOB_SEGMENT.test(n))) return null;
  const jobPath = names.map((n) => `/job/${encodeURIComponent(n)}`).join("");
  return { jobName: names.join("/"), number: Number(number), buildUrl: `${url.origin}${jobPath}/${number}/` };
}

function truncate(text, max) {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

/** Lines about the failing tests that the shared feed calls flaky (at most 5). */
function flakyContext(facts, testNames) {
  const lines = [];
  const seen = new Set();
  for (const raw of testNames || []) {
    const key = String(raw).trim().toLowerCase();
    if (!key || seen.has(key)) continue;
    seen.add(key);
    const t = facts && facts.tests ? facts.tests.get(key) : undefined;
    if (t && t.flaky) {
      const name = truncate(String(t.name).replace(/[\u0000-\u001f\u007f-\u009f]/g, " ").replace(/\s+/g, " ").trim(), 120);
      const count = (n) => (Number.isSafeInteger(n) && n >= 0 ? n : 0);
      lines.push(`- "${name}" is known to be flaky (failed ${count(t.failed)} of the last ${count(t.of)} builds).`);
      if (lines.length >= MAX_FLAKY_LINES) break;
    }
  }
  return lines;
}

/** The prompt for a plain Claude Code session (when no pack offers a richer way to diagnose). */
function buildBasicPrompt({ buildUrl, jobName, number, flakyLines = [] }) {
  const parts = [
    `Diagnose why Jenkins build #${number} of "${jobName}" failed.`,
    `Build: ${buildUrl}`,
    "",
    "Work read-only. Find and read the console output and the test report (use the Jenkins tools you have; if you have none, say what you would need). Explain, in this order:",
    "1. What failed — the first real error, not the last line.",
    "2. Whether it looks like a regression from a code change, a flaky test, or an infrastructure problem, and why.",
    "3. The most likely cause (suspect commits or files, if you can tell) and what to try next.",
    "Do not change any files, re-run builds or post anything.",
  ];
  if (flakyLines.length > 0) {
    parts.push("", "Context from the team's shared test history (it may be out of date):", ...flakyLines);
  }
  return parts.join("\n");
}

module.exports = { parseBuildUrl, flakyContext, buildBasicPrompt };
