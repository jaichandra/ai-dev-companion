// The stateful half of Task 3's credential wiring: a short read cache, a
// "warn once, not on every call" latch, and the undecryptable-store
// fallbacks — all pulled out of core/credentials.ts into plain JS so this
// logic actually gets a test file. credentials.ts is TypeScript, and
// `node --test` (companion-service/package.json's test script) never
// compiles TypeScript, so anything living only there is untestable by the
// project's own test runner — see the Global Constraint that pure logic
// belongs in core/*.js with a sibling *.test.js. credentials.ts becomes
// thin wiring: build the real store, call createTokenCache once, export
// its methods.
//
// Deliberately duck-types the "undecryptable" error (`err.code ===
// "undecryptable"`) rather than requiring core/credential-store.js's
// CredentialStoreError class — keeps this module usable with any store
// shape a test hands it, real or fake, with no import of that module at
// all.
const CACHE_TTL_MS = 60_000;

function isUndecryptable(err) {
  return !!err && err.code === "undecryptable";
}

/**
 * Wraps `store` (credential-store.js's `{get, set, remove, list}`, or a
 * fake in tests) with:
 *   - getToken: `store.get`, cached per name for 60 seconds.
 *   - setToken: `store.set`/`store.remove`, and immediately refreshes the
 *     cache entry so a Settings save takes effect on the very next
 *     getToken rather than waiting for the old entry to expire.
 *   - listNames: `store.list`.
 *
 * All three treat an undecryptable store (wrong/missing key, tampering —
 * `store.get`/`store.remove`/`store.list` all throw via credential-
 * store.js's loadMap; `store.set` never does, by that module's own
 * design) as "nothing saved" rather than a hard failure:
 *   - getToken returns undefined.
 *   - listNames returns [].
 *   - setToken(name, null) (clearing a token) is a no-op — there is
 *     nothing to remove from a file that can't be read anyway, so this
 *     is "already removed" in every observable sense, not a failure the
 *     caller needs to handle. Both server.ts routes that reach here
 *     (GET/PUT /settings) would otherwise 500 or crash the process on
 *     exactly the condition their own error message tells the user to
 *     fix from the Settings panel that just failed to load.
 *   - setToken(name, someValue) is unaffected: `store.set` already
 *     starts fresh on an undecryptable file instead of throwing, which
 *     is also this cache's one recovery path — see below.
 *
 * The underlying warning is logged at most once per createTokenCache
 * instance (not once per method, and not once per name) — repeated calls
 * to getToken/listNames/setToken(name, null) against a store that's still
 * broken share this one latch, only reset once a `store.set` call
 * actually succeeds (the real recovery: someone re-entered a token in
 * Settings, and credential-store.js's `set` wrote a fresh file). A
 * swallowed `store.remove` failure does NOT reset it — nothing about the
 * file changed, so warning again on the very next call would just be the
 * same noise this latch exists to avoid.
 *
 * `clock`/`warn` are injectable for tests; `store` always is (this
 * factory has no default — every caller, real or test, supplies one).
 */
function createTokenCache({ store, clock = Date.now, warn = (message) => console.warn(message) }) {
  const cache = new Map();
  let warnedUndecryptable = false;

  function warnOnce(err) {
    if (warnedUndecryptable) return;
    warn(`[credentials] ${err.message}`);
    warnedUndecryptable = true;
  }

  function cacheSet(name, value) {
    cache.set(name, { value, expiresAt: clock() + CACHE_TTL_MS });
  }

  function getToken(name) {
    const now = clock();
    const cached = cache.get(name);
    if (cached && cached.expiresAt > now) return cached.value;

    let value;
    try {
      value = store.get(name);
    } catch (err) {
      if (!isUndecryptable(err)) throw err;
      warnOnce(err);
      value = undefined;
    }
    cache.set(name, { value, expiresAt: now + CACHE_TTL_MS });
    return value;
  }

  function setToken(name, value) {
    if (value === null) {
      try {
        store.remove(name);
      } catch (err) {
        if (!isUndecryptable(err)) throw err;
        warnOnce(err);
        // Nothing to remove from a file that can't be read — already
        // gone, as far as anything downstream can tell.
      }
    } else {
      store.set(name, value);
      // A successful set is exactly how someone recovers from a corrupt
      // store (credential-store.js's `set` starts fresh instead of
      // throwing) — give the very next getToken/listNames a fair try at
      // the now-valid file instead of leaving it suppressed by a stale
      // warning.
      warnedUndecryptable = false;
    }
    cacheSet(name, value === null ? undefined : value);
  }

  function listNames() {
    try {
      return store.list();
    } catch (err) {
      if (!isUndecryptable(err)) throw err;
      warnOnce(err);
      return [];
    }
  }

  return { getToken, setToken, listNames };
}

module.exports = { createTokenCache };
