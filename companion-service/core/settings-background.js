// The Settings panel's background-work keys, kept apart from core/settings.js
// (which calls these three functions) so that file doesn't grow another
// 300 lines: watchers, the daily Claude budget, quiet hours, notification
// batching, the morning digest, the LLM proxy and the pre-push
// check, and (Phase 8) similar items. None needs a restart — the scheduler
// and analyze-issue read config live. Defaults are never persisted (same
// convention as reviewEditor's "auto").
const schedule = require("./schedule.js");
const llm = require("./llm-proxy-request.js");
const similar = require("./similar.js");

const BACKGROUND_KEYS = ["watchers", "budget", "scheduler", "notify", "digest", "llmProxy", "prePush", "similar"];
const PRE_PUSH_MODES = ["warn", "off"];
const LLM_TOKEN_NAME = "llmProxy.apiKey";
const USER_RE = /^[A-Za-z0-9._@-]{1,100}$/;
const MAX_TOKEN_LENGTH = 1000;

const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);
const isInt = (v, min, max) => Number.isInteger(v) && v >= min && v <= max;

function unknownKeys(errors, prefix, value, allowed) {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) errors.push({ field: `${prefix}.${key}`, message: "isn't a known setting." });
  }
}

function objectOrError(errors, field, value) {
  if (isObject(value)) return true;
  errors.push({ field, message: "must be an object." });
  return false;
}

/** Pushes `{field, message}` errors for the background keys in `update`. */
function validateBackgroundSettings(update, current, errors) {
  if (update.watchers !== undefined && objectOrError(errors, "watchers", update.watchers)) {
    unknownKeys(errors, "watchers", update.watchers, schedule.WATCHERS);
    for (const name of schedule.WATCHERS) {
      const w = update.watchers[name];
      if (w === undefined) continue;
      if (!objectOrError(errors, `watchers.${name}`, w)) continue;
      unknownKeys(errors, `watchers.${name}`, w, ["enabled", "intervalMinutes"]);
      if (w.enabled !== undefined && typeof w.enabled !== "boolean") errors.push({ field: `watchers.${name}.enabled`, message: "must be true or false." });
      if (w.intervalMinutes !== undefined && !isInt(w.intervalMinutes, schedule.MIN_INTERVAL_MINUTES, schedule.MAX_INTERVAL_MINUTES)) {
        errors.push({ field: `watchers.${name}.intervalMinutes`, message: "must be a whole number of minutes from 1 to 1440." });
      }
    }
  }
  if (update.budget !== undefined && objectOrError(errors, "budget", update.budget)) {
    unknownKeys(errors, "budget", update.budget, ["claudeRunsPerDay"]);
    const n = update.budget.claudeRunsPerDay;
    if (n !== undefined && !isInt(n, 0, schedule.MAX_CLAUDE_RUNS_PER_DAY)) {
      errors.push({ field: "budget.claudeRunsPerDay", message: `must be a whole number from 0 to ${schedule.MAX_CLAUDE_RUNS_PER_DAY} (0 turns background Claude runs off).` });
    }
  }
  if (update.scheduler !== undefined && objectOrError(errors, "scheduler", update.scheduler)) {
    unknownKeys(errors, "scheduler", update.scheduler, ["quietHours"]);
    const q = update.scheduler.quietHours;
    if (q !== undefined && q !== null) {
      if (!isObject(q) || schedule.parseHm(q.start) === null || schedule.parseHm(q.end) === null) {
        errors.push({ field: "scheduler.quietHours", message: 'needs a start and an end time like "19:00" and "08:00" (or null for none).' });
      } else if (schedule.parseHm(q.start) === schedule.parseHm(q.end)) {
        errors.push({ field: "scheduler.quietHours", message: "can't start and end at the same time." });
      } else {
        unknownKeys(errors, "scheduler.quietHours", q, ["start", "end"]);
      }
    }
  }
  if (update.notify !== undefined && objectOrError(errors, "notify", update.notify)) {
    unknownKeys(errors, "notify", update.notify, ["minIntervalMinutes"]);
    const m = update.notify.minIntervalMinutes;
    if (m !== undefined && !isInt(m, 0, 1440)) errors.push({ field: "notify.minIntervalMinutes", message: "must be a whole number of minutes from 0 to 1440." });
  }
  if (update.digest !== undefined && objectOrError(errors, "digest", update.digest)) {
    unknownKeys(errors, "digest", update.digest, ["enabled", "time"]);
    if (update.digest.enabled !== undefined && typeof update.digest.enabled !== "boolean") errors.push({ field: "digest.enabled", message: "must be true or false." });
    if (update.digest.time !== undefined && schedule.parseHm(update.digest.time) === null) errors.push({ field: "digest.time", message: 'must be a time like "08:30".' });
  }
  if (update.llmProxy !== undefined && objectOrError(errors, "llmProxy", update.llmProxy)) {
    const p = update.llmProxy;
    for (const key of Object.keys(p)) {
      if (key === "allowedModels") {
        errors.push({ field: "llmProxy.allowedModels", message: "can only be changed in config.json, so the Settings panel can't point the proxy at an outside model." });
      } else if (key === "allowedHostSuffixes") {
        errors.push({ field: "llmProxy.allowedHostSuffixes", message: "can only be changed in config.json, so the Settings panel can't send your key to an outside host." });
      } else if (!["baseUrl", "user", "userAgent", "chatModel", "embeddingModel", "apiKey", "clearApiKey"].includes(key)) {
        errors.push({ field: `llmProxy.${key}`, message: "isn't a known setting." });
      }
    }
    if (p.baseUrl !== undefined && (typeof p.baseUrl !== "string" || (p.baseUrl.trim() && !llm.isAllowedProxyUrl(p.baseUrl.trim(), llm.llmProxySettings(current).allowedHostSuffixes)))) {
      errors.push({ field: "llmProxy.baseUrl", message: llm.proxyUrlMessage(llm.llmProxySettings(current).allowedHostSuffixes) });
    }
    if (p.user !== undefined && (typeof p.user !== "string" || (p.user.trim() && !USER_RE.test(p.user.trim())))) {
      errors.push({ field: "llmProxy.user", message: "must be your sign-on user name (letters, digits, . _ @ -), or blank for your OS user name." });
    }
    if (p.userAgent !== undefined && (typeof p.userAgent !== "string" || (p.userAgent.trim() && !llm.USER_AGENT_RE.test(p.userAgent.trim())))) {
      errors.push({ field: "llmProxy.userAgent", message: "must be a short name like ai-dev-companion/1.0 (printable characters, up to 100), or blank for the default." });
    }
    const choices = llm.modelChoices(llm.llmProxySettings(current));
    for (const [key, list] of [["chatModel", choices.chat], ["embeddingModel", choices.embedding]]) {
      const v = p[key];
      if (v === undefined) continue;
      if (typeof v !== "string" || (v.trim() && !list.includes(v.trim()))) {
        errors.push({ field: `llmProxy.${key}`, message: `must be one of the allowed ${key === "chatModel" ? "chat" : "embedding"} models: ${list.join(", ")}.` });
      }
    }
    if (p.apiKey !== undefined && (typeof p.apiKey !== "string" || p.apiKey.length > MAX_TOKEN_LENGTH || /\s/.test(p.apiKey.trim()))) {
      errors.push({ field: "llmProxy.apiKey", message: "must be the key from the proxy site, with no spaces." });
    }
    if (p.clearApiKey !== undefined && typeof p.clearApiKey !== "boolean") errors.push({ field: "llmProxy.clearApiKey", message: "must be true or false." });
  }
  if (update.prePush !== undefined && objectOrError(errors, "prePush", update.prePush)) {
    unknownKeys(errors, "prePush", update.prePush, ["mode"]);
    if (update.prePush.mode !== undefined && !PRE_PUSH_MODES.includes(update.prePush.mode)) {
      errors.push({ field: "prePush.mode", message: `must be one of: ${PRE_PUSH_MODES.join(", ")}.` });
    }
  }
  if (update.similar !== undefined && objectOrError(errors, "similar", update.similar)) {
    unknownKeys(errors, "similar", update.similar, ["enabled"]);
    if (update.similar.enabled !== undefined && typeof update.similar.enabled !== "boolean") {
      errors.push({ field: "similar.enabled", message: "must be true or false." });
    }
  }
}

/** Applies an already-validated update's background keys onto `next` (a
 * copy of the saved config), pushing any LLM key change onto `tokenOps`. */
function mergeBackgroundSettings(next, update, tokenOps) {
  if (update.watchers !== undefined) {
    const merged = {};
    for (const name of schedule.WATCHERS) {
      const saved = isObject(next.watchers) && isObject(next.watchers[name]) ? next.watchers[name] : {};
      const w = { ...saved, ...(update.watchers[name] || {}) };
      const out = {};
      if (w.enabled === true) out.enabled = true;
      if (w.intervalMinutes !== undefined && w.intervalMinutes !== schedule.WATCHER_DEFAULT_MINUTES[name]) out.intervalMinutes = w.intervalMinutes;
      if (Object.keys(out).length) merged[name] = out;
    }
    if (Object.keys(merged).length) next.watchers = merged;
    else delete next.watchers;
  }
  if (update.budget && update.budget.claudeRunsPerDay !== undefined) {
    if (update.budget.claudeRunsPerDay === schedule.DEFAULT_CLAUDE_RUNS_PER_DAY) delete next.budget;
    else next.budget = { claudeRunsPerDay: update.budget.claudeRunsPerDay };
  }
  if (update.scheduler && update.scheduler.quietHours !== undefined) {
    const q = update.scheduler.quietHours;
    if (q === null) delete next.scheduler;
    else next.scheduler = { quietHours: { start: q.start.trim(), end: q.end.trim() } };
  }
  if (update.notify && update.notify.minIntervalMinutes !== undefined) {
    if (update.notify.minIntervalMinutes === schedule.DEFAULT_NOTIFY_MIN_INTERVAL_MINUTES) delete next.notify;
    else next.notify = { minIntervalMinutes: update.notify.minIntervalMinutes };
  }
  if (update.digest !== undefined) {
    const d = { ...(isObject(next.digest) ? next.digest : {}), ...update.digest };
    const out = {};
    if (d.enabled === true) out.enabled = true;
    if (typeof d.time === "string" && d.time.trim() !== schedule.DEFAULT_DIGEST_TIME) out.time = d.time.trim();
    if (Object.keys(out).length) next.digest = out;
    else delete next.digest;
  }
  if (update.llmProxy !== undefined) {
    const p = update.llmProxy;
    const out = { ...(isObject(next.llmProxy) ? next.llmProxy : {}) };
    for (const [key, fallback] of [
      ["baseUrl", llm.DEFAULTS.baseUrl],
      ["user", null],
      ["userAgent", llm.DEFAULT_USER_AGENT],
      ["chatModel", llm.DEFAULTS.chatModel],
      ["embeddingModel", llm.DEFAULTS.embeddingModel],
    ]) {
      if (p[key] === undefined) continue;
      let v = p[key].trim();
      if (key === "baseUrl") v = v.replace(/\/+$/, "");
      if (v && v !== fallback) out[key] = v;
      else delete out[key];
    }
    if (p.clearApiKey === true) tokenOps.push({ name: LLM_TOKEN_NAME, value: null });
    else if (typeof p.apiKey === "string" && p.apiKey.trim()) tokenOps.push({ name: LLM_TOKEN_NAME, value: p.apiKey.trim() });
    if (Object.keys(out).length) next.llmProxy = out;
    else delete next.llmProxy;
  }
  if (update.prePush && update.prePush.mode !== undefined) {
    if (update.prePush.mode === "warn") delete next.prePush;
    else next.prePush = { mode: update.prePush.mode };
  }
  if (update.similar && update.similar.enabled !== undefined) {
    // On is the default: only "off" is saved.
    if (update.similar.enabled) delete next.similar;
    else next.similar = { enabled: false };
  }
}

/** What the panel shows for the background keys — the key only as `apiKeySet`. */
function publicBackgroundSettings(config, savedTokenNames) {
  const quiet = schedule.quietHoursFrom(config);
  const proxy = llm.llmProxySettings(config);
  const saved = (config && config.llmProxy) || {};
  const mode = config && config.prePush && PRE_PUSH_MODES.includes(config.prePush.mode) ? config.prePush.mode : "warn";
  return {
    watchers: schedule.watcherSettings(config),
    budget: { claudeRunsPerDay: schedule.budgetLimit(config) },
    scheduler: { quietHours: quiet ? { start: config.scheduler.quietHours.start.trim(), end: config.scheduler.quietHours.end.trim() } : null },
    notify: { minIntervalMinutes: schedule.notifyMinIntervalMs(config) / 60000 },
    digest: schedule.digestSettings(config),
    llmProxy: {
      baseUrl: typeof saved.baseUrl === "string" ? saved.baseUrl : "",
      user: typeof saved.user === "string" ? saved.user : "",
      userAgent: typeof saved.userAgent === "string" ? saved.userAgent : "",
      defaultUserAgent: llm.DEFAULT_USER_AGENT,
      chatModel: proxy.chatModel,
      embeddingModel: proxy.embeddingModel,
      allowedModels: proxy.allowedModels,
      chatModels: llm.modelChoices(proxy).chat,
      embeddingModels: llm.modelChoices(proxy).embedding,
      allowedHostSuffixes: proxy.allowedHostSuffixes,
      apiKeySet: savedTokenNames.includes(LLM_TOKEN_NAME),
      keyPageUrl: llm.KEY_PAGE_URL,
      defaultBaseUrl: llm.DEFAULTS.baseUrl,
    },
    prePush: { mode },
    similar: similar.similarSettings(config),
  };
}

module.exports = {
  BACKGROUND_KEYS,
  PRE_PUSH_MODES,
  LLM_TOKEN_NAME,
  validateBackgroundSettings,
  mergeBackgroundSettings,
  publicBackgroundSettings,
};
