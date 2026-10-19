// The "address-review-comments" feature of the ✨ menu: its helpers and its registry entry (the contract is in registry.js).

/** Directories that run hooks or CI at commit/push/CI time — matched
 * anywhere in the path (a nested package's .husky/ counts too), not just
 * at the repo root. `.yarn/` holds Yarn's plugins and releases, which Yarn
 * executes whenever a hook or script invokes it. */
const RISKY_CHANGE_PATH_DIRS = [".husky/", ".githooks/", ".git-hooks/", ".github/workflows/", ".yarn/"];

/** Filenames that matter by name alone, wherever they live (package.json's
 * scripts run via npm install/prepare; Jenkinsfile/Makefile run at CI or
 * build time; lefthook's config defines git hooks; .npmrc can point npm at
 * a different registry or script shell). */
const RISKY_CHANGE_PATH_EXACT_NAMES = [
  "package.json",
  ".pre-commit-config.yaml",
  "Jenkinsfile",
  "Makefile",
  "lefthook.yml",
  "lefthook.yaml",
  ".lefthook.yml",
  ".lefthook.yaml",
  ".npmrc",
];

/** Basename prefixes for configs that may carry any of several extensions
 * — and that commit hooks commonly load and EXECUTE (a `.js`/`.cjs`/`.mjs`
 * config is code): lint-staged, Yarn, ESLint, Prettier, Babel, Jest and
 * commitlint. */
const RISKY_CHANGE_PATH_NAME_PREFIXES = [
  ".lintstagedrc",
  "lint-staged.config.",
  ".yarnrc",
  ".eslintrc",
  "eslint.config.",
  ".prettierrc",
  "prettier.config.",
  ".babelrc",
  "babel.config.",
  "jest.config.",
  "commitlint.config.",
  ".commitlintrc",
];

/**
 * True if `filePath` (a repo-relative path, as core/diff.js's
 * computeFileDiffs returns it) is on a path that runs at commit, push or
 * CI time — a git hook, a hook-loaded tool config, a CI workflow, or a
 * script a package manager or build tool invokes automatically. The
 * approve step runs `git commit` with hooks on and unsandboxed (see
 * companion-service/features/address-review-comments/index.ts), so a
 * human reviewer needs to notice a change here before clicking Approve.
 * This is a best-effort list of the common places, not a guarantee: a
 * hook can run any file the change touches (a test, a script it imports),
 * so a path not matched here isn't thereby safe.
 */
function isRiskyChangePath(filePath) {
  const normalized = String(filePath || "").replace(/^\.\//, "");
  const base = normalized.slice(normalized.lastIndexOf("/") + 1);
  if (RISKY_CHANGE_PATH_EXACT_NAMES.includes(base)) return true;
  if (RISKY_CHANGE_PATH_NAME_PREFIXES.some((prefix) => base.startsWith(prefix))) return true;
  return RISKY_CHANGE_PATH_DIRS.some(
    (dir) => normalized === dir.slice(0, -1) || normalized.startsWith(dir) || normalized.includes(`/${dir}`),
  );
}

/** Ruled default for the per-comment "Reply on push" checkbox: ticked by
 * default only for a fixed comment, since the fix itself is the reply's
 * substance and needs no further review; declined and needs-discussion
 * start unticked, since those carry Claude's own wording on a point the
 * human reviewer should read and decide to send, not one ticked for them. */
function defaultReplyChecked(action) {
  return action === "fixed";
}

/** "path:line" for an anchored comment, the bare path when Bitbucket gave
 * no line, or a label for a general (unanchored) PR comment. */
function reviewCommentLocation(anchor) {
  if (!anchor || !anchor.path) return "General comment";
  return typeof anchor.line === "number" ? `${anchor.path}:${anchor.line}` : anchor.path;
}

/** Row-order groups for the review-comments table, first to last. A
 * comment whose action isn't one of these (no report entry at all, or an
 * action outside REVIEW_ACTION_BADGE's three) sorts after all of them. */
const REVIEW_COMMENT_SORT_ORDER = ["fixed", "needs-discussion", "declined"];

/**
 * A copy of `comments` (never the input array, and never job.data.comments
 * itself), reordered by each comment's report action per
 * REVIEW_COMMENT_SORT_ORDER, with unreported/unknown actions last. `reportById`
 * is the same commentId → report-entry map renderPanel already builds. Uses
 * Array#sort's guaranteed stability so that within one action group, rows
 * keep the order comments arrived in (BLOCKER first, then path, then line —
 * the server's own order).
 */
function sortReviewCommentsByAction(comments, reportById) {
  const rank = (comment) => {
    const index = REVIEW_COMMENT_SORT_ORDER.indexOf(reportById.get(comment.id)?.action);
    return index === -1 ? REVIEW_COMMENT_SORT_ORDER.length : index;
  };
  return [...comments].sort((a, b) => rank(a) - rank(b));
}

/**
 * Splits `files` (job.result.files — the same FileDiff array the
 * address-review-comments panel renders at its bottom today) between the
 * ones to show inline under a comment row and the ones left over.
 *
 * A comment attaches to a file only when its `anchor.path` exactly matches
 * that file's `path` — the same field the bottom section has always read.
 * A comment with no anchor, or whose path matches no file, contributes no
 * entry to `byCommentId` (ruled: no diff row for it). Several comments
 * naming the same file each get their own entry pointing at that same
 * FileDiff, since each renders its own independent, separately-collapsed
 * copy. `others` holds every file not claimed by any comment, in its
 * original order, for the "leftover" section below the table.
 *
 * Pure and DOM-free so it can be unit-exercised on its own.
 */
function splitDiffsByComment(comments, files) {
  const fileByPath = new Map(files.map((file) => [file.path, file]));
  const claimedPaths = new Set();
  const byCommentId = new Map();
  for (const comment of comments) {
    const path = comment.anchor?.path;
    if (!path) continue;
    const file = fileByPath.get(path);
    if (!file) continue;
    byCommentId.set(comment.id, file);
    claimedPaths.add(path);
  }
  const others = files.filter((file) => !claimedPaths.has(file.path));
  return { byCommentId, others };
}

/**
 * Whether the address-review-comments menu row should show for `pr` (the
 * raw `GET .../pull-requests/{id}` JSON), and if so, the payload to POST.
 * `ids` is `{project, repo, prId}` straight from the URL match.
 *
 * On Bitbucket Server/DC up to ~9.x, this single-PR GET's `properties`
 * carries `openTaskCount`/`commentCount`, which is enough to skip listing
 * the feature on a PR with nothing open. On 9.4.16 (confirmed against a
 * real instance), that GET omits `properties` entirely — only the list
 * endpoint's `withProperties=true` returns it, per-PR, and there's no
 * single-PR equivalent. So: when either count comes back as a number,
 * gate on it as before; when neither does (this GET's `properties` is
 * missing, or present without them), show the menu unconditionally rather
 * than treating "we don't know" as zero. companion-service's own
 * collectOpenComments (features/address-review-comments/plan.js) already
 * handles "actually nothing to address" by failing the job with a clear
 * message (index.ts), so showing the row when we can't tell costs nothing
 * worse than one extra click.
 */
function reviewCommentsMenuPayload(pr, ids) {
  if (!pr || typeof pr !== "object") return null;
  // A merged or declined PR has nothing to push to; the service fails the
  // job outright (index.ts), which used to leave a failed run on the menu.
  if (typeof pr.state === "string" && pr.state !== "OPEN") return null;
  // The service works in a clone of the target repository, so a fork PR is refused there; don't offer it.
  if (pr.isFork === true) return null;
  const openTaskCount = typeof pr.openTaskCount === "number" ? pr.openTaskCount : null;
  const commentCount = typeof pr.commentCount === "number" ? pr.commentCount : null;
  const knowsCounts = openTaskCount !== null || commentCount !== null;
  if (knowsCounts && !((openTaskCount || 0) > 0 || (commentCount || 0) > 0)) {
    return null;
  }
  return { project: ids.project, repo: ids.repo, prId: Number(ids.prId) };
}

/** Action → status-badge text/tone, matching plan.js's ACTIONS. A comment
 * with no report entry (Claude's report didn't cover it, or didn't parse
 * at all) shows as unreported rather than guessing. */
const REVIEW_ACTION_BADGE = {
  fixed: { text: "Fixed", tone: "ok" },
  declined: { text: "Declined", tone: "neutral" },
  "needs-discussion": { text: "Needs discussion", tone: "warn" },
};

PaiRegistry.register({
  site: "git",
  id: "address-review-comments",
  settingsGroups: ["git"],
  describePendingStart: PaiGit.prSummary,
  menuLabel: "Address review comments",
  panelTitle: "Review comments",
  icon: "feedback",
  quietCompleted: true,
  // While running, the service may stop to ask where to work when the branch
  // is checked out in the main clone (job.data.pendingChoice).
  repoChoiceAction: "confirm-repo",
  progressSteps: [
    { id: "fetch", label: "Fetch the pull request" },
    { id: "comments", label: "Collect open review comments" },
    { id: "address", label: "Address comments with Claude" },
    { id: "diff", label: "Prepare diff for review" },
  ],
  cancellable: true,

  urlPattern: (targets) => PaiGit.urlPattern(targets),

  // Same PR, same scopeKey shape as resolve-conflict above — see its comment.
  scopeKey(match) {
    return PaiGit.scopeKey(match);
  },

  // One read of the PR itself, never its comments (that's the companion service's job, once started, and
  // crawling them here just to decide whether to show a menu row would be a second round trip for no
  // benefit). On Bitbucket that is the PR's own JSON with `?withProperties=true` — harmless if ignored, which
  // is what a real 9.4.16 instance does: its GET .../pull-requests/{id} has no `properties` key at all (only
  // the LIST endpoint returns one). reviewCommentsMenuPayload (above) decides from whatever comes back whether
  // to show the row. Any failure (network, non-2xx, bad JSON) just hides the feature for this refresh.
  async condition(ctx) {
    const [, project, repo, prId] = ctx.match;
    const pr = await PaiGit.pr(ctx, { project, repo, prId }, { properties: true });
    return reviewCommentsMenuPayload(pr, { project, repo, prId });
  },

  renderPanel(job) {
    const el = (tag, className, text) => {
      const node = document.createElement(tag);
      if (className) node.className = className;
      if (text !== undefined) node.textContent = text;
      return node;
    };

    // companion-service/features/address-review-comments/index.ts's
    // JobData: comments is the open root comments Claude was asked to
    // address; report is Claude's parsed per-comment verdict (or null
    // if it didn't return a readable one); replies is the server-built
    // reply text per report entry — never re-derived here (ruled).
    const reportById = new Map((job.data?.report || []).map((entry) => [entry.commentId, entry]));
    const repliesById = new Map((job.data?.replies || []).map((reply) => [reply.commentId, reply]));
    // Sorted copy for display only — job.data.comments itself (the
    // server's own order) is left untouched.
    const comments = sortReviewCommentsByAction(job.data?.comments || [], reportById);
    // Read by getApprovalPayload below, once per row that has a
    // checkbox at all (a comment with no replies entry gets none).
    const checkboxByCommentId = new Map();

    const files = job.result?.files || [];
    // Which file (if any) each comment's own collapsed diff should show, and which files are left for the
    // "Other changed files" card — see splitDiffsByComment's own doc comment for the matching rule.
    const { byCommentId: diffsByCommentId, others: otherFiles } = splitDiffsByComment(comments, files);

    const prId = job.data?.payload?.prId;
    const prTitle = job.data?.pr?.title;
    const open = comments.length;
    const header = {
      subtitle: [prId ? `PR #${prId}` : "", prTitle || ""].filter(Boolean).join(" · "),
      tag: job.status === "failed" ? "Couldn't finish" : open > 0 ? `${open} addressed` : "No comments",
      tone: job.status === "failed" ? "bad" : open > 0 ? "warn" : "neutral",
    };

    const rows = comments.map((comment) => {
      const reportEntry = reportById.get(comment.id);
      const badgeInfo = reportEntry ? REVIEW_ACTION_BADGE[reportEntry.action] : null;
      const extra = [];
      if (reportEntry?.note) extra.push(el("div", "pai-row-sub", reportEntry.note));

      // Ruled: no replies entry for this comment (e.g. report is null) means no checkbox and no reply —
      // nothing to tick, and nothing to show as "the exact text that will be posted".
      const replyEntry = repliesById.get(comment.id);
      if (replyEntry) {
        const toggle = el("label", "reply-toggle");
        const checkbox = document.createElement("input");
        checkbox.type = "checkbox";
        checkbox.checked = defaultReplyChecked(replyEntry.action);
        checkboxByCommentId.set(comment.id, checkbox);
        toggle.appendChild(checkbox);
        toggle.appendChild(document.createTextNode("Reply on push"));
        extra.push(toggle);
        // Read-only (ruled) — the server-built text, shown verbatim, never edited here.
        extra.push(el("pre", "reply-text", replyEntry.text));
      }

      const diffFile = diffsByCommentId.get(comment.id);
      if (diffFile) {
        // Collapsed disclosure labelled with the file this diff belongs to; lazy (ruled): the diff itself is
        // only built the first time it is opened.
        const details = document.createElement("details");
        details.className = "row-detail";
        details.appendChild(el("summary", null, `Show diff — ${diffFile.path}`));
        let diffRendered = false;
        details.addEventListener("toggle", () => {
          if (!details.open || diffRendered) return;
          diffRendered = true;
          if (typeof diffFile.note === "string" && diffFile.note) details.appendChild(el("p", "file-diff-note", diffFile.note));
          details.appendChild(window.renderDiff(diffFile.diff));
        });
        extra.push(details);
      }

      return paiRow({
        icon: "chat",
        tone: badgeInfo ? (badgeInfo.tone === "neutral" ? "mute" : badgeInfo.tone) : "mute",
        title: comment.text || "",
        sub: [reviewCommentLocation(comment.anchor), comment.authorName || comment.authorSlug || "Unknown"].join(" · "),
        tag: badgeInfo ? badgeInfo.text : "No report",
        tagTone: badgeInfo ? badgeInfo.tone : "neutral",
        extra,
      });
    });

    // Same gate and widget as resolve-conflict's renderPanel: only while there's still a worktree on disk to
    // open, and claudeSession swaps its button for "Continue in Claude Code" once set.
    let worktreeBox = null;
    const worktreeDir = job.data?.worktree?.dir;
    if (worktreeDir && (job.status === "awaiting-approval" || job.status === "failed")) {
      worktreeBox = window.renderOpenInEditor(worktreeDir, job.id, { claudeSession: job.data?.claudeSession, editor: job.data?.editor });
    }

    // Ruled: a banner above the diffs for any changed file on a hook, CI, tool-config or script path — approve
    // runs `git commit` with hooks on and unsandboxed. It's a pointer to the likeliest places, not a safety
    // verdict, and says so: a hook can execute any changed file, so the diff as a whole still needs reviewing.
    let banner = null;
    const riskyPaths = files.map((file) => file.path).filter(isRiskyChangePath);
    if (riskyPaths.length > 0) {
      banner = el("div", "risky-paths-banner");
      banner.appendChild(
        el(
          "p",
          null,
          "Approving runs this repo's commit hooks unsandboxed, and a hook may execute any changed file. " +
            "This diff changes hook, CI, tool-config or script files that commonly run then — review them " +
            "closely. (No banner doesn't mean a change is safe.)",
        ),
      );
      const list = el("ul");
      for (const path of riskyPaths) list.appendChild(el("li", null, path));
      banner.appendChild(list);
    }

    // otherFiles is `files` minus whatever renders under a comment above (splitDiffsByComment) — never
    // re-filtered here. Ruled: when nothing's left, the card is omitted rather than shown empty.
    const fileNodes = otherFiles.map((file) => {
      const details = document.createElement("details");
      details.open = diffsByCommentId.size === 0;
      details.className = "row-detail";
      details.appendChild(el("summary", null, file.path));
      if (typeof file.note === "string" && file.note) details.appendChild(el("p", "file-diff-note", file.note));
      details.appendChild(window.renderDiff(file.diff));
      return details;
    });
    const fileCard = paiCard(diffsByCommentId.size > 0 ? "Other changed files" : "Changed files", "edit", fileNodes);

    for (const loose of [worktreeBox, banner]) if (loose) loose.style.marginBottom = "20px";
    const cards = [
      paiProseCard("Summary", "light_bulb", job.result?.summary),
      paiCard("Review comments", "chat", rows),
      worktreeBox,
      banner,
      fileCard,
    ];
    if (!fileCard && files.length === 0 && !job.error) cards.push(paiProseCard("Changes", "edit", "No file changes to review."));
    if (job.error) cards.push(el("p", "error", job.error));

    return {
      node: paiPage(cards, "Changes are made in a worktree. Nothing is pushed until you approve."),
      header,
      // Approve's body (companion-service/features/address-review-comments/
      // index.ts's approve(): ctx.body.replies): only the ticked rows,
      // each with the exact text the table showed — the service itself
      // re-validates commentId against job.data.comments and trims/caps
      // the text, so nothing here needs to duplicate that.
      getApprovalPayload() {
        const replies = [];
        for (const [commentId, checkbox] of checkboxByCommentId) {
          if (!checkbox.checked) continue;
          const entry = repliesById.get(commentId);
          if (entry) replies.push({ commentId, text: entry.text });
        }
        return { replies };
      },
    };
  },

  // Same footerAction as resolve-conflict's Refresh diff, reused
  // verbatim. Unlike analyze-issue, this feature has a worktree box
  // (the renderOpenInEditor widget above), and that widget's own button
  // already turns into "Continue in Claude Code" once job.data.claudeSession
  // is set — so, same as resolve-conflict, there's no separate
  // footerActions entry for it here.
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
