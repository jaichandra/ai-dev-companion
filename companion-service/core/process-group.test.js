const test = require("node:test");
const assert = require("node:assert/strict");
const { spawn } = require("child_process");
const { isProcessGroupAlive } = require("./process-group.js");

/** Resolves once `child` has exited (and been reaped), so the group it led
 * is really gone before the test asserts "dead". */
function exited(child) {
  return new Promise((resolve) => child.once("exit", resolve));
}

test("a detached child's process group is alive while it runs, and dead once it's killed", async () => {
  // detached: true makes the child a process-group leader (pgid === pid),
  // exactly how core/exec.ts spawns claude.
  const child = spawn("sleep", ["30"], { detached: true, stdio: "ignore" });
  const done = exited(child);
  try {
    assert.equal(isProcessGroupAlive(child.pid), true);
  } finally {
    process.kill(-child.pid, "SIGKILL");
  }
  await done;
  assert.equal(isProcessGroupAlive(child.pid), false);
});

test("a non-positive or non-integer pgid is never alive", () => {
  // 0 and -1 would mean "my own group" / "every process" to kill(2) — the
  // exact targets this must never probe on the strength of a job file.
  for (const pgid of [0, -1, -42, 1.5, Number.NaN, Number.POSITIVE_INFINITY, "123", null, undefined, {}]) {
    assert.equal(isProcessGroupAlive(pgid), false, `pgid ${String(pgid)}`);
  }
});
