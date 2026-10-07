// The LLM proxy client: chat, embed and detect, over an injected
// fetch (no new dependency). 60 s timeout; one retry, only on 429/5xx.
// Optional: with no key, or off the VPN, `detect()` says so and every
// caller carries on without it. core/llm-proxy.ts wires the real config,
// credential and OS user; doctor.js uses this directly.
const { createHash } = require("crypto");
const req = require("./llm-proxy-request.js");
const environment = require("../environment.js");

/** What the Settings page for this proxy is called: the distribution's branding, else a plain name. */
function proxyName() {
  return (environment.branding.llmProxy && environment.branding.llmProxy.name) || "LLM proxy";
}

const DETECT_TTL_MS = 10 * 60 * 1000;

class LlmProxyError extends Error {
  constructor(code, message, status) {
    super(message);
    this.name = "LlmProxyError";
    this.code = code;
    this.status = status;
  }
}

function createLlmProxyClient({
  getConfig,
  getApiKey,
  osUser = "",
  fetchImpl = fetch,
  now = Date.now,
  sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms)),
  timeoutSignal = (ms) => AbortSignal.timeout(ms),
  timeoutMs = 60 * 1000,
  retryDelayMs = 1000,
}) {
  let detected = null; // { fingerprint, at, result }

  const settings = () => req.llmProxySettings(getConfig(), { osUser });

  /** The first few KB of an error answer (to classify it), then the body is freed. */
  async function readSome(res) {
    try {
      const text = typeof res.text === "function" ? await res.text() : "";
      return text.slice(0, 4000);
    } catch {
      return "";
    } finally {
      await release(res);
    }
  }

  function applicationMessage(app) {
    return `The LLM proxy rejected the request as not coming from the "${app}" application your key was created for. It recognises applications by the client name (User-Agent): set ⚙ Settings → ${proxyName()} → Client name to match, or ask the proxy team to add an application for this tool.`;
  }

  /** An answer we won't read: close its body so the connection is freed. */
  async function release(res) {
    try {
      await (res.body && typeof res.body.cancel === "function" ? res.body.cancel() : undefined);
    } catch {
      // nothing to free
    }
  }

  async function send({ url, init }) {
    for (let attempt = 0; ; attempt++) {
      let res;
      try {
        res = await fetchImpl(url, { ...init, redirect: "error", signal: timeoutSignal(timeoutMs) });
      } catch (err) {
        const tls = req.tlsErrorCode(err);
        if (tls) {
          throw new LlmProxyError("tls", `The LLM proxy's certificate isn't trusted by this Node (${tls}). Update the companion (node install.js --yes) so it starts with NODE_USE_SYSTEM_CA=1.`);
        }
        const reason = err && err.cause && err.cause.code ? ` ${err.cause.code}` : "";
        throw new LlmProxyError("unreachable", `The LLM proxy didn't answer (${err && err.message ? err.message : err}${reason}).`);
      }
      if (res.ok) {
        const text = await res.text();
        try {
          return JSON.parse(text);
        } catch {
          throw new LlmProxyError("bad-reply", "The LLM proxy's reply isn't JSON.");
        }
      }
      if (attempt === 0 && req.shouldRetry(res.status)) {
        await release(res);
        await sleep(retryDelayMs);
        continue;
      }
      const verdict = req.classifyReply(res.status, await readSome(res));
      if (verdict.state === "application") {
        throw new LlmProxyError("application", applicationMessage(verdict.app), res.status);
      }
      if (verdict.state === "user") {
        throw new LlmProxyError("user", `The LLM proxy didn't accept the sign-on user name (⚙ Settings → ${proxyName()} → Sign-on user name; it is usually your plain SSO name, not an email).`, res.status);
      }
      const code = verdict.state === "key-rejected" ? "key-rejected" : "http";
      throw new LlmProxyError(code, `The LLM proxy answered HTTP ${res.status}.`, res.status);
    }
  }

  /** `{text, value?, model, usage}`; see llm-proxy-request.js's parseChatResponse. */
  async function chat({ system, messages, responseSchema, maxTokens } = {}) {
    const request = req.buildChatRequest({ settings: settings(), apiKey: getApiKey(), system, messages, maxTokens, responseSchema });
    return req.parseChatResponse(await send(request), { responseSchema });
  }

  async function embed(texts) {
    const request = req.buildEmbedRequest({ settings: settings(), apiKey: getApiKey(), texts });
    return req.parseEmbedResponse(await send(request), texts.length);
  }

  /**
   * `{state, label, at}` with state ready | no-key | unreachable | tls |
   * application | user | key-rejected | error. The key check is live; the one tiny call (the
   * model list, no retry) is cached for 10 minutes per base URL and key.
   */
  async function detect({ refresh = false } = {}) {
    const apiKey = getApiKey();
    const s = settings();
    if (typeof apiKey !== "string" || !apiKey.trim()) {
      return { state: "no-key", label: req.DETECTION_LABELS["no-key"], at: now() };
    }
    // The whole key, hashed: two keys that end alike must never share a cached answer.
    const fingerprint = createHash("sha256").update([s.baseUrl, apiKey.trim(), s.userAgent, s.user, s.embeddingModel].join("\u0000")).digest("hex");
    if (!refresh && detected && detected.fingerprint === fingerprint && now() - detected.at < DETECT_TTL_MS) {
      return detected.result;
    }
    let state;
    let httpStatus = null;
    let app = null;
    try {
      const { url, init } = req.buildKeyCheckRequest({ settings: s, apiKey });
      const res = await fetchImpl(url, { ...init, redirect: "error", signal: timeoutSignal(10 * 1000) });
      httpStatus = res.status;
      if (res.status >= 200 && res.status < 300) {
        state = "ready";
        await release(res);
      } else {
        const verdict = req.classifyReply(res.status, await readSome(res));
        state = verdict.state;
        app = verdict.app || null;
      }
    } catch (err) {
      state = req.detectionState({ networkError: true, tlsError: req.tlsErrorCode(err) !== null });
    }
    let label = req.DETECTION_LABELS[state];
    // "error" alone doesn't say what to fix; the HTTP status does. And which
    // application the key belongs to is exactly what to match.
    if (state === "error" && Number.isInteger(httpStatus)) label = `${label} (HTTP ${httpStatus})`;
    if (state === "application" && app) label = `the key is for the "${app}" application, not this client (see Client name)`;
    const result = { state, label, at: now() };
    detected = { fingerprint, at: result.at, result };
    return result;
  }

  async function available() {
    return (await detect()).state === "ready";
  }

  return { chat, embed, detect, available, settings };
}

module.exports = { createLlmProxyClient, LlmProxyError, DETECT_TTL_MS };
