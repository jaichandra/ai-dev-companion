// A minimal client for the companion's own /mcp route: one stateless
// JSON-RPC `tools/call` per invocation. Used by the `companion` command so it
// needs neither the MCP SDK nor any route besides /mcp.
function parseRpcBody(text, contentType) {
  const type = String(contentType || "");
  if (type.includes("text/event-stream")) {
    const data = text.split("\n").filter((l) => l.startsWith("data:")).pop();
    if (!data) throw new Error("The companion sent an empty answer.");
    return JSON.parse(data.slice(5).trim());
  }
  return JSON.parse(text);
}

async function callTool({ url, token, name, args = {}, fetchImpl = fetch, timeoutMs = 60000 }) {
  let res;
  try {
    res = await fetchImpl(url, {
      method: "POST",
      headers: {
        "content-type": "application/json",
        accept: "application/json, text/event-stream",
        authorization: `Bearer ${token}`,
      },
      body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name, arguments: args } }),
      signal: AbortSignal.timeout(timeoutMs),
    });
  } catch (err) {
    const code = err && err.cause && err.cause.code;
    if (code === "ECONNREFUSED" || code === "ECONNRESET") {
      const port = /:(\d+)\//.exec(String(url));
      throw new Error(
        `The companion service isn't running${port ? ` (nothing is listening on port ${port[1]})` : ""}. Try \`companion doctor\`.`,
      );
    }
    throw new Error(`Couldn't reach the companion service: ${err && err.message ? err.message : err}`);
  }
  if (res.status === 401 || res.status === 403) {
    throw new Error("The companion refused the token. Run `companion doctor` — the MCP token may have been rotated.");
  }
  const text = await res.text();
  if (!res.ok) throw new Error(`The companion answered HTTP ${res.status}.`);
  let body;
  try {
    body = parseRpcBody(text, res.headers && res.headers.get && res.headers.get("content-type"));
  } catch {
    throw new Error("The companion sent an answer this command can't read.");
  }
  if (body.error) throw new Error(body.error.message || "The companion returned an error.");
  const result = body.result || {};
  const first = Array.isArray(result.content) ? result.content.find((c) => c && c.type === "text") : undefined;
  const textOut = first ? first.text : "";
  if (result.isError) throw new Error(textOut || "The tool failed.");
  try {
    return JSON.parse(textOut);
  } catch {
    return textOut;
  }
}

module.exports = { callTool };
