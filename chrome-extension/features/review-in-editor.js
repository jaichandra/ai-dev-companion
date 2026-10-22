// The "review-in-editor" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

// The pack's add-ons for a finished review (GET /jobs/:id/addons), as one card that keeps itself
// current while it is on the page: "Start <label>" -> "Starting <label>…" -> a link that opens in a
// new tab (+ Stop) -> the error and Retry. The service decides what is offered, so the card stays
// empty when nothing is.
function paiAddonsCard(jobId) {
  const body = document.createElement("div");
  const POLL_MS = 3000;
  const autoStarted = new Set();
  let busy = false;
  const ask = (message) =>
    new Promise((resolve, reject) =>
      chrome.runtime.sendMessage(message, (response) => {
        const failure = chrome.runtime.lastError?.message || response?.error;
        failure ? reject(new Error(failure)) : resolve(response);
      }),
    );
  const button = (label, onClick, className = "open-in-editor-btn") => {
    const b = document.createElement("button");
    b.type = "button";
    b.className = className;
    b.textContent = label;
    b.addEventListener("click", onClick);
    return b;
  };
  const op = async (addon, name) => {
    busy = true;
    try {
      await ask({ type: "addon-op", jobId, addon: addon.id, op: name });
    } catch (err) {
      alert(`${addon.label}: ${err.message}`);
    }
    busy = false;
    await refresh();
  };
  const row = (addon) => {
    const box = document.createElement("div");
    box.className = "open-in-editor-actions";
    if (addon.state === "idle") {
      box.append(button(`Start ${addon.label}`, () => op(addon, "start")));
      if (addon.autoStart && !autoStarted.has(addon.id)) {
        autoStarted.add(addon.id);
        void op(addon, "start");
      }
    } else if (addon.state === "starting") {
      const pending = document.createElement("span");
      pending.className = "open-in-editor-btn";
      pending.textContent = `Starting ${addon.label}…`;
      box.append(pending);
    } else if (addon.state === "ready") {
      const link = document.createElement("a");
      link.className = "open-in-editor-btn";
      link.textContent = `View ${addon.label}`;
      link.href = addon.href;
      link.target = "_blank";
      link.rel = "noopener noreferrer";
      box.append(link, button(`Stop ${addon.label}`, () => op(addon, "stop"), "open-in-editor-btn open-in-editor-btn-secondary"));
    } else {
      const error = document.createElement("pre");
      error.className = "action-error";
      error.textContent = addon.error || "Failed.";
      box.append(error, button("Retry", () => op(addon, "start")));
    }
    return box;
  };
  async function refresh() {
    if (!body.isConnected && body.dataset.shown) return;
    try {
      const { addons } = await ask({ type: "addons-list", jobId });
      body.replaceChildren(...addons.map(row));
      body.dataset.shown = "1";
      body.style.marginBottom = addons.length ? "20px" : "0";
    } catch {
      // The service may be restarting; the next poll tries again.
    }
  }
  (function tick() {
    if (body.dataset.shown && !body.isConnected) return;
    if (!busy) void refresh();
    setTimeout(tick, POLL_MS);
  })();
  return body;
}

PaiRegistry.register({
  site: "git",
  id: "review-in-editor",
  menuLabel: "Review PR",
  icon: "code",
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
    const target = pr.targetBranch || "default branch";
    const subtitle = [pr.prId !== undefined && pr.prId !== null ? `PR #${pr.prId}` : "Pull request", `${pr.sourceBranch} → ${target}`].join(" · ");
    // What the panel says is going on: the header's tag and the status row's icon, title and tag.
    const result = (tone, tag, icon, title, sub) => ({ tone, tag, icon, title, sub });
    // Loose nodes under the cards (buttons, the worktree box, notes) get the cards' spacing.
    const loose = (node) => {
      node.style.marginBottom = "20px";
      return node;
    };
    const prCard = () =>
      paiCard("Checkout", "folder", [
        paiRow({ icon: "copy_replicate", tone: "info", title: pr.title || "Pull request", sub: pr.prId !== undefined && pr.prId !== null ? `PR #${pr.prId}` : "" }),
        paiRow({ icon: "view_tree", tone: "info", title: pr.sourceBranch, sub: `Source branch → ${target}` }),
        review
          ? paiRow({ icon: "folder", tone: "info", title: review.worktree, sub: "Review worktree", tag: "Ready", tagTone: "ok" })
          : null,
      ]);
    const foot = "The branch is checked out into its own worktree. Your clone is not touched.";
    const page = (st, ...rest) =>
      paiPage(
        [
          prCard(),
          paiCard("Review", "play", [paiRow({ icon: st.icon, tone: st.tone === "neutral" ? "mute" : st.tone === "busy" ? "info" : st.tone, title: st.title, sub: st.sub, tag: st.tag, tagTone: st.tone })]),
          ...rest,
        ],
        foot,
      );

    if (status === "running") {
      const st = result(
        "busy",
        "In progress",
        "refresh",
        payload.replaceJobId ? `Cancelling the other review, then checking out ${pr.sourceBranch}…` : `Checking out ${pr.sourceBranch} into the review worktree…`,
        "Once it's checked out, the review opens in your editor or a new terminal window. This panel stays open so you can come back to it.",
      );
      return { node: page(st), header: { subtitle, tag: "Checking out", tone: "busy" } };
    }

    if (status === "failed") {
      const st = result("bad", "Failed", "error", "The review couldn't start", "");
      return { node: page(st, loose(el("pre", "action-error", error || "Something went wrong."))), header: { subtitle, tag: "Failed", tone: "bad" } };
    }

    // The repo's one review worktree is busy with another PR's review
    // (companion-service review-in-editor's buildConflict); nothing was
    // checked out.
    if (status === "conflict") {
      const other = conflict.existing.payload || {};
      const otherPr = other.prId !== undefined && other.prId !== null ? `PR #${other.prId}` : "another PR";
      const st = result(
        "warn",
        "Another review is running",
        "notice_triangle",
        "Nothing was checked out",
        `A review of ${otherPr} (${other.sourceBranch || "unknown branch"}) is in progress in ${conflict.existing.worktree}. Checking out this pull request there would change the files under it.`,
      );
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
      const actions = loose(el("div", "actions review-actions"));
      actions.append(goBtn, replaceBtn);
      return { node: page(st, actions), header: { subtitle, tag: "Waiting", tone: "warn" } };
    }

    if (!review) {
      const st = result("ok", "Done", "success", job?.result?.summary || "Done.", "");
      return { node: page(st), header: { subtitle, tag: "Done", tone: "ok" } };
    }

    const live = review.tracked ? session || { state: "starting" } : null;
    let st;
    if (!review.reviewStarted) {
      st = result("warn", "Not started", "notice_triangle", "Checked out — review not started", review.reviewNote && !live ? review.reviewNote : "");
    } else if (!live) {
      st = result("ok", "Started", "success", `Started in ${review.editorLabel}`, "Its progress can't be followed from here.");
    } else if (live.state === "starting") {
      st = result("busy", "Starting", "refresh", "Starting", "Waiting for the terminal to start the review…");
    } else if (live.state === "running") {
      st = result("busy", "Running", "play", "Review running", `${review.agentLabel || "The agent"} is reviewing in a terminal window.`);
    } else if (live.state === "finished") {
      st = result("ok", "Finished", "success", "Review finished", live.exitCode ? `The agent exited with code ${live.exitCode}.` : "The agent has exited.");
    } else if (live.state === "cancelled") {
      st = result("neutral", "Cancelled", "close", "Review cancelled", "Stopped from here; its terminal is still open in the worktree.");
    } else {
      st = result("neutral", "Ended", "info", "Review ended", "Its terminal window was closed.");
    }

    const rest = [];
    if (review.switched) {
      rest.push(
        loose(
          el(
            "p",
            review.switched.focused ? "review-hint" : "review-hint review-switched-warn",
            `This review was already running, so no new one was started. ${review.switched.note}`,
          ),
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
      const actions = loose(el("div", "actions review-actions"));
      actions.appendChild(cancelBtn);
      rest.push(actions);
    }
    if (stopError) rest.push(loose(el("pre", "action-error", stopError)));

    const facts = [paiRow({ icon: "code", tone: "info", title: review.editorLabel || review.editor, sub: "Editor" })];
    if (review.agentLabel) facts.push(paiRow({ icon: "AI", tone: "info", title: review.agentLabel, sub: "Reviewer" }));
    if (review.reviewStarted) {
      facts.push(paiRow({ icon: "file_search", tone: "info", title: "Uses your review skills if you have any", sub: "Instructions" }));
    }
    rest.push(paiCard("Setup", "settings", facts));

    // Same look as open-in-editor.js's widget, not the widget itself: its
    // wording is about approving a job, and its Claude Code button needs
    // a job with data.worktree.
    const box = loose(el("div", "open-in-editor"));
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
    rest.push(box);

    if (review.reviewStarted !== false && job?.id) rest.push(paiAddonsCard(job.id));

    const stashed = review.stashedFiles || [];
    if (stashed.length > 0) {
      const note = loose(el("div", "review-stash"));
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
      rest.push(note);
    }
    return { node: page(st, ...rest), header: { subtitle, tag: st.tag, tone: st.tone } };
  },
});
