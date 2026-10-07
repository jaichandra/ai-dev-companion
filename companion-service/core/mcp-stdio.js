// `companion mcp-stdio`: the companion's MCP server over stdin/stdout, for
// the Claude Code plugin (plugin/.claude-plugin/plugin.json). Each JSON-RPC
// line from Claude Code is POSTed to the running service's own /mcp route
// with the MCP token from credentials.enc — the plugin itself never holds a
// token — and every answer is written back as one line. When the service
// is down, each request gets a JSON-RPC error saying "companion service not
// running", so Claude Code shows why instead of failing silently. Plain JS
// with fetch, the token and the writer injected, so node:test covers it.
// Requests are handled one at a time, in order (a slow answer holds the
// next request up for at most the 60 s request timeout); nothing here ever
// throws, and every request id gets exactly one answer.
const { redactSecrets } = require("./history-record.js");

const SERVER_ERROR = -32000;
const MAX_RESPONSE_BYTES = 8 * 1024 * 1024;
const REQUEST_TIMEOUT_MS = 60000;

function parseBody(text, contentType) {
  if (!text || !text.trim()) return [];
  if (String(contentType || "").includes("text/event-stream")) {
    return text
      .split("\n")
      .filter((l) => l.startsWith("data:"))
      .map((l) => JSON.parse(l.slice(5).trim()));
  }
  const body = JSON.parse(text);
  return Array.isArray(body) ? body : [body];
}

/** The ids of the requests in one JSON-RPC message or batch (notifications have none). */
function requestIds(msg) {
  const list = Array.isArray(msg) ? msg : [msg];
  return list.filter((m) => m && typeof m === "object" && typeof m.method === "string" && m.id !== undefined && m.id !== null).map((m) => m.id);
}

/** The ids answered by a list of JSON-RPC responses (messages with an id and no method). */
function answeredIds(messages) {
  const ids = new Set();
  for (const m of messages) if (m && typeof m === "object" && !("method" in m) && m.id !== undefined && m.id !== null) ids.add(m.id);
  return ids;
}

function createStdioBridge({ url, getToken, write: initialWrite, fetchImpl = fetch, timeoutMs = REQUEST_TIMEOUT_MS, maxBytes = MAX_RESPONSE_BYTES }) {
  const port = (/:(\d+)\//.exec(String(url)) || [])[1];
  let out = initialWrite;
  // Writing must never throw into the request that is being answered.
  const write = (msg) => {
    try {
      if (out) out(msg);
    } catch {
      // stdout is gone; runStdio notices through the stream's own error event.
    }
  };
  const fail = (ids, message) => {
    for (const id of ids) write({ jsonrpc: "2.0", id, error: { code: SERVER_ERROR, message } });
  };

  /** Handles one line from Claude Code. Never throws. */
  async function handleLine(line) {
    const text = String(line || "").trim();
    if (!text) return;
    let msg;
    try {
      msg = JSON.parse(text);
    } catch {
      write({ jsonrpc: "2.0", id: null, error: { code: -32700, message: "Parse error" } });
      return;
    }
    const ids = requestIds(msg);
    let token;
    try {
      try {
        token = getToken();
      } catch {
        token = undefined;
      }
      await forward(text, ids, token);
    } catch (err) {
      let message = redactSecrets(err && err.message ? err.message : String(err));
      if (token) message = message.split(String(token)).join("***");
      fail(ids, `The MCP bridge hit an error: ${message}`);
    }
  }

  async function forward(text, ids, token) {
    if (!token) {
      fail(ids, "No MCP token is saved yet: run `npm run setup` in the companion-service folder (or `companion doctor`).");
      return;
    }
    let res;
    try {
      res = await fetchImpl(url, {
        method: "POST",
        headers: {
          "content-type": "application/json",
          accept: "application/json, text/event-stream",
          authorization: `Bearer ${token}`,
        },
        body: text,
        signal: AbortSignal.timeout(timeoutMs),
      });
    } catch (err) {
      const code = err && err.cause && err.cause.code;
      fail(
        ids,
        err && (err.name === "TimeoutError" || err.name === "AbortError")
          ? `The companion didn't answer within ${Math.round(timeoutMs / 1000)} seconds.`
          : code === "ECONNREFUSED" || code === "ECONNRESET"
            ? `companion service not running${port ? ` (nothing is listening on port ${port})` : ""}. Start it, or run \`companion doctor\`.`
            : `Couldn't reach the companion service: ${err && err.message ? err.message : err}`,
      );
      return;
    }
    if (res.status === 401 || res.status === 403) {
      fail(ids, "The companion refused the MCP token. Run `companion doctor` — it may have been rotated.");
      return;
    }
    const declared = Number(res.headers && res.headers.get && res.headers.get("content-length"));
    if (Number.isFinite(declared) && declared > maxBytes) {
      fail(ids, "The companion's answer is too large to pass on.");
      return;
    }
    const body = await res.text();
    if (typeof body === "string" && body.length > maxBytes) {
      fail(ids, "The companion's answer is too large to pass on.");
      return;
    }
    const contentType = res.headers && res.headers.get && res.headers.get("content-type");
    if (!res.ok) {
      // A JSON-RPC error body (for example an invalid request) is passed through.
      let passed = [];
      try {
        passed = parseBody(body, contentType).filter((m) => m && typeof m === "object" && m.jsonrpc === "2.0" && m.error);
      } catch {
        passed = [];
      }
      for (const m of passed) write(m);
      const answered = answeredIds(passed);
      fail(ids.filter((id) => !answered.has(id)), `The companion answered HTTP ${res.status}.`);
      return;
    }
    let messages;
    try {
      messages = parseBody(body, contentType);
    } catch {
      fail(ids, "The companion sent an answer that isn't JSON-RPC.");
      return;
    }
    for (const m of messages) write(m);
    const answered = answeredIds(messages);
    fail(ids.filter((id) => !answered.has(id)), "The companion sent no answer to this request.");
  }

  return {
    handleLine,
    setWrite(fn) {
      out = fn;
    },
  };
}

/**
 * Reads JSON-RPC lines from `stdin`, answers each through `bridge` on
 * `stdout`, one at a time. Resolves 0 once stdin has ended and the last
 * request is answered, or as soon as stdout stops listening (EPIPE): a
 * closed reader is a normal way for a stdio server to end, not an error.
 * Only JSON-RPC ever goes to `stdout`; diagnostics go to `stderr`.
 * @param {{stdin: object, stdout: object, stderr?: object, bridge: {handleLine(line: string): Promise<void>, setWrite?(fn: Function): void}}} io
 * @returns {Promise<number>}
 */
function runStdio({ stdin, stdout, stderr, bridge }) {
  return new Promise((resolve) => {
    let done = false;
    let gone = false;
    const finish = () => {
      if (done) return;
      done = true;
      resolve(0);
    };
    const note = (text) => {
      try {
        if (stderr) stderr.write(`${text}\n`);
      } catch {
        // stderr is gone too.
      }
    };
    const onGone = () => {
      gone = true;
      finish();
    };
    stdout.on("error", onGone);
    if (stderr && typeof stderr.on === "function") stderr.on("error", () => {});
    if (typeof bridge.setWrite === "function") {
      bridge.setWrite((msg) => {
        if (gone) return;
        try {
          stdout.write(`${JSON.stringify(msg)}\n`);
        } catch {
          onGone();
        }
      });
    }
    let chain = Promise.resolve();
    let buffered = "";
    const enqueue = (line) => {
      if (!line.trim()) return;
      chain = chain
        .then(() => (gone ? undefined : bridge.handleLine(line)))
        .catch((err) => note(`mcp-stdio: ${err && err.message ? err.message : err}`));
    };
    if (typeof stdin.setEncoding === "function") stdin.setEncoding("utf8");
    stdin.on("data", (chunk) => {
      buffered += chunk;
      let nl;
      while ((nl = buffered.indexOf("\n")) !== -1) {
        enqueue(buffered.slice(0, nl).replace(/\r$/, ""));
        buffered = buffered.slice(nl + 1);
      }
    });
    const onEnd = () => {
      if (buffered) {
        enqueue(buffered);
        buffered = "";
      }
      chain.then(finish);
    };
    stdin.on("end", onEnd);
    stdin.on("error", onEnd);
  });
}

module.exports = { createStdioBridge, runStdio, requestIds, MAX_RESPONSE_BYTES, REQUEST_TIMEOUT_MS };
