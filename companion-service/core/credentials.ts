// Thin wiring: the real companion-service credential store (over real
// paths) plus core/token-cache.js's stateful wrapper (the 60-second read
// cache, the warn-once latch, the undecryptable-store fallbacks) — all
// built once as module-level singletons and exposed as plain functions.
//
// Deliberately thin. Fix round 1 moved every bit of actual logic (the
// cache, setToken's immediate refresh, the undecryptable handling) into
// core/token-cache.js: this file is TypeScript, and `node --test`
// (companion-service/package.json's test script) never compiles
// TypeScript, so nothing meant to be unit-tested can live only here — see
// the Global Constraint that pure/stateful logic worth testing belongs in
// core/*.js with a sibling *.test.js. What's left here — resolving real
// paths, building the real store, wiring it into createTokenCache once —
// has no interesting behavior of its own to test.
import * as path from "path";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const claudeArgs = require("./claude-args.js") as { companionServiceDir(): string };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const paths = require("./paths.js") as { stateDir(home?: string): string };

/** The shape core/credential-store.js's createCredentialStore returns —
 * exported so server.ts/setup.js can type the store they hand to
 * core/settings.js's migrateTokensToStore without reaching into that JS
 * module themselves. */
export interface CredentialStore {
  get(name: string): string | undefined;
  set(name: string, value: string): void;
  remove(name: string): void;
  list(): string[];
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const credentialStoreModule = require("./credential-store.js") as {
  createCredentialStore(opts: { filePath: string; keyPath: string }): CredentialStore;
};

interface TokenCache {
  getToken(name: string): string | undefined;
  setToken(name: string, value: string | null): void;
  listNames(): string[];
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const tokenCacheModule = require("./token-cache.js") as {
  createTokenCache(opts: { store: CredentialStore; clock?: () => number; warn?: (message: string) => void }): TokenCache;
};

let store: CredentialStore | null = null;

/**
 * The one companion-service credential store: `companionServiceDir()/
 * credentials.enc`, keyed by `paths.stateDir()/credentials.key`. Built
 * lazily on first use (not at module load) and cached — every caller in
 * this process shares one instance, which is also what makes the read
 * cache in core/token-cache.js safe to key by name alone.
 */
export function credentialStore(): CredentialStore {
  if (!store) {
    store = credentialStoreModule.createCredentialStore({
      filePath: path.join(claudeArgs.companionServiceDir(), "credentials.enc"),
      keyPath: path.join(paths.stateDir(), "credentials.key"),
    });
  }
  return store;
}

let cache: TokenCache | null = null;

function tokenCache(): TokenCache {
  if (!cache) cache = tokenCacheModule.createTokenCache({ store: credentialStore() });
  return cache;
}

/** See core/token-cache.js's createTokenCache doc comment for the actual
 * behavior (60s cache, undecryptable -> undefined, warn once). */
export function getToken(name: string): string | undefined {
  return tokenCache().getToken(name);
}

/** See core/token-cache.js's createTokenCache doc comment — in particular,
 * `setToken(name, null)` against an undecryptable store is a no-op, not a
 * throw. */
export function setToken(name: string, value: string | null): void {
  tokenCache().setToken(name, value);
}

/** Every saved credential name, or `[]` on an undecryptable store (see
 * core/token-cache.js) — what core/settings.js's publicSettings needs to
 * compute `apiTokenSet` without ever risking a 500 on GET /settings. */
export function listTokenNames(): string[] {
  return tokenCache().listNames();
}
