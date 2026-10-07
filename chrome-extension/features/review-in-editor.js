// The "review-in-editor" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

PaiRegistry.register({
  site: "git",
  id: "review-in-editor",
  menuLabel: "Review PR",
  oneShot: true,

  urlPattern: (targets) => PaiGit.urlPattern(targets),

  // Deliberately no merge-conflict check (unlike resolve-conflict's condition) — this shows up on every PR,
  // conflicted or not; all it needs is the PR's own source branch name. A fork PR is not offered: its branch
  // isn't in the clone's origin to check out.
  async condition(ctx) {
    const [, project, repo, prId] = ctx.match;
    const pr = await PaiGit.pr(ctx, { project, repo, prId });
    if (!pr || !pr.fromBranch || pr.isFork === true) return null;

    return {
      project,
      repo,
      prId: Number(prId),
      sourceBranch: pr.fromBranch,
      targetBranch: pr.toBranch || undefined,
      title: pr.title || undefined,
      prUrl: PaiGit.prUrl(ctx, { project, repo, prId }),
    };
  },

  // No renderPanel/progressSteps/renderStartForm — this is the "oneShot"
  // shape (see its doc comment above): content.js's runOneShotInPanel
  // handles the whole click-to-result flow generically, and this only
  // draws the panel for each state. The review itself runs in a
  // terminal, so the panel is the summary to come back to afterwards.
  // `job.result.review` is buildReviewResult's output (companion-service
  // /features/review-in-editor/plan.js); an older service sends only
  // `job.result.summary`, shown as-is.
  renderOneShotPanel({ payload, status, job, error, session, stopping, stopError, stopSession, conflict, startWith }) {
    const review = job?.result?.review;
    const pr = { ...payload, ...(review || {}) };
    const el = (tag, className, text) => {
      const node = document.createElement(tag);
      if (className) node.className = className;
      if (text !== undefined) node.textContent = text;
      return node;
    };
    const container = el("div", "review-summary");

    const headline = el("div", "report-headline");
    if (pr.prId !== undefined && pr.prId !== null) headline.appendChild(el("span", "review-pr-id", `#${pr.prId}`));
    headline.appendChild(document.createTextNode(pr.title || "Pull request"));
    container.appendChild(headline);

    const branches = el("div", "review-branches");
    branches.append(
      el("span", "branch-chip", pr.sourceBranch),
      el("span", "review-arrow", "→"),
      el("span", "branch-chip target", pr.targetBranch || "default branch"),
    );
    container.appendChild(branches);

    const statusRow = el("div", "review-status");
    container.appendChild(statusRow);

    if (status === "running") {
      statusRow.append(
        el("span", "status-badge busy", "In progress"),
        el(
          "span",
          "review-status-text",
          payload.replaceJobId
            ? `Cancelling the other review, then checking out ${pr.sourceBranch}…`
            : `Checking out ${pr.sourceBranch} into the review worktree…`,
        ),
      );
      container.appendChild(
        el(
          "p",
          "review-hint",
          "Once it's checked out, the review opens in your editor or a new terminal window. " +
            "This panel stays open so you can come back to it.",
        ),
      );
      return container;
    }

    if (status === "failed") {
      statusRow.appendChild(el("span", "status-badge bad", "Failed"));
      container.appendChild(el("pre", "action-error", error || "Something went wrong."));
      return container;
    }

    // The repo's one review worktree is busy with another PR's review
    // (companion-service review-in-editor's buildConflict); nothing was
    // checked out.
    if (status === "conflict") {
      const other = conflict.existing.payload || {};
      const otherPr = other.prId !== undefined && other.prId !== null ? `PR #${other.prId}` : "another PR";
      statusRow.append(
        el("span", "status-badge warn", "Another review is running"),
        el("span", "review-status-text", "Nothing was checked out."),
      );
      const note = el("p", "review-hint");
      note.append(
        `A review of ${otherPr} (`,
        el("span", "branch-chip", other.sourceBranch || "unknown branch"),
        `) is in progress in ${conflict.existing.worktree}. Checking out this pull request there ` +
          "would change the files under it.",
      );
      container.appendChild(note);
      const goBtn = el("button", "approve", "Go to that review");
      goBtn.type = "button";
      goBtn.addEventListener("click", () => startWith(other));
      const replaceBtn = el("button", "reject", "Cancel it and review this PR");
      replaceBtn.type = "button";
      replaceBtn.addEventListener("click", () => {
        const ok = confirm(
          `Stop the review of ${otherPr} and check out ${pr.sourceBranch} in its place? ` +
            "Its terminal window stays open at a prompt.",
        );
        if (ok) startWith({ ...payload, replaceJobId: conflict.existing.jobId });
      });
      const actions = el("div", "actions review-actions");
      actions.append(goBtn, replaceBtn);
      container.appendChild(actions);
      return container;
    }

    if (!review) {
      statusRow.appendChild(el("span", "status-badge ok", "Done"));
      container.appendChild(el("p", "review-hint", job?.result?.summary || "Done."));
      return container;
    }

    const setStatus = (badgeClass, badge, text) => {
      statusRow.appendChild(el("span", `status-badge ${badgeClass}`, badge));
      if (text) statusRow.appendChild(el("span", "review-status-text", text));
    };
    const live = review.tracked ? session || { state: "starting" } : null;
    if (!review.reviewStarted) {
      setStatus("warn", "Checked out — review not started");
    } else if (!live) {
      setStatus("ok", `Started in ${review.editorLabel}`, "Its progress can't be followed from here.");
    } else if (live.state === "starting") {
      setStatus("busy", "Starting", "Waiting for the terminal to start the review…");
    } else if (live.state === "running") {
      setStatus("busy", "Review running", `${review.agentLabel || "The agent"} is reviewing in a terminal window.`);
    } else if (live.state === "finished") {
      setStatus(
        "ok",
        "Review finished",
        live.exitCode ? `The agent exited with code ${live.exitCode}.` : "The agent has exited.",
      );
    } else if (live.state === "cancelled") {
      setStatus("neutral", "Review cancelled", "Stopped from here; its terminal is still open in the worktree.");
    } else {
      setStatus("neutral", "Review ended", "Its terminal window was closed.");
    }
    if (review.switched) {
      container.appendChild(
        el(
          "p",
          review.switched.focused ? "review-hint" : "review-hint review-switched-warn",
          `This review was already running, so no new one was started. ${review.switched.note}`,
        ),
      );
    }

    if (live && ["starting", "running"].includes(live.state) && stopSession) {
      const cancelBtn = el("button", "reject", stopping ? "Cancelling…" : "Cancel review");
      cancelBtn.type = "button";
      cancelBtn.disabled = !!stopping || live.state !== "running";
      cancelBtn.addEventListener("click", () => {
        const ok = confirm(
          `Stop ${review.agentLabel || "the review"}? Its terminal window stays open at a prompt in the worktree.`,
        );
        if (ok) stopSession();
      });
      const actions = el("div", "actions review-actions");
      actions.appendChild(cancelBtn);
      container.appendChild(actions);
    }
    if (stopError) container.appendChild(el("pre", "action-error", stopError));

    const facts = el("dl", "review-facts");
    const fact = (label, value) => facts.append(el("dt", "", label), el("dd", "", value));
    fact("Editor", review.editorLabel || review.editor);
    if (review.agentLabel) fact("Reviewer", review.agentLabel);
    if (review.reviewStarted) {
      fact("Instructions", review.usedSkill ? "Your review-pr skill" : "Built-in review prompt");
    }
    container.appendChild(facts);
    if (review.reviewNote && !live) container.appendChild(el("p", "review-hint", review.reviewNote));

    // Same look as open-in-editor.js's widget, not the widget itself: its
    // wording is about approving a job, and its Claude Code button needs
    // a job with data.worktree.
    const box = el("div", "open-in-editor");
    box.appendChild(el("div", "open-in-editor-label", "Worktree"));
    box.appendChild(el("code", "open-in-editor-path", review.worktree));
    const actions = el("div", "open-in-editor-actions");
    // Every editor except the one this review already used.
    const editors = [
      { id: "claude-code", label: "Claude Code" },
      { id: "cursor", label: "Cursor", scheme: "cursor" },
    ].filter((editor) => editor.id !== review.editor);
    for (const editor of editors) {
      if (editor.id === "claude-code") {
        // No URL scheme: the service opens a terminal in the worktree.
        const claudeBtn = el("button", "open-in-editor-btn", "Open in Claude Code");
        claudeBtn.type = "button";
        claudeBtn.addEventListener("click", () => {
          claudeBtn.disabled = true;
          chrome.runtime.sendMessage({ type: "open-in-claude-code", jobId: job.id }, (response) => {
            claudeBtn.disabled = false;
            const message = chrome.runtime.lastError?.message || response?.error;
            if (message) alert(`Could not open Claude Code: ${message}`);
          });
        });
        actions.appendChild(claudeBtn);
        continue;
      }
      const link = el("a", "open-in-editor-btn", `Open in ${editor.label}`);
      // cursor://file<absolute-path>, as open-in-editor.js explains.
      link.href = `${editor.scheme}://file${encodeURI(review.worktree)}`;
      actions.appendChild(link);
    }
    const copyBtn = el("button", "open-in-editor-btn open-in-editor-btn-secondary", "Copy path");
    copyBtn.type = "button";
    copyBtn.addEventListener("click", async () => {
      try {
        await navigator.clipboard.writeText(review.worktree);
        copyBtn.textContent = "Copied!";
      } catch {
        copyBtn.textContent = "Select path above";
      }
      setTimeout(() => {
        copyBtn.textContent = "Copy path";
      }, 1500);
    });
    actions.appendChild(copyBtn);
    box.appendChild(actions);
    container.appendChild(box);

    const stashed = review.stashedFiles || [];
    if (stashed.length > 0) {
      const note = el("div", "review-stash");
      note.appendChild(
        el(
          "p",
          "",
          `Your uncommitted changes in the worktree (${stashed.length} file${stashed.length === 1 ? "" : "s"}) ` +
            "were stashed before checking out. Get them back with:",
        ),
      );
      const dir = /^[\w@%+=:,./-]+$/.test(review.worktree)
        ? review.worktree
        : `'${review.worktree.replace(/'/g, `'\\''`)}'`;
      note.appendChild(el("code", "", `git -C ${dir} stash pop`));
      const list = el("ul");
      for (const file of stashed) list.appendChild(el("li", "", file));
      note.appendChild(list);
      container.appendChild(note);
    }
    return container;
  },
});
