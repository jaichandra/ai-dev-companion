// The "summarize-comments" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

PaiRegistry.register({
  site: "issues",
  id: "summarize-comments",
  menuLabel: "Summarize comments",
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

  // Read-only report, laid out like the analyze-issue report: headline,
  // meta line, then titled sections. Every string is model output or ticket
  // text — textContent only.
  renderPanel(job) {
    const container = document.createElement("div");
    const data = job.data;
    const summary = data?.summary;
    if (job.status === "failed" || !summary || typeof summary !== "object") {
      const box = document.createElement("pre");
      box.className = "action-error";
      box.textContent = job.error || "No summary was produced.";
      container.appendChild(box);
      return container;
    }

    const headline = document.createElement("div");
    headline.className = "report-headline";
    headline.textContent = [data.issueKey, data.title].filter(Boolean).join(" — ") || "Comment summary";
    container.appendChild(headline);

    const meta = document.createElement("div");
    meta.className = "report-meta";
    const bits = [`${data.commentCount} comment${data.commentCount === 1 ? "" : "s"}`];
    if (Number(data.omitted) > 0) bits.push(`${data.omitted} older left out for length`);
    meta.textContent = bits.join(" · ");
    container.appendChild(meta);

    if (data.fromCache) {
      const saved = document.createElement("div");
      saved.className = "report-meta";
      saved.style.marginTop = "-8px";
      const when = formatAnalysisSavedAt(data.completedAt);
      saved.textContent = when ? `Saved summary · ${when}` : "Saved summary";
      container.appendChild(saved);
    }

    // The model didn't return the structured shape: show its text as is.
    if (typeof summary.raw === "string" && !summary.tldr) {
      const pre = document.createElement("pre");
      pre.className = "action-error";
      pre.style.background = "#fafbfc";
      pre.style.borderColor = "#dfe1e6";
      pre.textContent = summary.raw;
      container.appendChild(pre);
      return container;
    }

    const tldr = document.createElement("p");
    tldr.style.lineHeight = "1.5";
    tldr.style.margin = "0 0 16px";
    tldr.textContent = summary.tldr || "";
    const tldrSection = renderSection("Summary", tldr);
    if (tldrSection) container.appendChild(tldrSection);

    for (const [title, items] of [
      ["Decisions", summary.decisions],
      ["Open questions", summary.openQuestions],
      ["Next steps", summary.nextSteps],
    ]) {
      const section = renderSection(title, renderList(items));
      if (section) container.appendChild(section);
    }
    return container;
  },
});
