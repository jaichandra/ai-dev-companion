// Reads `claude -p --output-format stream-json --verbose` output (one JSON
// object per line) and turns tool calls into short progress labels for the
// panel, plus the final result. Labels come from Claude's own tool inputs, so
// they are cut to one line of at most 80 characters, and WebFetch/WebSearch
// labels never include the URL.
const path = require("path");

const LABEL_MAX = 80;

function clean(text) {
  const oneLine = String(text).replace(/[\u0000-\u001f\u007f]/g, " ").replace(/\s+/g, " ").trim();
  return oneLine.length > LABEL_MAX ? `${oneLine.slice(0, LABEL_MAX - 1)}…` : oneLine;
}

function parseStreamLine(line) {
  if (typeof line !== "string" || !line.startsWith("{")) return null;
  try {
    const event = JSON.parse(line);
    return event && typeof event === "object" && typeof event.type === "string" ? event : null;
  } catch {
    return null;
  }
}

function shortPath(p, cwd) {
  if (typeof p !== "string" || !p) return "";
  if (typeof cwd === "string" && cwd && p.startsWith(cwd + path.sep)) return p.slice(cwd.length + 1);
  return p;
}

function labelForToolUse(block, cwd) {
  if (!block || block.type !== "tool_use" || typeof block.name !== "string") return null;
  const input = block.input && typeof block.input === "object" ? block.input : {};
  switch (block.name) {
    case "Read":
      return clean(`Reading ${shortPath(input.file_path, cwd)}`);
    case "Edit":
    case "MultiEdit":
    case "Write":
      return clean(`Editing ${shortPath(input.file_path, cwd)}`);
    case "NotebookEdit":
      return clean(`Editing ${shortPath(input.notebook_path, cwd)}`);
    case "Grep":
      return clean(`Searching for ${input.pattern ?? "text"}`);
    case "Glob":
      return clean(`Finding files ${input.pattern ?? ""}`);
    case "Bash":
      return clean(`Running ${input.command ?? "a command"}`);
    case "WebFetch":
    case "WebSearch":
      return "Fetching a web page";
    default:
      if (block.name.startsWith("mcp__")) return clean(`Asking ${block.name.split("__")[1] || "a tool"}`);
      return clean(`Using ${block.name}`);
  }
}

/** The label for the latest tool call in an assistant message, or null. */
function labelForEvent(event, cwd) {
  if (!event || event.type !== "assistant" || !event.message || !Array.isArray(event.message.content)) return null;
  const uses = event.message.content.filter((b) => b && b.type === "tool_use");
  if (uses.length === 0) return null;
  return labelForToolUse(uses[uses.length - 1], cwd);
}

function createStreamAccumulator({ cwd } = {}) {
  let initSession;
  let resultEvent;
  return {
    feed(line) {
      const event = parseStreamLine(line);
      if (!event) return {};
      if (event.type === "system" && event.subtype === "init" && typeof event.session_id === "string") {
        initSession = event.session_id;
      }
      if (event.type === "result") resultEvent = event;
      const label = labelForEvent(event, cwd);
      return label ? { label } : {};
    },
    result() {
      if (!resultEvent) {
        return { sawResult: false, text: "", sessionId: initSession, permissionDenials: [], isError: false };
      }
      const denials = Array.isArray(resultEvent.permission_denials)
        ? resultEvent.permission_denials.map((d) => (d && typeof d.tool_name === "string" ? d.tool_name : "?"))
        : [];
      return {
        sawResult: true,
        text: typeof resultEvent.result === "string" ? resultEvent.result : "",
        sessionId: typeof resultEvent.session_id === "string" ? resultEvent.session_id : initSession,
        permissionDenials: denials,
        isError: resultEvent.is_error === true,
      };
    },
  };
}

/** Lets one call through per `minMs`; the rest are dropped (a label is only a hint). */
function createThrottle(minMs, now = Date.now) {
  let last = -Infinity;
  return (fn) => {
    const t = now();
    if (t - last >= minMs) {
      last = t;
      fn();
    }
  };
}

module.exports = { parseStreamLine, labelForToolUse, labelForEvent, createStreamAccumulator, createThrottle };
