// similar(item, k) for Phase 8: the past tickets and PRs in this user's
// local history most like a given one, from two rankings merged by
// reciprocal rank fusion — the FTS5 words it shares (always) and the
// cosine of its embedding (only when vectors exist and a query vector can
// be had: the item's own stored one, or one embedded on-prem through the
// LLM proxy when that is ready). With no proxy it is words only. A ticket
// and its analysis count as one result. Used by analyze-issue's prompt,
// the MCP tool find_similar and `companion similar`.
const similar = require("./similar.js");
const { normalizeKey } = require("./history-schema.js");
const { chooseTier } = require("./route-policy.js");

const CANDIDATES = 20;

function withTimeout(promise, ms) {
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error("timed out")), ms);
    if (timer.unref) timer.unref();
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer));
}

/**
 * @param {{history?: object, llm?: {available(): Promise<boolean>, embed(t: string[]): Promise<number[][]>, settings(): {embeddingModel: string}}, getConfig: () => object, embedTimeoutMs?: number}} deps
 */
function createSimilarSearch({ history, llm, getConfig, embedTimeoutMs = 8000 }) {
  /** A query vector: the item's stored one, else the text embedded on-prem; null when neither. */
  async function queryVector(itemId, text, model) {
    // No stored vectors to compare with: don't send text to the proxy for nothing.
    if (!model || !history.hasVectors(model)) return null;
    if (itemId !== null) {
      const stored = history.vectorFor(itemId, model);
      if (stored) return stored;
    }
    const embedInput = similar.embedText({ title: text });
    if (!embedInput || !llm) return null;
    try {
      if (!(await llm.available())) return null;
      const route = chooseTier({ task: "embed", bytes: Buffer.byteLength(embedInput, "utf8") }, { onpremAvailable: true });
      if (route.tier !== "onprem") return null;
      const [v] = await withTimeout(llm.embed([embedInput]), embedTimeoutMs);
      return similar.decodeVector(similar.encodeVector(v));
    } catch {
      // Words alone still work; a slow or failing proxy never blocks the caller.
      return null;
    }
  }

  function hydrate(r) {
    const main = history.getItem(r.key);
    const analysis = r.key.startsWith("jira:") ? history.getItem(`analysis:${r.key}`) : null;
    return {
      key: r.key,
      kind: main ? main.item.kind : r.key.startsWith("jira:") ? "ticket" : "pr",
      title: main ? main.item.title : null,
      analysis: analysis ? analysis.item.excerpt : null,
      repo: analysis ? analysis.item.repo : null,
      updatedAt: Math.max(main ? main.item.updatedAt : 0, analysis ? analysis.item.updatedAt : 0) || null,
      score: r.score,
      via: r.via,
    };
  }

  /**
   * `{key?, text?, k?}` -> `{enabled, mode: "off" | "text" | "vector+text", items}`.
   * With a key, its own history text is the query (unless `text` is given)
   * and it is left out of the results. Throws when there is nothing to
   * compare: no history, or a key with nothing recorded and no text.
   */
  async function find({ key, text, k } = {}) {
    if (!similar.similarSettings(getConfig()).enabled) return { enabled: false, mode: "off", items: [] };
    if (!history) throw new Error("Local history is off on this copy of the companion (it only records on the installed copy).");
    const selfKey = typeof key === "string" ? normalizeKey(key) : null;
    const exclude = selfKey ? similar.groupKeyOf(selfKey) || selfKey : null;
    let queryText = typeof text === "string" ? text : "";
    let itemId = null;
    if (selfKey) {
      const detail = history.getItem(selfKey);
      if (detail) {
        itemId = detail.item.id;
        if (!queryText.trim()) {
          const analysis = selfKey.startsWith("jira:") ? history.getItem(`analysis:${selfKey}`) : null;
          queryText = [detail.item.title, detail.item.excerpt, analysis && analysis.item.excerpt].filter(Boolean).join(" ");
        }
      }
    }
    if (!queryText.trim()) throw new Error(selfKey ? "Nothing is recorded under that key." : "Give a key or some text to compare.");
    const textHits = history.searchAny(queryText, { kinds: similar.VECTOR_KINDS, limit: CANDIDATES });
    // Any trouble on the vector side leaves the words-only answer.
    let vector = null;
    let vectorHits = [];
    try {
      const model = llm ? llm.settings().embeddingModel : null;
      vector = await queryVector(itemId, queryText, model);
      vectorHits = vector ? history.nearest({ model, vector, kinds: similar.VECTOR_KINDS, limit: CANDIDATES }) : [];
    } catch {
      vector = null;
      vectorHits = [];
    }
    const grouped = (hits) => hits.map((h) => ({ key: similar.groupKeyOf(h.key) })).filter((h) => h.key);
    const ranked = similar.combineScores({ vector: grouped(vectorHits), text: grouped(textHits) }, { k: similar.clampK(k), exclude });
    return { enabled: true, mode: vector ? "vector+text" : "text", items: ranked.map(hydrate) };
  }

  return { find };
}

module.exports = { createSimilarSearch };
