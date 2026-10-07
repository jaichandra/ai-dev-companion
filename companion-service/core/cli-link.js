// Where install.js links the `companion` command, and whether it may. Pure.
const path = require("path");

const OWN_TARGET_SUFFIX = path.join("companion-service", "bin", "companion.js");

function planLink({ platform, home, stableDir, existing }) {
  const target = path.join(stableDir, OWN_TARGET_SUFFIX);
  const linkPath = path.join(home, ".local", "bin", "companion");
  if (platform === "win32") return { action: "skip", linkPath, target, reason: "windows" };
  const e = existing || { kind: "none" };
  if (e.kind === "none") return { action: "create", linkPath, target };
  if (e.kind === "symlink" && e.target === target) return { action: "leave", linkPath, target, reason: "already-linked" };
  // A link left by an earlier install of this same tool, from another folder.
  if (e.kind === "symlink" && typeof e.target === "string" && e.target.endsWith(OWN_TARGET_SUFFIX)) {
    return { action: "replace", linkPath, target };
  }
  return { action: "leave", linkPath, target, reason: "something-else-is-there" };
}

module.exports = { planLink };
