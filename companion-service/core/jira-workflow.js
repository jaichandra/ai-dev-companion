// Pure pieces of the Jira calls Ticket to PR makes after opening a PR: read
// a ticket's basics, pick the review transition by name, and build the
// remote-link body. core/jira.ts does the HTTP (Jira Server/DC, API v2).
function isPlainObject(value) {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

const str = (v) => (typeof v === "string" ? v : null);

/** `GET /rest/api/2/issue/{key}?fields=summary,issuetype,status` ->
 * `{key, summary, issueType, status}`, or null for a non-object. */
function normalizeIssueBasics(raw) {
  if (!isPlainObject(raw)) return null;
  const fields = isPlainObject(raw.fields) ? raw.fields : {};
  return {
    key: str(raw.key),
    summary: str(fields.summary),
    issueType: isPlainObject(fields.issuetype) ? str(fields.issuetype.name) : null,
    status: isPlainObject(fields.status) ? str(fields.status.name) : null,
  };
}

/** `GET .../transitions` -> `[{id, name, to}]` (entries without an id dropped). */
function normalizeTransitions(raw) {
  const list = isPlainObject(raw) && Array.isArray(raw.transitions) ? raw.transitions : [];
  return list
    .filter((t) => isPlainObject(t) && (typeof t.id === "string" || typeof t.id === "number"))
    .map((t) => ({
      id: String(t.id),
      name: str(t.name),
      to: isPlainObject(t.to) ? str(t.to.name) : null,
    }));
}

const fold = (s) => (typeof s === "string" ? s.trim().toLowerCase() : "");

/** The transition whose name — or, failing that, whose target status —
 * matches `wanted` (case- and space-insensitive), or null. */
function pickTransition(transitions, wanted) {
  const want = fold(wanted);
  if (!want) return null;
  return (
    transitions.find((t) => fold(t.name) === want) ||
    transitions.find((t) => fold(t.to) === want) ||
    null
  );
}

// ---- Phase 7: the assigned-bugs watcher and the digest ----

const DESCRIPTION_MAX = 2000;

/** `GET /rest/api/2/search` -> `[{key, summary, status, statusCategory,
 * issueType, priority, updated, description, url}]` (no key -> dropped). */
function normalizeSearchResults(raw, baseUrl) {
  const issues = isPlainObject(raw) && Array.isArray(raw.issues) ? raw.issues : [];
  const base = typeof baseUrl === "string" ? baseUrl.replace(/\/+$/, "") : "";
  return issues
    .filter((i) => isPlainObject(i) && typeof i.key === "string" && /^[A-Z][A-Z0-9_]*-\d+$/.test(i.key))
    .map((i) => {
      const f = isPlainObject(i.fields) ? i.fields : {};
      const name = (o) => (isPlainObject(o) ? str(o.name) : null);
      const category = isPlainObject(f.status) && isPlainObject(f.status.statusCategory) ? str(f.status.statusCategory.key) : null;
      return {
        key: i.key,
        summary: str(f.summary),
        status: name(f.status),
        statusCategory: category,
        issueType: name(f.issuetype),
        priority: name(f.priority),
        updated: str(f.updated),
        description: typeof f.description === "string" ? f.description.slice(0, DESCRIPTION_MAX) : null,
        url: base ? `${base}/browse/${i.key}` : null,
      };
    });
}

/** The remote-link body that shows a PR on the ticket. `globalId` makes a
 * retry update the same link instead of adding a second one. */
function remoteLinkBody({ url, title }) {
  if (typeof url !== "string" || !/^https:\/\//.test(url)) throw new Error("A remote link needs an https URL.");
  const text = typeof title === "string" && title.trim() ? title.trim() : url;
  return {
    globalId: `${require("./app-slug.js").APP_SLUG}:pr:${url}`,
    application: { type: "com.atlassian.bitbucket", name: "Bitbucket" },
    relationship: "Pull request",
    object: { url, title: text.slice(0, 255) },
  };
}

module.exports = { normalizeIssueBasics, normalizeTransitions, pickTransition, remoteLinkBody, normalizeSearchResults };
