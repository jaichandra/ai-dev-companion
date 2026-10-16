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

  // Words that look like a ticket key (ABC-123) but never are one.
  const NOT_PROJECTS = new Set([
    "UTF", "SHA", "ISO", "MD", "AES", "RSA", "TLS", "SSL", "HTTP", "CVE", "GPT",
    "RELEASE", "HOTFIX", "VERSION", "BUILD", "SPRINT", "FIX", "BUGFIX", "FEATURE", "RC", "PR", "DAY", "WEEK",
  ]);
  const KEY_IN_TEXT = /(?:^|[^A-Za-z0-9])([A-Za-z][A-Za-z0-9_]*-[1-9]\d*)(?![0-9A-Za-z])/g;

  // The first ticket key in the given texts (a PR's branch, then its title; a build's job name, then its
  // parameters), upper-cased; null if none. Texts are tried in the order given.
  function ticketKeyFrom(...texts) {
    for (const text of texts) {
      if (typeof text !== "string") continue;
      for (const m of text.matchAll(KEY_IN_TEXT)) {
        const key = m[1].toUpperCase();
        const project = key.slice(0, key.lastIndexOf("-"));
        // A Jira project key is 2 to 10 letters or digits, starting with a letter.
        if (project.length >= 2 && project.length <= 10 && !NOT_PROJECTS.has(project)) return key;
      }
    }
    return null;
  }

  function validTargets(t) {
    return !!(t && t.analyzeIssue && Array.isArray(t.analyzeIssue.projects) && Array.isArray(t.analyzeIssue.issueTypes));
  }

  const api = {
    DEFAULT_TARGETS,
    escapeRegExp,
    issueUrlPattern,
    ticketKeyFrom,
    validTargets,
  };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.PaiTargets = api;
})(typeof self !== "undefined" ? self : this);
