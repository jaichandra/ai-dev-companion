// Test helper: runs `fn` with the profile's git site switched to `provider` (and `baseUrl`, when given), then
// restores it. The placeholder profile's git host is GitHub, so tests of the Bitbucket behaviour go through this
// rather than depending on the placeholder. Not used by the service.
const environment = require("../environment.js");

function withGitProvider(provider, fn, baseUrl) {
  const site = environment.sites.find((s) => s.kind === "git");
  const saved = { ...site };
  site.provider = provider;
  if (baseUrl) site.baseUrl = baseUrl;
  const restore = () => {
    for (const key of Object.keys(site)) if (!(key in saved)) delete site[key];
    Object.assign(site, saved);
  };
  let result;
  try {
    result = fn();
  } catch (err) {
    restore();
    throw err;
  }
  if (result && typeof result.then === "function") return result.finally(restore);
  restore();
  return result;
}

/** Switches the git site to `provider` for the rest of this process: a whole test file written around one provider's
 * URL shapes. (`node --test` runs each file in its own process, so it cannot leak into another file.) */
function useGitProvider(provider, baseUrl) {
  const site = environment.sites.find((s) => s.kind === "git");
  site.provider = provider;
  if (baseUrl) site.baseUrl = baseUrl;
}

/** For a test file about a Bitbucket profile (the git site is `bitbucket`, so config and the token are keyed by it):
 * switches both the source profile and the build's copy (tests that load dist/ read the latter). Call it before
 * requiring anything else, since some modules read the site list once, at load. */
function useBitbucketProfile() {
  const copies = [require("../environment.js")];
  try {
    // Built path, not a literal: tsc follows literal requires, and must not take dist/ as an input.
    copies.push(require(require("node:path").join(__dirname, "..", "dist", "environment.js")));
  } catch {
    // No build yet: only the source profile exists.
  }
  for (const environment of copies) {
    const site = environment.sites.find((s) => s.kind === "git");
    site.id = "bitbucket";
    site.provider = "bitbucket-dc";
    site.label = "Bitbucket";
    site.baseUrl = "https://bitbucket.example.com";
    delete site.tokenOnly;
    delete site.pageMatches;
  }
}

module.exports = { withGitProvider, useGitProvider, useBitbucketProfile };
