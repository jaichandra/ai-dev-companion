// The "resolve-conflict" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

PaiRegistry.register({
  site: "git",
  id: "resolve-conflict",
  settingsGroups: ["git"],
  describePendingStart: PaiGit.prSummary,
  menuLabel: "Resolve Conflict",
  quietCompleted: true,
  progressSteps: [
    { id: "fetch", label: "Fetch & prepare workspace" },
    { id: "merge", label: "Merge destination branch" },
    { id: "resolve", label: "Resolve conflicts with Claude" },
    { id: "verify", label: "Verify resolution" },
    { id: "diff", label: "Prepare diff for review" },
  ],
  cancellable: true,
  cancelConfirm: "The merge in progress is discarded — nothing has been pushed.",

  // Which git host this is (Bitbucket or GitHub) decides what a PR page's address looks like and how the PR is
  // read; see git-host.js.
  urlPattern: (targets) => PaiGit.urlPattern(targets),

  // Built straight from urlPattern's own captures rather than the (network-backed) condition payload below —
  // a lookup has to work before condition has even run. Same string core/prereqs.js's prKey makes.
  scopeKey(match) {
    return PaiGit.scopeKey(match);
  },

  // Listed only when the PR conflicts with its target. A fork PR is not offered: the companion resolves
  // conflicts in a clone of the target repository and pushes the source branch back to it.
  async condition(ctx) {
    const [, project, repo, prId] = ctx.match;
    const pr = await PaiGit.pr(ctx, { project, repo, prId }, { conflicts: true });
    if (!pr || !pr.conflicted || pr.isFork === true) return null;
    return {
      project,
      repo,
      prId: Number(prId),
      sourceBranch: pr.fromBranch,
      destBranch: pr.toBranch,
    };
  },

  renderPanel(job) {
    const container = document.createElement("div");

    const summary = document.createElement("p");
    summary.textContent = job.result?.summary || "";
    container.appendChild(summary);

    // job.data is this feature's own (opaque-to-the-core) payload — see
    // companion-service/features/resolve-conflict/index.ts's JobData —
    // so it's fine for this feature's own renderPanel to read
    // job.data.worktree.dir directly. Only offer it while the worktree
    // still exists on disk: gone once approve()/reject() has run, which
    // is exactly why this is gated to the two statuses where the panel
    // still shows Approve/Discard at all.
    const worktreeDir = job.data?.worktree?.dir;
    if (worktreeDir && (job.status === "awaiting-approval" || job.status === "failed")) {
      // job.data.claudeSession (set once this resolution's headless
      // Claude run has finished — core/jobs.ts's ClaudeSession) turns
      // the widget's button into "Continue in Claude Code" (see
      // open-in-editor.js). Its mere presence doesn't mean the session
      // is STILL resumable, though — whether the worktree/transcript it
      // points at are still on disk is only checked server-side when you
      // actually click it (core/resume.js), which shows a message
      // instead of opening a terminal if it's since expired. Passing it
      // whether or not it's set lets that widget decide, the same way it
      // already decides everything else about the button from opts.
      container.appendChild(window.renderOpenInEditor(worktreeDir, job.id, { claudeSession: job.data?.claudeSession }));
    }

    const files = job.result?.files || [];
    if (files.length === 0 && !job.error) {
      const none = document.createElement("p");
      none.textContent = "No file changes to review.";
      container.appendChild(none);
    }

    for (const file of files) {
      const details = document.createElement("details");
      details.open = true;
      const summaryEl = document.createElement("summary");
      summaryEl.textContent = file.path;
      details.appendChild(summaryEl);
      // Optional per-file explanation of a merge call Claude made while
      // resolving this file (e.g. which branch's version it kept) —
      // companion-service/features/resolve-conflict/index.ts may set
      // this on a FileDiff. Rendered as a callout above the diff, same
      // look as review-in-editor's stash note; textContent only, since
      // this string comes straight from the server.
      if (typeof file.note === "string" && file.note) {
        const note = document.createElement("p");
        note.className = "file-diff-note";
        note.textContent = file.note;
        details.appendChild(note);
      }
      // window.renderDiff comes from diff-viewer.js (loaded before this
      // file — see manifest.json) and builds a side-by-side, color-coded
      // table rather than a flat <pre> block.
      details.appendChild(window.renderDiff(file.diff));
      container.appendChild(details);
    }

    if (job.error) {
      const err = document.createElement("p");
      err.className = "error";
      err.textContent = job.error;
      container.appendChild(err);
    }

    return container;
  },

  // Refresh diff: for a job the user has been sitting on (see it, poke
  // at the worktree via "Continue in Claude Code" above, come back
  // later), re-reads the current working tree instead of the stale
  // snapshot taken when the job first landed here. Only meaningful in
  // the two statuses where there's still a worktree at all — same gate
  // renderPanel's Open/Continue widget above uses. content.js's
  // showPanel renders this next to Approve/Discard (or alone, on a
  // failed job where Approve doesn't show).
  footerActions: [
    {
      id: "refresh-diff",
      label: "Refresh diff",
      when(job) {
        return job.status === "awaiting-approval" || job.status === "failed";
      },
      async run(job, api) {
        const { job: updatedJob } = await api.send("refresh-diff");
        api.rerender(updatedJob);
      },
    },
  ],
});
