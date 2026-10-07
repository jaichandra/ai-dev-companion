// An LLM proxy's request and response shapes (OpenAI-compatible:
// chat completions, embeddings, the model list), and the one rule that
// matters most: only models in `llmProxy.allowedModels` — by default the
// on-prem ones — are ever sent anything, so a typo in a model name can't
// send ticket text to an outside vendor. Pure: core/llm-proxy-client.js
// does the HTTP.
// Some proxies match each request to the "application" the key was created for
// (QwenCode, Cline, OpenCode, …) by the client's User-Agent, and refuse
// requests that don't look like it ("Invalid request for qwencode
// application"). There is no generic application, so the client name is a
// setting: llmProxy.userAgent, by default this tool's own name.
let PACKAGE_VERSION = "0";
try {
  PACKAGE_VERSION = require("../package.json").version || "0";
} catch {
  // keep the placeholder
}
const DEFAULT_USER_AGENT = `ai-dev-companion/${PACKAGE_VERSION}`;
const USER_AGENT_RE = /^[\x21-\x7e][\x20-\x7e]{0,99}$/;

// The profile's chat and embedding models (environment.js `llm`) are the on-prem ones. Names it lists as
// retired are only aliases that resolve to the current chat model, and an alias can be re-pointed at any
// time, so they are not on the default allow-list.
const environment = require("../environment.js");
const ON_PREM_CHAT_MODELS = [environment.llm.chatModel];
const RETIRED_CHAT_MODELS = new Set(environment.llm.retiredChatModels);
const ON_PREM_EMBEDDING_MODELS = [environment.llm.embeddingModel];
const DEFAULT_ALLOWED_MODELS = [...ON_PREM_CHAT_MODELS, ...ON_PREM_EMBEDDING_MODELS];
const DEFAULTS = {
  baseUrl: environment.llm.baseUrl,
  chatModel: environment.llm.chatModel,
  embeddingModel: environment.llm.embeddingModel,
};
/** Where a user creates their key (the Settings "Create key" link). */
const KEY_PAGE_URL = environment.llm.keyPageUrl;
const PATHS = { chat: "/v1/chat/completions", embed: "/v1/embeddings", models: "/v1/models", keyCheck: "/v1/key/spend" };
const MODEL_RE = /^[A-Za-z0-9][A-Za-z0-9._:/-]{0,99}$/;
const USER_RE = /^[A-Za-z0-9._@-]{1,100}$/;
const MAX_MESSAGES = 20;
const MAX_PROMPT_CHARS = 200 * 1024;
const MAX_TOKENS = 4096;
const MAX_EMBED_TEXTS = 64;
const MAX_EMBED_CHARS = 32 * 1024;
const ROLES = ["system", "user", "assistant"];

class LlmProxyRequestError extends Error {}

const isObject = (v) => typeof v === "object" && v !== null && !Array.isArray(v);

function isHttpUrl(value) {
  try {
    const u = new URL(value);
    return (u.protocol === "https:" || u.protocol === "http:") && !!u.hostname && !u.username && !u.password;
  } catch {
    return false;
  }
}

// The key and the prompt text go to the proxy's host, so that host must be a
// allowed one (the profile's `llm.hostSuffixes`) over https. A teammate on a different internal host lists it in
// `llmProxy.allowedHostSuffixes` in config.json (hand-edit only, like allowedModels).
const DEFAULT_HOST_SUFFIXES = environment.llm.hostSuffixes.slice();
const SUFFIX_RE = /^\.?[a-z0-9-]+(\.[a-z0-9-]+)+$/i;

/** Whether `value` is an https URL with no credentials whose host is one of the suffixes (or the bare domain). */
function isAllowedProxyUrl(value, suffixes = DEFAULT_HOST_SUFFIXES) {
  try {
    const u = new URL(value);
    if (u.protocol !== "https:" || u.username || u.password || !u.hostname) return false;
    const host = u.hostname.toLowerCase();
    return suffixes.some((raw) => {
      const suffix = raw.toLowerCase().replace(/^\./, "");
      return host === suffix || host.endsWith(`.${suffix}`);
    });
  } catch {
    return false;
  }
}

function proxyUrlMessage(suffixes = DEFAULT_HOST_SUFFIXES) {
  return `must be an https:// URL on a ${suffixes.join(" / ")} host, without a user name (or blank for the default) — the key and your prompts are only sent there. A different internal host can be allowed in config.json (llmProxy.allowedHostSuffixes).`;
}

/** The effective proxy settings. `osUser` is the fallback sign-on name. */
function llmProxySettings(config, { osUser = "" } = {}) {
  const p = (config && config.llmProxy) || {};
  const baseUrl = typeof p.baseUrl === "string" && isHttpUrl(p.baseUrl.trim()) ? p.baseUrl.trim().replace(/\/+$/, "") : DEFAULTS.baseUrl;
  const user = typeof p.user === "string" && USER_RE.test(p.user.trim()) ? p.user.trim() : USER_RE.test(osUser) ? osUser : "";
  const allowed =
    Array.isArray(p.allowedModels) && p.allowedModels.length > 0 && p.allowedModels.every((m) => typeof m === "string" && MODEL_RE.test(m))
      ? [...new Set(p.allowedModels)]
      : DEFAULT_ALLOWED_MODELS.slice();
  const allowedHostSuffixes =
    Array.isArray(p.allowedHostSuffixes) && p.allowedHostSuffixes.length > 0 && p.allowedHostSuffixes.every((x) => typeof x === "string" && SUFFIX_RE.test(x.trim()))
      ? [...new Set(p.allowedHostSuffixes.map((x) => x.trim().toLowerCase()))]
      : DEFAULT_HOST_SUFFIXES.slice();
  const pick = (value, fallback) => (typeof value === "string" && value.trim() ? value.trim() : fallback);
  return {
    baseUrl,
    user,
    // The two retired default names move to the current default (a saved
    // retired alias would otherwise fail every call). Any other name outside the
    // allow-list is NOT replaced: it is refused, loudly, when a call is built.
    chatModel: RETIRED_CHAT_MODELS.has(pick(p.chatModel, "")) && !allowed.includes(pick(p.chatModel, "")) ? DEFAULTS.chatModel : pick(p.chatModel, DEFAULTS.chatModel),
    embeddingModel: pick(p.embeddingModel, DEFAULTS.embeddingModel),
    userAgent: typeof p.userAgent === "string" && USER_AGENT_RE.test(p.userAgent.trim()) ? p.userAgent.trim() : DEFAULT_USER_AGENT,
    allowedModels: allowed,
    allowedHostSuffixes,
  };
}

/**
 * The allow-list split by what each model is for, so the Settings dropdowns
 * only offer a chat model where a chat model goes and an embedding model
 * where an embedding model goes. A name counts as an embedding model when it
 * is a known one or says "embedding"; everything else is a chat model.
 */
function modelChoices(settings) {
  const allowed = (settings && settings.allowedModels) || DEFAULT_ALLOWED_MODELS;
  const isEmbedding = (m) => ON_PREM_EMBEDDING_MODELS.includes(m) || /embedding/i.test(m);
  return { chat: allowed.filter((m) => !isEmbedding(m)), embedding: allowed.filter(isEmbedding) };
}

/** Nothing is built for a base URL that is not https on an allowed host, so a hand-edited one never sends. */
function assertAllowedBaseUrl(settings) {
  const suffixes = settings.allowedHostSuffixes || DEFAULT_HOST_SUFFIXES;
  if (!isAllowedProxyUrl(settings.baseUrl, suffixes)) {
    throw new LlmProxyRequestError(`The LLM proxy address ${String(settings.baseUrl).slice(0, 80)} ${proxyUrlMessage(suffixes)} Nothing was sent.`);
  }
}

function assertAllowedModel(model, allowedModels) {
  if (typeof model !== "string" || !MODEL_RE.test(model) || !allowedModels.includes(model)) {
    throw new LlmProxyRequestError(
      `"${String(model).slice(0, 60)}" isn't one of the allowed models (${allowedModels.join(", ")}), so nothing was sent.`,
    );
  }
}

function headers(apiKey, userAgent) {
  if (typeof apiKey !== "string" || !apiKey.trim() || /\s/.test(apiKey.trim())) {
    throw new LlmProxyRequestError("No LLM proxy key is saved (✨ → ⚙ Settings → LLM proxy).");
  }
  const agent = typeof userAgent === "string" && USER_AGENT_RE.test(userAgent) ? userAgent : DEFAULT_USER_AGENT;
  return { authorization: `Bearer ${apiKey.trim()}`, "content-type": "application/json", accept: "application/json", "user-agent": agent };
}

function checkMessages(system, messages) {
  const list = [];
  if (system !== undefined) {
    if (typeof system !== "string" || !system.trim()) throw new LlmProxyRequestError("system must be non-empty text.");
    list.push({ role: "system", content: system });
  }
  if (!Array.isArray(messages) || messages.length === 0 || messages.length > MAX_MESSAGES) {
    throw new LlmProxyRequestError(`messages must be a list of 1 to ${MAX_MESSAGES} messages.`);
  }
  for (const m of messages) {
    if (!isObject(m) || !ROLES.includes(m.role) || typeof m.content !== "string") {
      throw new LlmProxyRequestError("each message needs a role (system, user or assistant) and text content.");
    }
    list.push({ role: m.role, content: m.content });
  }
  const total = list.reduce((n, m) => n + m.content.length, 0);
  if (total > MAX_PROMPT_CHARS) throw new LlmProxyRequestError(`the prompt is over ${MAX_PROMPT_CHARS} characters.`);
  return list;
}

/** `{url, init}` for one chat completion. `responseSchema` asks for JSON. */
function buildChatRequest({ settings, apiKey, system, messages, maxTokens = 1024, responseSchema }) {
  assertAllowedBaseUrl(settings);
  assertAllowedModel(settings.chatModel, settings.allowedModels);
  const body = {
    model: settings.chatModel,
    messages: checkMessages(system, messages),
    max_tokens: Math.max(1, Math.min(Number.isInteger(maxTokens) ? maxTokens : 1024, MAX_TOKENS)),
    temperature: 0,
    stream: false,
  };
  if (settings.user) body.user = settings.user;
  if (responseSchema) body.response_format = { type: "json_object" };
  return { url: `${settings.baseUrl}${PATHS.chat}`, init: { method: "POST", headers: headers(apiKey, settings.userAgent), body: JSON.stringify(body) } };
}

function buildEmbedRequest({ settings, apiKey, texts }) {
  assertAllowedBaseUrl(settings);
  assertAllowedModel(settings.embeddingModel, settings.allowedModels);
  if (!Array.isArray(texts) || texts.length === 0 || texts.length > MAX_EMBED_TEXTS) {
    throw new LlmProxyRequestError(`texts must be a list of 1 to ${MAX_EMBED_TEXTS} strings.`);
  }
  for (const t of texts) {
    if (typeof t !== "string" || !t.trim() || t.length > MAX_EMBED_CHARS) {
      throw new LlmProxyRequestError(`each text must be 1 to ${MAX_EMBED_CHARS} characters.`);
    }
  }
  const body = { model: settings.embeddingModel, input: texts };
  if (settings.user) body.user = settings.user;
  return { url: `${settings.baseUrl}${PATHS.embed}`, init: { method: "POST", headers: headers(apiKey, settings.userAgent), body: JSON.stringify(body) } };
}

/** The detection call: the model list, the smallest request there is. */
function buildModelsRequest({ settings, apiKey }) {
  assertAllowedBaseUrl(settings);
  return { url: `${settings.baseUrl}${PATHS.models}`, init: { method: "GET", headers: headers(apiKey, settings.userAgent) } };
}

/**
 * The detection call: one tiny embedding on the on-prem model. The model
 * list (/v1/models) is public on this proxy — 200 to any key or none — so it
 * can't tell a good key from a bad one, and /v1/key/spend wants the user in
 * a shape that varies. A real (free, on-prem) embedding exercises everything
 * a caller needs: the key, the application it was created for (matched by
 * the User-Agent), the `user` field and the model.
 */
function buildKeyCheckRequest({ settings, apiKey }) {
  return buildEmbedRequest({ settings, apiKey, texts: ["ping"] });
}

/**
 * What a non-2xx answer means, from its status and (a prefix of) its body.
 * Only fixed words come back — never the proxy's text, which can echo the
 * key. `state`: application | user | key-rejected | error.
 */
function classifyReply(status, bodyText) {
  const text = typeof bodyText === "string" ? bodyText.slice(0, 4000) : "";
  const app = /Invalid request for ([A-Za-z0-9._-]{1,40}) application/i.exec(text);
  if (app) return { state: "application", app: app[1] };
  if (/'user' (json )?field/i.test(text)) return { state: "user" };
  if (status === 401 || status === 403) return { state: "key-rejected" };
  return { state: "error" };
}

/** A tiny JSON-schema subset: type, properties, required, maxLength. Returns an error or null. */
function checkShape(value, schema, where = "reply") {
  const type = schema.type;
  const actual = Array.isArray(value) ? "array" : value === null ? "null" : typeof value;
  if (type && actual !== type) return `${where} should be ${type}, not ${actual}`;
  if (type === "string" && Number.isInteger(schema.maxLength) && value.length > schema.maxLength) {
    return `${where} is longer than ${schema.maxLength} characters`;
  }
  if (type === "object") {
    for (const key of schema.required || []) {
      if (!Object.prototype.hasOwnProperty.call(value, key)) return `${where}.${key} is missing`;
    }
    for (const [key, sub] of Object.entries(schema.properties || {})) {
      if (value[key] === undefined) continue;
      const err = checkShape(value[key], sub, `${where}.${key}`);
      if (err) return err;
    }
  }
  return null;
}

/** The first JSON object in a model's text (it may wrap it in a code fence). */
function extractJsonObject(text) {
  const trimmed = String(text).trim().replace(/^```(?:json)?\s*/i, "").replace(/```$/, "").trim();
  try {
    return JSON.parse(trimmed);
  } catch {
    const start = trimmed.indexOf("{");
    const end = trimmed.lastIndexOf("}");
    if (start === -1 || end <= start) return undefined;
    try {
      return JSON.parse(trimmed.slice(start, end + 1));
    } catch {
      return undefined;
    }
  }
}

/** `{text, value?, model, usage}` from a chat completion. With a
 * responseSchema, `value` is the parsed JSON, checked against it; a reply
 * that doesn't fit throws (callers fall back, never guess). */
function parseChatResponse(json, { responseSchema } = {}) {
  const choice = isObject(json) && Array.isArray(json.choices) ? json.choices[0] : undefined;
  const text = isObject(choice) && isObject(choice.message) ? choice.message.content : undefined;
  // Reasoning models think first (reasoning_content, 40-100+ tokens
  // even for a trivial prompt) and only then answer, all inside max_tokens: a
  // small budget leaves the answer empty or cut off, with finish_reason "length".
  if (isObject(choice) && choice.finish_reason === "length" && (typeof text !== "string" || !text.trim() || responseSchema)) {
    throw new LlmProxyRequestError("The LLM proxy's reply was cut off before the answer (the model's thinking used up the token budget).");
  }
  if (typeof text !== "string") throw new LlmProxyRequestError("The LLM proxy's reply has no message text.");
  const out = {
    text,
    model: typeof json.model === "string" ? json.model : null,
    usage: isObject(json.usage) ? { prompt: json.usage.prompt_tokens ?? null, completion: json.usage.completion_tokens ?? null } : null,
  };
  if (responseSchema) {
    const value = extractJsonObject(text);
    if (value === undefined) throw new LlmProxyRequestError("The LLM proxy's reply isn't JSON.");
    const err = checkShape(value, responseSchema);
    if (err) throw new LlmProxyRequestError(`The LLM proxy's reply doesn't fit: ${err}.`);
    out.value = value;
  }
  return out;
}

/** The vectors of an embeddings reply, in input order. */
function parseEmbedResponse(json, count) {
  const data = isObject(json) && Array.isArray(json.data) ? json.data : null;
  if (!data || data.length !== count) throw new LlmProxyRequestError("The LLM proxy returned the wrong number of embeddings.");
  const sorted = data.slice().sort((a, b) => (a && a.index) - (b && b.index));
  return sorted.map((d) => {
    if (!isObject(d) || !Array.isArray(d.embedding) || !d.embedding.every(Number.isFinite)) {
      throw new LlmProxyRequestError("An embedding in the LLM proxy's reply isn't a list of numbers.");
    }
    return d.embedding;
  });
}

/** Only rate limits and server errors are worth one retry. */
function shouldRetry(status) {
  return status === 429 || (status >= 500 && status <= 599);
}

/** A detection state from what the one tiny call did. */
// Node's fetch reports a certificate it doesn't trust as "fetch failed" with
// the reason in `cause.code`; a private-CA proxy certificate is only trusted
// through the system certificate store (NODE_USE_SYSTEM_CA=1).
const TLS_ERROR_CODES = new Set([
  "SELF_SIGNED_CERT_IN_CHAIN",
  "DEPTH_ZERO_SELF_SIGNED_CERT",
  "UNABLE_TO_VERIFY_LEAF_SIGNATURE",
  "UNABLE_TO_GET_ISSUER_CERT",
  "UNABLE_TO_GET_ISSUER_CERT_LOCALLY",
  "CERT_HAS_EXPIRED",
  "ERR_TLS_CERT_ALTNAME_INVALID",
]);

/** The certificate error code behind a failed fetch, or null. */
function tlsErrorCode(err) {
  const code = err && err.cause && typeof err.cause.code === "string" ? err.cause.code : err && typeof err.code === "string" ? err.code : null;
  return code && TLS_ERROR_CODES.has(code) ? code : null;
}

function detectionState({ status, networkError, tlsError } = {}) {
  if (tlsError) return "tls";
  if (networkError) return "unreachable";
  if (status === 401 || status === 403) return "key-rejected";
  if (status >= 200 && status < 300) return "ready";
  return "error";
}

const DETECTION_LABELS = {
  ready: "ready",
  "no-key": "no key",
  unreachable: "unreachable (VPN?)",
  tls: "certificate not trusted (update the companion and restart it)",
  application: "the key is for a different application (see Client name)",
  user: "sign-on user name not accepted",
  "key-rejected": "key rejected",
  error: "error",
};

module.exports = {
  ON_PREM_CHAT_MODELS,
  ON_PREM_EMBEDDING_MODELS,
  DEFAULT_ALLOWED_MODELS,
  DEFAULTS,
  KEY_PAGE_URL,
  PATHS,
  DETECTION_LABELS,
  LlmProxyRequestError,
  isHttpUrl,
  isAllowedProxyUrl,
  proxyUrlMessage,
  DEFAULT_HOST_SUFFIXES,
  assertAllowedBaseUrl,
  llmProxySettings,
  assertAllowedModel,
  buildChatRequest,
  buildEmbedRequest,
  buildModelsRequest,
  buildKeyCheckRequest,
  tlsErrorCode,
  modelChoices,
  classifyReply,
  DEFAULT_USER_AGENT,
  USER_AGENT_RE,
  checkShape,
  extractJsonObject,
  parseChatResponse,
  parseEmbedResponse,
  shouldRetry,
  detectionState,
};
