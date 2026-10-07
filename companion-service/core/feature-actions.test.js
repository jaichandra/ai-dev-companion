// The actions route's handler from the build (npm run build first): 404 for
// an unknown job, another feature's job or an unknown/prototype action name.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "feature-actions-home-"));
const { featureActionsHandler } = require("../dist/core/feature-actions.js");
const { jobStore } = require("../dist/core/jobs.js");

function call(handler, params) {
  return new Promise((resolve) => {
    const res = {
      code: 200,
      status(c) { this.code = c; return this; },
      json(body) { resolve({ code: this.code, body }); },
    };
    handler({ params }, res);
  });
}

test("actions route: unknown job, another feature's job and unknown or prototype action names are 404; a known action runs", async () => {
  const ran = [];
  const feature = {
    id: "demo",
    label: "Demo",
    actions: { hello: async (job, ctx) => { ran.push([job.id, ctx.body]); return { ok: true }; }, boom: async () => { throw new Error("nope"); } },
  };
  const errors = [];
  const handler = featureActionsHandler(feature, {
    contextFor: () => ({ auth: {}, body: { x: 1 } }),
    sendError: (res, err) => { errors.push(err.message); res.status(409).json({ error: err.message }); },
    present: (j) => j,
  });
  const mine = jobStore.create("demo", {});
  const other = jobStore.create("someone-else", {});

  assert.equal((await call(handler, { jobId: "no-such-job", action: "hello" })).code, 404);
  assert.equal((await call(handler, { jobId: other.id, action: "hello" })).code, 404);
  for (const action of ["missing", "constructor", "__proto__", "toString", "hasOwnProperty"]) {
    const r = await call(handler, { jobId: mine.id, action });
    assert.equal(r.code, 404, action);
    assert.match(r.body.error, /no action/);
  }
  assert.deepEqual(ran, []);
  const ok = await call(handler, { jobId: mine.id, action: "hello" });
  assert.deepEqual([ok.code, ok.body], [200, { ok: true }]);
  assert.deepEqual(ran, [[mine.id, { x: 1 }]]);
  const bad = await call(handler, { jobId: mine.id, action: "boom" });
  assert.equal(bad.code, 409);
  assert.deepEqual(errors, ["nope"]);
});
