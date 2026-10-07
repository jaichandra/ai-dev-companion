// The scheduler's "history.embed" task (Phase 8): embeds history items that
// have no vector yet with the configured on-prem embedding model, so similar
// items can be found by meaning, not only by shared words. It runs only when
// the LLM proxy is ready (otherwise it records "skipped: no proxy"
// and similar-item search uses the words alone), never sends text anywhere
// but the proxy's on-prem model (route-policy "embed" -> "onprem", and the
// proxy client refuses a model outside llmProxy.allowedModels), masks
// secrets and clips every text, and does at most 48 items a run.
const similar = require("./similar.js");
const { chooseTier } = require("./route-policy.js");

/**
 * @param {{history: object, llm: {detect(): Promise<{state: string}>, embed(texts: string[]): Promise<number[][]>, settings(): {embeddingModel: string}}, getConfig: () => object, now?: () => number}} deps
 * @returns {() => Promise<Record<string, unknown>>} a scheduler TaskFn
 */
function createEmbedTask({ history, llm, getConfig, now = Date.now }) {
  return async function embedTask() {
    if (!similar.similarSettings(getConfig()).enabled) return { skipped: "similar items are off", embedded: 0 };
    if (!history) return { skipped: "local history is off", embedded: 0 };
    const detection = await llm.detect();
    if (detection.state !== "ready") return { skipped: "no proxy", proxy: detection.state, embedded: 0 };
    const model = llm.settings().embeddingModel;
    // One more than the run's share, to know whether there is more waiting.
    const found = history.pendingEmbeddings({ model, kinds: similar.VECTOR_KINDS, limit: similar.EMBED_PER_RUN + 1 });
    const more = found.length > similar.EMBED_PER_RUN;
    const pending = found.slice(0, similar.EMBED_PER_RUN);
    let skipped = 0;
    // An item that can't be embedded (no text, or the model's answer for it
    // can't be stored) gets a one-number placeholder vector, which the
    // nearest-neighbour search never matches (it compares equal sizes only).
    // That takes it out of the pending list until it changes, so it can't
    // starve the items behind it run after run.
    const skip = (item) => {
      skipped++;
      try {
        history.saveVector({ itemId: item.id, model, vector: [1], itemUpdatedAt: item.updatedAt, at: now() });
      } catch {
        // Still pending; the other items are not held up by it.
      }
    };
    const work = [];
    for (const item of pending) {
      const text = similar.embedText(item);
      if (text) work.push({ item, text });
      else skip(item);
    }
    const tail = () => (skipped ? { skippedItems: skipped } : {});
    if (work.length === 0) return { embedded: 0, model, tier: "onprem", ...tail() };
    const bytes = Math.max(...work.map((w) => Buffer.byteLength(w.text, "utf8")));
    const route = chooseTier({ task: "embed", bytes }, { onpremAvailable: true });
    // Embedding never falls back to Claude: if the rules say no, it waits.
    if (route.tier !== "onprem") return { skipped: route.reason, embedded: 0 };
    let embedded = 0;
    for (let i = 0; i < work.length; i += similar.EMBED_BATCH) {
      const batch = work.slice(i, i + similar.EMBED_BATCH);
      let vectors;
      try {
        vectors = await llm.embed(batch.map((w) => w.text));
      } catch (err) {
        return { outcome: "failed", error: err && err.message ? err.message : String(err), embedded, model, tier: "onprem", ...tail() };
      }
      batch.forEach((w, j) => {
        try {
          history.saveVector({ itemId: w.item.id, model, vector: vectors[j], itemUpdatedAt: w.item.updatedAt, at: now() });
          embedded++;
        } catch {
          skip(w.item);
        }
      });
    }
    return { embedded, model, tier: "onprem", more, ...tail() };
  };
}

module.exports = { createEmbedTask };
