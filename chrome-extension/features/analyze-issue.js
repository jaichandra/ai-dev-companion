// The "analyze-issue" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

/** Confidence → status-tag tone. Mirrors the report schema's high|medium|low values; anything else still
 * renders as itself. */
function confidenceTone(confidence) {
  const key = String(confidence || "").toLowerCase();
  return key === "high" ? "ok" : key === "medium" ? "warn" : "neutral";
}

/** The panel's body and header for the analysis of a ticket. Every string is model output or ticket text. */
function renderAnalyzeIssuePanel(job) {
  const analysis = job.data?.analysis;
  if (job.status === "failed" || !analysis) {
    return paiErrorBox(job.error || "No analysis was produced.");
  }

  const issueKey = job.data?.issueKey || "";
  const summary = job.data?.summary || "";
  const repoKey = job.data?.repoKey || "";
  // Companion stores repoMatch; tolerate older/alternate field names.
  const matchReason = job.data?.repoMatch || job.data?.repoMatchReason || job.data?.matchReason || job.data?.reason || "";
  const saved = job.data?.fromCache ? formatAnalysisSavedAt(job.data.completedAt) || "earlier" : "";
  const header = {
    subtitle: [[issueKey, summary].filter(Boolean).join(" · "), saved ? `Saved analysis · ${saved}` : ""].filter(Boolean).join(" · "),
    tag: saved ? "Saved" : "Ready",
    tone: saved ? "neutral" : "ok",
  };
  const foot = "Uses WebFetch and your MCP servers. Nothing is written to Jira.";

  // Parse failure from the companion: show the raw Claude text rather than pretending the structured sections exist.
  if (typeof analysis.raw === "string" && analysis.raw && !analysis.tldr) {
    const text = paiProseCard("Analysis", "file_search", analysis.raw);
    text.querySelector(".pai-prose").style.whiteSpace = "pre-wrap";
    return { node: paiPage([text], foot), header };
  }

  const preferred = Object.entries(analysis.toolsUsed?.preferred || {});
  const toolTags = preferred.map(([server, used]) => {
    const tag = paiTag(`${server} used: ${used === true ? "yes" : "no"}`, used === true ? "ok" : "neutral");
    tag.style.marginRight = "6px";
    return tag;
  });
  const repoRow = paiRow({
    icon: "source",
    tone: repoKey ? "info" : "mute",
    title: repoKey || "No repository matched",
    sub: matchReason,
    tag: repoKey ? "Matched" : "",
    tagTone: "ok",
    extra: toolTags,
  });

  const affected = (analysis.affectedArea || []).map((entry) =>
    paiRow({
      icon: "code",
      tone: "info",
      title: [entry?.path || "", entry?.symbol || ""].filter(Boolean).join(" · ") || "(unknown location)",
      sub: entry?.why,
    }),
  );
  const hypotheses = (analysis.hypotheses || []).map((entry) =>
    paiRow({
      icon: "light_bulb",
      tone: "warn",
      title: entry?.hypothesis || "",
      sub: entry?.evidence,
      tag: entry?.confidence || "unknown",
      tagTone: confidenceTone(entry?.confidence),
    }),
  );
  const references = (analysis.referencesReviewed || []).map((entry) =>
    paiRow({
      icon: "link",
      tone: "mute",
      title: entry?.url || "",
      href: ticketWorkspaceHttpsUrl(entry?.url) || undefined,
      sub: entry?.takeaway,
    }),
  );

  return {
    node: paiPage(
      [
        paiProseCard("Initial assessment", "light_bulb", analysis.tldr),
        paiCard("Matched repository", "source", [repoRow]),
        paiCard("Likely affected area", "code", affected),
        paiCard("Root-cause hypotheses", "search", hypotheses),
        paiProseCard("Repro and scope", "info", analysis.reproScope),
        paiCard("References reviewed", "link", references),
        paiListCard("Suggested next steps", "timeline", analysis.nextSteps, "arrow_right", "info"),
        paiListCard("Open questions", "info", analysis.openQuestions, "chat", "info"),
      ],
      foot,
    ),
    header,
  };
}

//
// Kept as small, standalone pure functions (no DOM) so the matching rules
// are easy to read on their own and to exercise outside the extension.

PaiRegistry.register({
  site: "issues",
  id: "analyze-issue",
  settingsGroups: ["jira"],
  menuLabel: "Analyze ticket",
  panelTitle: "Ticket analysis",
  icon: "file_search",
  readOnly: true,
  // The report is kept on the companion (analysis-cache), not by this tab:
  // every click starts a run (instant when saved) and opens the panel, and
  // the menu item carries a green check while a saved analysis exists.
  alwaysFresh: true,
  savedResultIssueKey(match) {
    return match[1];
  },
  // Cancel while running (companion abort); finished panel is Done +
  // Re-analyze (force: true skips the on-disk analysis cache).
  cancellable: true,
  cancelConfirm: "The analysis in progress will be stopped and discarded.",
  // While running, the service may stop to ask which repository (job.data.pendingChoice);
  // the answer goes to this named action (companion's analyze-issue "confirm-repo").
  repoChoiceAction: "confirm-repo",
  reanalyzeLabel: "Re-analyze",
  postCommentLabel: "Add as comment",

  // Configured projects' Bugs only (GET /targets; default PROJ Bug) — the
  // urlPattern pins the project keys; the condition checks issuetype the
  // same way create-jira-subtasks does for Story/Epic. A Story, or a Bug
  // in another project, never shows a row.
  urlPattern: PaiTargets.issueUrlPattern,

  // match[1] is already the full issue key ("PROJ-123"), matching
  // core/scope-key.js's jiraScopeKey output.
  scopeKey(match) {
    return `jira:${match[1].toUpperCase()}`;
  },

  async condition(ctx) {
    const issueKey = ctx.match[1];

    const res = await fetch(`${ctx.origin}/rest/api/2/issue/${issueKey}?fields=issuetype`, {
      credentials: "include",
    });
    if (!res.ok) return null;
    const issue = await res.json();

    const typeName = issue.fields?.issuetype?.name;
    // No issue types configured = any type of the configured projects.
    const wantedTypes = (ctx.targets || PaiTargets.DEFAULT_TARGETS).analyzeIssue.issueTypes;
    if (wantedTypes.length > 0 && !wantedTypes.includes(typeName)) return null;

    return { issueKey };
  },

  progressSteps: [
    { id: "fetch", label: "Read ticket" },
    { id: "repo", label: "Identify and confirm repository" },
    { id: "analyze", label: "Analyze with Claude" },
    { id: "format", label: "Prepare report" },
  ],

  // Read-only report panel: the job lands in awaiting-approval with job.data.analysis, and the footer is
  // Done + Re-analyze (content.js's reanalyzeLabel path). Add as comment posts the report to the ticket via
  // the companion.
  renderPanel(job) {
    return renderAnalyzeIssuePanel(job);
  },

  // One button. With ticket-to-pr enabled it is the ticket's Claude Code
  // session: the recorded one reopened, else plan mode on the ticket's branch
  // with this analysis (an existing checkout first; the service asks before it
  // makes a worktree). With it disabled, it reopens this analysis's own headless
  // session read-only, as "Discuss in Claude Code" did: job.data.claudeSession
  // is set by the companion once the analysis has run, and whether its working
  // copy and transcript are still on disk is checked when the button is clicked
  // (core/resume.js).
  footerActions: [
    {
      id: "continue",
      label: "Continue in Claude Code",
      hint: "Reopens this ticket's Claude Code session, or opens Claude Code in plan mode on the ticket's branch with this analysis. Uses a checkout you already have; asks before creating a worktree.",
      when(job, env) {
        return (
          job.status === "awaiting-approval" &&
          !!job.data?.repoKey &&
          (!env?.enabledFeatureIds || env.enabledFeatureIds.has("ticket-to-pr"))
        );
      },
      async run(job, api) {
        await api.startFeature("ticket-to-pr", { issueKey: job.data.issueKey });
      },
    },
    {
      id: "continue-claude-code",
      label: "Continue in Claude Code",
      hint: "Reopens this analysis in a terminal so you can ask follow-up questions. Read-only: it changes nothing.",
      when(job, env) {
        // Without a repo the session's folder is an empty per-ticket one: nothing to continue in.
        return (
          !!job.data?.claudeSession &&
          !!job.data?.repoKey &&
          (job.status === "awaiting-approval" || job.status === "failed") &&
          !!env?.enabledFeatureIds &&
          !env.enabledFeatureIds.has("ticket-to-pr")
        );
      },
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
