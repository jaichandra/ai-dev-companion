// The review mover against the build (npm run build first), with Bitbucket and
// Jira injected: no network, no credentials, HOME pointed at a temp dir.
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

process.env.HOME = fs.mkdtempSync(path.join(os.tmpdir(), "review-mover-home-"));
const { createReviewMover } = require("../../dist/features/ticket-to-pr/review-mover.js");
const { jobStore } = require("../../dist/core/jobs.js");
const plan = require("./plan.js");

const config = { repos: {}, ticketToPr: {}, bitbucket: { baseUrl: "https://bb.example.com" } };
const PR = { id: 12, title: "PROJ-7: fix", state: "OPEN", url: "https://bb.example.com/projects/ACME/repos/sample-app/pull-requests/12" };
const T_REVIEW = { id: "31", name: "Start review", to: "In Review" };

function tracked(extra = {}) {
  const job = jobStore.create("ticket-to-pr", { issueKey: "PROJ-7", adopt: false });
  jobStore.patchData(job.id, { repoKey: "ACME/sample-app", branch: "bugfix/PROJ-7-login", ...extra });
  jobStore.update(job.id, { status: "awaiting-approval" });
  return job;
}

function fakeIo({ pr = PR, transitions = [T_REVIEW], failTransition = false } = {}) {
  const calls = { find: 0, transitions: [] };
  return {
    calls,
    io: {
      findOpenPr: async () => (calls.find++, pr),
      listTransitions: async () => transitions,
      transition: async (_a, key, id) => {
        if (failTransition) throw new Error("Jira said no");
        calls.transitions.push([key, id]);
      },
    },
  };
}

test("runReviewMove moves, finds nothing to move with, or reports a failure", async () => {
  const deps = (over) => ({ listTransitions: async () => [T_REVIEW], pickTransition: (l, w) => l.find((t) => t.to === w) || null, transition: async () => {}, ...over });
  assert.deepEqual(await plan.runReviewMove({ issueKey: "PROJ-7", transitionName: "In Review" }, deps()), { status: "moved", detail: "Moved PROJ-7 to In Review." });
  const none = await plan.runReviewMove({ issueKey: "PROJ-7", transitionName: "Code Review" }, deps());
  assert.equal(none.status, "no-transition");
  assert.match(none.detail, /no "Code Review" transition/);
  const failed = await plan.runReviewMove({ issueKey: "PROJ-7", transitionName: "In Review" }, deps({ transition: async () => { throw new Error("boom https://u:p@host/x"); } }));
  assert.equal(failed.status, "failed");
  assert.doesNotMatch(failed.detail, /u:p@/);
});

test("a tick with an open PR moves the ticket, records the PR and ends the job as approved", async () => {
  const job = tracked();
  const { io, calls } = fakeIo();
  const ended = await createReviewMover(config, io).tick();
  assert.equal(ended >= 1, true);
  assert.deepEqual(calls.transitions, [["PROJ-7", "31"]]);
  const after = jobStore.get(job.id);
  assert.equal(after.status, "approved");
  assert.equal(after.data.reviewMove.moved, true);
  assert.deepEqual(after.data.pr, { id: 12, url: PR.url, title: "PROJ-7: fix" });
  assert.match(after.result.summary, /Pull request #12 is open\. Moved PROJ-7 to In Review\./);
});

test("a tick with no open PR changes nothing and moves nothing", async () => {
  const job = tracked();
  const { io, calls } = fakeIo({ pr: null });
  await createReviewMover(config, io).tick();
  assert.deepEqual(calls.transitions, []);
  assert.equal(jobStore.get(job.id).status, "awaiting-approval");
  assert.equal(jobStore.get(job.id).data.reviewMove, undefined);
  jobStore.update(job.id, { status: "rejected" });
});

test("autoMoveToReview: false stops the poll but not the button", async () => {
  const off = { ...config, ticketToPr: { autoMoveToReview: false } };
  const job = tracked();
  const { io, calls } = fakeIo();
  const mover = createReviewMover(off, io);
  assert.equal(await mover.tick(), 0);
  assert.equal(calls.find, 0);
  assert.equal(jobStore.get(job.id).status, "awaiting-approval");
  const out = await mover.moveNow(jobStore.get(job.id), {});
  assert.equal(out.status, "moved");
  assert.equal(jobStore.get(job.id).status, "approved"); // a PR exists too, so the job ends
});

test("the button moves the ticket even before a PR exists, and the job keeps waiting for one", async () => {
  const job = tracked();
  const { io, calls } = fakeIo({ pr: null });
  const out = await createReviewMover(config, io).moveNow(jobStore.get(job.id), {});
  assert.equal(out.status, "moved");
  assert.deepEqual(calls.transitions, [["PROJ-7", "31"]]);
  assert.equal(jobStore.get(job.id).status, "awaiting-approval");
  assert.equal(jobStore.get(job.id).data.reviewMove.moved, true);
  // Once the PR shows up the poll ends the job without moving the ticket again.
  const later = fakeIo();
  await createReviewMover(config, later.io).tick();
  assert.deepEqual(later.calls.transitions, []);
  assert.equal(jobStore.get(job.id).status, "approved");
});

test("a ticket with no matching transition is settled, not retried", async () => {
  const job = tracked();
  const { io, calls } = fakeIo({ transitions: [{ id: "9", name: "Close", to: "Done" }] });
  const mover = createReviewMover(config, io);
  await mover.tick();
  assert.deepEqual(calls.transitions, []);
  assert.equal(jobStore.get(job.id).data.reviewMove.status, "no-transition");
  assert.equal(jobStore.get(job.id).status, "approved");
});

test("a failed move is recorded, leaves the job waiting and is tried again", async () => {
  const job = tracked();
  const bad = fakeIo({ failTransition: true });
  const out = await createReviewMover(config, bad.io).moveNow(jobStore.get(job.id), {});
  assert.equal(out.status, "failed");
  assert.equal(jobStore.get(job.id).status, "awaiting-approval");
  assert.equal(jobStore.get(job.id).data.reviewMove.moved, false);
  const good = fakeIo();
  await createReviewMover(config, good.io).tick();
  assert.deepEqual(good.calls.transitions, [["PROJ-7", "31"]]);
  assert.equal(jobStore.get(job.id).status, "approved");
});

test("jobs that aren't waiting for a PR, or have an unsafe branch, are ignored", async () => {
  const other = jobStore.create("analyze-issue", { issueKey: "PROJ-8" });
  jobStore.update(other.id, { status: "awaiting-approval" });
  const unsafe = tracked({ branch: "--upload-pack=evil" });
  const noBranch = tracked({ branch: undefined });
  const { io, calls } = fakeIo();
  await createReviewMover(config, io).tick();
  assert.equal(jobStore.get(other.id).status, "awaiting-approval");
  assert.equal(jobStore.get(unsafe.id).status, "awaiting-approval");
  assert.equal(jobStore.get(noBranch.id).status, "awaiting-approval");
  assert.deepEqual(calls.transitions.filter(([k]) => k !== "PROJ-7"), []);
  for (const j of [other, unsafe, noBranch]) jobStore.update(j.id, { status: "rejected" });
});

test("a PR address off the configured Bitbucket host is not kept as a link", async () => {
  const job = tracked();
  const { io } = fakeIo({ pr: { ...PR, url: "https://evil.example/projects/ACME/repos/sample-app/pull-requests/12" } });
  await createReviewMover(config, io).tick();
  assert.equal(jobStore.get(job.id).data.pr.url, null);
});
