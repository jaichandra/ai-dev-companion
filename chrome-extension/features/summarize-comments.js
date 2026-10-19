// The "summarize-comments" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

PaiRegistry.register({
  site: "issues",
  id: "summarize-comments",
  menuLabel: "Summarize comments",
  panelTitle: "Comment summary",
  icon: "chat",
  readOnly: true,
  // The summary is kept on the companion (summary-cache) like analyze-issue's
  // report: every click starts a run (instant when the saved one is current),
  // the panel opens when it finishes, and the menu item carries a green check
  // while a saved summary exists.
  alwaysFresh: true,
  savedResultIssueKey(match) {
    return match[1];
  },
  savedResultMessage: "summary-saved",
  // The saved summary covers `saved.commentCount` comments; once the ticket
  // has a different number it is stale (no check, and the next click re-runs).
  async savedResultIsCurrent(ctx, saved) {
    const res = await fetch(`${ctx.origin}/rest/api/2/issue/${ctx.match[1]}/comment?maxResults=0`, { credentials: "include" });
    if (!res.ok) return false;
    return (await res.json()).total === saved.commentCount;
  },
  cancellable: true,
  cancelConfirm: "The summary in progress will be stopped and discarded.",
  reanalyzeLabel: "Re-summarize",

  // Every Jira ticket page; the row shows only when the ticket has comments.
  urlPattern: /\/browse\/([A-Z][A-Z0-9_]*-\d+)/,

  scopeKey(match) {
    return `jira:${match[1].toUpperCase()}`;
  },

  async condition(ctx) {
    const issueKey = ctx.match[1];
    // Same-origin call to the Jira page's own host, session cookie sent by
    // default; maxResults=0 returns just the total.
    const res = await fetch(`${ctx.origin}/rest/api/2/issue/${issueKey}/comment?maxResults=0`, {
      credentials: "include",
    });
    if (!res.ok) return null;
    const data = await res.json();
    if (!(data.total > 0)) return null;
    return { issueKey, commentCount: data.total };
  },

  progressSteps: [
    { id: "fetch", label: "Read comments" },
    { id: "summarize", label: "Summarize with Claude" },
  ],

  // Read-only report: the summary and its lists as cards, like the analyze-issue report. Every string is model
  // output or ticket text — textContent only.
  renderPanel(job) {
    const data = job.data;
    const summary = data?.summary;
    if (job.status === "failed" || !summary || typeof summary !== "object") {
      return paiErrorBox(job.error || "No summary was produced.");
    }

    const count = Number(data.commentCount) || 0;
    const saved = data.fromCache ? formatAnalysisSavedAt(data.completedAt) || "earlier" : "";
    const header = {
      subtitle: [
        [data.issueKey, data.title].filter(Boolean).join(" · "),
        `${count} comment${count === 1 ? "" : "s"}${Number(data.omitted) > 0 ? ` (${data.omitted} older left out for length)` : ""}`,
        saved ? `Saved summary · ${saved}` : "",
      ]
        .filter(Boolean)
        .join(" · "),
      tag: saved ? "Saved" : "Updated",
      tone: saved ? "neutral" : "busy",
    };
    const foot = "Summary only. No comments are posted.";

    // The model didn't return the structured shape: show its text as is.
    if (typeof summary.raw === "string" && !summary.tldr) {
      const text = paiProseCard("Summary", "chat", summary.raw);
      text.querySelector(".pai-prose").style.whiteSpace = "pre-wrap";
      return { node: paiPage([text], foot), header };
    }

    return {
      node: paiPage(
        [
          paiProseCard("Summary", "light_bulb", summary.tldr),
          paiListCard("Decisions", "check", summary.decisions, "check", "ok"),
          paiListCard("Open questions", "info", summary.openQuestions, "chat", "warn"),
          paiListCard("Next steps", "timeline", summary.nextSteps, "arrow_right", "info"),
        ],
        foot,
      ),
      header,
    };
  },
});
