// The handlers behind two core MCP tools (core/mcp.ts registers them):
//   list_notifications  the inbox, for `companion inbox` and terminal Claude
//   get_change_risk     the pre-push check (core/push-risk.js) against the
//                       shared risk-facts/v1 feed, for `companion precheck`
//                       and terminal Claude / Cursor. With no feed configured
//                       it says so and checks nothing.
// Plain JS with injected dependencies, so node:test covers it directly.
const pushRisk = require("./push-risk.js");

function listNotifications(inbox, args = {}) {
  if (!inbox) throw new Error("The inbox isn't available on this copy of the companion.");
  const items = inbox.list({ includeSeen: args.all === true, limit: 50 }).map((i) => ({
    id: i.id,
    kind: i.kind,
    title: i.title,
    body: i.body,
    url: i.url,
    urgent: i.urgent,
    scopeKey: i.scopeKey,
    jobId: i.jobId,
    createdAt: i.createdAt,
    seen: !!i.seenAt,
  }));
  return { unseen: inbox.unseenCount(), items };
}

async function assessPushRisk(args, { riskUrl, riskFacts, now = Date.now() }) {
  const checked = pushRisk.normalizeChangedFiles(args.files);
  if (!checked.ok) throw new Error(checked.reason);
  const repo = typeof args.repo === "string" ? args.repo : null;
  if (typeof riskUrl !== "string" || !riskUrl.trim()) {
    return { skipped: true, repo, reason: "The shared test history (⚙ Settings → Shared test history) isn't set up, so there is nothing to check against." };
  }
  const got = await riskFacts.get(riskUrl);
  if (!got.ok) return { skipped: true, repo, reason: `The shared test history can't be read right now (${got.reason}).` };
  return { skipped: false, repo, stale: got.stale === true, ...pushRisk.assessPushRisk({ files: checked.value, facts: got.facts, now }) };
}

module.exports = { listNotifications, assessPushRisk };
