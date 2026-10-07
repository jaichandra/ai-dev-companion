// Page-matching patterns that depend on the companion's configured targets (GET /targets):
// which Jira projects the analysis features cover. Loaded as a content script before the
// feature files, and also `require`d by companion-service's tests — hence the guarded export.
// A pack's own targets (e.g. a pack's Jenkins pipelines) are read by its feature file.
(function (root) {
  // Used until /targets answers, or if it can't be reached. No projects means "any project":
  // the service enforces the configured list when a feature starts.
  const DEFAULT_TARGETS = {
    analyzeIssue: { projects: [], issueTypes: [] },
  };

  function escapeRegExp(s) {
    return String(s).replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  }

  // /browse/<PROJECT>-<n> — group 1 is the issue key.
  function issueUrlPattern(targets) {
    const projects = ((targets && targets.analyzeIssue && targets.analyzeIssue.projects) || []).map(escapeRegExp);
    const keys = projects.length > 0 ? "(?:" + projects.join("|") + ")" : "[A-Z][A-Z0-9_]*";
    return new RegExp("/browse/(" + keys + "-\\d+)");
  }

  function validTargets(t) {
    return !!(t && t.analyzeIssue && Array.isArray(t.analyzeIssue.projects) && Array.isArray(t.analyzeIssue.issueTypes));
  }

  const api = {
    DEFAULT_TARGETS,
    escapeRegExp,
    issueUrlPattern,
    validTargets,
  };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.PaiTargets = api;
})(typeof self !== "undefined" ? self : this);
