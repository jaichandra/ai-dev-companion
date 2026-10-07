// Client for the optional "risk-facts/v1" feed: shared, derived facts about
// test flakiness and per-file regressions, published by a separate project
// (a Jenkins job or small service that watches build events). The companion
// only reads it. Off unless `riskFacts.url` is set; a missing or broken feed
// never breaks a feature — callers get { ok: false, reason } and carry on.
//
//   {
//     "schema": "risk-facts/v1",
//     "generatedAt": "2026-09-29T02:00:00Z",
//     "tests": [ { "name": "Login with SSO", "flaky": true, "failed": 8, "of": 30 } ],
//     "files": [ { "path": "src/app/login.ts", "regressions": [ { "build": 812, "tests": ["Login with SSO"] } ] } ]
//   }
//
// Test names are matched case-insensitively after trimming. Names and paths
// end up in prompts and terminal headers, so any that contain a control
// character (newline included) make the whole document invalid. Pure apart
// from the injected fetch and clock.
const SCHEMA = "risk-facts/v1";
const MAX_ITEMS = 50_000;
const MAX_NAME = 500;
const MAX_REGRESSIONS = 50;
const MAX_TESTS_PER_REGRESSION = 50;
const STALE_LIMIT_MS = 24 * 60 * 60 * 1000;

// eslint-disable-next-line no-control-regex
const CONTROL_CHARS = /[\u0000-\u001f\u007f]/;

const isInt = (v, min) => Number.isSafeInteger(v) && v >= min;
const isName = (v) => typeof v === "string" && v.trim().length > 0 && v.length <= MAX_NAME && !CONTROL_CHARS.test(v);

function isHttpUrl(value) {
  try {
    const u = new URL(value);
    return (u.protocol === "https:" || u.protocol === "http:") && !!u.hostname;
  } catch {
    return false;
  }
}

/** @returns {{ ok: true, facts: { generatedAt: string | null, tests: Map<string, { name: string, flaky: boolean, failed: number, of: number }>, files: Map<string, { build: number, tests: string[] }[]> } } | { ok: false, reason: string }} */
function validateFacts(json) {
  if (!json || typeof json !== "object" || json.schema !== SCHEMA) return { ok: false, reason: `not a ${SCHEMA} document` };
  if (!Array.isArray(json.tests) || json.tests.length > MAX_ITEMS) return { ok: false, reason: "tests must be a list (at most 50000)" };
  const files = json.files === undefined ? [] : json.files;
  if (!Array.isArray(files) || files.length > MAX_ITEMS) return { ok: false, reason: "files must be a list (at most 50000)" };

  const tests = new Map();
  for (const t of json.tests) {
    if (!t || !isName(t.name) || typeof t.flaky !== "boolean") return { ok: false, reason: "a test entry needs a name and a flaky flag" };
    const failed = t.failed === undefined ? 0 : t.failed;
    const of = t.of === undefined ? 0 : t.of;
    if (!isInt(failed, 0) || !isInt(of, 0) || failed > of) return { ok: false, reason: `"${t.name.slice(0, 40)}" has inconsistent failed/of counts` };
    tests.set(t.name.trim().toLowerCase(), { name: t.name.trim(), flaky: t.flaky, failed, of });
  }

  const fileMap = new Map();
  for (const f of files) {
    if (!f || !isName(f.path) || !Array.isArray(f.regressions) || f.regressions.length > MAX_REGRESSIONS) {
      return { ok: false, reason: "a file entry needs a path and a list of regressions" };
    }
    const regressions = [];
    for (const r of f.regressions) {
      if (!r || !isInt(r.build, 1) || !Array.isArray(r.tests) || r.tests.length > MAX_TESTS_PER_REGRESSION || !r.tests.every(isName)) {
        return { ok: false, reason: "a regression needs a build number and a list of test names" };
      }
      regressions.push({ build: r.build, tests: r.tests.map((n) => n.trim()) });
    }
    fileMap.set(f.path.trim(), regressions);
  }

  const generatedAt = typeof json.generatedAt === "string" && !Number.isNaN(Date.parse(json.generatedAt)) ? json.generatedAt : null;
  return { ok: true, facts: { generatedAt, tests, files: fileMap } };
}

function createRiskFactsClient({ fetchImpl = fetch, now = Date.now, ttlMs = 60 * 60 * 1000, timeoutMs = 5000, maxBytes = 5 * 1024 * 1024 } = {}) {
  const cache = new Map(); // url -> { at, facts }
  const inflight = new Map(); // url -> Promise

  async function load(url) {
    const res = await fetchImpl(url, { redirect: "error", signal: AbortSignal.timeout(timeoutMs), headers: { accept: "application/json" } });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const declared = Number(res.headers && res.headers.get ? res.headers.get("content-length") : NaN);
    if (Number.isFinite(declared) && declared > maxBytes) throw new Error("the feed is too large");
    const text = await res.text();
    if (text.length > maxBytes) throw new Error("the feed is too large");
    let json;
    try {
      json = JSON.parse(text);
    } catch {
      throw new Error("not valid JSON");
    }
    const checked = validateFacts(json);
    if (!checked.ok) throw new Error(checked.reason);
    return checked.facts;
  }

  /** @returns {Promise<{ ok: true, facts: object, at: number, stale?: boolean } | { ok: false, reason: string }>} */
  async function get(url) {
    if (typeof url !== "string" || !url.trim()) return { ok: false, reason: "not-configured" };
    if (!isHttpUrl(url.trim())) return { ok: false, reason: "invalid-url" };
    const key = url.trim();
    const hit = cache.get(key);
    if (hit && now() - hit.at < ttlMs) return { ok: true, facts: hit.facts, at: hit.at };
    if (inflight.has(key)) return inflight.get(key);
    const pending = (async () => {
      try {
        const facts = await load(key);
        const at = now();
        cache.set(key, { at, facts });
        return { ok: true, facts, at };
      } catch (err) {
        const old = cache.get(key);
        if (old && now() - old.at < STALE_LIMIT_MS) return { ok: true, facts: old.facts, at: old.at, stale: true };
        return { ok: false, reason: `unreachable: ${err && err.message ? err.message : err}` };
      } finally {
        inflight.delete(key);
      }
    })();
    inflight.set(key, pending);
    return pending;
  }

  return { get, reset: () => { cache.clear(); inflight.clear(); } };
}

/** One client for the whole service, so every feature shares its cache. */
const shared = createRiskFactsClient();

module.exports = { SCHEMA, validateFacts, createRiskFactsClient, isHttpUrl, shared };
