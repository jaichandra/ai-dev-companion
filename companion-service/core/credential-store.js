// An encrypted-at-rest map of the API tokens the service holds for the
// browser extension: Jira, Jenkins, Bitbucket, the MCP token, the
// Slack token and the LLM proxy key (ALLOWED_NAMES) — nothing else
// may be stored here. Plain
// JS (not TypeScript) for the same reason as the other core/*.js
// helpers: credential-store.test.js runs it with no build step, and
// Task 3's TypeScript loads it with require + a hand-written type cast.
//
// The file (companion-service/credentials.enc — see Task 3) holds one
// AES-256-GCM ciphertext of the whole {name: value} map, never
// individual values, so there's nothing to leak by reading its bytes
// without the key. The key lives in a separate file outside the
// project (also Task 3's job to place); this module only knows it by
// path. `fsImpl`/`cryptoImpl` are injectable for tests.
const fs = require("fs");
const crypto = require("crypto");
const path = require("path");

const ALG = "aes-256-gcm";
const IV_LENGTH = 12;
const KEY_LENGTH = 32;

// The only credential names this store will read or write. Anything
// else is a programming error in the caller, not a user-facing case.
const ALLOWED_NAMES = [
  ...require("../environment.js").siteCredentialNames(),
  "mcp.token",
  "slack.token",
  "llmProxy.apiKey",
];

// Shown whenever the ciphertext can't be turned back into the credential
// map — wrong/missing key, a tampered file, or corruption. There's no way
// to tell those apart from the outside, and no partial recovery: the only
// safe recommendation is to re-enter the tokens.
const UNDECRYPTABLE_MESSAGE = "Saved credentials can't be decrypted — re-enter your tokens in ✨ → ⚙ Settings.";

class CredentialStoreError extends Error {
  constructor(code, message) {
    super(message);
    this.name = "CredentialStoreError";
    this.code = code;
  }
}

function assertAllowedName(name) {
  if (!ALLOWED_NAMES.includes(name)) {
    throw new CredentialStoreError(
      "unknown-name",
      `"${name}" isn't a known credential name. Allowed: ${ALLOWED_NAMES.join(", ")}.`,
    );
  }
}

/** Reads keyPath, or null if it doesn't exist yet. Any other read
 * failure (permissions, a directory where a file is expected) is not
 * swallowed. */
function loadKeyIfExists(keyPath, fsImpl) {
  try {
    return fsImpl.readFileSync(keyPath);
  } catch (err) {
    if (err.code === "ENOENT") return null;
    throw err;
  }
}

/** Loads the key, creating a fresh 32-byte one on first use. The
 * directory is created 0700 if missing, and the key file itself 0600
 * with flag "wx" so two processes racing to create it can't stomp on
 * each other — the loser just re-reads what the winner wrote. */
function loadOrCreateKey(keyPath, fsImpl, cryptoImpl) {
  const existing = loadKeyIfExists(keyPath, fsImpl);
  if (existing) return existing;
  fsImpl.mkdirSync(path.dirname(keyPath), { recursive: true, mode: 0o700 });
  const key = cryptoImpl.randomBytes(KEY_LENGTH);
  try {
    fsImpl.writeFileSync(keyPath, key, { mode: 0o600, flag: "wx" });
    return key;
  } catch (err) {
    if (err.code === "EEXIST") return fsImpl.readFileSync(keyPath);
    throw err;
  }
}

/** Reads and JSON-parses filePath, or returns null if it doesn't exist
 * yet — a missing file is the normal "nothing saved" case, not an
 * error (see requirement: get on a missing file returns undefined). A
 * file that exists but isn't valid JSON is corruption, same as any
 * other undecryptable file. */
function readEnvelope(filePath, fsImpl) {
  let raw;
  try {
    raw = fsImpl.readFileSync(filePath, "utf8");
  } catch (err) {
    if (err.code === "ENOENT") return null;
    throw err;
  }
  try {
    return JSON.parse(raw);
  } catch {
    throw new CredentialStoreError("undecryptable", UNDECRYPTABLE_MESSAGE);
  }
}

/** Decrypts an envelope ({v, alg, iv, tag, data}, all base64 except v/alg)
 * back into the {name: value} map, using `key`. Any failure along the
 * way — a malformed envelope, a wrong key, a tampered iv/tag/data — is
 * folded into the same CredentialStoreError: there's no safe way to
 * distinguish them from outside, and no partial map is ever returned. */
function decryptEnvelope(envelope, key, cryptoImpl) {
  if (
    !envelope ||
    envelope.v !== 1 ||
    envelope.alg !== ALG ||
    typeof envelope.iv !== "string" ||
    typeof envelope.tag !== "string" ||
    typeof envelope.data !== "string"
  ) {
    throw new CredentialStoreError("undecryptable", UNDECRYPTABLE_MESSAGE);
  }
  try {
    const iv = Buffer.from(envelope.iv, "base64");
    const tag = Buffer.from(envelope.tag, "base64");
    const data = Buffer.from(envelope.data, "base64");
    const decipher = cryptoImpl.createDecipheriv(ALG, key, iv);
    decipher.setAuthTag(tag);
    const plain = Buffer.concat([decipher.update(data), decipher.final()]);
    const map = JSON.parse(plain.toString("utf8"));
    if (typeof map !== "object" || map === null || Array.isArray(map)) throw new Error("not a map");
    return map;
  } catch {
    throw new CredentialStoreError("undecryptable", UNDECRYPTABLE_MESSAGE);
  }
}

/** The full {name: value} map as currently saved, or {} if there's
 * nothing saved yet. Throws CredentialStoreError if the file exists but
 * can't be turned back into a map (missing/wrong key, tampering,
 * corruption). `get`, `list` and `remove` only ever read, so they all
 * throw straight from here — see loadMapOrEmpty for the one place
 * (`set`) that recovers instead. */
function loadMap(filePath, keyPath, fsImpl, cryptoImpl) {
  const envelope = readEnvelope(filePath, fsImpl);
  if (envelope === null) return {};
  const key = loadKeyIfExists(keyPath, fsImpl);
  if (!key) throw new CredentialStoreError("undecryptable", UNDECRYPTABLE_MESSAGE);
  return decryptEnvelope(envelope, key, cryptoImpl);
}

/** loadMap, but an undecryptable file is treated as an empty map instead
 * of thrown. Used only by `set`: the old values behind an undecryptable
 * file are gone no matter what `set` does (there's no key that can
 * recover them), and CredentialStoreError's own message tells the user
 * to re-enter their tokens in Settings — which would be a lie if the
 * very re-entry (`set`) also refused. So `set` on an undecryptable file
 * quietly starts fresh and atomically replaces it with whatever it can
 * write going forward, rather than leaving the user stuck. Any other
 * error (a real fs failure, not a decryption failure) still propagates. */
function loadMapOrEmpty(filePath, keyPath, fsImpl, cryptoImpl) {
  try {
    return loadMap(filePath, keyPath, fsImpl, cryptoImpl);
  } catch (err) {
    if (err instanceof CredentialStoreError && err.code === "undecryptable") return {};
    throw err;
  }
}

/** Encrypts `map` under a fresh random IV and writes it atomically
 * (temp file in the same directory, then rename) so a crash or a
 * concurrent reader never sees a half-written file. Always 0600 —
 * these are secrets, not settings with a looser default to preserve. */
function writeMap(map, filePath, key, fsImpl, cryptoImpl) {
  const iv = cryptoImpl.randomBytes(IV_LENGTH);
  const cipher = cryptoImpl.createCipheriv(ALG, key, iv);
  const plain = Buffer.from(JSON.stringify(map), "utf8");
  const encrypted = Buffer.concat([cipher.update(plain), cipher.final()]);
  const tag = cipher.getAuthTag();
  const envelope = {
    v: 1,
    alg: ALG,
    iv: iv.toString("base64"),
    tag: tag.toString("base64"),
    data: encrypted.toString("base64"),
  };
  const dir = path.dirname(filePath);
  const tmp = path.join(dir, `.${path.basename(filePath)}.${process.pid}.${Date.now()}.tmp`);
  try {
    fsImpl.writeFileSync(tmp, JSON.stringify(envelope), { mode: 0o600 });
    fsImpl.renameSync(tmp, filePath);
  } catch (err) {
    try {
      fsImpl.rmSync(tmp, { force: true });
    } catch {
      // Nothing to clean up.
    }
    throw err;
  }
}

/**
 * The pure core of the credential store Task 3 wires up as
 * companion-service/credentials.enc: `get`/`set`/`remove`/`list` over an
 * AES-256-GCM-encrypted {name: value} map, restricted to ALLOWED_NAMES.
 * `fsImpl`/`cryptoImpl` default to the real `fs`/`crypto` modules and are
 * only overridden by tests.
 */
function createCredentialStore({ filePath, keyPath, fsImpl = fs, cryptoImpl = crypto }) {
  function get(name) {
    assertAllowedName(name);
    return loadMap(filePath, keyPath, fsImpl, cryptoImpl)[name];
  }

  function set(name, value) {
    assertAllowedName(name);
    const map = loadMapOrEmpty(filePath, keyPath, fsImpl, cryptoImpl);
    map[name] = value;
    const key = loadOrCreateKey(keyPath, fsImpl, cryptoImpl);
    writeMap(map, filePath, key, fsImpl, cryptoImpl);
  }

  function remove(name) {
    assertAllowedName(name);
    const map = loadMap(filePath, keyPath, fsImpl, cryptoImpl);
    if (!(name in map)) return;
    delete map[name];
    const key = loadOrCreateKey(keyPath, fsImpl, cryptoImpl);
    writeMap(map, filePath, key, fsImpl, cryptoImpl);
  }

  function list() {
    return Object.keys(loadMap(filePath, keyPath, fsImpl, cryptoImpl));
  }

  return { get, set, remove, list };
}

module.exports = { createCredentialStore, CredentialStoreError, ALLOWED_NAMES };
