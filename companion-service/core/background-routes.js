// The inbox routes server.ts mounts behind the Host guard and the shared
// secret, split out so they're testable without starting the server:
//   GET  /notifications            unseen items (?all=1: seen too), the count, the scheduler's status
//   POST /notifications/seen       { ids: [...] } or { all: true }
//   POST /notifications/announce   what (if anything) to show as one Chrome notification now
//   POST /notifications/:id/open   the user clicked an item: seen, opened, and for a
//                                  pre-warmed read-only result a "used" history event
// Plain JS; every dependency is injected.
// Inbox ids are crypto.randomUUID() values (core/notifications.js).
const ID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
const MAX_IDS = 100;

function createBackgroundRoutes({ inbox, scheduler, history, usedEvent, getJob, minIntervalMs, now = Date.now, log = () => {} }) {
  function list(req, res) {
    const all = req.query && req.query.all === "1";
    res.json({ items: inbox.list({ includeSeen: all }), unseen: inbox.unseenCount(), background: scheduler.status() });
  }

  function seen(req, res) {
    const body = req.body && typeof req.body === "object" && !Array.isArray(req.body) ? req.body : {};
    const keys = Object.keys(body);
    let ids;
    // Exactly one of ids / all, and nothing else: an ambiguous or padded body is a mistake, not a request.
    if (keys.length !== 1 || !["ids", "all"].includes(keys[0])) ids = undefined;
    else if (body.all === true) ids = "all";
    else if (Array.isArray(body.ids) && body.ids.length <= MAX_IDS && body.ids.every((id) => typeof id === "string" && ID_RE.test(id))) ids = body.ids;
    if (ids === undefined) {
      res.status(400).json({ error: "Send { ids: [...] } (at most 100 ids) or { all: true }." });
      return;
    }
    const n = inbox.markSeen(ids);
    res.json({ ok: true, seen: n, unseen: inbox.unseenCount() });
  }

  function announce(_req, res) {
    res.json({ announce: inbox.announce({ quiet: scheduler.quietNow(), minIntervalMs: minIntervalMs() }), unseen: inbox.unseenCount() });
  }

  function open(req, res) {
    const id = req.params && req.params.id;
    if (typeof id !== "string" || !ID_RE.test(id)) {
      res.status(404).json({ error: "unknown notification" });
      return;
    }
    // Only the first open counts: clicking the same item again is not another use.
    const firstOpen = !(typeof inbox.get === "function" && inbox.get(id) && inbox.get(id).openedAt);
    const item = inbox.open(id);
    if (!item) {
      res.status(404).json({ error: "unknown notification" });
      return;
    }
    // A conflict resolution needs an approval to be "used" (the approve records
    // it); merely opening the item is not a use. Only read-only results count.
    const needsApproval = item.kind === "conflict" || item.featureId === "resolve-conflict";
    if (item.jobId && history && firstOpen && !needsApproval) {
      try {
        const job = getJob(item.jobId);
        const event = job && job.featureId === "resolve-conflict" ? null : usedEvent(job, now());
        if (event) history.recordEvent(event);
      } catch (err) {
        log(`couldn't record that notification ${id} was opened: ${err.message}`);
      }
    }
    res.json({ ok: true, item, unseen: inbox.unseenCount() });
  }

  return { list, seen, announce, open };
}

module.exports = { createBackgroundRoutes };
