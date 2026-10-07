// The morning digest: your PRs (conflicts, builds, approvals and
// needs-work), PRs waiting on your review, your open tickets, and what the
// watchers pre-warmed. Pure: features/digest/index.ts collects the inputs
// (every read is best effort; a failed one becomes a "Couldn't check"
// line) and the ✨ panel, `companion digest`, the get_digest MCP tool and
// the scheduled weekday digest all show what buildDigest returns.
const MAX_ITEMS = 10;
// Items past the cap that still travel with the section, for the panel's "Show more".
const MAX_MORE = 100;
const TEXT_MAX = 200;
const URGENT_PRIORITIES = ["Blocker", "Highest", "Critical", "High"];
const TONE_ORDER = { bad: 0, warn: 1, busy: 2, ok: 3, neutral: 4 };

// Text from PRs and tickets: control, C1, zero-width and bidi characters go, whitespace collapses.
const INVISIBLE_RE = /[\u0000-\u001f\u007f-\u009f\u200b-\u200f\u202a-\u202e\u2060-\u2064\u2066-\u2069\ufeff]/g;
const oneLine = (text, max = TEXT_MAX) => {
  const clean = String(text ?? "").replace(/\s+/g, " ").replace(INVISIBLE_RE, "").trim();
  return clean.length > max ? `${clean.slice(0, max - 1)}…` : clean;
};

function httpsUrl(value) {
  try {
    return typeof value === "string" && new URL(value).protocol === "https:" ? value : null;
  } catch {
    return null;
  }
}

const prLabel = (pr) => oneLine(`${pr.project}/${pr.repo} #${pr.id}`, 100);
const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;

function myPrItem({ pr, conflicted, build }) {
  const tags = [];
  let tone = "neutral";
  if (conflicted === true) {
    tags.push("conflicts");
    tone = "bad";
  }
  if (build && build.state === "FAILED") {
    tags.push("build failing");
    tone = "bad";
  } else if (build && build.state === "INPROGRESS") {
    tags.push("build running");
    if (tone === "neutral") tone = "busy";
  }
  const needsWork = (pr.reviewers || []).filter((r) => r.status === "NEEDS_WORK").map((r) => oneLine(r.name, 40));
  if (needsWork.length) {
    tags.push(`needs work (${needsWork.slice(0, 3).join(", ")})`);
    if (tone !== "bad") tone = "warn";
  }
  const approvals = pr.approvals || 0;
  if (approvals > 0) {
    tags.push(`approved by ${approvals}`);
    if (tone === "neutral" && (!build || build.state === "SUCCESSFUL")) tone = "ok";
  }
  return {
    text: `${prLabel(pr)} ${oneLine(pr.title, 120)}${tags.length ? ` — ${tags.join(", ")}` : ""}`,
    tone,
    url: httpsUrl(pr.url),
    flags: { conflicted: conflicted === true, failing: !!(build && build.state === "FAILED"), needsWork: needsWork.length > 0, approved: approvals > 0 },
  };
}

function readyItem(job) {
  const where = job.scopeKey ? oneLine(String(job.scopeKey).replace(/^(bitbucket|github|jira):/, ""), 100) : "";
  const featureId = oneLine(job.featureId, 60);
  if (job.status === "failed") {
    return { text: `A background ${featureId} run for ${where} failed: ${oneLine(job.error || "no details", 120)}`, tone: "bad", url: httpsUrl(job.url) };
  }
  const what = job.featureId === "resolve-conflict" ? "Conflict resolution ready to review" : job.featureId === "analyze-issue" ? "Analysis ready" : `${featureId} ready`;
  return { text: `${what}: ${where}${job.summary ? ` — ${oneLine(job.summary, 100)}` : ""}`, tone: "ok", url: httpsUrl(job.url) };
}

const byTone = (a, b) => TONE_ORDER[a.tone] - TONE_ORDER[b.tone];

function section(id, title, items) {
  const sorted = items.slice().sort(byTone);
  const strip = ({ flags, ...item }) => item;
  const shown = sorted.slice(0, MAX_ITEMS).map(strip);
  // The rest travel with the section (the panel's "Show N more"); the closing
  // "…and N more" row stays for readers that only print items.
  const more = sorted.slice(MAX_ITEMS, MAX_ITEMS + MAX_MORE).map(strip);
  if (sorted.length > MAX_ITEMS) shown.push({ text: `…and ${sorted.length - MAX_ITEMS} more`, tone: "neutral", url: null });
  return more.length ? { id, title, items: shown, more } : { id, title, items: shown };
}

/**
 * @param {{ now: number, myPrs?: {pr, conflicted?: boolean|null, build?: {state}|null}[],
 *   reviewPrs?: object[], tickets?: object[], prewarmed?: object[], notes?: string[] }} input
 */
function buildDigest({ now, myPrs = [], reviewPrs = [], tickets = [], prewarmed = [], notes = [] }) {
  const mine = myPrs.map(myPrItem);
  const counts = {
    conflicts: mine.filter((i) => i.flags.conflicted).length,
    failingBuilds: mine.filter((i) => i.flags.failing).length,
    needsWork: mine.filter((i) => i.flags.needsWork).length,
    approved: mine.filter((i) => i.flags.approved).length,
    toReview: reviewPrs.length,
    tickets: tickets.length,
    // Ready to act on: a failed background run is reported separately, not as "ready".
    ready: prewarmed.filter((j) => j.status !== "failed").length,
    failedRuns: prewarmed.filter((j) => j.status === "failed").length,
  };
  const sections = [
    section("my-prs", "Your pull requests", mine),
    section(
      "to-review",
      "Waiting on your review",
      reviewPrs.map((pr) => ({ text: `${prLabel(pr)} ${oneLine(pr.title, 120)}${pr.authorSlug ? ` — by ${oneLine(pr.authorSlug, 40)}` : ""}`, tone: "neutral", url: httpsUrl(pr.url) })),
    ),
    section(
      "tickets",
      "Your open tickets",
      tickets.map((t) => ({
        text: `${oneLine(t.key, 40)} ${oneLine(t.summary, 120)}${t.status ? ` (${oneLine(t.status, 30)})` : ""}`,
        tone: URGENT_PRIORITIES.includes(t.priority) ? "warn" : "neutral",
        url: httpsUrl(t.url),
      })),
    ),
    section("ready", "Ready for you", prewarmed.map(readyItem)),
    section("notes", "Couldn't check", notes.map((n) => ({ text: oneLine(n), tone: "warn", url: null }))),
  ].filter((s) => s.items.length > 0);

  const needs = [];
  if (counts.conflicts) needs.push(plural(counts.conflicts, "conflict"));
  if (counts.failingBuilds) needs.push(plural(counts.failingBuilds, "failing build"));
  if (counts.needsWork) needs.push(plural(counts.needsWork, "PR needing work", "PRs needing work"));
  if (counts.toReview) needs.push(plural(counts.toReview, "PR to review", "PRs to review"));
  if (counts.ready) needs.push(`${counts.ready} ready for you`);
  if (counts.failedRuns) needs.push(plural(counts.failedRuns, "background run failed", "background runs failed"));
  // With sources that couldn't be read, "nothing needs you" would be a guess.
  const headline = needs.length
    ? `Needs you: ${needs.join(", ")}.`
    : notes.length
      ? "Some sources couldn't be checked — see below."
      : "Nothing needs you right now.";
  return { generatedAt: new Date(Number.isFinite(now) ? now : Date.now()).toISOString(), headline, counts, sections };
}

/** The inbox item the scheduled weekday digest posts. */
function digestNotification(digest, dayKey) {
  // The scheduled run has no browser session, only a cached cookie or a saved token, so
  // its failed reads are usually "no sign-in"; the ✨ button relays the page's session.
  const unread = digest.sections.some((s) => s.id === "notes");
  const noteLines = digest.sections.filter((s) => s.id === "notes").flatMap((s) => s.items.map((i) => i.text)).slice(0, 3);
  const lines = digest.sections.filter((s) => s.id !== "notes").flatMap((s) => s.items.slice(0, 2).map((i) => i.text)).slice(0, 3);
  return {
    key: `digest:${dayKey}`,
    kind: "digest",
    title: unread && !lines.length ? "Morning digest — couldn't read some sources in the background" : `Morning digest — ${digest.headline}`,
    // Keep the reasons a source couldn't be read, so the inbox item says what failed.
    body: lines.length
      ? [...lines, ...noteLines].join("\n")
      : unread
        ? "Open ✨ Morning digest to read them with your browser sign-in, or save tokens in ⚙ Settings so the background run can sign in."
        : "Open ✨ Morning digest for the details.",
    url: null,
    urgent: false,
  };
}

module.exports = { MAX_ITEMS, buildDigest, digestNotification };
