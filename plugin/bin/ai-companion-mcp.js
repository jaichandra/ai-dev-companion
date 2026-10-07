#!/usr/bin/env node
"use strict";
// The ai-companion plugin's MCP server entry. It holds no token and no code
// of its own: it starts the installed companion's `companion mcp-stdio`
// (companion-service/core/mcp-stdio.js), which talks to the running service
// with the MCP token from credentials.enc. Not installed -> a clear message
// on stderr (Claude Code shows it) and exit 1, never a silent hang.
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawn } = require("child_process");

const bin = path.join(os.homedir(), "ai-dev-companion", "companion-service", "bin", "companion.js");
if (!fs.existsSync(bin)) {
  process.stderr.write(`ai-companion: the companion service isn't installed (${bin} is missing). Install it with \`node install.js\`.\n`);
  process.exit(1);
}
const child = spawn(process.execPath, [bin, "mcp-stdio"], { stdio: "inherit" });
child.on("error", (err) => {
  process.stderr.write(`ai-companion: couldn't start the companion's MCP bridge: ${err.message}\n`);
  process.exit(1);
});
child.on("exit", (code) => process.exit(code === null ? 1 : code));
