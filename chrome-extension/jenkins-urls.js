// Reading Jenkins build pages: which URLs may be a build, and which build a page belongs to.
// Jenkins-specific but not specific to any one installation; used by Diagnose build. Loaded
// as a content script (before the feature files) and also `require`d by companion-service's
// tests — hence the guarded export.
(function (root) {
  // A page that may be a Jenkins build: classic (/job/<a>/job/<b>/<n>/…) or
  // Blue Ocean (/blue/organizations/<org>/<pipeline>/detail/<pipeline or
  // branch>/<n>/…). Only decides whether to look at the page; the companion
  // service re-validates whatever is sent (core/diagnose-prompt.js).
  const JENKINS_BUILD_PAGE =
    /\/(?:job\/[^/]+\/)+[1-9]\d*(?:\/|$)|\/blue\/organizations\/[^/]+\/[^/]+\/detail\/[^/]+\/[1-9]\d*(?:\/|$)/;

  // NOTE: companion-service/core/diagnose-prompt.js (parseBuildUrl, JOB_SEGMENT)
  // holds a copy of this parser and the companion re-validates every URL with
  // it — change both together (companion-service/core/url-parser-twins.test.js
  // checks they agree).
  const JOB_SEGMENT = /^[A-Za-z0-9][A-Za-z0-9._ -]{0,99}$/;

  function decodeSegment(segment) {
    try {
      return decodeURIComponent(segment);
    } catch (e) {
      return null;
    }
  }

  // The build a page belongs to, or null. `statusUrl` is the JSON that says
  // whether it failed (same origin as the page).
  function jenkinsBuildFromUrl(href) {
    let url;
    try {
      url = new URL(href);
    } catch (e) {
      return null;
    }
    const segs = url.pathname.split("/").filter(Boolean);
    let names;
    let number;
    if (segs[0] === "blue" && segs[1] === "organizations" && segs[4] === "detail" && segs.length >= 7) {
      const pipeline = decodeSegment(segs[3]);
      const leaf = decodeSegment(segs[5]);
      if (pipeline === null || leaf === null) return null;
      names = pipeline.split("/");
      if (leaf !== names[names.length - 1]) names.push(leaf);
      number = segs[6];
    } else {
      names = [];
      let i = 0;
      while (i + 1 < segs.length && segs[i] === "job") {
        const name = decodeSegment(segs[i + 1]);
        if (name === null) return null;
        names.push(name);
        i += 2;
      }
      number = segs[i];
    }
    if (names.length === 0 || !/^[1-9]\d{0,8}$/.test(number || "")) return null;
    if (!names.every((n) => JOB_SEGMENT.test(n))) return null;
    const jobPath = names.map((n) => "/job/" + encodeURIComponent(n)).join("");
    const buildUrl = url.origin + jobPath + "/" + number + "/";
    return {
      jobName: names.join("/"),
      number: Number(number),
      buildUrl,
      statusUrl: buildUrl + "api/json?tree=result,building",
    };
  }

  const api = { JENKINS_BUILD_PAGE, jenkinsBuildFromUrl };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.PaiJenkins = api;
})(typeof self !== "undefined" ? self : this);
