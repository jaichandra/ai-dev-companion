#!/usr/bin/env node
// Manages the companion service as a background job via launchd — a
// per-user "LaunchAgent" that starts it automatically at login and
// restarts it if it crashes, so you don't need to keep a terminal open
// running `npm start`. macOS-only (launchd itself is macOS-only); on
// other platforms this just says so and exits.
//
//   npm run service:install    (builds first)
//   npm run service:status
//   npm run service:uninstall
const launchd = require("./core/launchd.js");

function main() {
  const cmd = process.argv[2];

  if (process.platform !== "darwin") {
    console.error(
      "Background-service mode uses launchd, which is macOS-only. On other platforms, " +
        "run `npm start` in a terminal (or set this up under your own systemd/pm2/etc. instead).",
    );
    process.exitCode = 1;
    return;
  }

  switch (cmd) {
    case "install": {
      const info = launchd.installService();
      console.log("Installed and started the companion service as a background job.");
      console.log(`  Label: ${info.label}`);
      console.log(`  Plist: ${info.plistPath}`);
      console.log(`  Logs:  ${info.outLog}`);
      console.log(`         ${info.errLog}`);
      console.log(
        "\nIt will now start automatically at login and restart itself if it crashes — " +
          "no terminal needs to stay open. Re-run `npm run service:install` any time to rebuild " +
          "and restart it. `npm run doctor` checks that everything is connected.",
      );
      break;
    }
    case "uninstall": {
      const removed = launchd.uninstallService();
      console.log(
        removed
          ? `Stopped and removed the background service (${launchd.plistPath()}).`
          : "No background service was installed — nothing to do.",
      );
      break;
    }
    case "status": {
      const status = launchd.serviceStatus();
      if (!status.loaded) {
        console.log("Not installed as a background service. Run `npm run service:install` to set it up.");
      } else if (status.pid) {
        console.log(`Running as a background service (pid ${status.pid}).`);
      } else {
        console.log(
          "Installed, but not currently running (it may have crashed and not been " +
            `respawned yet) — check ${launchd.logPaths().err}`,
        );
      }
      console.log(`Plist: ${launchd.plistPath()}`);
      break;
    }
    default:
      console.error("Usage: node service.js <install|uninstall|status>");
      process.exitCode = 1;
  }
}

try {
  main();
} catch (err) {
  console.error("Failed:", err.message);
  process.exitCode = 1;
}
