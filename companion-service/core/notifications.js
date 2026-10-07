// The inbox: what background work found for you (a conflict resolution
// ready to review, an analysis, a review request, the morning digest). The
// ✨ badge counts the unseen items, the ✨ menu lists them under "Ready for
// you", `companion inbox` prints them, and the extension turns new ones
// into Chrome notifications — batched, at most one every
// notify.minIntervalMinutes unless one is urgent, and none in quiet hours
// (held for the digest). Saved to stateDir()/notifications.json (0600) on
// the installed copy, in memory otherwise. Titles and bodies come from PR
// and ticket text, so they are clipped plain text and links must be https.
// Redacting secrets is the caller's job (the watchers and the digest build
// their text from PR and ticket fields only, never from raw error output).
const fs = require("fs");
const path = require("path");
const { randomUUID } = require("crypto");

const KINDS = ["conflict", "analysis", "review-request", "digest"];
const MAX_ITEMS = 100;
const DAY_MS = 24 * 60 * 60 * 1000;
const SEEN_TTL_MS = 7 * DAY_MS;
const UNSEEN_TTL_MS = 14 * DAY_MS;
const TITLE_MAX = 200;
const BODY_MAX = 1000;
const KEY_MAX = 400;
const DEFAULT_MIN_INTERVAL_MS = 30 * 60 * 1000;
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;

const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);

function clip(text, max, { multiline = false } = {}) {
  if (typeof text !== "string") return "";
  // Control, C1 and invisible/bidi characters out (newlines stay for multiline text).
  let clean = text.replace(/[\u0000-\u0008\u000b-\u001f\u007f-\u009f]/g, " ").replace(/[\u200b-\u200f\u202a-\u202e\u2060-\u2064\u2066-\u2069\ufeff]/g, "");
  clean = multiline ? clean.replace(/\n{3,}/g, "\n\n") : clean.replace(/\s+/g, " ");
  clean = clean.trim();
  return clean.length > max ? `${clean.slice(0, max - 1)}…` : clean;
}

function httpsUrl(value) {
  try {
    const u = new URL(value);
    return u.protocol === "https:" && !u.username && !u.password ? u.toString() : null;
  } catch {
    return null;
  }
}

const optionalString = (v, max) => (typeof v === "string" && v && v.length <= max ? v : undefined);

/** A new item from untrusted parts; throws on a missing key, kind or title. */
function cleanItem(input, now, id) {
  if (!isObject(input)) throw new Error("A notification must be an object.");
  const key = typeof input.key === "string" ? input.key.trim() : "";
  if (!key || key.length > KEY_MAX) throw new Error("A notification needs a key.");
  if (!KINDS.includes(input.kind)) throw new Error(`Unknown notification kind: ${String(input.kind).slice(0, 30)}`);
  const title = clip(input.title, TITLE_MAX);
  if (!title) throw new Error("A notification needs a title.");
  return {
    id,
    key,
    kind: input.kind,
    title,
    body: clip(input.body, BODY_MAX, { multiline: true }),
    url: input.url ? httpsUrl(input.url) : null,
    urgent: input.urgent === true,
    watcher: optionalString(input.watcher, 40),
    scopeKey: optionalString(input.scopeKey, 300),
    jobId: typeof input.jobId === "string" && UUID_RE.test(input.jobId) ? input.jobId : undefined,
    featureId: optionalString(input.featureId, 60),
    createdAt: now,
    updatedAt: now,
    seenAt: null,
    openedAt: null,
    announcedAt: null,
  };
}

/** Drops seen items after a week, unseen ones after two, and keeps the newest 100. */
function prune(items, now) {
  return items
    .filter((i) => (i.seenAt ? now - i.seenAt < SEEN_TTL_MS : now - i.createdAt < UNSEEN_TTL_MS))
    .sort((a, b) => b.updatedAt - a.updatedAt)
    .slice(0, MAX_ITEMS);
}

/**
 * What to announce as one Chrome notification now, or null: nothing new,
 * quiet hours, or the last one was under `minIntervalMs` ago — unless a
 * new item is urgent (a conflict on a PR that already has approvals).
 */
function planAnnouncement(items, { lastAnnouncedAt = 0, now, minIntervalMs, quiet }) {
  const pending = items.filter((i) => !i.seenAt && !i.announcedAt);
  if (pending.length === 0 || quiet) return null;
  const urgent = pending.some((i) => i.urgent);
  if (!urgent && now - lastAnnouncedAt < minIntervalMs) return null;
  const ordered = pending.slice().sort((a, b) => Number(b.urgent) - Number(a.urgent) || b.updatedAt - a.updatedAt);
  const single = ordered.length === 1 ? ordered[0] : null;
  return {
    ids: ordered.map((i) => i.id),
    urgent,
    title: single ? single.title : `${ordered.length} things are ready for you`,
    message: single ? single.body || "Open ✨ to see it." : ordered.slice(0, 3).map((i) => `• ${i.title}`).join("\n"),
    url: single ? single.url : null,
  };
}

const timeOrNull = (v) => (Number.isFinite(v) ? v : null);

/** An item read back from the file goes through the same cleaning as a new
 * one (a hand-edited file must not smuggle in a long title or a javascript:
 * link); ids and times are kept. Null when it isn't a usable item. */
function cleanLoaded(raw) {
  if (!isObject(raw) || typeof raw.id !== "string" || !raw.id || raw.id.length > 100 || !Number.isFinite(raw.createdAt)) return null;
  let item;
  try {
    item = cleanItem(raw, raw.createdAt, raw.id);
  } catch {
    return null;
  }
  return {
    ...item,
    updatedAt: Number.isFinite(raw.updatedAt) ? raw.updatedAt : raw.createdAt,
    seenAt: timeOrNull(raw.seenAt),
    openedAt: timeOrNull(raw.openedAt),
    announcedAt: timeOrNull(raw.announcedAt),
  };
}

function loadState(file, fsImpl) {
  if (!file) return { items: [], lastAnnouncedAt: 0 };
  let raw;
  try {
    raw = JSON.parse(fsImpl.readFileSync(file, "utf8"));
  } catch {
    return { items: [], lastAnnouncedAt: 0 };
  }
  const items = Array.isArray(raw && raw.items) ? raw.items.map((i) => cleanLoaded(i)).filter(Boolean) : [];
  return { items, lastAnnouncedAt: Number.isFinite(raw && raw.lastAnnouncedAt) ? raw.lastAnnouncedAt : 0 };
}

function createNotificationStore({ file = null, now = Date.now, fsImpl = fs, idGen = randomUUID } = {}) {
  let state = loadState(file, fsImpl);
  state.items = prune(state.items, now());
  state.lastAnnouncedAt = Math.min(state.lastAnnouncedAt, now());
  /** Expired items go the moment anything looks at the inbox, not only on the next write. */
  const live = () => {
    state.items = prune(state.items, now());
    return state.items;
  };

  function save() {
    state.items = prune(state.items, now());
    if (!file) return;
    const tmp = path.join(path.dirname(file), `.${path.basename(file)}.${process.pid}.tmp`);
    try {
      fsImpl.mkdirSync(path.dirname(file), { recursive: true, mode: 0o700 });
      fsImpl.writeFileSync(tmp, JSON.stringify({ version: 1, ...state }, null, 2) + "\n", { mode: 0o600 });
      fsImpl.renameSync(tmp, file);
    } catch (err) {
      try {
        fsImpl.rmSync(tmp, { force: true });
      } catch {
        /* nothing to clean up */
      }
      console.log(`[notifications] WARN - couldn't save the inbox: ${err.message}`);
    }
  }

  const pub = (i) => ({ ...i });

  return {
    /** Adds an item, or refreshes an unseen one with the same key. An item
     * already seen is never brought back (returns null). */
    add(input) {
      const at = now();
      const item = cleanItem(input, at, idGen());
      const existing = live().find((i) => i.key === item.key);
      if (existing) {
        if (existing.seenAt) return null;
        // A refresh that turns urgent (an approval landed) is worth telling again.
        if (item.urgent && !existing.urgent) existing.announcedAt = null;
        Object.assign(existing, { title: item.title, body: item.body, url: item.url, urgent: item.urgent, jobId: item.jobId, featureId: item.featureId, updatedAt: at });
        save();
        return pub(existing);
      }
      state.items.push(item);
      save();
      return pub(item);
    },
    list({ includeSeen = false, limit = 50 } = {}) {
      return prune(state.items, now())
        .filter((i) => includeSeen || !i.seenAt)
        .slice(0, Math.max(1, Math.min(limit, MAX_ITEMS)))
        .map(pub);
    },
    unseenCount() {
      return live().filter((i) => !i.seenAt).length;
    },
    get(id) {
      const item = live().find((i) => i.id === id);
      return item ? pub(item) : null;
    },
    /** `ids` is a list of ids, or "all". Returns how many became seen. */
    markSeen(ids) {
      const at = now();
      let n = 0;
      for (const i of live()) {
        if (i.seenAt || (ids !== "all" && !(Array.isArray(ids) && ids.includes(i.id)))) continue;
        i.seenAt = at;
        n++;
      }
      if (n) save();
      return n;
    },
    /** The user clicked an item: seen and opened (the first time is kept). */
    open(id) {
      const item = live().find((i) => i.id === id);
      if (!item) return null;
      const at = now();
      item.seenAt = item.seenAt || at;
      item.openedAt = item.openedAt || at;
      save();
      return pub(item);
    },
    /** Plans and records one announcement (see planAnnouncement). */
    announce({ quiet = false, minIntervalMs } = {}) {
      const at = now();
      // No (or a nonsense) interval fails closed: the default spacing, never "no limit".
      const spacing = Number.isFinite(minIntervalMs) && minIntervalMs >= 0 ? minIntervalMs : DEFAULT_MIN_INTERVAL_MS;
      state.lastAnnouncedAt = Math.min(state.lastAnnouncedAt, at);
      const plan = planAnnouncement(live(), { lastAnnouncedAt: state.lastAnnouncedAt, now: at, minIntervalMs: spacing, quiet });
      if (!plan) return null;
      for (const i of state.items) if (plan.ids.includes(i.id)) i.announcedAt = at;
      state.lastAnnouncedAt = at;
      save();
      return plan;
    },
    /** The morning digest covers what quiet hours held: mark it announced
     * so it doesn't also arrive as its own notification. */
    absorbPending() {
      const at = now();
      const ids = [];
      for (const i of live()) {
        if (i.seenAt || i.announcedAt || i.kind === "digest") continue;
        i.announcedAt = at;
        ids.push(i.id);
      }
      if (ids.length) save();
      return ids;
    },
  };
}

module.exports = { KINDS, MAX_ITEMS, cleanItem, prune, planAnnouncement, createNotificationStore };
