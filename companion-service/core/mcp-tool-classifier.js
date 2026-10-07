// Fail-closed classification of MCP tool names into "allow" (read-only) or
// "deny" (everything else), used by core/mcp-guard.js (the PreToolUse hook
// script) at run time to decide whether an MCP tool call may proceed.
// features/analyze-issue/plan.js's MUTATING_MCP_TOOLS is a separate,
// hand-maintained deny list (defence in depth for that one feature) — it
// does NOT call this classifier, so the two are not code-shared; keep them
// conceptually aligned by hand when either changes. Plain JS, not
// TypeScript — same reason as core/paths.js: mcp-guard.js requires it
// directly with zero build step, since it runs as a hook script Claude
// invokes with plain `node`.
//
// An MCP tool name looks like `mcp__<server>__<action>`, e.g.
// `mcp__someserver__jira_get_issue`. The action is everything
// after the LAST "__" (server names can contain "__" of their own in
// theory; the action can't, since it's `snake_case`). A tool is allowed
// only when its action's first `_`-separated token is a known read verb —
// or, for actions namespaced by service (`jira_get_issue`,
// `confluence_search`), when the first token is that known namespace and
// the SECOND token is a read verb. Anything that doesn't fit this shape —
// no action, no verb, an unrecognized namespace — is denied. This is
// deliberately conservative: `jira_batch_get_changelogs` is denied because
// its first non-namespace token is `batch`, even though the tool is
// actually read-only, because guessing wrong the other way (allowing a
// mutation) is the failure mode that matters here. Additionally, the
// companion's own MCP server (ai-companion) is always denied to prevent
// headless Claude runs from calling back into the companion's services.

const COMPANION_MCP_SERVER = "ai-companion";

const READ_VERBS = [
  "get",
  "list",
  "search",
  "query",
  "read",
  "grep",
  "glob",
  "find",
  "download",
  "view",
  "fetch",
  "ask",
];

// Service namespaces that legitimately prefix an action before the verb,
// e.g. "jira_get_issue" (verb is the second token, not the first).
const NAMESPACES = ["jira", "confluence", "bitbucket", "jenkins", "argocd", "slack"];

function classifyMcpTool(toolName) {
  const name = String(toolName || "");
  const sep = name.lastIndexOf("__");
  if (sep === -1) return "deny";

  // Check if the server is the companion's own MCP server (always deny).
  // Server name is everything between "mcp__" and the last "__".
  if (name.startsWith("mcp__")) {
    const server = name.slice(5, sep);
    if (server === COMPANION_MCP_SERVER) return "deny";
  }

  const action = name.slice(sep + 2);
  const tokens = action.split("_").filter(Boolean);
  if (tokens.length === 0) return "deny";

  const first = tokens[0].toLowerCase();
  if (READ_VERBS.includes(first)) return "allow";

  if (tokens.length > 1 && NAMESPACES.includes(first) && READ_VERBS.includes(tokens[1].toLowerCase())) {
    return "allow";
  }

  return "deny";
}

module.exports = { READ_VERBS, COMPANION_MCP_SERVER, classifyMcpTool };
