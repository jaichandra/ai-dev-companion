// Builds the URL that opens a file at a line in Cursor or VS Code. Both
// editors register a URL scheme on install (cursor://file/<path>:<line>:<col>
// and vscode://file/…), so no command-line tool has to be on PATH; the caller
// opens the URL with macOS `open`. Pure.
const SCHEMES = { cursor: "cursor", vscode: "vscode" };
const PREFERENCE = ["cursor", "vscode"];

/** The editor to use: the configured one if it can open a file at a line and is installed, else the first installed of Cursor, VS Code; null if neither. */
function pickEditor({ configured, installed }) {
  const has = (id) => Object.prototype.hasOwnProperty.call(SCHEMES, id) && installed.includes(id);
  if (has(configured)) return configured;
  return PREFERENCE.find((id) => installed.includes(id)) || null;
}

function positive(value, fallback, max) {
  const n = Number(value);
  return Number.isSafeInteger(n) && n >= 1 && n <= max ? n : fallback;
}

/** cursor://file/Users/me/a%20b/x.ts:10:5 — every path segment percent-encoded. */
function editorUrl(editor, absPath, line, column) {
  if (!Object.prototype.hasOwnProperty.call(SCHEMES, editor)) throw new Error(`Can't open a file at a line in "${editor}".`);
  if (typeof absPath !== "string" || !absPath.startsWith("/") || /[\u0000-\u001f\u007f]/.test(absPath)) {
    throw new Error("A file location needs an absolute path.");
  }
  const encoded = absPath.split("/").map(encodeURIComponent).join("/");
  return `${SCHEMES[editor]}://file${encoded}:${positive(line, 1, 10_000_000)}:${positive(column, 1, 100_000)}`;
}

module.exports = { pickEditor, editorUrl };
