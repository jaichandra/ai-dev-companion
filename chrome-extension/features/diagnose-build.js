// The "diagnose-build" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

PaiRegistry.register({
  site: "ci",
  id: "diagnose-build",
  settingsGroups: ["git"],
  menuLabel: "Diagnose this failed build",
  oneShot: true,
  urlPattern: PaiJenkins.JENKINS_BUILD_PAGE,

  // Shown only when Jenkins says the build finished FAILURE or UNSTABLE.
  // conditions re-run on every page change, so the answer is cached briefly.
  condition: (() => {
    const cache = new Map(); // buildUrl -> { at, payload }; only definitive answers
    const MAX_ENTRIES = 50;
    return async function condition(ctx) {
      const build = PaiJenkins.jenkinsBuildFromUrl(ctx.url);
      if (!build) return null;
      const hit = cache.get(build.buildUrl);
      if (hit && Date.now() - hit.at < 60000) return hit.payload;
      let payload = null;
      let definitive = false;
      try {
        const res = await fetch(build.statusUrl, { credentials: "include" });
        if (res.ok) {
          const status = await res.json();
          if (!status.building) {
            definitive = true;
            if (status.result === "FAILURE" || status.result === "UNSTABLE") {
              payload = { buildUrl: build.buildUrl };
            }
          }
        }
      } catch (e) {
        // Can't tell — no row, and nothing cached so the next check retries.
      }
      if (definitive) {
        cache.delete(build.buildUrl);
        cache.set(build.buildUrl, { at: Date.now(), payload });
        while (cache.size > MAX_ENTRIES) cache.delete(cache.keys().next().value);
      }
      return payload;
    };
  })(),
});
