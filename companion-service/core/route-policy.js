// Where a piece of AI work runs: the on-prem models through the LLM
// proxy ("onprem") or Claude through the user's own Claude Code login
// ("claude"). Explicit rules, never a learned router: code changes,
// anything that needs tools, diagnosis and analysis always go to Claude;
// the small text-only steps (triage, classifying, condensing, embedding)
// go on-prem when the proxy was detected, otherwise to Claude. Pure.
// Callers record the answer in events.metrics_json.tier.
const ONPREM_TASKS = ["triage", "classify", "condense", "embed"];
const CLAUDE_TASKS = ["code-change", "diagnosis", "analysis"];
// The on-prem chat models take 262K tokens in; stay well inside that.
const MAX_ONPREM_BYTES = 200 * 1024;

/** @returns {{ tier: "onprem" | "claude", reason: string }} */
function chooseTier({ task, bytes = 0, needsTools = false } = {}, { onpremAvailable = false } = {}) {
  if (needsTools) return { tier: "claude", reason: "needs tools" };
  if (CLAUDE_TASKS.includes(task)) return { tier: "claude", reason: `${task} always runs on Claude` };
  if (!ONPREM_TASKS.includes(task)) return { tier: "claude", reason: `no on-prem rule for "${String(task).slice(0, 40)}"` };
  if (!onpremAvailable) return { tier: "claude", reason: "the LLM proxy isn't available" };
  if (!Number.isFinite(bytes) || bytes > MAX_ONPREM_BYTES) return { tier: "claude", reason: "too large for the on-prem model" };
  return { tier: "onprem", reason: `${task} runs on-prem` };
}

module.exports = { ONPREM_TASKS, CLAUDE_TASKS, MAX_ONPREM_BYTES, chooseTier };
