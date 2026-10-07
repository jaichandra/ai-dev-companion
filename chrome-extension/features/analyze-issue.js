// The "analyze-issue" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

/** Confidence → status-badge colour. Mirrors the report schema's
 * high|medium|low values; anything else still renders as itself. */
function confidenceBadge(confidence) {
  const badge = document.createElement("span");
  const key = String(confidence || "").toLowerCase();
  const tone = key === "high" ? "ok" : key === "medium" ? "warn" : "neutral";
  badge.className = `status-badge ${tone}`;
  badge.textContent = confidence || "unknown";
  return badge;
}

//
// Kept as small, standalone pure functions (no DOM) so the matching rules
// are easy to read on their own and to exercise outside the extension.

PaiRegistry.register({
  site: "issues",
  id: "analyze-issue",
  settingsGroups: ["jira"],
  menuLabel: "Analyze ticket",
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

  // Read-only report panel — same shape as pre-deployment-stats: the job
  // lands in awaiting-approval with job.data.analysis, and the footer is
  // Done + Re-analyze (content.js's reanalyzeLabel path). Add as
  // comment posts the report to the ticket via the companion.
  renderPanel(job) {
    const container = document.createElement("div");
    const analysis = job.data?.analysis;

    if (job.status === "failed" || !analysis) {
      const box = document.createElement("pre");
      box.className = "action-error";
      box.textContent = job.error || "No analysis was produced.";
      container.appendChild(box);
      return container;
    }

    // Parse failure from the companion: show the raw Claude text rather
    // than pretending the structured sections exist.
    if (typeof analysis.raw === "string" && analysis.raw && !analysis.tldr) {
      const headline = document.createElement("div");
      headline.className = "report-headline";
      headline.textContent = job.result?.summary || job.data?.issueKey || "Analysis";
      container.appendChild(headline);
      const pre = document.createElement("pre");
      pre.className = "action-error";
      pre.style.background = "#fafbfc";
      pre.style.borderColor = "#dfe1e6";
      pre.textContent = analysis.raw;
      container.appendChild(pre);
      return container;
    }

    const issueKey = job.data?.issueKey || "";
    const summary = job.data?.summary || "";
    const repoKey = job.data?.repoKey || "";
    // Companion stores repoMatch; tolerate older/alternate field names.
    const matchReason =
      job.data?.repoMatch ||
      job.data?.repoMatchReason ||
      job.data?.matchReason ||
      job.data?.reason ||
      "";
    const preferred = Object.entries(analysis.toolsUsed?.preferred || {});

    const headline = document.createElement("div");
    headline.className = "report-headline";
    headline.textContent = [issueKey, summary].filter(Boolean).join(" — ") || job.result?.summary || "Analysis";
    container.appendChild(headline);

    const meta = document.createElement("div");
    meta.className = "report-meta";
    const metaBits = [];
    if (repoKey) {
      metaBits.push(matchReason ? `Repo: ${repoKey} (${matchReason})` : `Repo: ${repoKey}`);
    } else {
      metaBits.push(matchReason ? `Repo: none (${matchReason})` : "Repo: none matched");
    }
    meta.appendChild(document.createTextNode(metaBits.join(" · ")));
    for (const [server, used] of preferred) {
      meta.appendChild(document.createTextNode(" · "));
      const badge = document.createElement("span");
      badge.className = `status-badge ${used === true ? "ok" : "neutral"}`;
      badge.textContent = `${server} used: ${used === true ? "yes" : "no"}`;
      meta.appendChild(badge);
    }
    container.appendChild(meta);

    if (job.data?.fromCache) {
      const saved = document.createElement("div");
      saved.className = "report-meta";
      saved.style.marginTop = "-8px";
      const when = formatAnalysisSavedAt(job.data.completedAt);
      saved.textContent = when ? `Saved analysis · ${when}` : "Saved analysis";
      container.appendChild(saved);
    }

    if (analysis.tldr) {
      const tldr = document.createElement("p");
      tldr.style.lineHeight = "1.5";
      tldr.style.margin = "0 0 16px";
      tldr.textContent = analysis.tldr;
      const section = renderSection("Initial assessment", tldr);
      if (section) container.appendChild(section);
    }

    const affectedList = renderList(analysis.affectedArea, (entry) => {
      const wrap = document.createElement("div");
      const where = document.createElement("div");
      const path = entry?.path || "";
      const symbol = entry?.symbol || "";
      where.textContent = [path, symbol].filter(Boolean).join(" · ") || "(unknown location)";
      wrap.appendChild(where);
      if (entry?.why) {
        const why = document.createElement("div");
        why.className = "report-meta";
        why.style.marginBottom = "0";
        why.textContent = entry.why;
        wrap.appendChild(why);
      }
      return wrap;
    });
    const affectedSection = renderSection("Likely affected area", affectedList);
    if (affectedSection) container.appendChild(affectedSection);

    const hypothesisList = renderList(analysis.hypotheses, (entry) => {
      const wrap = document.createElement("div");
      const top = document.createElement("div");
      top.style.display = "flex";
      top.style.alignItems = "baseline";
      top.style.gap = "8px";
      top.style.flexWrap = "wrap";
      const text = document.createElement("span");
      text.textContent = entry?.hypothesis || "";
      top.append(confidenceBadge(entry?.confidence), text);
      wrap.appendChild(top);
      if (entry?.evidence) {
        const evidence = document.createElement("div");
        evidence.className = "report-meta";
        evidence.style.marginBottom = "0";
        evidence.textContent = entry.evidence;
        wrap.appendChild(evidence);
      }
      return wrap;
    });
    const hypSection = renderSection("Root-cause hypotheses", hypothesisList);
    if (hypSection) container.appendChild(hypSection);

    if (analysis.reproScope) {
      const repro = document.createElement("p");
      repro.style.lineHeight = "1.5";
      repro.style.margin = "0";
      repro.textContent = analysis.reproScope;
      const section = renderSection("Repro and scope", repro);
      if (section) container.appendChild(section);
    }

    const refList = renderList(analysis.referencesReviewed, (entry) => {
      const wrap = document.createElement("div");
      if (entry?.url) {
        const link = document.createElement("a");
        link.href = entry.url;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        link.textContent = entry.url;
        wrap.appendChild(link);
      }
      if (entry?.takeaway) {
        const takeaway = document.createElement("div");
        takeaway.className = "report-meta";
        takeaway.style.marginBottom = "0";
        takeaway.textContent = entry.takeaway;
        wrap.appendChild(takeaway);
      }
      return wrap;
    });
    const refSection = renderSection("References reviewed", refList);
    if (refSection) container.appendChild(refSection);

    const nextList = renderList(analysis.nextSteps);
    const nextSection = renderSection("Suggested next steps", nextList);
    if (nextSection) container.appendChild(nextSection);

    const questionsList = renderList(analysis.openQuestions);
    const questionsSection = renderSection("Open questions", questionsList);
    if (questionsSection) container.appendChild(questionsSection);

    return container;
  },

  // Resumes this analysis's headless Claude session in a terminal —
  // job.data.claudeSession is set by the companion (core/jobs.ts's
  // ClaudeSession) once the analysis has run; whether its working copy
  // and transcript are STILL both on disk is only checked server-side
  // when this button is actually clicked (core/resume.js), which shows a
  // message instead of opening a terminal if it's since expired. Unlike
  // resolve-conflict, analyze-issue has no worktree box to put a
  // Continue button in (it's a report, not a diff), so this is a plain
  // footerActions entry next to Done/Re-analyze/Add as comment instead.
  footerActions: [
    {
      id: "continue-claude-code",
      label: "Discuss in Claude Code",
      hint: "Reopens this analysis in a terminal so you can ask follow-up questions. Read-only: it changes nothing.",
      when(job) {
        // Without a repo the session's folder is an empty per-ticket one: nothing to continue in.
        return !!job.data?.claudeSession && !!job.data?.repoKey && (job.status === "awaiting-approval" || job.status === "failed");
      },
      async run(job, api) {
        try {
          await api.send("open-in-claude-code", { body: { resume: true } });
        } catch (err) {
          api.showError(`Continue in Claude Code failed:\n\n${err.message}`);
        }
      },
    },
    // Ticket to PR's first step, from the report it builds on: a
    // persistent worktree for this ticket and Claude Code in plan mode
    // with this analysis. Only while ticket-to-pr is enabled and the
    // analysis found a repository.
    {
      id: "start-fix",
      label: "Start fix",
      hint: "Creates a branch and worktree for this ticket, then opens Claude Code in plan mode with this analysis so you can work out and make the fix.",
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
  ],
});
