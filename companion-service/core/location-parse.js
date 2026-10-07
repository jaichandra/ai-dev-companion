// Finds "file:line[:column]" locations in a pasted stack trace or log
// excerpt (JS/webpack, TypeScript, Java, Python, Go, Playwright/TestCafe).
// Pure. The text is untrusted (it comes from a web page), so this only
// extracts candidates; core/location-resolve.js decides whether one names a
// real tracked file, and nothing here touches the filesystem.
const SOURCE_EXT =
  "(?:[cm]?[jt]sx?|vue|svelte|py|go|java|kt|kts|scala|groovy|rb|rs|cs|php|swift|cc|cpp|hpp|c|h|json|ya?ml|sh|css|scss)";

const MAX_TEXT = 8192;
const MAX_RESULTS = 20;
const MAX_PATH = 500;

// Python:  File "/app/tests/test_login.py", line 57, in test_x
const PYTHON_RE = /File "([^"]+)", line (\d+)/g;

// Java/Kotlin/Scala/Groovy:  at com.example.ci.LoginPage.click(LoginPage.java:88)
const JAVA_RE = /\bat\s+((?:[a-z_][\w$]*\.)+)[A-Z][\w$]*[\w$.<>]*\(([\w$]+\.(?:java|kt|scala|groovy)):(\d+)\)/g;

// Everything else: path:line, path:line:col, path(line,col), with an
// optional webpack:// or file:// prefix and an optional ?query before the
// position. A path must start at a token boundary, so a URL such as
// https://cdn.example.com/bundle.js:1:2 (not a local file) yields nothing.
const GENERIC_RE = new RegExp(
  "(?<=^|[\\s(\"'\\[=>@,;])" +
    "(?:(?:webpack|webpack-internal):\\/\\/[^\\s\\/]*\\/|file:\\/\\/)?" +
    "([^\\s:()'\"<>|,;?]+?\\." +
    SOURCE_EXT +
    ")" +
    "(?:(?:\\?[^\\s:()'\"]*)?:(\\d+)(?::(\\d+))?|\\((\\d+),(\\d+)\\))",
  "g",
);

function cleanPath(p) {
  let out = p;
  try {
    out = decodeURIComponent(out);
  } catch {
    /* keep the raw text */
  }
  out = out.replace(/\\/g, "/").replace(/^(?:\.\/)+/, "");
  if (!out || out.length > MAX_PATH) return null;
  if (out.split("/").includes("..")) return null;
  return out;
}

function toPosInt(s) {
  const n = Number(s);
  return Number.isSafeInteger(n) && n >= 1 && n <= 10_000_000 ? n : null;
}

/** @returns {{ path: string, line: number, column: number | null }[]} in the order found, de-duplicated. */
function parseLocations(text) {
  if (typeof text !== "string" || !text.trim()) return [];
  const source = text.length > MAX_TEXT ? text.slice(0, MAX_TEXT) : text;
  const found = []; // { start, end, path, line, column }

  const add = (start, end, rawPath, rawLine, rawCol) => {
    const p = cleanPath(rawPath);
    const line = toPosInt(rawLine);
    if (!p || line === null) return;
    const column = rawCol === undefined || rawCol === null ? null : toPosInt(rawCol);
    found.push({ start, end, path: p, line, column });
  };

  for (const m of source.matchAll(PYTHON_RE)) add(m.index, m.index + m[0].length, m[1], m[2], null);
  for (const m of source.matchAll(JAVA_RE)) {
    add(m.index, m.index + m[0].length, `${m[1].replace(/\./g, "/")}${m[2]}`, m[3], null);
  }
  const claimed = found.map((f) => [f.start, f.end]);
  for (const m of source.matchAll(GENERIC_RE)) {
    const start = m.index;
    const end = m.index + m[0].length;
    if (claimed.some(([s, e]) => start < e && end > s)) continue; // already read as Python/Java
    add(start, end, m[1], m[2] ?? m[4], m[3] ?? m[5]);
  }

  found.sort((a, b) => a.start - b.start);
  const seen = new Set();
  const out = [];
  for (const f of found) {
    const key = `${f.path}\u0000${f.line}\u0000${f.column}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ path: f.path, line: f.line, column: f.column });
    if (out.length >= MAX_RESULTS) break;
  }
  return out;
}

module.exports = { MAX_TEXT, parseLocations };
