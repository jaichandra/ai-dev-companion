// The "ticket-to-pr" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

const TICKET_TO_PR_STEP_TONES = { done: "ok", skipped: "neutral", warn: "warn", failed: "bad" };

function renderTicketToPrPanel(job) {
  const container = document.createElement("div");
  const d = job.data || {};
  if (job.status === "failed") {
    const box = document.createElement("pre");
    box.className = "action-error";
    box.textContent = job.error || "Start fix failed.";
    container.appendChild(box);
    return container;
  }
  const headline = document.createElement("div");
  headline.className = "report-headline";
  headline.textContent = job.result?.summary || `${d.issueKey || ""} fix`;
  container.appendChild(headline);
  if (d.ticketWorktree?.dir) {
    const where = document.createElement("p");
    where.style.overflowWrap = "anywhere";
    where.textContent = `${d.repoKey}: ${d.branch} (from origin/${d.base}) — ${d.ticketWorktree.dir}`;
    container.appendChild(where);
  }

  if (Array.isArray(d.steps) && d.steps.length > 0) {
    const rows = d.steps.map((s) => ticketWorkspaceLine(s.status, TICKET_TO_PR_STEP_TONES[s.status], s.detail));
    const stepSection = renderSection(job.status === "approved" ? "Done" : "Last attempt", rows);
    if (stepSection) container.appendChild(stepSection);
  }
  if (job.status === "approved" && d.reviewMove && d.reviewMove.detail) {
    container.appendChild(ticketWorkspaceLine(d.reviewMove.moved ? "ok" : "warn", d.reviewMove.moved ? "ok" : "warn", d.reviewMove.detail));
  }
  if (job.status === "approved" && d.pr?.url) {
    container.appendChild(ticketWorkspaceLine("PR", "ok", `#${d.pr.id} ${d.pr.title || ""}`, d.pr.url));
  }
  if (job.status !== "awaiting-approval") return container;

  // Waiting for the pull request. The fix is committed, pushed and opened as
  // a PR from the Claude Code terminal; the service moves the ticket to its
  // review status when it sees the PR (or on the button below).
  const move = d.reviewMove;
  const note = document.createElement("p");
  note.className = "comment-compose-hint";
  if (move && move.moved) {
    note.textContent = `${move.detail} Waiting for the pull request to be opened.`;
  } else if (move && !move.moved) {
    note.textContent = `${move.detail} It will be tried again.`;
  } else {
    note.textContent =
      "Commit your fix, push it and open the pull request from the Claude Code terminal. Once the pull request is open, the ticket is moved to review automatically (or click Move ticket to review).";
  }
  container.appendChild(note);
  return container;
}

PaiRegistry.register({
  site: "issues",
  id: "ticket-to-pr",
  settingsGroups: ["jira", "git"],
  menuLabel: "Ticket to PR",
  // No Create PR form: the pull request is opened from the Claude Code
  // terminal, and the service moves the ticket to review when it is open.
  hideApprove: true,
  rejectLabel: "Stop tracking",

  // No ✨ row of its own: it starts from Start fix (the analysis report,
  // the ticket workspace) or the workspace's Create PR. The pattern lets
  // this page keep tracking its job.
  urlPattern: /\/browse\/([A-Z][A-Z0-9_]*-\d+)/,
  condition() {
    return null;
  },

  // With no saved analysis the service may stop to ask which repository
  // (job.data.pendingChoice); the answer goes to this action.
  repoChoiceAction: "confirm-repo",

  progressSteps: [
    { id: "analysis", label: "Read the saved analysis" },
    { id: "repo", label: "Identify the repository" },
    { id: "ticket", label: "Read the ticket" },
    { id: "base", label: "Find the default branch" },
    { id: "worktree", label: "Prepare the worktree" },
    { id: "claude", label: "Open Claude Code" },
  ],

  renderPanel(job) {
    return renderTicketToPrPanel(job);
  },

  footerActions: [
    {
      id: "move-to-review",
      label: "Move ticket to review",
      hint: "Moves the Jira ticket to its review status (In Review unless you changed it in Settings) now. This also happens by itself once the pull request is open.",
      when(job) {
        return job.status === "awaiting-approval" && !job.data?.reviewMove?.moved;
      },
      async run(job, api) {
        api.rerender(await api.action("move-to-review"));
      },
    },
    // No "Continue in Claude Code" here: Start fix has already opened that
    // session, and a second click would open a duplicate terminal. To pick
    // it up later, use Resume Claude session in the Ticket workspace panel.
    {
      id: "open-terminal",
      label: "Open in Claude Code",
      when(job) {
        return job.status === "awaiting-approval" && !job.data?.claudeSession;
      },
      async run(job, api) {
        await api.action("open-terminal");
      },
    },
  ],
});
