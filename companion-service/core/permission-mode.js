// The --permission-mode Claude Code is started with. The framework default is "auto"; a distribution
// can change it for everything (claude.permissionMode) or for one feature (claude.permissionModes[featureId])
// in its environment profile.
const environment = require("../environment.js");

const { PERMISSION_MODES } = require("./environment-base.js");
const DEFAULT_PERMISSION_MODE = "auto";

/** The permission mode for the Claude Code session a feature starts: its override, else the profile's, else "auto". */
function permissionModeFor(featureId) {
  const claude = environment.claude || {};
  return (claude.permissionModes && claude.permissionModes[featureId]) || claude.permissionMode || DEFAULT_PERMISSION_MODE;
}

module.exports = { DEFAULT_PERMISSION_MODE, permissionModeFor };
