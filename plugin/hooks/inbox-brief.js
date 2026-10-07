#!/usr/bin/env node
"use strict";
// SessionStart hook: prints `companion inbox --brief` — one line with counts
// and fixed labels when something is waiting, nothing otherwise. Silent and
// exit 0 when the companion isn't installed, isn't running or is slow (3 s),
// so it never delays or clutters a session; the skills are where "companion
// service not running" is reported.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

const bin = path.join(os.homedir(), "ai-dev-companion", "companion-service", "bin", "companion.js");
try {
  if (fs.existsSync(bin)) {
    const r = spawnSync(process.execPath, [bin, "inbox", "--brief"], { encoding: "utf8", timeout: 3000 });
    const line = r && r.status === 0 && typeof r.stdout === "string" ? r.stdout.trim().split("\n")[0] : "";
    if (line.startsWith("AI companion: ")) process.stdout.write(`${line}\n`);
  }
} catch {
  // Advisory only: whatever went wrong, the session starts as usual.
}
process.exit(0);
