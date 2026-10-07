// The background-work settings, exercised through core/settings.js's own
// validateSettingsUpdate / mergeSettings / publicSettings.
const test = require("node:test");
const assert = require("node:assert/strict");
const settings = require("./settings.js");

const BASE = { port: 8787, sharedSecret: "s3cret", repos: {} };
const creds = (names = []) => ({ list: () => names });
const fields = (update, current = BASE) => settings.validateSettingsUpdate(update, current).map((e) => e.field);



test("out-of-range, unknown and wrongly typed background values are refused", () => {
  assert.deepEqual(fields({ watchers: { conflicts: { intervalMinutes: 0 }, builds: {} } }), ["watchers.builds", "watchers.conflicts.intervalMinutes"]);
  assert.deepEqual(fields({ watchers: { conflicts: { enabled: "yes" } } }), ["watchers.conflicts.enabled"]);
  assert.deepEqual(fields({ budget: { claudeRunsPerDay: 51 } }), ["budget.claudeRunsPerDay"]);
  assert.deepEqual(fields({ scheduler: { quietHours: { start: "7pm", end: "08:00" } } }), ["scheduler.quietHours"]);
  assert.deepEqual(fields({ scheduler: { quietHours: { start: "08:00", end: "08:00" } } }), ["scheduler.quietHours"]);
  assert.deepEqual(fields({ notify: { minIntervalMinutes: -1 } }), ["notify.minIntervalMinutes"]);
  assert.deepEqual(fields({ digest: { time: "8:30" } }), ["digest.time"]);
  assert.deepEqual(fields({ prePush: { mode: "block" } }), ["prePush.mode"]);
  assert.deepEqual(fields({ budget: 5 }), ["budget"]);
});



test("mergeSettings merges onto saved values, clears back to defaults, and can clear the key", () => {
  const saved = { ...BASE, watchers: { conflicts: { enabled: true, intervalMinutes: 5 } }, digest: { enabled: true, time: "07:00" }, llmProxy: { allowedModels: ["qwen3.8-27b"], chatModel: "qwen3.8-27b" } };
  const { config, tokenOps } = settings.mergeSettings(saved, {
    watchers: { conflicts: { enabled: false } },
    digest: { time: "08:30" },
    scheduler: { quietHours: null },
    llmProxy: { clearApiKey: true, apiKey: "ignored" },
  });
  assert.deepEqual(config.watchers, { conflicts: { intervalMinutes: 5 } });
  assert.deepEqual(config.digest, { enabled: true });
  assert.equal(config.scheduler, undefined);
  assert.deepEqual(config.llmProxy, { allowedModels: ["qwen3.8-27b"], chatModel: "qwen3.8-27b" }, "allowedModels stays as hand-edited");
  assert.deepEqual(tokenOps, [{ name: "llmProxy.apiKey", value: null }]);
  const off = settings.mergeSettings({ ...BASE, watchers: { reviewRequests: { enabled: true } } }, { watchers: { reviewRequests: { enabled: false } } });
  assert.equal(off.config.watchers, undefined);
});

test("fractional, NaN, infinite and string numbers are refused where a whole number is needed", () => {
  for (const bad of [1.5, 0.5, NaN, Infinity, -Infinity, "5", "", null, true, [5], 1441]) {
    assert.deepEqual(fields({ watchers: { conflicts: { intervalMinutes: bad } } }), ["watchers.conflicts.intervalMinutes"], `intervalMinutes ${String(bad)}`);
  }
  for (const bad of [2.5, NaN, Infinity, "3", null, false, 51, -1]) {
    assert.deepEqual(fields({ budget: { claudeRunsPerDay: bad } }), ["budget.claudeRunsPerDay"], `claudeRunsPerDay ${String(bad)}`);
  }
  for (const bad of [0.5, NaN, Infinity, "30", null, 1441, -1]) {
    assert.deepEqual(fields({ notify: { minIntervalMinutes: bad } }), ["notify.minIntervalMinutes"], `minIntervalMinutes ${String(bad)}`);
  }
  assert.deepEqual(fields({ watchers: { conflicts: { intervalMinutes: 1440 } }, budget: { claudeRunsPerDay: 50 }, notify: { minIntervalMinutes: 1440 } }), []);
});


test("similar.enabled: on by default, only off is saved, and only a boolean is accepted", () => {
  assert.deepEqual(settings.publicSettings(BASE, creds()).similar, { enabled: true });
  assert.deepEqual(settings.publicSettings({ ...BASE, similar: { enabled: false } }, creds()).similar, { enabled: false });
  assert.deepEqual(fields({ similar: { enabled: false } }), []);
  assert.deepEqual(fields({ similar: { enabled: "no" } }), ["similar.enabled"]);
  assert.deepEqual(fields({ similar: { k: 3 } }), ["similar.k"]);
  assert.deepEqual(fields({ similar: true }), ["similar"]);
  const off = settings.mergeSettings(BASE, { similar: { enabled: false } }).config;
  assert.deepEqual(off.similar, { enabled: false });
  const on = settings.mergeSettings(off, { similar: { enabled: true } }).config;
  assert.equal(on.similar, undefined);
});

test("similar: an update without it, or with an empty object, leaves the saved off; null and arrays are refused", () => {
  const saved = { ...BASE, similar: { enabled: false } };
  assert.deepEqual(settings.mergeSettings(saved, {}).config.similar, { enabled: false });
  assert.deepEqual(settings.mergeSettings(saved, { similar: {} }).config.similar, { enabled: false });
  assert.deepEqual(fields({ similar: null }), ["similar"]);
  assert.deepEqual(fields({ similar: [] }), ["similar"]);
  assert.deepEqual(fields({ similar: [{ enabled: false }] }), ["similar"]);
});

test("llmProxy.userAgent: a short printable name is saved, blank goes back to the default, junk is refused", () => {
  assert.deepEqual(fields({ llmProxy: { userAgent: "  QwenCode/1.0 " } }), []);
  const saved = settings.mergeSettings(BASE, { llmProxy: { userAgent: "  QwenCode/1.0 " } }).config;
  assert.equal(saved.llmProxy.userAgent, "QwenCode/1.0");
  const cleared = settings.mergeSettings(saved, { llmProxy: { userAgent: "" } }).config;
  assert.equal(cleared.llmProxy, undefined);
  for (const bad of ["a\r\nb: c", "é", "x".repeat(101), 5]) {
    assert.ok(fields({ llmProxy: { userAgent: bad } }).includes("llmProxy.userAgent"), JSON.stringify(bad));
  }
});
