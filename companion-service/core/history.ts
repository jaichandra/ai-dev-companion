// The TypeScript face of core/history-db.js (plain JS so node:test covers it,
// and because @types/node has no node:sqlite types).
export interface HistoryEdge {
  rel: string;
  at: number;
  kind: string;
  key: string;
  title: string | null;
  direction: "in" | "out";
}

export interface HistoryDetail {
  item: {
    id: number;
    kind: string;
    key: string;
    repo: string | null;
    title: string | null;
    url: string | null;
    excerpt: string | null;
    updatedAt: number;
    data: unknown;
  };
  edges: HistoryEdge[];
  facts: { id: number; kind: string; value: unknown; source: string | null; provenance: string; at: number }[];
}

export interface HistoryMetricRow {
  featureId: string;
  outcome: string;
  count: number;
  medianMs: number | null;
  avgMs: number | null;
}

export interface HistoryPrewarmRow {
  watcher: string;
  runs: number;
  used: number;
  discarded: number;
  failed: number;
  expired: number;
  usedFraction: number | null;
}

export interface HistoryEventInput {
  jobId: string;
  featureId: string;
  scopeKey?: string;
  status: string;
  at?: number;
  durationMs?: number;
  outcome: string;
  metrics?: Record<string, unknown>;
}

/** An item as the similar-item search sees it (Phase 8). */
export interface HistorySimilarItem {
  id: number;
  kind: string;
  key: string;
  title: string | null;
  excerpt: string | null;
  updatedAt: number;
  /** Cosine similarity, for nearest() hits only. */
  score?: number;
}

export interface HistoryStore {
  upsertItem(item: {
    kind: string;
    key: string;
    repo?: string;
    title?: string;
    url?: string;
    excerpt?: string;
    data?: unknown;
    at?: number;
  }): number;
  addEdge(edge: { src: number; dst: number; rel: string; at?: number }): void;
  addFact(fact: {
    itemId: number;
    kind: string;
    value: unknown;
    source?: string;
    provenance: string;
    at?: number;
  }): number;
  recordEvent(event: HistoryEventInput): void;
  getItem(key: string): HistoryDetail | null;
  search(query: string, limit?: number): { kind: string; key: string; title: string | null; updatedAt: number }[];
  forget(key: string): { items: number; events: number };
  prune(retentionDays: number, at?: number): { items: number; events: number };
  metrics(opts?: { days?: number; at?: number }): HistoryMetricRow[];
  prewarmMetrics(opts?: { days?: number; at?: number }): HistoryPrewarmRow[];
  pendingEmbeddings(opts: { model: string; kinds?: string[]; limit?: number }): HistorySimilarItem[];
  saveVector(v: { itemId: number; model: string; vector: number[]; itemUpdatedAt: number; at?: number }): void;
  vectorFor(itemId: number, model: string): Float32Array | null;
  nearest(opts: { model: string; vector: Float32Array; kinds?: string[]; limit?: number; minScore?: number }): HistorySimilarItem[];
  searchAny(text: string, opts?: { kinds?: string[]; limit?: number }): HistorySimilarItem[];
  vectorStats(): { vectors: number; embeddable: number; models: { model: string; dim: number; count: number }[] };
  close(): void;
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const historyDb = require("./history-db.js") as { openHistory(file: string): HistoryStore };

export function openHistoryStore(file: string): HistoryStore {
  return historyDb.openHistory(file);
}
