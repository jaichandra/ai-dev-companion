// The "ticket-workspace" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

const TICKET_WORKSPACE_BUILD_TONES = { SUCCESSFUL: "ok", FAILED: "bad", INPROGRESS: "busy" };

/** One CI build as a badge — "my_job #6" from "my_job » branch #6" —
 * linking to the build when it has an https URL. */
function ticketWorkspaceBuildBadge(build) {
  const name = typeof build.name === "string" ? build.name : "";
  const m = name.match(/^(.*?)\s+».*?(#\d+)\s*$/);
  const label = m ? `${m[1]} ${m[2]}` : name || "build";
  const state = typeof build.state === "string" ? build.state.toLowerCase() : "unknown";
  const tone = TICKET_WORKSPACE_BUILD_TONES[build.state] || "neutral";
  const href = ticketWorkspaceHttpsUrl(build.url);
  const badge = document.createElement(href ? "a" : "span");
  badge.className = `status-badge ${tone}`;
  badge.textContent = label;
  badge.title = `${name || "build"} — ${state}`;
  badge.style.marginLeft = "6px";
  if (href) {
    badge.href = href;
    badge.target = "_blank";
    badge.rel = "noopener noreferrer";
  }
  return badge;
}

/** A ticketWorkspaceLine styled as a digest-panel row (tone stripe, 13px). */
function ticketWorkspaceRow(badgeText, tone, text, href) {
  const row = ticketWorkspaceLine(badgeText, tone, text, href);
  row.className = `digest-row ${tone || "neutral"}`;
  row.removeAttribute("style");
  return row;
}

/** A digest-style section card: uppercase header with a count pill, then rows. */
function ticketWorkspaceCard(title, rows) {
  const node = renderSection(title, rows);
  if (!node) return null;
  node.removeAttribute("style");
  node.classList.add("digest-section");
  const count = document.createElement("span");
  count.className = "digest-count";
  count.textContent = String(rows.length);
  node.firstElementChild.appendChild(count);
  return node;
}

function renderTicketWorkspacePanel(job, panelApi) {
  const container = document.createElement("div");
  container.className = "digest";
  const ws = job.data?.workspace;
  if (job.status === "failed" || !ws) {
    const box = document.createElement("pre");
    box.className = "action-error";
    box.textContent = job.error || "The scan found nothing to show.";
    container.appendChild(box);
    return container;
  }
  const headline = document.createElement("div");
  headline.className = "digest-headline";
  headline.textContent = job.result?.summary || ws.issueKey;
  container.appendChild(headline);

  const worktrees = (ws.worktrees || []).map((w) => {
    const state = w.exists === false ? ["gone", "neutral"] : w.dirty ? [`${w.changed} changed`, "warn"] : ["clean", "ok"];
    const ahead = w.ahead ? `, ${w.ahead} ahead` : "";
    const row = ticketWorkspaceRow(state[0], state[1], `${w.repoKey}: ${w.branch || "(detached)"}${ahead} — `);
    if (w.exists === false || !panelApi) {
      row.appendChild(document.createTextNode(w.dir));
      return row;
    }
    // A folder isn't a URL: the link asks the service to open it in the editor.
    const a = document.createElement("a");
    a.href = "#";
    a.textContent = w.dir;
    a.title = "Open in your editor";
    a.addEventListener("click", async (event) => {
      event.preventDefault();
      try {
        await panelApi.action("open-worktree", { dir: w.dir });
      } catch (err) {
        panelApi.showError(err && err.message ? err.message : "Couldn't open the worktree.");
      }
    });
    row.appendChild(a);
    return row;
  });
  const worktreeSection = ticketWorkspaceCard("Worktrees", worktrees);
  if (worktreeSection) container.appendChild(worktreeSection);

  const branches = (ws.branches || []).map((b) =>
    ticketWorkspaceRow(b.remote ? (b.local ? "local + origin" : "origin") : "local only", "neutral", `${b.repoKey}: ${b.name}`),
  );
  const branchSection = ticketWorkspaceCard("Branches", branches);
  if (branchSection) container.appendChild(branchSection);

  const prs = (ws.prs || []).map((p) => {
    const tone = p.build && p.build.state ? TICKET_WORKSPACE_BUILD_TONES[p.build.state] || "neutral" : p.state === "OPEN" ? "ok" : "neutral";
    const row = ticketWorkspaceRow((p.state || "seen").toLowerCase(), tone, `${p.repoKey} #${p.id} ${p.title || ""}`, p.url);
    for (const b of (p.build && p.build.builds) || []) row.appendChild(ticketWorkspaceBuildBadge(b));
    return row;
  });
  const prSection = ticketWorkspaceCard("Pull requests", prs);
  if (prSection) container.appendChild(prSection);

  const analyses = (ws.analyses || []).map((a) =>
    ticketWorkspaceRow(null, "neutral", `${a.title || a.key}${a.excerpt ? ` — ${a.excerpt}` : ""}`),
  );
  const analysisSection = ticketWorkspaceCard("Analyses", analyses);
  if (analysisSection) container.appendChild(analysisSection);

  const sessions = (ws.sessions || []).map((s) => ticketWorkspaceRow(null, "neutral", `${s.permissionMode} session in ${s.cwd}`));
  const sessionSection = ticketWorkspaceCard("Claude Code sessions", sessions);
  if (sessionSection) container.appendChild(sessionSection);

  const notes = (ws.notes || []).map((n) => ticketWorkspaceRow(null, "warn", String(n)));
  const noteSection = ticketWorkspaceCard("Couldn't check", notes);
  if (noteSection) container.appendChild(noteSection);

  if (container.childNodes.length === 1) {
    const none = document.createElement("p");
    none.className = "digest-when";
    none.textContent = "Nothing on this machine mentions this ticket yet.";
    container.appendChild(none);
  }
  return container;
}

PaiRegistry.register({
  site: "issues",
  id: "ticket-workspace",
  settingsGroups: ["jira", "git"],
  menuLabel: "Ticket workspace",
  readOnly: true,
  // The scan reflects live branches/PRs/builds, so a result is never kept:
  // every click runs it again and opens the panel when it finishes, and
  // the menu item never turns green (content.js).
  alwaysFresh: true,

  // Every Jira ticket; the scan itself runs only when clicked.
  urlPattern: /\/browse\/([A-Z][A-Z0-9_]*-\d+)/,
  condition(ctx) {
    return { issueKey: ctx.match[1] };
  },

  progressSteps: [
    { id: "history", label: "Read the local history" },
    { id: "git", label: "Find branches and worktrees" },
    { id: "git", label: "Check pull requests" },
  ],

  renderPanel(job, panelApi) {
    return renderTicketWorkspacePanel(job, panelApi);
  },

  // Acts on the workspace's main item: the session in the ticket's own
  // worktree (else the newest). PRs and worktrees are links in the panel.
  footerActions: [
    {
      id: "resume-session",
      label: "Resume Claude session",
      when(job) {
        return job.status === "awaiting-approval" && !!job.data?.workspace?.resume?.session;
      },
      async run(job, api) {
        await api.action("resume-session", { sessionId: job.data.workspace.resume.session.id });
      },
    },
    {
      id: "start-fix",
      label: "Start fix",
      hint: "Creates a branch and worktree for this ticket, then opens Claude Code in plan mode with this analysis so you can work out and make the fix.",
      when(job, env) {
        return (
          job.status === "awaiting-approval" &&
          !!job.data?.workspace?.suggestions?.startFix &&
          (!env?.enabledFeatureIds || env.enabledFeatureIds.has("ticket-to-pr"))
        );
      },
      async run(job, api) {
        // The scan's related PRs point at the repository (no analysis needed).
        const hintRepoKeys = [...new Set((job.data.workspace.prs || []).map((pr) => pr.repoKey).filter(Boolean))];
        await api.startFeature("ticket-to-pr", { issueKey: job.data.issueKey, hintRepoKeys });
      },
    },
    {
      id: "create-pr",
      label: "Track PR",
      hint: "Watches this branch: once its pull request is open, the ticket is moved to review automatically.",
      when(job, env) {
        return (
          job.status === "awaiting-approval" &&
          !!job.data?.workspace?.suggestions?.createPr &&
          (!env?.enabledFeatureIds || env.enabledFeatureIds.has("ticket-to-pr"))
        );
      },
      async run(job, api) {
        const target = job.data.workspace.suggestions.createPr;
        await api.startFeature("ticket-to-pr", { issueKey: job.data.issueKey, repoKey: target.repoKey, adopt: true });
      },
    },
  ],
});
