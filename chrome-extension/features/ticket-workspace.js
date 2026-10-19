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
  badge.style.marginTop = "4px";
  badge.style.display = "inline-block";
  badge.style.marginRight = "6px";
  if (href) {
    badge.href = href;
    badge.target = "_blank";
    badge.rel = "noopener noreferrer";
  }
  return badge;
}

// A tone of the scan's CI states -> the colour of a row's icon.
const TICKET_WORKSPACE_ICON_TONES = { ok: "ok", bad: "bad", busy: "info", warn: "warn", neutral: "mute" };

/** The most of a long excerpt a row shows. */
function ticketWorkspaceClip(text, max) {
  const flat = String(text || "").replace(/\s+/g, " ").trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
}

function renderTicketWorkspacePanel(job, panelApi) {
  const ws = job.data?.workspace;
  if (job.status === "failed" || !ws) {
    return paiErrorBox(job.error || "The scan found nothing to show.");
  }

  const worktrees = (ws.worktrees || []).map((w) => {
    const state = w.exists === false ? ["gone", "neutral"] : w.dirty ? [`${w.changed} changed`, "warn"] : ["clean", "ok"];
    const ahead = w.ahead ? `, ${w.ahead} ahead` : "";
    return paiRow({
      icon: "folder",
      tone: "info",
      title: w.dir,
      sub: `${w.repoKey}: ${w.branch || "(detached)"}${ahead}`,
      tag: state[0],
      tagTone: state[1],
      // A folder isn't a URL: the link asks the service to open it in the editor.
      ...(w.exists === false || !panelApi
        ? {}
        : {
            hint: "Open in your editor",
            async onClick() {
              try {
                await panelApi.action("open-worktree", { dir: w.dir });
              } catch (err) {
                panelApi.showError(err && err.message ? err.message : "Couldn't open the worktree.");
              }
            },
          }),
    });
  });

  const branches = (ws.branches || []).map((b) =>
    paiRow({
      icon: "view_tree",
      tone: b.local ? "info" : "mute",
      title: b.name,
      sub: b.repoKey,
      tag: b.remote ? (b.local ? "local + origin" : "origin") : "local only",
      tagTone: "neutral",
    }),
  );

  const prs = (ws.prs || []).map((p) => {
    const tone = p.build && p.build.state ? TICKET_WORKSPACE_BUILD_TONES[p.build.state] || "neutral" : p.state === "OPEN" ? "ok" : "neutral";
    return paiRow({
      icon: "copy_replicate",
      tone: TICKET_WORKSPACE_ICON_TONES[tone] || "mute",
      title: `#${p.id} ${p.title || ""}`.trim(),
      href: ticketWorkspaceHttpsUrl(p.url) || undefined,
      sub: p.repoKey,
      tag: (p.state || "seen").toLowerCase(),
      tagTone: tone,
      extra: ((p.build && p.build.builds) || []).map(ticketWorkspaceBuildBadge),
    });
  });

  const analyses = (ws.analyses || []).map((a) =>
    paiRow({ icon: "file_search", tone: "info", title: a.title || a.key, sub: ticketWorkspaceClip(a.excerpt, 220) }),
  );
  const sessions = (ws.sessions || []).map((s) =>
    paiRow({ icon: "chat", tone: "info", title: `${s.permissionMode} session`, sub: s.cwd }),
  );
  const notes = (ws.notes || []).map((n) => paiRow({ icon: "notice_triangle", tone: "warn", title: String(n) }));

  const cards = [
    paiCard("Worktrees", "folder", worktrees),
    paiCard("Branches", "view_tree", branches),
    paiCard("Pull requests", "copy_replicate", prs),
    paiCard("Analyses", "file_search", analyses),
    paiCard("Claude Code sessions", "chat", sessions),
    paiCard("Couldn't check", "notice_triangle", notes),
  ];
  if (!cards.some(Boolean)) cards.push(paiProseCard("Workspace", "workspace", "Nothing on this machine mentions this ticket yet."));
  // The live refresh adds its own status tag; this is the line under the title.
  return { node: paiPage(cards, "Read-only. Nothing is changed until you start a fix."), header: { subtitle: job.result?.summary || ws.issueKey } };
}

const TICKET_WORKSPACE_JIRA_PAGE = /\/browse\/([A-Z][A-Z0-9_]*-\d+)/;

/** The Jira, pull request and CI build page patterns as one: the entry is offered on all three. */
function ticketWorkspacePages(targets) {
  return new RegExp(
    [TICKET_WORKSPACE_JIRA_PAGE, PaiGit.urlPattern(targets), PaiJenkins.JENKINS_BUILD_PAGE].map((re) => `(?:${re.source})`).join("|"),
  );
}

// The ticket a PR or a build belongs to is read from its branch, title or job name; these are asked
// again on every page change, so a definitive answer is kept for a minute.
const ticketWorkspaceKeyCache = new Map(); // page url -> { at, issueKey }
async function ticketWorkspaceKeyCached(url, find) {
  const hit = ticketWorkspaceKeyCache.get(url);
  if (hit && Date.now() - hit.at < 60000) return hit.issueKey;
  const found = await find();
  if (found !== undefined) {
    ticketWorkspaceKeyCache.delete(url);
    ticketWorkspaceKeyCache.set(url, { at: Date.now(), issueKey: found });
    while (ticketWorkspaceKeyCache.size > 50) ticketWorkspaceKeyCache.delete(ticketWorkspaceKeyCache.keys().next().value);
  }
  return found === undefined ? null : found;
}

/** The ticket key of the page: the ticket itself on Jira; on a pull request, the key in its branch or title; on a
 * build, the key in its job name (a branch job), parameters, description or commit messages. null if it names none. */
async function ticketWorkspaceIssueKey(ctx) {
  const jira = ctx.url.match(TICKET_WORKSPACE_JIRA_PAGE);
  if (jira) return jira[1];
  const pr = ctx.url.match(PaiGit.urlPattern(ctx.targets));
  if (pr) {
    return ticketWorkspaceKeyCached(ctx.url, async () => {
      const read = await PaiGit.pr(ctx, { project: pr[1], repo: pr[2], prId: pr[3] });
      return read ? PaiTargets.ticketKeyFrom(read.fromBranch, read.title) : undefined;
    });
  }
  const build = PaiJenkins.jenkinsBuildFromUrl(ctx.url);
  if (!build) return null;
  return ticketWorkspaceKeyCached(build.buildUrl, async () => {
    try {
      const res = await fetch(
        `${build.buildUrl}api/json?tree=description,displayName,actions[parameters[name,value]],changeSets[items[msg]]`,
        { credentials: "include" },
      );
      if (!res.ok) return undefined;
      const b = await res.json();
      const params = (b.actions || []).flatMap((a) => (a.parameters || []).map((p) => String(p.value ?? "")));
      const commits = (b.changeSets || []).flatMap((c) => (c.items || []).map((i) => i.msg));
      return PaiTargets.ticketKeyFrom(build.jobName, ...params, b.description, b.displayName, ...commits);
    } catch (e) {
      return undefined; // can't tell: no row, and nothing kept so the next check retries
    }
  });
}

PaiRegistry.register({
  site: ["issues", "git", "ci"],
  id: "ticket-workspace",
  settingsGroups: ["jira", "git"],
  menuLabel: "Ticket workspace",
  icon: "workspace",
  readOnly: true,
  // The scan reflects live branches/PRs/builds, so a result is never kept:
  // every click runs it again and opens the panel when it finishes, and
  // the menu item never turns green (content.js).
  // Opens the last report straight away and refreshes it behind the panel (content.js openLiveReport).
  liveReport: true,
  liveScope: (payload) => payload && payload.issueKey,
  alwaysFresh: true,

  // Every Jira ticket, and every pull request or build whose branch, title or job names one; the scan
  // itself runs only when clicked.
  urlPattern: ticketWorkspacePages,
  async condition(ctx) {
    const issueKey = await ticketWorkspaceIssueKey(ctx);
    return issueKey ? { issueKey } : null;
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
    // One button for both: the ticket's recorded Claude session if there is one,
    // else Claude Code in plan mode on the ticket's branch (an existing checkout
    // first; the service asks before it makes a worktree).
    {
      id: "continue",
      label: "Continue in Claude Code",
      hint: "Reopens this ticket's Claude Code session. With none, opens Claude Code in plan mode on the ticket's branch, using a checkout you already have; it asks before creating a worktree.",
      when(job, env) {
        if (job.status !== "awaiting-approval") return false;
        if (job.data?.workspace?.resume?.session) return true;
        return (
          !!job.data?.workspace?.suggestions?.startFix &&
          (!env?.enabledFeatureIds || env.enabledFeatureIds.has("ticket-to-pr"))
        );
      },
      async run(job, api) {
        const session = job.data.workspace.resume?.session;
        if (session) {
          await api.action("resume-session", { sessionId: session.id });
          return;
        }
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
