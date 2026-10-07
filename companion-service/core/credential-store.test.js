const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("fs");
const os = require("os");
const path = require("path");
const crypto = require("crypto");
const { createCredentialStore, CredentialStoreError, ALLOWED_NAMES } = require("./credential-store.js");

const UNDECRYPTABLE_MESSAGE = "Saved credentials can't be decrypted — re-enter your tokens in ✨ → ⚙ Settings.";

function makeStore(overrides = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "credential-store-test-"));
  const filePath = overrides.filePath || path.join(dir, "credentials.enc");
  const keyPath = overrides.keyPath || path.join(dir, "keys", "credentials.key");
  const store = createCredentialStore({ filePath, keyPath, ...overrides });
  return { dir, filePath, keyPath, store };
}

test("get on a missing file returns undefined, without creating a key", () => {
  const { store, keyPath } = makeStore();
  assert.equal(store.get("jira.apiToken"), undefined);
  assert.equal(fs.existsSync(keyPath), false);
});

test("list on a missing file returns an empty array", () => {
  const { store } = makeStore();
  assert.deepEqual(store.list(), []);
});

test("round trip: set then get returns the same value", () => {
  const { store } = makeStore();
  store.set("jira.apiToken", "secret-123");
  assert.equal(store.get("jira.apiToken"), "secret-123");
});

test("round trip survives a fresh store instance reading the same files", () => {
  const { filePath, keyPath } = makeStore();
  const first = createCredentialStore({ filePath, keyPath });
  first.set("slack.token", "xoxb-abc");
  const second = createCredentialStore({ filePath, keyPath });
  assert.equal(second.get("slack.token"), "xoxb-abc");
});

test("set stores multiple names independently", () => {
  const { store } = makeStore();
  store.set("jira.apiToken", "a");
  store.set("jenkins.apiToken", "b");
  assert.equal(store.get("jira.apiToken"), "a");
  assert.equal(store.get("jenkins.apiToken"), "b");
  assert.deepEqual(store.list().sort(), ["jenkins.apiToken", "jira.apiToken"]);
});

test("list returns names only, never values", () => {
  const { store } = makeStore();
  store.set("bitbucket.apiToken", "top-secret-value");
  const names = store.list();
  assert.deepEqual(names, ["bitbucket.apiToken"]);
  for (const name of names) assert.notEqual(name, "top-secret-value");
});

test("every write uses a fresh IV, so the ciphertext differs for the same value", () => {
  const { store, filePath } = makeStore();
  store.set("mcp.token", "same-value");
  const first = JSON.parse(fs.readFileSync(filePath, "utf8"));
  store.set("mcp.token", "same-value");
  const second = JSON.parse(fs.readFileSync(filePath, "utf8"));
  assert.notEqual(first.iv, second.iv);
  assert.notEqual(first.data, second.data);
  assert.equal(second.v, 1);
  assert.equal(second.alg, "aes-256-gcm");
});

test("remove deletes a stored name", () => {
  const { store } = makeStore();
  store.set("jira.apiToken", "a");
  store.set("jenkins.apiToken", "b");
  store.remove("jira.apiToken");
  assert.equal(store.get("jira.apiToken"), undefined);
  assert.equal(store.get("jenkins.apiToken"), "b");
  assert.deepEqual(store.list(), ["jenkins.apiToken"]);
});

test("remove on a missing file is a harmless no-op", () => {
  const { store, filePath } = makeStore();
  store.remove("jira.apiToken");
  assert.equal(fs.existsSync(filePath), false);
});

test("remove of a name never set is a harmless no-op", () => {
  const { store } = makeStore();
  store.set("jira.apiToken", "a");
  store.remove("jenkins.apiToken");
  assert.equal(store.get("jira.apiToken"), "a");
});

test("an unknown name throws CredentialStoreError for get, set and remove", () => {
  const { store } = makeStore();
  assert.throws(() => store.get("not.a.thing"), CredentialStoreError);
  assert.throws(() => store.set("not.a.thing", "x"), CredentialStoreError);
  assert.throws(() => store.remove("not.a.thing"), CredentialStoreError);
});

test("ALLOWED_NAMES is exactly the six known credential names", () => {
  assert.deepEqual(ALLOWED_NAMES, [
    "jira.apiToken",
    "jenkins.apiToken",
    "bitbucket.apiToken",
    "mcp.token",
    "slack.token",
    "llmProxy.apiKey",
  ]);
});

test("a tampered data field throws CredentialStoreError with code undecryptable", () => {
  const { store, filePath } = makeStore();
  store.set("jira.apiToken", "secret");
  const payload = JSON.parse(fs.readFileSync(filePath, "utf8"));
  const dataBytes = Buffer.from(payload.data, "base64");
  dataBytes[0] ^= 0xff;
  payload.data = dataBytes.toString("base64");
  fs.writeFileSync(filePath, JSON.stringify(payload));

  assert.throws(
    () => store.get("jira.apiToken"),
    (err) => {
      assert.ok(err instanceof CredentialStoreError);
      assert.equal(err.code, "undecryptable");
      assert.equal(err.message, UNDECRYPTABLE_MESSAGE);
      return true;
    },
  );
});

test("a tampered tag field throws CredentialStoreError with code undecryptable", () => {
  const { store, filePath } = makeStore();
  store.set("jira.apiToken", "secret");
  const payload = JSON.parse(fs.readFileSync(filePath, "utf8"));
  const tagBytes = Buffer.from(payload.tag, "base64");
  tagBytes[0] ^= 0xff;
  payload.tag = tagBytes.toString("base64");
  fs.writeFileSync(filePath, JSON.stringify(payload));

  assert.throws(
    () => store.get("jira.apiToken"),
    (err) => {
      assert.ok(err instanceof CredentialStoreError);
      assert.equal(err.code, "undecryptable");
      return true;
    },
  );
});

test("a wrong key throws CredentialStoreError with code undecryptable", () => {
  const { store, keyPath } = makeStore();
  store.set("jira.apiToken", "secret");
  fs.writeFileSync(keyPath, crypto.randomBytes(32));

  assert.throws(
    () => store.get("jira.apiToken"),
    (err) => {
      assert.ok(err instanceof CredentialStoreError);
      assert.equal(err.code, "undecryptable");
      return true;
    },
  );
});

test("a missing key when the file exists throws CredentialStoreError with code undecryptable", () => {
  const { store, keyPath } = makeStore();
  store.set("jira.apiToken", "secret");
  fs.unlinkSync(keyPath);

  assert.throws(
    () => store.get("jira.apiToken"),
    (err) => {
      assert.ok(err instanceof CredentialStoreError);
      assert.equal(err.code, "undecryptable");
      return true;
    },
  );
});

test("set recovers from a wrong key: it succeeds, starts fresh, and the old names are gone", () => {
  const { store, keyPath } = makeStore();
  store.set("jira.apiToken", "old-secret");
  fs.writeFileSync(keyPath, crypto.randomBytes(32));

  assert.doesNotThrow(() => store.set("jenkins.apiToken", "new-secret"));
  assert.equal(store.get("jenkins.apiToken"), "new-secret");
  assert.equal(store.get("jira.apiToken"), undefined);
  assert.deepEqual(store.list(), ["jenkins.apiToken"]);
});

test("set recovers from a missing key when the file exists: it creates a key and succeeds", () => {
  const { store, keyPath } = makeStore();
  store.set("jira.apiToken", "old-secret");
  fs.unlinkSync(keyPath);

  assert.doesNotThrow(() => store.set("jenkins.apiToken", "new-secret"));
  assert.equal(fs.existsSync(keyPath), true);
  assert.equal(fs.readFileSync(keyPath).length, 32);
  assert.equal(store.get("jenkins.apiToken"), "new-secret");
  assert.equal(store.get("jira.apiToken"), undefined);
});

test("remove on an undecryptable file still throws CredentialStoreError", () => {
  const { store, keyPath } = makeStore();
  store.set("jira.apiToken", "secret");
  fs.writeFileSync(keyPath, crypto.randomBytes(32));

  assert.throws(
    () => store.remove("jira.apiToken"),
    (err) => {
      assert.ok(err instanceof CredentialStoreError);
      assert.equal(err.code, "undecryptable");
      return true;
    },
  );
});

test("a decryption failure never returns partial data — list also throws", () => {
  const { store, filePath } = makeStore();
  store.set("jira.apiToken", "secret");
  store.set("jenkins.apiToken", "other");
  const payload = JSON.parse(fs.readFileSync(filePath, "utf8"));
  const dataBytes = Buffer.from(payload.data, "base64");
  dataBytes[0] ^= 0xff;
  payload.data = dataBytes.toString("base64");
  fs.writeFileSync(filePath, JSON.stringify(payload));

  assert.throws(() => store.list(), CredentialStoreError);
  assert.throws(() => store.get("jenkins.apiToken"), CredentialStoreError);
});

test("the credentials file and the key file are created 0600, the key's directory 0700", () => {
  const { store, filePath, keyPath } = makeStore();
  store.set("jira.apiToken", "secret");
  assert.equal(fs.statSync(filePath).mode & 0o777, 0o600);
  assert.equal(fs.statSync(keyPath).mode & 0o777, 0o600);
  assert.equal(fs.statSync(path.dirname(keyPath)).mode & 0o777, 0o700);
});

test("the key is created on the first set, not before", () => {
  const { store, keyPath } = makeStore();
  assert.equal(fs.existsSync(keyPath), false);
  store.set("jira.apiToken", "secret");
  assert.equal(fs.existsSync(keyPath), true);
  const key = fs.readFileSync(keyPath);
  assert.equal(key.length, 32);
});

test("the key is reused across sets, not regenerated", () => {
  const { store, keyPath } = makeStore();
  store.set("jira.apiToken", "a");
  const keyAfterFirst = fs.readFileSync(keyPath);
  store.set("jenkins.apiToken", "b");
  const keyAfterSecond = fs.readFileSync(keyPath);
  assert.deepEqual(keyAfterFirst, keyAfterSecond);
});
