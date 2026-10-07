#!/usr/bin/env node
// Applies an update, started detached by core/updater.js's startUpdate so
// it outlives the service it restarts. Progress goes to update-status.json,
// which the (old, then new) service reports to the extension; output goes
// to update.log.
//
// Steps: sync the install source, run its install.js non-interactively
// with --no-service (copies the new files over this install, keeps
// config.json, rebuilds), then — if the service runs under launchd — stop
// it so the job's KeepAlive starts the new build. launchd itself is never
// unloaded or reloaded here, so an interrupted update can't leave the
// background job missing.
const path = require("path");
const { spawnSync } = require("child_process");
const updater = require("./updater.js");

const pidFlag = process.argv.indexOf("--restart-pid");
const restartPid = pidFlag !== -1 ? parseInt(process.argv[pidFlag + 1], 10) || null : null;

const base = { from: updater.installedVersion(), pid: process.pid, startedAt: Date.now() };
let to = null;

function report(fields) {
  updater.writeStatus({ ...base, to, ...fields });
  console.log(
    `[update] ${fields.state}${fields.label ? `: ${fields.label}` : ""}${fields.error ? `: ${fields.error}` : ""}`,
  );
}

async function main() {
  report({ state: "running", step: "download", label: "Downloading the latest version…" });
  const source = updater.readInstallSource();
  const dir = await updater.syncSource(source, updater.stagingDir());
  to = updater.installedVersion(dir);

  report({ state: "running", step: "install", label: `Installing v${to} (this takes a minute)…` });
  const install = spawnSync(process.execPath, [path.join(dir, "install.js"), "--yes", "--no-service"], {
    cwd: dir,
    stdio: "inherit",
    env: process.env,
  });
  if (install.status !== 0) {
    throw new Error(`The installer exited with code ${install.status ?? "?"}. See ${updater.logPath()}.`);
  }

  if (!restartPid) {
    report({
      state: "done",
      needsRestart: true,
      finishedAt: Date.now(),
      label: `Installed v${to}. Restart the companion service to finish.`,
    });
    return;
  }
  report({
    state: "done",
    needsRestart: false,
    finishedAt: Date.now(),
    label: `Installed v${to}. Restarting the companion service…`,
  });
  try {
    process.kill(restartPid, "SIGTERM");
  } catch {
    // Already gone — launchd starts the new build either way.
  }
}

main().catch((err) => {
  report({
    state: "failed",
    finishedAt: Date.now(),
    error:
      `${err.message}\n\nNothing was changed if this happened while downloading. To update by hand, ` +
      `run: ${updater.manualUpdateCommand(updater.readInstallSource())}`,
  });
  process.exitCode = 1;
});
