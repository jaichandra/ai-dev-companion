// Plain-text formatting for the `companion` command. Pure.
function ago(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function duration(ms) {
  if (ms === null || ms === undefined) return "-";
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

const cut = (text, n) => (text.length > n ? `${text.slice(0, n - 1)}…` : text);
// Everything the service (or a Jira/Bitbucket author) wrote is untrusted text
// for a terminal: whitespace collapses, and control characters (ESC, C1, DEL)
// and invisible/bidi characters are dropped so it can't move the cursor,
// clear the screen or hide text.
const INVISIBLE_RE = /[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2060-\u2064\u2066-\u2069\ufeff]/g;
const oneLine = (text) => String(text ?? "").replace(/\s+/g, " ").replace(INVISIBLE_RE, "").trim();

function formatJobs(jobs, now) {
  if (!jobs || jobs.length === 0) return "No jobs.";
  return jobs
    .map((j) =>
      [
        oneLine(j.id).slice(0, 8),
        oneLine(j.featureId).padEnd(22),
        oneLine(j.status).padEnd(17),
        ago(now - j.updatedAt).padEnd(8),
        oneLine(j.scopeKey || "").padEnd(24),
        cut(oneLine(j.summary || j.error || j.progress || ""), 60),
      ]
        .join(" ")
        .trimEnd(),
    )
    .join("\n");
}

function formatStatus(jobs, now, baseUrl) {
  const counts = new Map();
  for (const j of jobs || []) counts.set(oneLine(j.status), (counts.get(oneLine(j.status)) || 0) + 1);
  const parts = [...counts.entries()].map(([status, n]) => `${n} ${status}`);
  const head = `Companion is running at ${oneLine(baseUrl)} — ${(jobs || []).length} jobs${parts.length ? ` (${parts.join(", ")})` : ""}.`;
  const active = (jobs || []).filter((j) => ["running", "approving", "rejecting"].includes(j.status));
  return active.length ? `${head}\n\n${formatJobs(active, now)}` : head;
}

function formatHistory(detail, now) {
  const { item, edges, facts } = detail;
  const lines = [`${oneLine(item.key)} (${oneLine(item.kind)})${item.title ? `  ${cut(oneLine(item.title), 200)}` : ""}`];
  lines.push(`  updated ${ago(now - item.updatedAt)}${item.repo ? ` · repo ${oneLine(item.repo)}` : ""}`);
  if (item.excerpt) lines.push(`  ${cut(oneLine(item.excerpt), 200)}`);
  if (edges && edges.length) {
    lines.push("Links:");
    for (const e of edges) {
      lines.push(`  ${e.direction === "out" ? "→" : "←"} ${oneLine(e.rel).padEnd(10)} ${oneLine(e.key)}${e.title ? `  ${cut(oneLine(e.title), 60)}` : ""}`);
    }
  }
  if (facts && facts.length) {
    lines.push("Facts:");
    for (const f of facts) lines.push(`  ${oneLine(f.kind)} (${oneLine(f.provenance)}): ${cut(oneLine(JSON.stringify(f.value)), 120)}`);
  }
  return lines.join("\n");
}

function formatMetrics({ days, metrics, prewarmed, matches }) {
  const lines = [`How long jobs took, last ${oneLine(days)} days:`];
  if (!metrics || metrics.length === 0) lines.push("  (nothing recorded yet)");
  for (const m of metrics || []) {
    lines.push(
      `  ${oneLine(m.featureId).padEnd(24)} ${oneLine(m.outcome).padEnd(10)} ${oneLine(m.count).padStart(3)}  ${duration(m.medianMs).padEnd(8)} ${duration(m.avgMs)}`,
    );
  }
  if (prewarmed && prewarmed.length) {
    lines.push("", "Pre-warmed by the background watchers (used = approved, or opened from the inbox):");
    for (const p of prewarmed) {
      const pct = p.usedFraction === null ? "-" : `${Math.round(p.usedFraction * 100)}%`;
      lines.push(`  ${oneLine(p.watcher).padEnd(16)} ${oneLine(p.runs).padStart(3)} run(s)  ${oneLine(p.used).padStart(3)} used  ${pct.padStart(4)}`);
    }
  }
  if (matches && matches.length) {
    lines.push("", "Matches:");
    for (const m of matches) lines.push(`  ${oneLine(m.key).padEnd(30)} ${cut(oneLine(m.title || ""), 60)}`);
  }
  return lines.join("\n");
}

function formatInbox({ unseen, items }, now) {
  if (!items || items.length === 0) return unseen ? `${oneLine(unseen)} unseen.` : "Nothing new.";
  const lines = [`${oneLine(unseen)} unseen:`];
  for (const i of items) {
    lines.push(`${i.urgent ? "!" : "•"} ${cut(oneLine(i.title), 100)}  (${ago(now - i.createdAt)}${i.seen ? ", seen" : ""})`);
    if (i.body) lines.push(`    ${cut(oneLine(i.body), 120)}`);
    if (i.url) lines.push(`    ${oneLine(i.url)}`);
  }
  return lines.join("\n");
}

function formatDigest(digest) {
  const lines = [oneLine(digest.headline)];
  for (const s of digest.sections || []) {
    lines.push("", `${oneLine(s.title)}:`);
    for (const item of s.items) lines.push(`  ${item.tone === "bad" ? "✗" : item.tone === "ok" ? "✓" : "•"} ${cut(oneLine(item.text), 140)}`);
  }
  return lines.join("\n");
}

/** One pre-push assessment, as `companion precheck` prints it. */
function formatRisk(r) {
  if (!r || typeof r !== "object") return "companion precheck: skipped — no usable answer";
  if (r.skipped) return `companion precheck: skipped — ${cut(oneLine(r.reason), 200)}`;
  const lines = [`companion precheck: ${cut(oneLine(r.summary), 300)}`];
  for (const l of Array.isArray(r.lines) ? r.lines : []) lines.push(`  • ${cut(oneLine(l && l.text), 300)}`);
  return lines.join("\n");
}

/** `companion inbox --brief`: one line when something is unseen, else "".
 * The Claude Code plugin's SessionStart hook prints it into the session's
 * context, so it carries counts and fixed labels only — never a title,
 * which other people write. */
const BRIEF_LABELS = { conflict: "conflict resolution", analysis: "background analysis", "review-request": "review request", digest: "digest" };
function formatInboxBrief(r) {
  const unseen = r && Number.isInteger(r.unseen) ? r.unseen : 0;
  if (unseen <= 0) return "";
  const counts = new Map();
  for (const i of (r && Array.isArray(r.items) ? r.items : [])) {
    const label = BRIEF_LABELS[i && i.kind];
    if (label) counts.set(label, (counts.get(label) || 0) + 1);
  }
  const parts = [...counts.entries()].map(([label, n]) => `${label}: ${n}`);
  return `AI companion: ${unseen} new in your inbox${parts.length ? ` (${parts.join(", ")})` : ""}. Run \`companion inbox\` to see them.`;
}

/** `companion similar`: past tickets and PRs like one, each with where its text came from. */
function formatSimilar(r, label) {
  if (!r || r.mode === "off") return "Similar items are turned off (⚙ Settings → Local history).";
  const items = Array.isArray(r.items) ? r.items : [];
  if (items.length === 0) return `Nothing similar to ${oneLine(label)} in the local history yet.`;
  const how = r.mode === "vector+text" ? "by meaning and shared words" : "by shared words (no LLM proxy)";
  const lines = [`Similar to ${oneLine(label)} (${how}):`];
  for (const i of items) {
    const when = new Date(Number.isFinite(i.updatedAt) ? i.updatedAt : NaN);
    const day = Number.isNaN(when.getTime()) ? "" : when.toISOString().slice(0, 10);
    lines.push(`  ${oneLine(i.key).padEnd(18)} ${cut(oneLine(i.title || "(no title)"), 80)}${day ? `  (${oneLine(i.kind)}, ${day})` : ""}`);
    if (i.analysis) lines.push(`  ${"".padEnd(18)} earlier AI output: ${cut(oneLine(i.analysis), 140)}`);
  }
  lines.push("", "Titles are written by people and analyses are earlier AI output — check before relying on them.");
  return lines.join("\n");
}

module.exports = { oneLine, ago, duration, formatJobs, formatStatus, formatHistory, formatMetrics, formatInbox, formatDigest, formatRisk, formatSimilar, formatInboxBrief };
