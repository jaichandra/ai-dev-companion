// The "diagnose-build" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

const DIAGNOSE_CATEGORIES = {
  regression: ["Regression", "bad"],
  flaky: ["Flaky", "warn"],
  infrastructure: ["Infrastructure", "warn"],
  unknown: ["Cause unknown", "neutral"],
};
// A stage's result -> [icon, row tone, tag text, tag tone]
const DIAGNOSE_STAGES = {
  passed: ["success", "ok", "", ""],
  failed: ["error", "bad", "Failed", "bad"],
  skipped: ["clock", "mute", "Skipped", "neutral"],
  unknown: ["info", "mute", "", ""],
};

/** "job #12" for the header and the stage list. */
function diagnoseBuildLabel(data) {
  return data && data.jobName && data.number ? `${data.jobName} #${data.number}` : "Build";
}

/** The panel's body and header for a finished (or failed) diagnosis. Every string is model output or build
 * data, so textContent only (paiRow does that). */
function renderDiagnoseBuildPanel(job) {
  const data = job.data || {};
  const subtitle = `${diagnoseBuildLabel(data)} · Jenkins`;
  const report = data.report;
  if (job.status === "failed" || !report) {
    return { node: paiErrorBox(job.error || "No diagnosis was produced."), header: { subtitle, tag: "Couldn't diagnose", tone: "bad" } };
  }
  const header = { subtitle, tag: "Failed", tone: "bad" };
  const foot = "Read-only diagnosis. Nothing is changed, re-run or posted.";
  // The model didn't return the structured shape: show its text as is.
  if (typeof report.raw === "string" && !report.tldr) {
    const text = paiProseCard("Diagnosis", "light_bulb", report.raw);
    text.querySelector(".pai-prose").style.whiteSpace = "pre-wrap";
    return { node: paiPage([text], foot), header };
  }

  const [categoryLabel, categoryTone] = DIAGNOSE_CATEGORIES[report.category] || DIAGNOSE_CATEGORIES.unknown;
  const causes = (report.causes || []).map((c, i) =>
    paiRow({
      icon: "error",
      tone: "bad",
      title: c.title,
      sub: [c.where, c.detail].filter(Boolean).join(" · "),
      tag: i === 0 ? categoryLabel : "",
      tagTone: categoryTone,
    }),
  );
  const suspects = (report.suspects || []).map((c) => paiRow({ icon: "code", tone: "info", title: c.title, sub: c.detail }));
  const tests = (report.failingTests || []).map((t) =>
    paiRow({
      icon: t.flaky ? "notice_triangle" : "error",
      tone: t.flaky ? "warn" : "bad",
      title: t.name,
      sub: [t.where, t.note].filter(Boolean).join(" · "),
      tag: t.flaky ? "Flaky" : "Failing",
      tagTone: t.flaky ? "warn" : "bad",
    }),
  );
  const stages = (report.stages || []).map((s) => {
    const [icon, tone, tag, tagTone] = DIAGNOSE_STAGES[s.result] || DIAGNOSE_STAGES.unknown;
    return paiRow({ icon, tone, title: s.name, sub: s.duration, tag, tagTone });
  });
  return {
    node: paiPage(
      [
        paiProseCard("Summary", "light_bulb", report.tldr),
        paiCard("Likely cause", "light_bulb", causes),
        paiCard("Changed in this build", "code", suspects),
        paiCard("Failing tests", "flag", tests),
        paiCard("Build stages", "timeline", stages),
        paiListCard("Next steps", "check", report.nextSteps, "chevron_right", "info"),
      ],
      foot,
    ),
    header,
  };
}

PaiRegistry.register({
  site: "ci",
  id: "diagnose-build",
  settingsGroups: ["git"],
  menuLabel: "Diagnose this failed build",
  panelTitle: "Failed build diagnosis",
  icon: "error",
  readOnly: true,
  // The panel opens at the click and shows the progress of the headless run; the report replaces it when done.
  panelWhileRunning: true,
  runningLabel: "Diagnosing with Claude…",
  runningHeader(payload) {
    const build = payload && PaiJenkins.jenkinsBuildFromUrl(payload.buildUrl);
    return { subtitle: build ? `${build.jobName} #${build.number} · Jenkins` : "Jenkins", tag: "Diagnosing", tone: "busy" };
  },
  reanalyzeLabel: "Re-diagnose",
  reanalyzePayload: (job) => (job.data && job.data.buildUrl ? { buildUrl: job.data.buildUrl, force: true } : null),
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

  renderPanel(job) {
    return renderDiagnoseBuildPanel(job);
  },

  footerActions: [
    {
      id: "open-jenkins",
      label: "Open in Jenkins",
      when: (job) => /^https:\/\//.test((job.data && job.data.buildUrl) || ""),
      async run(job) {
        window.open(job.data.buildUrl, "_blank", "noopener");
      },
    },
    {
      id: "continue-claude-code",
      label: "Continue in Claude Code",
      hint: "Reopens this diagnosis in a terminal so you can ask follow-up questions. Read-only: it changes nothing.",
      when: (job) => !!(job.data && job.data.claudeSession) && (job.status === "awaiting-approval" || job.status === "failed"),
      async run(job, api) {
        try {
          await api.send("open-in-claude-code", { body: { resume: true } });
        } catch (err) {
          api.showError(`Continue in Claude Code failed:\n\n${err.message}`);
        }
      },
    },
  ],
});
