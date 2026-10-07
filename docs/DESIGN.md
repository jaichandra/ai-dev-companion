# ai-dev-companion — technical design

This is the design of the framework behind the AI Dev Companion: the companion service, the Chrome extension and the Claude Code plugin template. It is written against the framework's placeholder profile; a **distribution** (its own repo) supplies an environment profile and optional packs for its own servers, and documents those itself. Phase and task names below are from the build history.

How the extension and companion service work, for anyone changing or extending them. For installing and using it, see the [README](../README.md).

- [Architecture](#architecture)
- [How each feature works](#how-each-feature-works)
- [The companion service](#the-companion-service)
- [Developing](#developing)
- [Why "merge master in," not rebase](#why-merge-master-in-not-rebase)
- [Extending it — adding a new feature](#extending-it--adding-a-new-feature)
- [Security notes](#security-notes)
- [Known limitations / open items](#known-limitations--open-items)
- [Verification performed](#verification-performed)

## Architecture

A Chrome extension (MV3) cannot run `git`, touch the filesystem, or spawn
processes — that's a sandbox boundary, not a missing permission. So:

- **`chrome-extension/`** — runs in the browser, decides which action(s)
  apply to the current page, shows the button, renders the review panel.
- **`companion-service/`** — a small Node service that runs on your machine
  (`127.0.0.1` only) and does the actual git/Claude/Jira work.

They talk over `http://127.0.0.1:<port>`, and only when the request carries
a shared secret only this extension knows — see **Security notes** below.
The companion also answers `POST /mcp`, a separate route with its own
bearer token, so Claude Code and Cursor in a terminal can drive a subset of
the same features as MCP tools — see **The MCP server**.

## Environment profile, packs and distributions

The framework knows how to do its jobs against *a* git host, issue tracker and CI
server; which ones, and what else a team wants, is a **distribution**: a separate
repo that depends on this one and adds files. Three things carry that.

- **The environment profile** (`companion-service/environment.js`, written with
  `defineEnvironment` from `core/environment-base.js`; the framework ships a
  placeholder pointing at `example.com`). `sites` lists the external systems:
  `{id, kind: "git" | "issues" | "ci", provider, label, baseUrl}`, where `id` is
  also the `config.json` key and the credential prefix (`<id>.apiToken`), and
  `provider` names the client implementation (`bitbucket-dc` or `github` for git,
  `jira-dc`, `jenkins`). A site may be `withUsername` (Jenkins' basic auth),
  `tokenOnly` (no browser login the companion can use — GitHub — so it is left
  out of the extension's host permissions and never has a session relayed) and may
  narrow where the extension runs with `pageMatches` (patterns starting
  `{origin}/`, e.g. `{origin}/*/*/pull/*`). The profile also holds the update
  source, the LLM proxy defaults, the default Jira projects and issue-type
  settings, the Settings panel's branding (`branding.supportEmail`,
  `branding.llmProxy`), and the packs to load. `config.json` overrides it.
- **Packs** (`core/packs.js`). A pack lists features and may contribute
  `siteSettings` (extra keys on a site's Settings), `targets` (data for
  `GET /targets`), `externalTokens` (tokens another tool already holds, used only
  for the host they name), `diagnose` (a richer way to diagnose a build),
  `doctorChecks` and `mcpDenyList`. A feature is a folder with an `index.ts` and a
  `feature.js` declaration (setup descriptor, factory thunk, MCP tools, scope-key
  rule, `persist`, `history` flags, and the extension script that registers its
  ✨ row). `packs/builtin.js` lists the framework's own.
- **Assembling** (`scripts/assemble.js`). `assemble({frameworkDir, rootDir,
  overlays, dest, version})` lays the framework's `companion-service/`,
  `chrome-extension/` and `plugin/`, the distribution's root files and its overlay
  folders (its profile replaces the placeholder) into one tree, stamps the
  distribution's version on the service, the extension manifest and the plugin
  manifest, and regenerates the manifest. The installer mirrors that tree into the
  install folder and a distribution's tests run in it, so what is tested is what
  is installed. `core/extension-manifest.js` generates `manifest.json` (hosts from
  the sites and `config.json` base URLs, scripts from the packs' features in menu
  order); `setup.js` rewrites it, `doctor` checks it, and saving a base URL in
  Settings regenerates it and asks for an extension reload.

`examples/minimal-env` is a distribution profile with no code of its own (GitHub,
Jira, Jenkins) with a test that assembles it with the framework.

## How each feature works

The extension adds one floating **✨** button, fixed to the bottom-right
corner, to any covered page where at least one feature currently applies.

**Resolve Conflict** (Bitbucket) — shown only when Bitbucket flags the PR as
actually conflicted:

1. The service fetches, creates a disposable worktree off the mapped
   clone, and merges the destination branch in. The ✨ button turns into a
   live checklist (fetch → merge → resolve → verify → prepare diff) so you
   can see what's actually happening, not just a generic spinner — and it
   survives a page refresh: the in-flight job is tracked in
   `chrome.storage.local`, so reloading the PR page resumes watching it
   instead of losing track (or letting you accidentally start a duplicate).


2. If there's an actual conflict, it runs `claude -p ...` (no `--model`:
   Claude Code's own default; only a feature's own model setting adds one) headlessly inside that worktree to resolve it, capturing
   its session (`job.data.claudeSession` — see **Session capture and the
   resume route** below) so it can be resumed later.
3. A panel slides in from the right showing every file that differs from
   **both** merge parents plus every file that had conflict markers, side
   by side with color highlighting (added/removed/unchanged) — a
   conflicted file resolved wholesale to one side (its content matches
   one parent exactly) gets a note explaining which side was kept and
   which side's changes were dropped, so that kind of resolution is never
   silently invisible. If you're not sure whether to approve, the panel
   also has **Open in Cursor** and **Open in Claude Code**/**Continue in
   Claude Code** (opens a real terminal, `cd`'d into the worktree; resumes
   the same Claude session once one exists) plus a **Copy Path** fallback for
   any other editor. **Refresh diff** re-renders the panel from the
   worktree's current state — for when you (or Claude Code, via Continue)
   changed something there since the panel last rendered.


4. **Approve & Push** stages the whole worktree, fingerprints it
   (`git write-tree`), and refuses — asking you to click **Refresh diff**
   first — if that fingerprint doesn't match the one the currently
   rendered panel was built from, so nothing gets pushed that wasn't
   actually reviewed. It then commits (skipping the commit if a terminal
   session already committed the merge itself — the message becomes
   "Resolve review edits for PR #N" for edits made after that terminal
   commit) and does a normal `git push` (no force — see *why merge, not
   rebase* below) to the PR's source branch, or reports "Nothing to push"
   for a merge that was already up to date. **Discard** throws everything
   away; nothing is pushed, and the worktree is cleaned up. A **failed**
   resolution (e.g. Claude couldn't fully resolve a conflict) gets the
   same open-in-editor/Discard/Refresh diff options, so you always have a
   way to inspect or clean up rather than being stuck. A per-job lock
   serializes Refresh diff and Approve so the two can never interleave on
   the same job. (This staging/fingerprinting/locking plumbing — plus
   whether a job may still be discarded — now lives in
   `core/reviewed-push.ts`/`core/reviewed-push-policy.js`, shared with
   **Address review comments** below.)

**Create Subtasks** (Jira) — shown only on a **Story** or **Epic** (a
Sub-task can never itself be typed Story/Epic, so that exclusion comes for
free). It's a repeatable "add more subtasks" tool, not a one-time setup
step, so it stays available even on a ticket that already has some — unlike
the standalone `~/.local/bin/create-jira-subtasks` script this was
originally ported from (still there, unchanged, for terminal use, with its
own fixed Implement→Deliver ladder), this version is a fully generic panel:


1. Click it to open the overlay panel with a row of **subtask name** /
   **assignee** fields. Add up to 10 rows, remove any but the last.


2. The assignee field searches Jira as you type — same
   `/rest/api/2/user/picker` endpoint Jira's own Assignee field calls, doing
   real substring matching on display name, username, or email, not just a
   username prefix. Arrow keys move through the results (top match
   pre-highlighted), Enter selects, or just type a raw SSO ID directly and
   ignore the dropdown.


3. **Create Subtasks** creates every row for real — there's no separate
   preview/approve step here (unlike Resolve Conflict above): the form
   itself is the review, since you typed exactly what should exist. On
   success the panel closes and the page reloads so Jira's own subtask list
   shows what was just created; on failure (e.g. an invalid assignee) the
   panel stays open with the error and whichever rows *did* get created
   named, so a retry isn't blind. **Close** (next to the action button, or
   the × in the corner) cancels without creating anything.

**Review in editor** (Bitbucket) — shown on every PR, conflicted or not; a
single click, with nothing to approve/reject afterward:

1. The service checks out the PR's source branch into that repo's
   dedicated `~/.ai-dev-companion/<repo>.worktrees/pr-review` worktree (fetching first, and
   provisioning that worktree on the spot if it somehow doesn't exist yet
   — normally it already does, from setup). If you had uncommitted changes
   sitting in that worktree from an earlier review, they're stashed first
   (`git stash push -u`) rather than lost, and the success message tells
   you exactly which files and where to `git stash pop` them back.
2. It opens the checkout in whichever editor you picked during setup — VS
   Code, Cursor, or Claude Code (a new terminal, `cd`'d into the worktree,
   running `claude`).
3. The ✨ popover shows the result in place — no side-panel, since there's
   nothing here to review inside the extension itself: the whole point is
   that you're about to look at it in your own editor. A failure (e.g. that
   branch is already checked out in some other worktree of the same repo)
   shows the full message via a **Show Details** button rather than a
   truncated native alert.


**Features added by a pack.** A distribution's pack can add features of its own (for example a build-history report for a particular Jenkins pipeline); they register like the ones here and are documented with that distribution.


**Analyze issue** (Jira) — shown on a ticket of a configured project (and
issue type, when the profile or Settings name any); a ticket elsewhere never
shows a row. Read-only footer: **Done** and **Re-analyze**:

1. The service fetches the ticket (summary, description, components,
   labels, versions, environment, comments, issue links, remote links)
   through the relayed browser session. Progress: fetch → repo → analyze →
   format. A successful analysis is written under
   `companion-service/analysis-cache/<issueKey>.json`; the next Analyze
   issue click for that key loads it (brief "Loading saved analysis…")
   instead of re-running Claude. **Re-analyze** starts with `force: true`
   and always runs fresh. The cache is gitignored and survives
   `node install.js` / in-app updates (same preserve path as `config.json`).
2. It picks a local repo from `config.repos` via heuristics (Bitbucket
   project/repo links on the ticket, an optional
   `analyzeIssue.componentRepoMap`, then name matches against components
   and labels). If there's no single strong match, a short Claude pick
   (no tools) chooses one key or `none`. Analysis can still run without a
   repo cwd when nothing matches.
3. It runs Claude headlessly in that repo (or a temp dir) with read-only
   built-ins (`Read`, `Glob`, `Grep`, `WebFetch`, `WebSearch`, and
   `git log`/`blame`/`show` via Bash) plus whatever MCP servers are listed
   in `~/.claude.json` (a pack can name one to prefer via `analysisServerHints`). This runs
   under the **`readOnly`** named policy (see **Security notes** below): a
   `PreToolUse` hook blocks any MCP tool whose action isn't a recognized
   read verb — including one from a server this analysis never explicitly
   named, discovered only via `~/.claude.json` at run time — and a
   hand-maintained deny list on top of that blocks known mutating actions
   by name (Jira create/update/transition/comment, Confluence page
   updates, Bitbucket merge/comment, Jenkins build/groovy, Argo CD
   sync/delete, and similar) as defense in depth. The prompt asks for an
   initial analysis only — not a fix — and a single fenced JSON report.
   Live progress stays short ("Analyzing with Claude…"); MCP names are not
   listed in the checklist. Its session is captured
   (`job.data.claudeSession`) so **Continue in Claude Code** can resume it
   later — see **Session capture and the resume route** below.
4. The slideout shows the structured report: issue key and summary, repo
   and how it was matched, a "<server> used" badge for each server a pack hinted, TL;DR, likely affected
   area, root-cause hypotheses with confidence, repro/scope notes,
   references reviewed, next steps and open questions. Cached loads show
   a "Saved analysis" timestamp. If Claude's JSON can't be parsed, the
   raw text is shown instead. Nothing is written to the working tree.
   **Add as comment** (next to Done / Re-analyze) opens a Markdown compose
   view pre-filled from `job.data.commentDraftMd` (built when analysis
   finishes or loads from cache). The user can edit, then **Post comment**
   sends the Markdown body to the companion, which converts it to Jira
   Server/DC wiki markup and posts via the relayed browser session — same
   SSO path as Create Subtasks. **Back** returns to the report without
   posting.

**Address review comments** (Bitbucket) — new in 0.8.0, on by default
(it was disabled for a while and is enabled again; `disabled: true` in
`core/feature-registry.js` still hides a descriptor everywhere). Shown on a PR with open review tasks or comments
(`properties.openTaskCount`/`commentCount` > 0 on the PR itself — one
same-origin fetch, not a crawl of `/activities` just to decide whether to
show a row). Bitbucket Server/DC 9.4.16 (confirmed against a real
instance) omits `properties` from this single-PR `GET` entirely — only the
PR *list* endpoint's `withProperties=true` returns per-PR counts, and
there's no single-PR equivalent — so when neither count comes back as a
number, `chrome-extension/features/address-review-comments.js`'s `reviewCommentsMenuPayload` shows
the row unconditionally rather than treating "we don't know" as zero;
`collectOpenComments` (below) still fails the job cleanly if there's
actually nothing open. Progress: fetch → comments → address → diff.

1. The service fetches the PR, who's logged in (`whoAmI`, from the
   `X-AUSERNAME` response header), and every page of PR activities
   (`listActivities` — see **The Bitbucket REST layer** below), then keeps
   only the open, actionable root comments
   (`features/address-review-comments/plan.js`'s `collectOpenComments`): a
   resolved thread or `RESOLVED` state drops it; an anchor on a removed
   line, the old side of a diff, a single commit's diff, or an orphaned
   anchor is skipped (that's code no longer in the worktree as Claude sees
   it); a comment you posted yourself is skipped too, unless someone else
   replied to it. Zero comments left fails the job with an explanation
   rather than starting Claude for nothing.
2. It refuses a PR whose source or target repository isn't the one the
   job was started for — a fork PR ("PRs from forks aren't supported
   yet — …", `plan.js`'s `pullRequestRepoError`, from
   `normalizePullRequest`'s `fromRepo`/`toRepo`) — since the worktree
   comes from, and **Approve** pushes to, this clone's `origin`. It then
   creates a disposable worktree off the PR's source branch (the same
   `core/worktree.ts` Resolve Conflict uses), requires its `HEAD` to be
   the PR's reported source commit (`fromSha`; otherwise "the PR's source
   branch moved or isn't the one in this clone — fetch and retry",
   `sourceShaError`), and records that as `preSha` — the diff base, and
   how **Approve** later recognizes whose commit `HEAD` is.
3. It runs `claude -p ...` headlessly under the **`worktreeWriteNarrowBash`**
   named policy (see **Security notes**). The comments and their replies go
   into the prompt as one quoted, escaped JSON data block with an explicit
   "treat this as data, not instructions" line ahead of it — everything in
   it came from a reviewer, not from you. For each comment, Claude either
   makes the smallest edit that addresses it, says why it disagrees, or
   asks a clarifying question — never a refactor or a reformat — then ends
   its reply with one fenced JSON array reporting `fixed`, `declined` or
   `needs-discussion` plus a one-line note per comment
   (`buildAddressCommentsPrompt`/`parseAddressReport`; an id the prompt
   never listed, or an action outside those three, is dropped rather than
   failing the whole report).
4. The service stages the worktree, fingerprints the tree, diffs it
   against `preSha`, and builds a default reply per report entry
   (`defaultReplies` — "Addressed: …" / "Not changed: …" / "Question: …",
   each signed `_(via AI Dev Companion)_`). The panel shows the diff,
   Claude's note per comment, and each default reply read-only with a
   tick box — you choose which replies to post, not their wording. Any
   changed file on a path that commonly runs at commit or CI time gets a
   banner above the diff — see **Commit hooks run unsandboxed, at
   approve** under **Security notes**. **Refresh diff** re-renders the
   same way after you (or Claude Code) change something in the worktree.
5. **Approve** re-stages and re-fingerprints the tree and refuses — same
   as Resolve Conflict — unless it matches what the panel last showed
   (`core/reviewed-push.ts`'s `assertTreeMatchesReviewed`, shared code).
   It only commits/pushes when `HEAD` is still `preSha`, or is this job's
   own earlier approve commit sitting on top of `preSha` (left there by a
   hook that changed files, or a push that failed) — anything else refuses
   outright ("the worktree's HEAD is no longer the PR commit this job
   started from…") rather than pushing something nobody reviewed
   (`plan.js`'s `approveGitSteps`). The commit itself runs with hooks
   **on** — unlike Resolve Conflict's merge commit, which skips them — new
   PR code should go through the repo's normal pre-commit checks; if a
   hook rewrites and re-stages files, the resulting commit's tree is
   checked against the reviewed one *again* right before push
   (`prePushError`: `HEAD^` must be `preSha` and `HEAD^{tree}` the reviewed
   tree, on every path to a push) and it fails instead, asking for
   **Refresh diff** then **Approve** again. That check guarantees the
   *commit pushed* is exactly the tree you reviewed, on `preSha`; it does
   not undo anything else a hook did while it ran unsandboxed on your
   machine (see **Commit hooks run unsandboxed, at approve**). It then
   pushes that exact checked commit with a normal
   `git push origin <sha>:refs/heads/<fromBranch>` (no force) and
   posts whichever replies you kept (`selectReplies` — only to comments
   this job actually collected, trimmed and capped at 4000 characters). A
   reply failing to post doesn't undo the push; the summary names which
   ones to post by hand. **Discard**, and cancelling a finished job, use
   the same `canReject` rule as Resolve Conflict
   (`core/reviewed-push-policy.js`): only once the job is
   `awaiting-approval` or `failed`.
6. The same git-metadata defenses as Resolve Conflict
   (`core/worktree-integrity.js`) apply: a snapshot of the worktree's
   gitlink and the main repo's `commondir`/`gitdir`/`config.worktree` is
   taken right before Claude runs, and every git command the service runs
   afterward (rendering the diff, Approve's stage/commit/push) checks it
   first — a mismatch marks the job compromised and blocks all further
   git against that worktree.

Resolve Conflict, Create Subtasks, Analyze issue and
Address review comments all authenticate to their respective site using
your existing browser session first (see **Security notes**), falling
back to a configured, encrypted API token/credential only if that doesn't
work (see **Credentials**) — so if you're already logged into Jenkins in
this browser, no Jenkins credential needs configuring at all.

## The companion service

### Running it

On macOS, setup installs the service as a per-user **launchd** background
job (no `sudo` needed), so nothing has to stay open. These commands all run
from `~/ai-dev-companion/companion-service`:

| Command | What it does |
|---|---|
| `npm run doctor` | Checks the whole setup and says what to fix. |
| `npm run service:status` | Shows whether the background service is running. |
| `npm run service:install` | Rebuilds and (re)starts the background service. |
| `npm run service:uninstall` | Stops it and removes the background job. Leaves settings, repos and the extension alone. |
| `npm start` | Runs it in this terminal instead (other platforms, or to watch the log). Stops when the terminal closes. |

The logs are `companion-service.out.log` and `companion-service.err.log`
in that folder (`tail -f companion-service.out.log` to follow along). At
startup the service logs one line per tool and repo:

```
[startup] OK   - git version 2.50.1 OK.
[startup] OK   - claude CLI 2.1.228 (Claude Code) found.
[startup] OK   - claude CLI logged in as you@example.com.
[startup] OK   - repos["ACME/sample-app"]: /Users/you/gitviews/sample-app OK.
ai-dev-companion companion v0.8.1 listening on http://127.0.0.1:8787
Features: resolve-conflict, create-jira-subtasks, review-in-editor, analyze-issue
```

A failed check is logged as `WARN`, and the service keeps running. Only the
feature that needs the missing tool refuses to start a job, with a message
saying how to fix it, and it works again once the tool does — no restart
needed. (Git and Claude can be briefly unavailable right after login; the
service used to exit when that happened, which left it unreachable.)

The background job remembers the `node` binary and `PATH` you had when it
was installed, because launchd's minimal environment wouldn't otherwise
find `git` or `claude` installed through nvm or Homebrew. If you switch
Node versions or move those tools, run `npm run service:install` again.

If the folder has already been deleted and you need to remove the job by
hand:

```sh
launchctl bootout gui/$(id -u)/com.ai-dev-companion.companion-service
rm ~/Library/LaunchAgents/com.ai-dev-companion.companion-service.plist
```

### What setup writes

- `companion-service/config.json` — your settings: port, shared secret,
  enabled features, repo map (see below), and per-feature settings. The service only
  reads it at startup, which is why setup restarts the service.
- `chrome-extension/companion-config.js` — the port and shared secret,
  generated to match `config.json` and loaded by the extension's
  `background.js`. It's gitignored and never copied between installs, so an
  update can't leave the extension holding a stale or placeholder secret.
- `companion-service/install-source.json` — written by `install.js`: the git
  remote and branch updates come from. It's the root `package.json`'s
  `updateSource`, falling back to the checkout's `origin`, then the default
  repo. It's rewritten on every install and update. Gitignored.

You shouldn't need to edit these files by hand.

### Versions and updating

The root `package.json`, `companion-service/package.json` and
`chrome-extension/manifest.json` carry one shared version, as do
`plugin/.claude-plugin/plugin.json` and `.claude-plugin/marketplace.json`
(`core/plugin-files.test.js`). `core/updater.test.js` fails if the first three
drift apart, so bump them together (and the lock file's two top-level
`version` fields).

**Release notes.** `CHANGELOG.md` has a `## <version>` section of user-facing
bullets per release. When releasing, rename its **Unreleased** section to the
new version; a test fails if the current version has no section. The update
panel shows the new release's sections newer than the installed version
under **What's New**.

**Publishing a release.** Installs follow the `release` branch (the root
`package.json`'s `updateSource.ref`), not `master`. Pushing to `master` ships
nothing. To publish, once the version and `CHANGELOG.md` are final:

```sh
git push origin master:release   # fast-forwards release to master
```

The Claude Code plugin ships in the same release: `install.js --plugin`
prints `/plugin marketplace add <repo url>#release`, so the marketplace is
pinned to the same branch, and a test keeps the plugin's and marketplace's
versions equal to the rest.

Every install offers the update on its next check (within 6 hours, or at
once from the update panel). To hold a release back, don't move `release`;
to roll back, force-push `release` to the previous release commit.
Installs made before this change still follow `master` until they apply
the release that introduced `release`; after that they follow it too.

**Moving the repo.** Installs follow the root `package.json`'s
`updateSource`, and an update runs the *new* release's `install.js`. So a move
needs no reinstall:

1. Set `updateSource.url` in the root `package.json`, and `DEFAULT_SOURCE` in
   `core/updater.js`, to the new repo. A test fails if the two differ. Bump
   the version.
2. Push that release to the new repo **and** to the old one.
3. Every install offers the update from the old repo as usual. Once it's
   applied, the install checks the new repo from then on.

Keep the old repo readable until everyone has updated. Anyone who missed it
can run the install command with the new URL: it updates in place and keeps
all settings.

- **Checking.** The service keeps a shallow clone of the install source in
  `~/.ai-dev-companion/update-source` and reads the version from its
  `package.json`. It uses your normal git credentials, with prompts turned
  off. It checks 15 seconds after startup and every 6 hours after that
  (restarting the service is the quick way to check now; see **Check for an
  update now** in the [README](../README.md#updating-and-changing-settings)).
  `GET /update` answers from that cache, or checks right away with
  `?refresh=1`. It returns `current` (the version the running process
  started with), `latest`, `available`, `whatsNew` (the clone's
  `CHANGELOG.md` sections after `current`, up to `latest`, newest first),
  whether this copy can update itself, the manual command, and the status of
  the last update.
- **Offering.** Every minute, content.js compares `/update` with its own
  `chrome.runtime.getManifest().version`. The ✨ button appears, even on
  pages with no feature, and its menu leads with one of these:
  - **Update to vX** when the source is newer.
  - **Update companion service to vX** when the extension is newer than the
    service.
  - **Reload extension to vX** when the service is newer than the
    extension.
  - **Restart the companion service to finish** when a foreground service
    installed an update but hasn't restarted.
- **Applying.** `POST /update/apply` starts `core/update-runner.js` as a
  detached process and returns immediately. The runner:
  1. Syncs a separate clone, `update-staging`, so a background check can't
     change it mid-install.
  2. Runs that clone's `install.js --yes --no-service`. This makes
     `~/ai-dev-companion` a copy of the release, deleting files the
     release no longer ships (a leftover `.ts` file would still be compiled
     and could break the build). It keeps `config.json`,
     `analysis-cache/` (saved Analyze issue results), the generated
     `companion-config.js` and `install-source.json`, `node_modules`,
     `dist/` and logs. Then it runs `npm install` and `setup.js --yes`.
     That keeps every setting,
     rewrites `companion-config.js` with the same port and secret, and
     rebuilds `dist/`.
  3. Writes `done` to `~/.ai-dev-companion/update-status.json`.
  4. Under launchd (the plist sets `AI_DEV_COMPANION_UNDER_LAUNCHD=1`),
     sends the service SIGTERM. The job's KeepAlive then starts the new
     build. launchd is never unloaded or reloaded, so an interrupted update
     can't leave the background job missing.

  The old service, and then the new one, report progress from that status
  file. The extension polls until the service reports the new version, then
  calls `chrome.runtime.reload()`, which reloads an unpacked extension from
  disk, and reloads the page. A foreground service (`npm start`) isn't
  restarted; the panel asks you to restart it. A runner that died mid-update
  shows as failed, and the output is in
  `~/.ai-dev-companion/update.log`.
- **Limits.** Only the installed copy in `~/ai-dev-companion` updates
  itself. A dev checkout run with `npm start` shows the manual command
  instead. A feature added in a new version stays off until you enable it
  with `npm run setup`. Changes to the launchd plist itself take effect only
  on the next `npm run setup` or `npm run service:install`.

### How a PR's clone is found

`config.json`'s `repos` map (`"PROJECT/repo"` → local path) is a cache that
fills itself; setup doesn't ask about it. Setup pre-fills it with the
Bitbucket clones it finds in common folders, keyed from each clone's
`origin`. When a feature with `needsRepos` starts, `repoPath()` in
`config.ts` resolves the PR's repo:

1. **Mapped:** use it, as long as the folder is still a git checkout. A
   mapping to a moved or deleted folder is dropped and resolved again.
2. **Inferred:** look for a folder named after the repo next to your other
   clones and in the common folders (`~/gitviews`, `~/git`, `~/src`, …).
   Adopt it only if its `origin` points at exactly that project/repo, and
   save it.
3. **Otherwise** answer `409 {code: "repo-not-found", project, repo}`. The
   extension then opens a **Set up PROJECT/repo** panel and retries the
   original action once the repo is resolved:
   - **Choose folder…** calls `POST /repos/choose`. The service opens the
     native macOS folder picker (`osascript`), checks that the chosen
     folder's `origin` matches, and saves it.
   - **Clone it for me** calls `POST /repos/clone`, which returns a job the
     extension polls like any other. The service derives the URL from an
     existing clone's origin (same host and transport), falling back to the
     Bitbucket page's origin. It never takes a URL from the request. The
     clone goes next to your other clones.

More about how the wizard behaves:

- **It only checks and asks about features you enabled.** With Resolve
  Merge Conflicts off, the `claude` CLI is never required. Create JIRA
  Subtasks needs neither `claude` nor `git`;
  Review PRs in your editor needs `git`.
- **The service only loads features you enabled.** It never registers a
  disabled feature's routes, and the extension asks the service which
  features exist (`GET /features`) before listing any, so the two always
  agree.
- **Re-runs keep your answers.** Every question defaults to the current
  value, and an already-enabled feature's settings are only asked again if
  you answer yes to `Change "<feature>" settings?`.
- **Review PRs in your editor** prepares a `~/.ai-dev-companion/<repo>.worktrees/pr-review`
  worktree for each repo in the background after setup (progress goes to
  `companion-service/review-worktrees-provision.log`; to redo it, run
  `npm run review-worktrees:provision`). It's safe to repeat and never
  disturbs a review in progress.
- `node setup.js --no-service` saves the settings without building or
  starting the service.

### State directory and migration

Everything this service keeps on disk (the repo map cache aside — that's
in `config.json`) lives under **`~/.ai-dev-companion/`**
(`core/paths.js`'s `stateDir()`, constant `STATE_DIR_NAME`): worktrees,
sessions, the update-source clone, `update-status.json`/`update.log`. The
older name, `~/.bitbucket-ai-companion/` (`LEGACY_STATE_DIR_NAME`), is kept
only as a migration source and a legacy-read fallback — it is **not** the
same thing as the npm package name `bitbucket-ai-companion`, which is
unchanged (`core/updater.js` identifies the update clone by that package
name, not by either directory name).

`migrateStateDir()` runs synchronously once, at server startup, before
`FEATURES` is built — moving the old directory is a one-time step, not
something worth threading through an async bootstrap. It's a no-op unless
the legacy directory exists and the new one doesn't (so a second startup,
or an install that never had a legacy directory, does nothing):

1. Renames `~/.bitbucket-ai-companion` to `~/.ai-dev-companion` (falls
   back to a recursive copy-then-delete on `EXDEV`, when the two are on
   different filesystems).
2. For every git worktree found under the moved tree
   (`worktrees/<repo>/<id>/`, identified by a `.git` **file** there — a
   real clone has a `.git` directory, a worktree's is a one-line file
   pointing back at the main repo), runs `git worktree repair` from inside
   it with its new path. This is necessary because moving the directory
   leaves the *main repo's* own record of where this worktree lives
   (`<main-repo>/.git/worktrees/<id>/gitdir`) pointing at the old, now-gone
   location — the worktree's own `.git` file is untouched by the move (it
   only ever points back at the main repo, which never moves); `repair`
   rewrites the main repo's stale record to match.
3. Writes a `MOVED` marker file inside the now-empty legacy directory,
   naming the new location.

Never throws: any failure (including the initial move) is captured into
`result.errors` and logged instead of raised, so a broken migration can
never keep the service from starting. A `git worktree repair` failure for
one worktree is likewise logged and skipped rather than aborting the rest
of the migration — recoverable by hand (re-running `repair`, or just
recreating that job) rather than stranding everything else that moved
cleanly. `core/updater.js`'s `statusPath()`/`logPath()` — used for READS
only — return the new dir's `update-status.json`/`update.log` when it
exists there, fall back to the legacy dir's file when only that one exists
(an update that was in flight across the migration), and default to the
new dir when neither exists yet. Every write (`writeStatus`'s default, and
the log file `startUpdate` opens) always goes to the new dir instead
(`newStatusPath()`/`newLogPath()`) — never sticky on the legacy fallback,
so a service that ever ran pre-migration can't keep writing into
`~/.bitbucket-ai-companion` indefinitely.

`migrateStateDir()` itself only ever runs for the stable install
(`~/ai-dev-companion`) — see "Developing" below for the gate and its
opt-out.

### Jobs across restarts

Every feature's jobs are persisted to disk except review-in-editor,
ticket-workspace and digest (the features that declare `persist: false`;
`core/job-files.js`; see **Job cleanup**), so Resolve Conflict, Analyze
issue, Create Jira subtasks and Ticket to PR survive a restart (`core/job-files.js`; wired into `core/jobs.ts`'s `JobStore` and turned on
in `server.ts`, right after `FEATURES` is built). A job's JSON, and its
worktree-integrity baseline (see **Security notes**), are written to
`stateDir()/jobs/<id>.json` / `<id>.integrity.json` and reloaded at
startup: a job caught `running`, `approving` or `rejecting` mid-restart
comes back `failed` with a message telling you to click **Refresh diff**
(or, if it never got as far as a worktree, to start it again),
rather than vanishing — the worktree itself is untouched, so Refresh diff
picks up whatever was actually left there. Job and baseline files are
pruned once they're 14 days old, at startup and daily, and the expired
job's worktree is removed too (see **Job cleanup**). Persistence is on only for the installed copy
(`~/ai-dev-companion`) — a dev checkout shares the same state dir as a
real install, and reconciling or pruning its jobs here could mark a job
the live service is still running "failed" out from under it — or when
`AI_DEV_COMPANION_PERSIST_JOBS=1` is set (`=0` forces it off either
way). Jobs of the excluded features still live in memory only.
On SIGTERM or SIGINT, `server.ts` stops saving jobs, sends SIGTERM to
every headless `claude` process group it started (`core/exec.ts`'s
`killDetachedChildren`), waits up to 2 seconds for them to exit, then
re-raises the signal so launchd still sees an exit by signal and restarts
the service; a job cut off this way keeps its on-disk `running` status and
comes back with the restart message. A crash or `kill -9` skips that
handler, so each job also records its running Claude's process-group id
(`data.claudePid`), and after a restart **Refresh diff** and **Approve**
refuse while that group is still alive (`core/process-group.js`) —
**Discard** is always allowed.

### Credentials

The API tokens every feature above can fall back to — `jira.apiToken`,
`jenkins.apiToken`, `bitbucket.apiToken`, plus `mcp.token` and
`slack.token` — are encrypted at rest instead of sitting in plain
`config.json` (`core/credential-store.js`, `ALLOWED_NAMES`; nothing else
may be stored here).

- **Format.** `companion-service/credentials.enc` holds one AES-256-GCM
  ciphertext of the whole `{name: value}` map, never individual values, as
  a small JSON envelope (`{v: 1, alg: "aes-256-gcm", iv, tag, data}`,
  iv/tag/data base64). Writes are atomic (temp file, then rename) so a
  crash or a concurrent read never sees a half-written file, and always
  mode `0600`.
- **Key location.** The key is 32 random bytes at
  `~/.ai-dev-companion/credentials.key` (`paths.stateDir()/credentials.key`)
  — created on first `set()`, in a `0700` directory, mode `0600`, written
  with `flag: "wx"` so two processes racing to create it can't stomp on
  each other (the loser just re-reads what the winner wrote).
- **Threat model.** This protects against the `ai-dev-companion`
  folder — or just `credentials.enc` on its own — being copied, zipped,
  backed up or shared elsewhere: the key lives in a different directory
  (`~/.ai-dev-companion/`) than the file it decrypts
  (`companion-service/credentials.enc`), so a copy of one without the
  other is just ciphertext. It does **not** protect against another
  program running as you on this same machine — anything that can read
  files as your user can read both files and decrypt them exactly the way
  this service does. The one place that distinction actually matters
  inside the product is Claude's own headless runs: `core/claude-args.js`'s
  `protectedPathRules`/`protectedSandboxPaths` deny `Read` of
  `credentials.key` and `credentials.enc` to every Claude Code invocation
  this service starts (`--disallowedTools`, and the Bash sandbox's
  `filesystem.denyRead` for any policy that grants Bash) — not because the
  sandbox is a security boundary against a human with shell access on
  their own machine, but because Claude, acting on untrusted PR/ticket
  content, is the one thing in this picture that should never be able to
  read them.
- **An undecryptable file** (wrong or missing key, tampering, corruption)
  is never partially recovered: `get`/`list`/`remove` all throw the same
  `CredentialStoreError("undecryptable", …)`, and `core/token-cache.js`'s
  wrapper (`credentials.ts`'s singleton) turns that into "nothing saved"
  for reads instead of a hard failure — `getToken` returns `undefined`,
  `listNames` returns `[]` — warning once, not on every call, so `GET
  /settings` still loads instead of 500ing on exactly the condition its
  own error message tells you to fix from that same Settings panel. `set()`
  is the one exception: it starts fresh instead of throwing, since
  re-entering a token in Settings is the only real recovery, and refusing
  that call would contradict the very message telling you to do it.
- **Migration.** `core/settings.js` moves any `jira`/`jenkins`/`bitbucket`
  `apiToken` still sitting in `config.json` into the store automatically
  at startup — nothing to do on upgrade.
- `npm run doctor` reports whether `credentials.enc` decrypts, whether it
  and `credentials.key` are owner-only (`0600`), and whether any token is
  still left in `config.json`.

### Git hosts: Bitbucket and GitHub

`core/providers.ts` defines `GitHost`; features only ever call it through
`deps.providers.git`, and which implementation backs it is the profile's `git`
site's `provider`. A profile names one git host. Project, repo and PR number mean
the same thing for both: Bitbucket project key / repo slug / PR id, GitHub owner /
repository / PR number. Everything host-specific that isn't an API call is in
`core/prereqs.js`'s `GIT_REMOTE_RULES`: reading a clone's origin, where a fresh
clone comes from (Bitbucket `/scm/<project>/<repo>.git`, GitHub
`/<owner>/<repo>.git`), a PR page's path, and the PR's key (`bitbucket:PROJ/repo#n`
with the project upper-cased, `github:owner/repo#n` lower-cased) used by scope
keys, history and watchers. An origin without a `.git` suffix matches, and a
repository name may start with one dot (`.github`) but is never `.`, `..` or `.git`.

**GitHub** (`core/github.ts`, `github-endpoints.js`, `github-normalize.js`) maps
onto the same shapes `core/bitbucket-normalize.js` produces, so no feature knows
which host it is on:

- **Auth.** A personal access token only (`<site id>.apiToken` in the credential
  store): GitHub's page login doesn't authenticate its API. The site is `tokenOnly`
  (`core/atlassian.ts`): no cookie is tried, GitHub's `Accept` and API-version
  headers are sent, a 401 means the token was rejected, a 403/429 with
  `x-ratelimit-remaining: 0` or `retry-after` is reported as a rate limit (with
  when it resets), and any other 403 shows GitHub's own message (a missing
  permission) rather than blaming the token. The web address (links, the page the
  extension runs on) and the API address differ: github.com uses
  `https://api.github.com` and `/graphql`; GitHub Enterprise `https://<host>/api/v3`
  and `/api/graphql`.
- **Pull requests.** `state` is OPEN, MERGED (`merged`) or DECLINED (closed
  without merge); branches and shas come from `head`/`base`; `fromRepo`/`toRepo`
  are the owner and name of each side (empty for a deleted fork), which is how
  `address-review-comments` refuses a fork PR. Logins are lower-cased wherever
  they are compared.
- **Review comments** come from one GraphQL query over `reviewThreads` (REST can't
  say whether a thread is resolved): the first comment of a thread is the root and
  the rest its replies, `isResolved` becomes `state: "RESOLVED"`, an outdated
  thread is `orphaned`, a comment on the removed side is `REMOVED`/`FROM`. There is
  no severity (all `NORMAL`). Up to five pages of 100 threads are read. Replies go
  to `/pulls/{n}/comments/{id}/replies`. Comments in the PR's conversation tab are
  not read.
- **Merge status.** `mergeable` is null while GitHub works it out, so an unknown
  answer is asked again twice (0.7 s, 1.4 s) and stays unknown rather than being
  read as "no conflict". `dirty` means conflicted.
- **Builds** are the commit's check runs and legacy commit statuses together, worst
  state first, an unknown state counting as in progress (never green).
- **Dashboard** is a GraphQL search (`is:pr is:open author:<you>` or
  `review-requested:<you>`, with your login from `/user`) that returns the shas,
  source repository, reviewers and approvals in one call. A team review request is
  skipped.
- **Create PR** opens the PR then requests reviewers in a second call; a refused
  reviewer doesn't fail it. There are no default reviewers.

**What the extension asks.** The extension cannot call GitHub's API (a content
script has no token and the cookies don't work), so `chrome-extension/git-host.js`
(`PaiGit`) answers "what is this PR" per host: for Bitbucket with the same
same-origin REST calls as ever, for GitHub through the companion's
`GET /prs/status?project=&repo=&prId=` (background message `pr-status`), which reads
the PR through the provider and answers one host-neutral shape — `state`,
`conflicted`, `isFork`, branches, title, PR address, comment counts — cached for 20
seconds (5 for a failure) and shared by simultaneous callers. Resolve conflict,
Address review comments and Review PR use it and don't offer a fork PR. The
provider arrives with `GET /targets` (`providers.git`), which also names each kind's
origin so a feature is skipped on a page of another server.

### The Bitbucket REST layer

`core/bitbucket.ts` is Address review comments' (and any future feature's)
way of talking to Bitbucket Server/DC, mirroring `core/jenkins.ts`: pinned
to REST API **1.0** (`/rest/api/1.0/`), Bearer token auth, and the same
SSO-relay-first-then-token-fallback chain as Jira/Jenkins
(`core/atlassian.ts`'s `authedJson`/`authedJsonWithHeaders` — see
**Security notes**' Jira relay bullet for how that chain works; Bitbucket
goes through the identical path). It fetches a PR (`getPullRequest`),
every page of its activities (`listActivities` — follows `nextPageStart`
until `isLastPage`, capped at 20 pages, a WARN rather than a failure if
the cap is hit before the last page), the Bitbucket version
(`applicationProperties`), who's logged in (`whoAmI`, from the
`X-AUSERNAME` response header on a cheap authenticated call), and posts a
reply (`replyToComment`). This file is pure HTTP plumbing — building
paths, following pagination — and normalizes every response through:

- **`core/bitbucket-normalize.js`** — pure, defensive normalizers
  (`normalizePullRequest`, `normalizeActivities`, `normalizeAppProperties`)
  turning raw REST JSON into the stable shapes every feature actually
  reads. Every field is read defensively: a missing field gets a
  documented default (e.g. a comment with no `state` defaults to `"OPEN"`,
  the conservative "still needs attention" choice), an unrecognized enum
  value maps to `"unknown"`, and nothing here ever throws — a Bitbucket
  upgrade that renames, removes or adds a field can't silently break
  review-comment collection, or worse, take the request down.
  `normalizeActivities` also dedups comments by id across every page and
  activity (the same comment can appear more than once as it's edited),
  keeping the highest `version`.
- **A capability table.** `capabilitiesFor(version)` turns the Bitbucket
  version string (or `{major, minor}`) into `{threadResolvedField,
  commitBuildsEndpoint}` — `threadResolvedField` (a comment's
  `threadResolved` field) needs Server/DC 8.x+; `commitBuildsEndpoint`
  needs 7.14+. An unparseable or missing version defaults to `{major: 0,
  minor: 0}`, below every threshold, so an unknown server gets the most
  conservative (everything off) capabilities rather than a guess.
- **`EXPECTED_KEYS`** — the single source of truth for which raw REST keys
  each normalizer reads (dotted paths for nested fields, e.g.
  `"fromRef.displayId"`). Nothing hand-maintains a second copy of this
  list: `core/bitbucket-contract.js`'s `missingKeys`/`missingKeysAcross`
  diff a live response's keys against it directly, for the live contract
  check below.
- **The fixtures directory convention.** `core/fixtures/bitbucket/<dir>/`
  — each subdirectory is a self-contained set of raw Bitbucket JSON,
  auto-discovered by `core/bitbucket-normalize.test.js`
  (`fs.readdirSync`; a new directory is tested automatically, no wiring
  needed). `docs-8.x/` is hand-modeled on **Atlassian's documented**
  response shapes — explicitly **not** a captured response from a real
  instance (see `core/fixtures/bitbucket/README.md`). Once a real
  Bitbucket Server/DC instance is available, a sibling `real-<version>/`
  directory with the same four file names
  (`pull-request.json`/`activities-page1.json`/`activities-page2.json`/
  `application-properties.json`), populated with an actual
  (secrets-scrubbed) capture, is compared against `EXPECTED_KEYS` before
  merge and picked up by the same test with no code changes.
- **`doctor --bitbucket-contract [PR url]`** — a read-only, live version of
  that same check, against a real server. Needs a saved
  `bitbucket.apiToken` (added in Settings) since a CLI run has no browser
  session to relay; without one it just prints `skipped`. Without a PR
  url, it only checks `application-properties` (and so the live Bitbucket
  version, recorded to `stateDir()/bitbucket-version.json` and compared
  against the last recorded one — `compareVersion` prints a line if it
  moved since). With a PR url, it also fetches that PR, follows its
  activities (the same capped `followPages` helper `bitbucket.ts`'s
  `listActivities` uses), and diffs the raw `pullRequest`/`activity`/
  `comment`/`commentAnchor` keys against `EXPECTED_KEYS`, one `WARN`
  (never a `FAIL`) per shape with anything missing — a field that's
  genuinely absent on this PR (no anchored comments at all, say) reports
  "nothing to check" rather than a false alarm. Never writes to
  `config.json` or `credentials.enc`; the only file it writes anywhere is
  `bitbucket-version.json`.

### Session capture and the resume route

Resolve Conflict and Analyze issue both stamp `job.data.claudeSession =
{id, cwd, permissionMode}` onto the job right after their headless `claude`
call — `id` from `claude -p --output-format json`'s own `session_id` (or
the `--session-id` this service passed, if the CLI didn't echo one back),
`cwd` the worktree (Resolve Conflict) or repo/temp dir (Analyze issue) the
run actually used, `permissionMode` the mode chosen for the *resumed*
session specifically — `plan` for Analyze issue, `default` for Resolve
Conflict. The headless run that just produced this session never used
either of those: it always ran with `--permission-mode bypassPermissions`
(see Security notes below); `permissionMode` here is purely what
`--resume` should pass later. A cached Analyze issue result keeps its
session in the analysis cache
file on disk (`companion-service/analysis-cache/<issueKey>.json`) so it
survives a service restart; an Analyze issue run with no matching repo
keeps its session's `cwd` under `~/.ai-dev-companion/sessions/<KEY>/`
instead of a temp dir that would otherwise vanish.

**`POST /jobs/:jobId/open-in-claude-code`** with a body of `{resume: true}`
resumes that session in a terminal (`core/resume.js` builds the pure
argv/validation logic; a plain or missing body keeps the route's original
behavior of opening a fresh terminal at the job's worktree):

1. `409` if the job is still `running`/`approving`/`rejecting`.
2. `400` if `job.data.claudeSession` doesn't pass `resume.validateSession`
   (id is a UUID, cwd is absolute, permissionMode is `plan` or `default`)
   — the only gate before any of those fields are trusted, since a cached
   result reads `claudeSession` back off disk rather than from memory.
3. `410` if the session's `cwd` or its saved transcript
   (`~/.claude/projects/<slug>/<id>.jsonl`, `slug` = the absolute cwd with
   every non-alphanumeric character replaced by `-`) no longer exists on
   disk — the session has expired; the message points at Re-analyze.
4. Otherwise runs `claude --resume <id> --permission-mode <plan|default>`
   in that `cwd`. Responds `{ok: true}`.

**This resumed session is a materially bigger trust boundary than the
headless run that produced it, and the boundary narrows to just that one
handoff moment — not the whole rest of the analysis.** The headless run's
`readOnly`/`worktreeWrite` policies, `core/mcp-guard.js`'s `PreToolUse`
hook, and `MUTATING_MCP_TOOLS` (see Security notes) only ever apply to
that first, automatic `claude -p` call. `claude --resume` here runs
**interactively**, in **plan or default permission mode** (not
`bypassPermissions`), with **no `--settings` guard hook** and **no
`--disallowedTools`**, using **the user's own Claude Code settings** —
because it's now a normal terminal session the user is driving by hand,
same as if they'd typed `claude` themselves. The one thing carried over
from the headless run is its transcript, which can contain untrusted
ticket/PR/branch text the headless run read — the user is trusted to
treat that the same way they'd treat any other file/ticket content they
paste into a terminal Claude session.

### The MCP server

The companion is also an MCP server named `ai-companion`, so Claude Code
and Cursor in a terminal can use a subset of its features
(`core/mcp.ts`, mounted by `server.ts`).

- **Route and transport.** `POST /mcp` only (`GET`/`DELETE` answer 405).
  Stateless: every request builds a fresh SDK `Server` plus a
  `StreamableHTTPServerTransport` with no sessions and no SSE stream. It
  uses the SDK's low-level `Server`, not the zod-based `McpServer`, because
  the tool schemas are plain JSON Schema and zod isn't a dependency.
- **Auth.** A bearer token of its own, `mcp.token` in `credentials.enc`
  (rotatable from Settings). The extension's `X-Companion-Secret` is never
  accepted here, and `mcp.token` is accepted nowhere else. `/mcp` is
  mounted before the global JSON parser and the secret middleware. The
  `gate` middleware checks Host (`core/mcp-auth.js`'s `hostAllowed`:
  `127.0.0.1:<port>` or `localhost:<port>` only) and then the bearer token
  (`bearerMatches`, constant time) **before** the body is parsed, so an
  unauthenticated body over the 4 MB limit is a 401, not a 413. A body the parser rejects
  is answered as a JSON-RPC error (-32700 parse error, -32600 too large),
  never Express's HTML stack-trace page. `post()` repeats the access check
  in case the gate is ever mounted wrong, and catches everything, so a
  credential-store read error can't become an unhandled rejection.
- **Tool catalog** (`core/mcp-tools.js`'s `TOOL_CATALOG`; no I/O in that
  file). Each tool has a `featureId` (or `null`), a `kind` and typed
  params; `validateToolArgs` is the real enforcement of the advertised
  length limits (the low-level `Server` doesn't enforce `maxLength`), and
  `capToolOutput` cuts any result at 100,000 characters.
  - Always: `list_jobs`, `get_job` (diffs cut at 20,000 characters),
    `lookup_local_repo`.
  - With their feature enabled: `get_issue_analysis` and `analyze_issue`
    (analyze-issue, run under the `readOnly` policy, so it starts at once);
    `start_resolve_conflict` and `start_address_review_comments`, which
    only queue a job (below).
  - `lookup_local_repo` doesn't call `config.ts`'s `repoPath()`, which
    writes `config.json`; it uses `prereqs.inferRepoPath`, which only
    reads, and says the mapping isn't saved.
  - No MCP tool pushes, posts, comments or writes config.
- **Pending-start.** The two queueing tools check the PR is open, then
  `jobStore.createPending` a job with status `pending-start` and return
  its id and the PR URL. Nothing runs until `POST /jobs/:jobId/start`,
  which sits behind the extension's shared secret, so only a click in
  Chrome can reach it. It runs the normal `startFeatureJob` (same
  readiness and repo checks, `startedVia: "mcp"`), and only after that
  succeeds marks the pending job `rejected` with `supersededBy`; a failed
  start leaves it retryable. `POST /jobs/:jobId/dismiss` discards it.
  Double-starting is guarded twice: the panel disables Start and Dismiss
  while a request is in flight (`pendingActionInFlight` in `content.js`),
  and the server keeps a `jobsBeingStarted` set, so an overlapping request
  (a second tab, a missed double click) gets a 409.
- **`scopeKey`, lookup and the seen list.** `core/scope-key.js` gives each
  job the page it belongs to: `bitbucket:PROJECT/repo#n` or `github:owner/repo#n` (by the profile's git host),
  `jira:KEY-n`, or `jenkins:<job>`, stamped by `startFeatureJob`.
  `isValidScopeKey` accepts nothing else (300 characters at most).
  `GET /jobs/lookup?scopeKey=&featureId=` returns the newest job for that
  pair that was started through MCP and isn't `approved` or `rejected`;
  `featureId` must be a running feature. When a page has no stored job,
  `content.js` (throttled per page) asks the lookup, and adopts the job it
  finds. The adopted ids are kept in a small "seen" list in extension
  storage (`pai:seenMcpJobs`), so a job the user dismissed or finished
  isn't adopted again by the next lookup.
- **Generic readers.** `jira_get_issue`, `bitbucket_get_pull_request` and
  `bitbucket_list_open_comments` read through the companion's own access.
  They are the fallback for a machine without Jira / Bitbucket MCP servers
  of its own: `selectTools` leaves out `jira_get_issue` when a server in
  `~/.claude.json` (`readMcpServers`, minus the companion itself) has
  "jira" in its name, and the two Bitbucket readers when one has
  "bitbucket" in its name, so Claude uses the servers the user already has
  and never gets a second way to read the same data. Chosen per request, so
  it follows servers being added or removed. Like the other tools, they are
  only advertised if a handler exists. There is no setting for this.
- **Registration.** `core/mcp-registration.js`'s `registerEverywhere` runs
  at the end of `setup.js` and, synchronously, after a token rotation from
  Settings. Claude Code: `claude mcp remove` then `claude mcp add --scope
  user --transport http` with an `Authorization: Bearer` header, only when
  `claude` is on PATH. Cursor: a merge into `~/.cursor/mcp.json` (atomic,
  mode 0600, other servers kept, invalid JSON refused rather than
  overwritten), only when `~/.cursor` exists. `npm run doctor` compares
  both against the current port and token (`registrationStatus`: `ok`,
  `missing`, `wrong-url`, `stale-token`, `unreadable`) without printing the
  token.
- **Recursion guard.** A headless Claude run must never call back into the
  companion. `classifyMcpTool` always denies `mcp__ai-companion__*`
  whatever the verb, so `core/mcp-guard.js` refuses it, and analyze-issue's
  `readMcpServers` leaves `ai-companion` out of the servers it hands the
  run.
- **Logging.** One line per call: tool name, ok/error and duration. Never
  arguments, results or the token.

### Session vault

`core/session-vault.js` is an in-memory `Map` of origin to the last browser
session cookie the extension relayed (`X-Relay-Cookie`/`X-Relay-Origin`).
It exists so a request that carries no cookie of its own, an MCP tool call,
can still act as the logged-in user for a while.

- **Wiring.** `core/auth-context.ts`: `contextFor` in `server.ts` records
  every relayed session, and `authContextFor` picks, in order, the
  request's own cookie for that site's origin, the vault's cookie, then
  nothing, leaving the saved API token fallback in `core/atlassian.ts`
  exactly as before. Every Jira, Jenkins and Bitbucket call goes through it.
- **What it accepts.** Only the origins of the configured Jira, Jenkins and
  Bitbucket base URLs, matched exactly (`X-Relay-Origin` is client-supplied),
  and only a cookie of at most 16 KB with no line breaks. The allowed list
  is re-read on every `record` and `get`, so changing a base URL stops the
  old origin at once.
- **Lifetime.** `sessionCache.ttlMinutes`, default 30; 0 turns the vault
  off (nothing is recorded, nothing is returned). Read live from `config`,
  so a change in Settings applies to the next request with no restart.
  Expiry is checked on `get`. A restart forgets everything.
- **Heartbeat.** `sessionCache.heartbeat` (default `false`). When on and
  the TTL is above 0, `GET /extension/session-hosts` returns
  `{heartbeat: true, hosts}` and `background.js` relays a fresh cookie for
  each host every 5 minutes (a `POST /extension/hello` with
  `heartbeat: true`), skipping hosts with no cookie. It only refreshes
  sessions the browser has; it never logs in. A hello also warms the vault
  when Chrome starts. Turning the setting on or off is picked up at once.
- **Never logged, never written.** The `Map` is the only copy. The vault's
  `toJSON` and `util.inspect` show only the entry count, so a stray
  `console.log` can't print a cookie. The tests never call
  `configureSessionVault`, so doctor, setup and the tests never cache one.

### Local history

`core/history-db.js` (schema in `core/history-schema.js`, recording rules in
`core/history-record.js`, the service-side wrapper in `core/history.ts`) keeps a
SQLite database, built on Node's built-in `node:sqlite` (which is why Node
22.13+ is required), at `stateDir()/history.db` with its `-wal` and `-shm`
files, all mode 0600.

- **What it stores.** Items (a ticket, PR, analysis, session or build, by kind
  and key), the links between them, short derived facts (titles, outcomes,
  the ids that tie a PR to its ticket), and timing events. Excerpts are at
  most 2 KB. An FTS5 index backs `companion history --search`.
- **What it never stores.** Tokens, passwords, cookies, the MCP token, raw
  page or log content. Ticket, PR and build text is untrusted, so only
  derived facts and short excerpts go in.
- **Who writes it.** Only the service, from `JobStore.onTransition` (the
  recorder built in `server.ts`), plus the Jenkins history reader. Claude
  runs never get its path: `history.db`, `history.db-wal` and
  `history.db-shm` are in both protected-path lists.
- **Retention and forgetting.** `history.retentionDays`, default 180, edited in
  Settings → Local history; the daily maintenance pass prunes past it.
  `companion forget <key>` (MCP `forget_item`) deletes an item and its links.
  Forgetting an item removes it, its links and its timing events, but not the
  other items it was linked to (for example forgetting a ticket leaves that
  ticket's stored analysis and job items until they are forgotten or age out);
  `companion forget` each key to remove them.
  Reads are `GET /history/metrics` and the MCP tools `get_history` and
  `list_history`.
- **Failure is never fatal.** A history failure logs a `WARN` and carries
  on; it can't stop the service starting or a job finishing. Only the
  `node:sqlite` ExperimentalWarning is suppressed, not warnings in general.
- **Threat model.** The file holds derived personal data only. A program
  running as you can read it, exactly as it can read `config.json`, which
  is why Claude runs are denied it. This is the lesson of Microsoft Recall's
  first version: a plaintext database of everything you did, readable by
  anything running as the user. The store keeps less (no screenshots, no
  credentials), is 0600, is off-limits to the agents that handle untrusted
  content, has a short default retention, and can be browsed and deleted
  from the terminal.
- **Vectors (Phase 8).** `SCHEMA_VERSION` is 2: a forward-only migration adds
  `item_vectors` (see **Similar items**). Forgetting an item and retention
  pruning delete its vector with it (the foreign key cascades; foreign keys
  are on). The recorder now reads an analysis item's excerpt from `tldr`
  first (analyze-issue's report leads with it; before, analysis items had no
  excerpt to embed or search).
- **Ticket edges (Phase 6).** Kind `worktree` (key `worktree:<abs dir>`; `kind`
  is plain TEXT, so no migration) and four edges: ticket -> worktree
  (`worktree`), ticket -> PR, worktree -> PR and worktree -> session
  (`links`). `core/history-record.js` writes them at job transitions (Start
  fix at `awaiting-approval`, Create PR at `approved`), not the feature. The
  `approved` event carries `startFixToPrMs` (Start fix to PR; an adopted
  worktree keeps the time the history recorded for it). It is stored in
  `metrics_json` and nothing reads it yet. A workspace scan records only its
  timing event, no items.
- **Timing caveat.** "Conflict -> push" time is approximated by job start
  to outcome still: the watcher stops at Approve and does not observe the push.
- **Background work (Phase 7).** The `used` outcome and an `opened` event
  record that a pre-warmed job paid off: an approve (Resolve Conflict pushed)
  or, for a read-only result, the first open of its inbox item. Job events
  carry `metrics.watcher` and `metrics.tier`; scheduler runs are `events` rows
  with `featureId: "scheduler:<task>"`. `history.prewarmMetrics({days})` returns
  `[{watcher, runs, used, discarded, failed, expired, usedFraction}]`, served
  in `GET /history/metrics` (`prewarmed`), the `list_history` tool and
  `companion history --metrics`. An analysis read on the ticket page without
  opening its inbox item is not counted, so the metric under-counts read-only
  use. `startFixToPrMs` (Phase 6) still has no reader.

### The `companion` command

`bin/companion.js` (behaviour in `core/cli-run.js`, output in
`core/cli-format.js`) is plain JS with no build step and no npm
dependencies, so it runs straight from the installed copy; the installer
links it into `~/.local/bin`. It is an MCP client of the service's
`/mcp` route (`core/mcp-client.js`), authenticated with the Phase 3 token
read from the credential store, so it obeys the same Host and token checks
as any other MCP client. Commands: `status`, `jobs`, `history`, `similar`, `resume`,
`forget`, `inbox` (`--brief`, Phase 8), `open`, `digest`, `precheck`, `hooks`,
`doctor` and `mcp-stdio` (see **Claude Code plugin**). `resume` runs `claude --resume` in the current
terminal (with the job's saved working directory), not in a new window.

### Streaming progress

Headless Claude runs use stream-json output; `core/claude-stream.js` turns
each tool use into a label ("Reading src/foo.ts") that the job publishes as
its progress text and the ✨ progress row shows. Labels come from Claude's
tool inputs, so they are truncated to 80 characters and stripped of control
characters.

### Targets from config

`core/targets.js` holds the Jira project keys and issue types the analysis
features accept; the defaults come from the profile (`targets.projects`,
`targets.issueTypes`) and are editable in Settings, validated by
`core/settings.js`. A pack can add targets of its own (a build pipeline, say):
its `siteSettings` hook puts extra keys on a site's Settings and its `targets`
hook adds them to `GET /targets`, which the extension asks (cached briefly in
`content.js`). URL patterns for Jira pages are built at runtime from the
configured names (`chrome-extension/target-patterns.js`), not written into the
manifest; the pack's extension file builds any pattern of its own.

### Open in editor

The extension adds a right-click **Open in editor** item (the `contextMenus`
permission; failures are reported with a notification, hence
`notifications`). The selection, capped at 8 KB and never trusted, goes to
`POST /open-location`. `core/location-parse.js` finds `path:line[:col]` patterns
in it (at most 20 locations); `core/location-resolve.js` (driven by
`core/open-location.ts`) resolves each against
`git ls-files` of the configured repo roots and nothing else, so a
selection can never name a file outside a tracked file of a repo you set
up. The editor is opened with `open cursor://file/<abs>:<line>` or
`vscode://file/...` (`core/editor-url.js`) (argv array, no shell); there is no editor CLI because
`open` with a URL scheme needs no PATH setup and works whether or not the
editor is already running. `companion open` and the `open_location` MCP tool
call the same service code.

Limitations: Chrome may cut a long selection, so a trace should be selected
a line at a time; a name that matches more than one file fails with a list
of the candidates rather than guessing; there is no Jenkins-job-to-repo
mapping, so only a Bitbucket page's own repo breaks ties. macOS only.

### Diagnose failed builds

On a failed or unstable Jenkins build the extension shows a ✨ **Diagnose
this failed build** row and posts the build URL. The service does not trust
it: the URL is re-validated (same origin as the configured Jenkins, a plain
job path, a numeric build) and only the rebuilt URL is used. If a pack offers
a diagnose command (its `diagnose` hook, usable when it reports itself
available), Claude Code starts with that; otherwise a plain diagnosis prompt
about the exact build is used, and Claude Code runs in plan (read-only) mode.
Both open in a new terminal window (macOS only) with a header; when the shared
test history is configured, the header lists which failing tests are known
flaky. The diagnosis is not meant to write anything.

### Shared test history (`risk-facts/v1`)

`core/risk-facts.js` reads an optional feed of shared, derived facts. The
producer is a separate project; the companion only reads. Shape:

```json
{ "schema": "risk-facts/v1", "generatedAt": "2026-09-29T02:00:00Z",
  "tests": [ { "name": "Login with SSO", "flaky": true, "failed": 8, "of": 30 } ],
  "files": [ { "path": "src/app/login.ts", "regressions": [ { "build": 812, "tests": ["Login with SSO"] } ] } ] }
```

It is off unless `riskFacts.url` is set (⚙ Settings). A fetched copy is
cached for 1 hour, and if a refresh fails a stale copy is served for up to
24 hours; after that, or with no feed, callers get `{ ok: false }` and
carry on. Test names match case-insensitively; names or paths with control
characters make the whole document invalid, because they end up in prompts
and terminal headers. `companion doctor` reports the feed.

### Job cleanup

Analyze issue (and Create Jira subtasks) jobs persist with Resolve
Conflict's, because their result is worth reopening after a restart.
Review-in-editor, the ticket workspace and the digest deliberately don't: they are
cheap to rerun or hold nothing worth keeping. Jobs untouched for 14 days
are expired at startup and by a daily timer (`dailyMaintenance` in
`server.ts`, which also prunes the history): the job file goes and so does
its worktree (`git worktree remove`), and the expiry is noted in the
history.

### Ticket workspace

`features/ticket-workspace/` (`plan.js` is pure, `index.ts` does the I/O). On a
click it builds one view of a ticket from three sources, in order:

1. **The local history.** The ticket's item and its neighbours
   (worktree, PR, analysis, job), then the sessions linked to those: at most
   two hops, with caps on how many of each it keeps.
2. **git**, per configured repo (the first 30): `for-each-ref` over
   `refs/heads` and `refs/remotes/origin` for branches whose name has the
   key (matched as a whole key, so `PROJ-12` does not match `PROJ-123`),
   `worktree list --porcelain`, and `status --porcelain` for dirty and the
   ahead count.
3. **Bitbucket**, for branches that exist on origin: their pull requests with
   `state=ALL` (so merged and declined ones show), for at most 10 branches,
   and the build state of at most 5 open PRs through the fallback pair
   (`/rest/api/1.0/…/commits/{sha}/builds`, then
   `/rest/build-status/1.0/commits/{sha}`; only a 404 moves on).

A failing repo or Bitbucket call (or a Bitbucket setting that can't be read)
becomes a line under "Couldn't check" (`notes`); it never fails the scan. The
PR links in the view are checked on the service: `https:` on the configured
Bitbucket host and a PR page path (a history PR with a bad address is replaced
by one rebuilt from its key); the extension checks `https:` again before it
links or opens one, and Ticket to PR jobs are re-checked (`Feature.present`)
each time they are served.

The history database is a local file and therefore untrusted. A worktree
from it is used only if it is exactly `ticketWorktreePath(config.repos[repoKey], key)`
for a configured repo, and a session only if its folder passes the same
allow-list a persisted job gets (`core/session-cwd.js`: a mapped clone, a
`~/.ai-dev-companion/<repo>.worktrees/<folder>`, or a folder under `stateDir/sessions`). Anything
else is dropped with a note. The resume fallback folder, `resumeSessionInTerminal`
and `companion resume <KEY>` apply the same allow-list.

It is a read-only polled job like Analyze issue and is **not persisted**
(`NOT_PERSISTED_FEATURES` in `core/job-files.js`): a scan is quick to redo,
and its folders and sessions must only ever be ones this process found. Its
actions (`open-worktree`, `resume-session`) take a folder or session id from
the request and accept it only if it is in that job's own scan data. The same
view is computed without a job by the `ticket_workspace` MCP tool (registered
only while the feature is enabled) and by `companion resume <KEY>`, which
resumes the workspace's preferred session if its folder and transcript still
exist, else starts a fresh `claude` in the ticket's worktree.

### Ticket to PR

`features/ticket-to-pr/` (on by default).
One job carries both steps through the usual `Feature` methods: `start` is
**Start fix**, `approve` is **Create PR** (body `{title, description}`), and
`reject` is **Stop tracking**. `start` with `adopt: true` reuses an existing
worktree without opening Claude; that is the workspace's Create PR, and it also
covers a job that expired or was stopped.

- **Start fix.** The repo comes from the ticket's saved analysis
  (`analysis-cache/<KEY>.json`) or, for the workspace, the workspace's
  `repoKey`; it must be a configured repo key. None means the job fails with
  "run ✨ Analyze issue on the ticket first". The branch is
  `bugfix/<KEY>-<slug>` for a Bug and `feature/<KEY>-<slug>` otherwise, the
  slug ASCII `[a-z0-9-]`, at most 40 characters, cut at a word boundary. The
  base is Bitbucket's default branch (`…/default-branch`, then
  `…/branches/default`, then git's `origin/HEAD`, then `master`, then `main`;
  only a 404 falls through). Names are checked with `isSafeBranchName` (and
  git's ref-format check) before git sees them. The worktree is
  `~/.ai-dev-companion/<repo>.worktrees/<KEY>`, persistent. The plan-mode session gets its own
  `--session-id`, recorded in `data.claudeSession`, so **Continue in Claude
  Code** and `companion resume` can resume it (`core/session-cwd.js` accepts
  `~/.ai-dev-companion/<repo>.worktrees/<one folder>` for this).
- **`data.ticketWorktree`, not `data.worktree`.** `data.worktree` means a
  per-job worktree under `stateDir()` that discard and the stale-job sweep
  delete (`core/job-files.js`, `core/worktree-prune.js`). This one is the
  user's and is never deleted. Because a persisted job file is untrusted,
  every Create PR, draft and terminal action recomputes the folder from config
  (`ticketWorktreePath(config.repos[repoKey], issueKey)`) and refuses a
  mismatch, and checks the worktree is on the job's branch.
- **The draft run.** **Draft with Claude** (the `draft-pr` action) fetches
  `origin/<base>`, refuses a branch with no commits ahead, then runs `claude`
  with the `noTools` policy in a throwaway temp directory. The prompt carries
  the ticket summary, the commit log (at most 8 KB) and `git diff --stat` (at
  most 4 KB) inline, fenced and labelled as data. Any failure or unparsable
  reply falls back to a plain `KEY: summary` draft; the form always starts
  filled.
- **Create PR.** `approve` is single-flight: it moves the job to `approving`
  before its first await, so a second click is refused by the status check.
  It then checks the branch, fetches `origin/<base>` (so it fails offline
  rather than guess from a stale ref) and refuses a branch with no commits
  ahead. The steps run in order: push (`git push -u origin <branch>`, hooks
  run, `GIT_TERMINAL_PROMPT=0` so a credential prompt fails instead of
  hanging), then find an open PR for the branch or open one. **Push and the
  PR are required.** The default reviewers
  (`/rest/default-reviewers/1.0/…/reviewers`), the Jira remote link and the
  transition are best effort ("warn" steps): the PR stands. A required-step
  failure puts the job back to `awaiting-approval` (not the `Feature.approve`
  contract's failed) with the step report as the error, so you can fix and
  retry; a retry reuses a PR already recorded in `data.pr` or found open for
  the branch. The author (`whoAmI`) is removed from the default reviewers,
  because Bitbucket refuses a PR whose author is also a reviewer; if it can't
  tell who you are, it adds none.
- **The Jira side.** The remote link carries a `globalId`
  (`ai-dev-companion:pr:<url>`) so a retry updates it rather than adding a
  second one, and is only sent for a PR URL that passes the configured-host
  check. The transition is `ticketToPr.reviewTransitionName` (default "In
  Review", at most 100 characters), matched case-insensitively against a
  transition's name, then its target status; it is skipped when the ticket has
  none.
- **Doctor** (with ticket-to-pr on) runs `git ls-remote --heads origin` with
  `GIT_TERMINAL_PROMPT=0` in the first 5 repos, because Create PR's push would
  fail on a prompt.
- **Stop tracking** only marks the job rejected. It never removes the worktree
  or the branch.

### Scheduler

`core/scheduler.ts` is one loop over the pure rules in `core/schedule.js`. Its
six tasks run one at a time, in this order, on each tick: `maintenance`
(job expiry and history pruning: the Phase 4 timers, moved here), then
`watcher.conflicts`, `watcher.assignedBugs`, `watcher.reviewRequests`, then
`digest`, then `history.embed` (Phase 8, see **Similar items**). A task
with no registered function is skipped, not marked run. The loop wakes when the next task is due but at least once a
minute, so a Settings change (a watcher turned on, a new digest time) applies
within a minute with no restart; the config object it reads is the live one
that Settings saves mutate in place. `maintenance` runs a minute after start
and then every 24 hours; its last run is never carried over a restart. A task
is marked run *before* it runs, so a failing one waits for its next turn;
`stop()` also stops a running tick from starting further tasks. A tick while
another is going does nothing.

- **State.** `stateDir()/scheduler.json` (0600, atomic writes) keeps the
  budget per local day, what each watcher has seen (at most 500 keys each),
  last runs and the last digest day, normalised on load (future days and
  times are clamped). Like job persistence it is written by the installed
  copy only; a dev checkout keeps it in memory. It holds keys, stamps and
  counts, no secrets.
- **Budget and `requestClaudeRun()`.** A watcher asks before starting a
  Claude run. It is refused as `busy` while the last background job it
  granted is still `running` (one at a time), `quiet` inside quiet hours, or
  `budget` once `budget.claudeRunsPerDay` (default 5, 0-50) is spent. `busy`
  is *not* marked seen, so the next tick retries it without a duplicate inbox
  item; `quiet` and `budget` are marked seen and filed as "Not pre-warmed: ..."
  inbox items, so a budget of 0 gives an inbox item and no run. A start that
  throws refunds its run (`refundClaudeRun`), so a failed start costs no budget.
- **Quiet hours** (`scheduler.quietHours: {start, end}`, may wrap midnight)
  stop the watchers (and the wake timer stops spinning until the window
  ends); maintenance and the digest still run.
- **Every run is an `events` row**: `featureId: "scheduler:<task>"`, outcome
  `completed` or `failed`, and metrics with `task`, the task's counts, `tier`,
  a redacted and clipped `error`, and `needsLogin`. String metrics also pass
  through `redactSecrets`.
- **Needs login.** An `AuthSetupError` or a 401/403 from a watcher's read
  marks the run failed with `needsLogin`; Settings shows it under that watcher.
- `core/background-wiring.ts` builds the scheduler, watcher runner and the
  inbox routes from injected dependencies (so it is tested with a fake clock);
  `server.ts` only supplies the real ones.

### Watchers

`core/watcher-runner.ts` (I/O, all injected) over `core/watchers.js` (pure
rules). Each is opt-in (`watchers.<name>.enabled`, default off) and every
value that ends up in an inbox item is clipped plain text with an https link
on the configured Bitbucket or Jira host (`core/link-guard.js`); anything else
becomes no link. At most 20 pull requests are read and 10 events handled per
run.

- **conflicts.** The author dashboard, then `/merge` per PR (`conflicted`, the
  field the page already reads). An event is an open, conflicted PR at a
  `(fromSha, toSha)` not seen before. No Claude run when Resolve Conflict is
  off, a job for that PR is open (`running`, `awaiting-approval`, `approving`,
  `rejecting`, `pending-start`), there is no configured clone (`config.repos`,
  never inferred, never written), the title is WIP/Draft, or the PR is from a
  fork. `urgent` when the PR already has an approval. Seen state is saved after
  each settled event.
- **assignedBugs.** JQL `assignee = currentUser() AND project in (...) AND
  issuetype in (...) AND statusCategory != Done AND updated >= -1d` over
  `analyzeIssue.projects`/`issueTypes` (every value JQL-quoted); once per
  ticket. No run when Analyze issue is off, an analysis is running, or one is
  cached. Its seen list is not pruned against the query (that would announce
  an old ticket again when it is touched); it is capped at 500.
- **reviewRequests.** The reviewer dashboard, a new head commit. Runs
  `git fetch --no-tags origin +refs/heads/<b>:refs/remotes/origin/<b>` with
  `GIT_TERMINAL_PROMPT=0` (argv only; the branch is re-checked with
  `isSafeBranchName`); never a checkout, never triage, never Claude. A PR
  from a fork is not fetched.
- **Sign-in** is the Phase 3 chain: the REST helpers get an empty
  `AuthContext` and `authedJsonWithHeaders` runs `authContextFor` (vault
  cookie, then saved token).
- **Triage** (only for events the rules would run Claude for). When
  `route-policy.chooseTier({task: "triage"})` says `onprem` (proxy detected,
  prompt within 200 KB) the watcher asks the proxy for `{worth, reason}` JSON,
  checked against a schema. The answer can only veto a run the rules allow; a
  broken or failed reply falls back to the rules. The prompt is a plain chat
  call with no tools, and ticket/PR text is untrusted: fenced, flattened to
  one line, with forged fences stripped, clipped. The decision is recorded as
  `triage: {onprem, rules, failed}` and `tier` in the run's events row; the
  pre-warmed job's own events carry `tier: "claude"` and `watcher`.
- **Jobs.** A run is started through the same `startFeatureJob` as a click,
  with `via: "watcher"` and `ctx.background: true`. Both are set only by the
  service's own runner (`featureContextFor` ignores `background` for any
  other `via`, so a request body can't select it). The job records
  `startedVia: "watcher"` and `watcher: "<name>"`, and `JobStore.lookup` (so
  the page's `/jobs/lookup`) finds it, so opening the PR or ticket shows it.
- **What a background run may do.**
  - *Analyze issue* under `ctx.background`: the `readOnlyBackground` policy
    (the `readOnly` allowlist without WebFetch/WebSearch, still MCP-guarded,
    no cwd writes), **no MCP servers** and no links in the prompt (MCP read
    tools take free-text arguments, which is a way out). This is stricter than
    a click-started analysis. The result is written to the **shared analysis
    cache** like any analysis, with no marker that it was a background run, so
    a later click on the ticket loads it (`loadCachedOrAnalyze`) unless the
    user re-analyses; it is the weaker report (no MCP, no links).
  - *Resolve Conflict* is not `readOnlyBackground`: it runs the same
    `worktreeWrite` policy as a click, sandboxed Bash and Claude's edits
    confined to its disposable worktree, with no click behind it. It stops at
    `awaiting-approval`; the watcher path never approves or rejects, so
    nothing is pushed until a person clicks Approve. It does spend budget.
  - *Review requests* only run `git fetch`.

### LLM proxy and routing

`core/llm-proxy-request.js` (pure request/response shapes),
`core/llm-proxy-client.js` (HTTP over an injected `fetch`; no new dependency)
and `core/llm-proxy.ts` (live config and credential). The paths are the
OpenAI-compatible `/v1/chat/completions`, `/v1/embeddings` and `GET /v1/models`
(a tiny detection call), with `Authorization: Bearer <key>` and the `user` field
from `llmProxy.user` (else the OS user name), all in one `PATHS`/`headers()`
place. The defaults (address, models, allowed host suffixes, where to get a key)
come from the profile's `llm` block; the Settings page that edits them is named
by the profile's `branding.llmProxy`, and is hidden when the profile has none.

- **Reasoning models.** A reasoning chat model thinks before it answers: a
  trivial prompt can use 40-100 hidden `reasoning_content` tokens inside
  `max_tokens` (a budget of 8 gave an empty answer). Chat therefore defaults to
  1024 tokens, the final `content` (which starts with blank lines) is what is
  parsed, and a reply with `finish_reason: "length"` that has no answer, or that
  must fit a JSON schema, is reported as "cut off before the answer" rather
  than as an empty or invalid reply.
- **Allowed models.** Only names in `llmProxy.allowedModels` (default: the
  profile's chat and embedding models) are ever sent anything; any other is
  refused before a request is built. `allowedModels` can be set by hand in
  `config.json` only, not from Settings. The chat and embedding model choices
  must be in the list. Names the profile lists as `retiredChatModels` are only
  aliases of the current model (an alias can be re-pointed at any time): a saved
  choice of one moves to the current default; any other name outside the list is
  refused, not replaced. Settings offers only what the proxy lists for each role,
  and a chat model can't be saved as the embedding model or the reverse.
- **Allowed host.** The key and the prompt text go to `llmProxy.baseUrl` only if
  it is `https:` with no credentials and its host is (or is under) one of
  `llmProxy.allowedHostSuffixes` (default: the profile's `llm.hostSuffixes`).
  Enforced twice: the Settings validator rejects anything else with a message
  saying so, and the request builders refuse to build for a hand-edited bad
  address (nothing is sent). Like `allowedModels`, the suffix list is
  config.json-only.
- **Key.** `llmProxy.apiKey` in `credentials.enc` (an allowed credential name);
  never in `config.json` and never in a response (only `apiKeySet`). No Keychain.
- **Timeouts and retry.** 60 s; one retry after 1 s on 429/5xx only (a network
  error is not retried); redirects are errors; a refused or failed reply's body
  is cancelled.
- **Detection.** `detect()` gives `ready | no-key | unreachable | tls |
  key-rejected | error`: the key check is live and the one call (10 s, no retry)
  is cached for 10 minutes per base URL and a hash of the whole key. The call is
  one tiny embedding (`POST /v1/embeddings`), not the model list, because some
  proxies answer the model list to anyone: a real embedding exercises the key,
  the application, the `user` field and the model. A non-2xx answer is classified
  by `classifyReply` into fixed states, never echoing the proxy's text (its errors
  can include the key): `application` (a gateway that ties each key to the
  application it was created for by the request's User-Agent and refuses a client
  it doesn't know), `user` (a missing or invalid `user` field), `key-rejected`
  (401/403), `error` (any other status, shown as "error (HTTP n)"). Because such
  gateways have no generic application, the client name is a setting,
  `llmProxy.userAgent` (default `ai-dev-companion/<version>`, printable ASCII
  up to 100 characters), sent on chat, embeddings and detection; prefer asking
  the proxy team for an application of this tool's own over borrowing another
  tool's name. `tls` means Node refused the proxy's certificate
  (`SELF_SIGNED_CERT_IN_CHAIN` and similar): a private-CA chain is only trusted
  through the system certificate store, so the launchd plist starts the service
  with `NODE_USE_SYSTEM_CA=1`; a shell-run `companion doctor` needs the same
  variable. `GET /settings` waits at most 1.5 s for it.
- **`route-policy.chooseTier({task, bytes, needsTools}, {onpremAvailable})`**
  returns `{tier, reason}`: `needsTools`, `code-change`, `diagnosis`, `analysis`
  and unknown tasks are `claude`; `triage`, `classify`, `condense` and `embed`
  are `onprem` when available and under 200 KB, else `claude`. For a watcher's
  triage, `claude` means "rules only": triage never spends Claude budget.

### Inbox and notifications

`core/notifications.js` keeps the inbox in `stateDir()/notifications.json`
(0600; installed copy only). An item has a `key`, a kind (`conflict`,
`analysis`, `review-request`, `digest`), a clipped title (200) and body
(1000), an https-only URL without credentials, `urgent`, the watcher, scope
key, job and feature, and times. Control, C1, zero-width and bidi characters
are stripped. A refresh of an unseen item with the same key updates it (and
re-announces it if it just became urgent); an item already seen is never
brought back by the same key. Seen items are kept a week, unseen two, at most
100. A file read back goes through the same cleaning as a new item.

- **Routes** (behind the Host guard and the secret check, in
  `core/background-routes.js`): `GET /notifications` (unseen, or `?all=1`; the
  count and the scheduler status), `POST /notifications/seen` (exactly one of
  `{ids}` (at most 100 uuids) or `{all: true}`), `POST /notifications/announce`
  and `POST /notifications/:id/open`.
- **Batching.** The extension's minute alarm asks `announce`; the store plans
  at most one Chrome notification per `notify.minIntervalMinutes` (default 30)
  unless a pending item is urgent, none in quiet hours, and records it. An
  omitted or invalid interval falls back to 30 minutes, never "no limit". The
  extension draws the badge, and if Chrome refuses the notification the click
  data is dropped (the service already counted it). Chrome has to be running.
- **Quiet hours and the digest.** Items stay in the inbox and on the badge.
  When the scheduled digest posts, `absorbPending()` marks the unannounced
  items announced so they don't also arrive as their own notification.
- **Opening an item** marks it seen and opened. For a read-only pre-warmed
  result (with a job) the first open records the history `used` outcome;
  opening a conflict item does not, because a resolution is used only when it
  is approved.

### Morning digest

The feature `digest` (`features/digest/`): read-only, on by default (an
upgrade enables it), click-only, a row on every covered page, not persisted;
it records only its timing event. `computeDigest` reads your open PRs (author
dashboard, the first 10 with `/merge` and build status), your review queue,
`assignee = currentUser() AND statusCategory != Done AND issuetype not in subTaskIssueTypes()` (20 tickets; sub-tasks excluded), and watcher
jobs of the last 3 days that are awaiting approval or failed. A failing source
becomes a "Couldn't check" note (redacted), links are checked against the
configured hosts, and the text is passed through the same one-line cleaning.
`buildDigest` (pure) orders by tone, shows at most 10 items a section (the rest sit behind "Show N more"), and
counts failed runs separately from those ready. The scheduled task
(`digest.enabled`, off by default, `digest.time` "08:30", weekdays, once per
day after that time) posts it as one inbox item (`digestNotification`) and
absorbs pending items as above. `get_digest` (MCP, only while the feature is
on) and `companion digest` show the same.

### Pre-push check

`core/push-risk.js` (pure, no LLM) compares a change's files with the shared
`risk-facts/v1` feed (`core/risk-facts.js`; `riskFacts.url` unset or
unreadable returns `{skipped: true, reason}`). Levels: `high` (a file in 3 or
more regressions, or 2 or more files in any), `medium` (one file in 1-2),
`low` (changed files are in the facts, none with regressions), and `null`,
"not enough data" (no changed file is in the facts). Each line cites the
builds and tests; tests in those regressions that the feed marks flaky get
their own line, and facts older than 7 days get a warning line.

- **MCP tool `get_change_risk`, not `assess_push_risk`.** The tool catalogue
  test forbids "push" in tool names (they start with a read verb), and it
  is read-only: it assesses files whether or not a push is planned. Params
  `{files: [...], repo?: "PROJECT/repo"}`. Like `list_notifications` it is a
  core read tool (always offered).
- **The hook** is git's config-based hook:
  `git config --local hook.companion-precheck.event pre-push` and
  `... .command "companion precheck --stdin"`, so it runs alongside husky and
  any existing pre-push hook and touches no file. `companion hooks install`
  first probes the local git in a throwaway repo (isolated from the user's and
  system git config) and otherwise prints the one line to add to
  `.git/hooks/pre-push` or `.husky/pre-push` and changes nothing. Uninstall
  uses `--unset-all` and treats exit 5 ("wasn't set") as fine.
- **`companion precheck --stdin`** reads git's pre-push lines (at most 50),
  lists each update's files (`git diff --name-only <remote> <local> --`; a new
  branch: `git log --name-only --format= <local> --not --remotes --`; SHAs are
  validated), and prints on stderr only a `medium`/`high` warning; silent on
  low, not enough data, skipped, a stopped service or a missing/refused token,
  and on a timeout. By hand it prints whatever it gets. It is **advisory**: it
  always exits 0 (`main()` wraps the whole command, and `bin/companion.js`
  exits 0 for `precheck` and ignores a broken pipe), gives up after a
  best-effort 3 s (the git calls are synchronous, so a stuck git can run a
  little over), `COMPANION_SKIP=1` skips it, and `prePush.mode` is `"warn"`
  (default) or `"off"`; there is no blocking mode. Anything a service returns
  is stripped of terminal control characters before printing.

### Similar items

Phase 8. Finds past tickets and PRs like a given one in the local history.
It works only where the history is on (the installed copy), and on by
default there (`similar.enabled`).

- **Vector storage: a scan in JS, not sqlite-vec.** The sqlite-vec extension
  was evaluated and is not used: it would be a new npm dependency, is
  pre-1.0, and needs extension loading enabled on the history database. At
  personal scale (hundreds to a few thousand items) a scan takes
  milliseconds. Schema v2 adds `item_vectors(item_id PRIMARY KEY -> items(id)
  ON DELETE CASCADE, model, dim, vec BLOB, item_updated_at, at)`, migrated in
  `history-schema.js`'s `MIGRATIONS` (tested on a real v1 file). Vectors are
  L2-normalised Float32 blobs, so cosine is a dot product. The only reader is
  `history.nearest()` (it ignores vectors older than the item and reads at
  most the 20000 newest), the one place a later switch would change.
- **What is embedded.** Items of kind `ticket`, `analysis` and `pr` with a
  title or excerpt (never jobs, sessions or worktrees). The text is
  `title - excerpt`, flattened (control, zero-width, bidi and tag characters
  removed), secret-masked with `redactSecrets`, flattened again and clipped
  to 4000 characters (`similar.embedText`). An item is embedded again when it
  changed (`items.updated_at` is newer than the vector's `item_updated_at`)
  or the model changed. An item that has no text, or whose vector can't be
  stored, gets a one-number placeholder vector that no search matches, so it
  leaves the pending list and can't starve the items behind it.
- **The `history.embed` task** (`core/embed-task.js`; scheduler task, last in
  a tick; first run 2 minutes after start, then every 30 minutes; quiet hours
  don't stop it, it notifies no one). At most 48 items a run, newest first,
  in batches of 16. Without a ready proxy its events row (`featureId:
  scheduler:history.embed`) says `{skipped: "no proxy", proxy: <state>,
  embedded: 0}` and nothing is sent. `chooseTier({task: "embed"})` must say
  `onprem` before anything is sent, embedding never falls back to Claude, and
  the proxy client refuses a model outside `llmProxy.allowedModels` and a base
  URL that isn't https on an allowed host suffix. A failed batch returns `outcome:
  "failed"` with what it managed; the scheduler redacts the error. It is due
  whenever `similar.enabled` isn't false but is registered only when the local
  history is on (`background-wiring.ts`); the scheduler only counts registered
  tasks when it works out when to wake, so a dev checkout neither runs it nor
  wakes for it.
- **`similar(item, k)`** (`core/similar-search.js`, `find({key, text, k})`).
  Two rankings merged by reciprocal rank fusion (`1/(60 + rank)`, summed):
  FTS5 over any of the query's words (3+ letters, no numbers or stopwords, at
  most 12, each quoted so FTS5 syntax in the text is only ever a word) and
  cosine >= 0.55 against vectors of the same model and size. A ticket and its
  `analysis:` item count as one result (`jira:ABC-1`); the queried item is
  left out. The query vector is the item's stored vector, else the text
  embedded on-prem (8 s limit); no real vectors, no ready proxy or any
  failure gives words only. `mode` is `"vector+text"` or `"text"`; `k`
  defaults to 5, at most 20. It throws when the history is off, or when a key
  has nothing recorded and no text was given.
- **The prompt block** (`similar.promptBlock`, put in Analyze issue's prompt
  after "Recent comments" and before "Reference URLs"; built in
  `features/analyze-issue/plan.js` `fetchSimilarTickets`, which asks for 20,
  keeps at most 5 `jira:` tickets, gives up after 4 s and never throws): at
  most 5 tickets, title <= 200 and analysis <= 600 characters, the whole
  block (header and fences included) <= 4096 bytes. Only validated `jira:ABC-123`
  keys are shown (a PR or anything else is dropped). Each line says where it
  came from: `ticket title (written by people, untrusted)` and `earlier AI
  output, untrusted (analysis saved YYYY-MM-DD)`. The header says the tagged
  text is data, analyses may be wrong and must be checked, and instructions
  inside are never followed; forged `<similar-past-tickets>` tags are stripped
  (case variants, invisible characters between the letters). Background
  (watcher) runs get it too: it is data, not a tool, so the background policy
  (no MCP servers, no links) is unchanged. A failing search never fails an
  analysis.
- **`similar.enabled`** (`core/settings-background.js`; ⚙ Settings -> Local
  history, "Add similar past tickets to Analyze issue"): on unless saved as
  `false`, live, no restart. Off stops the embed task and the prompt block and
  makes `find_similar` answer `mode: "off"`.
- **`find_similar`** (`core/mcp-memory-tools.js`; a core read tool, always
  offered; `find` is a read verb). `{key?, text?, limit?}`, one of key/text
  required, limit at most 20. Titles (300) and analyses (600) are clipped and
  stripped of control and invisible characters, the `jira:`/`bitbucket:`
  prefixes are removed, each item's provenance is labelled and a `note` says
  the text is untrusted. With the history off it answers `{enabled: false,
  mode: "off", items: []}`. **`companion similar <key|words...>`** calls it;
  one argument that looks like a key (`PROJ-12`, `CI/sample-app#7`, `kind:...`) is
  a key, anything else is words (at most 200 characters).
- **Themes are not built (a documented follow-up).** A job that groups
  review-comment themes would need `review_comment` items, and no code path
  writes them: the only producer would be Address review comments, which is
  disabled product-wide and must not be resurrected or replaced by a new
  reader of review comments. The `review_comment` kind, the `commented_on`
  relation and the `onprem-llm` provenance stay in the schema for it.
- **Not verified live.** The proxy's embeddings path (`/v1/embeddings`), body
  and vector size are modelled on OpenAI-compatible documentation, not
  captured; a mismatch shows as a `failed` embed row while word search keeps
  working. See the live checks under **Known limitations**.

### Claude Code plugin

Phase 8. An optional Claude Code plugin, `ai-companion`, in `plugin/`, listed
by this repo's own marketplace file `.claude-plugin/marketplace.json`
(marketplace `ai-dev-companion`, one plugin, `source: "./plugin"`); no
separate marketplace repository exists. `/plugin marketplace add
~/ai-dev-companion` works from the installed copy (`copyTree` copies
`plugin/` and `.claude-plugin/`), and `/plugin marketplace add <updateSource
url>` from the shared repository.

- **Layout.** `plugin/.claude-plugin/plugin.json` (with the `mcpServers`
  entry), `plugin/bin/ai-companion-mcp.js`, `plugin/skills/{companion-status,
  resume-ticket,pre-push-check}/SKILL.md`, `plugin/hooks/hooks.json` and
  `plugin/hooks/inbox-brief.js`. `core/plugin-files.test.js` checks the
  structure and that the plugin's and marketplace's versions equal the root
  `package.json`'s: a release bumps `package.json`, `plugin.json` and
  `marketplace.json` together, with the other version files
  (**Versions and updating**).
- **No token in the plugin.** Its MCP server runs `node
  ${CLAUDE_PLUGIN_ROOT}/bin/ai-companion-mcp.js`, a launcher that starts the
  installed `~/ai-dev-companion/companion-service/bin/companion.js
  mcp-stdio` (or says the companion service isn't installed and exits 1).
  `companion mcp-stdio` (`runStdio` in `core/mcp-stdio.js`) forwards each
  JSON-RPC line to the running service's stateless `/mcp`, with `mcp.token`
  read from `credentials.enc` by the CLI itself, so the plugin, which is
  copied from a shared marketplace, never holds a secret. Requests are
  handled one at a time, each limited to 60 s and an 8 MB answer; a JSON-RPC
  error body from a non-2xx answer is passed through; every request id gets
  an answer; a closed stdout ends it quietly. When the service is down each
  request gets a JSON-RPC error, "companion service not running (nothing is
  listening on port N)...".
- **Both paths.** Setup's `claude mcp add` stays. If both are active the
  tools appear twice; `node install.js --plugin` says how to avoid it and
  `companion doctor` warns.
- **Skills.** Each has `allowed-tools` limited to named `companion` (and, for
  pre-push-check, `git`) subcommands. Each starts with `companion status` and,
  when the service isn't running or `companion` isn't found, tells the user
  "companion service not running" and stops. `resume-ticket` uses the
  `ticket_workspace` tool (or `companion history <KEY>`) and gives the user
  `companion resume <KEY>` to run in a new terminal; it never starts that
  interactive session.
- **The SessionStart hook** (`hooks.json`, matcher `startup|resume`, timeout
  5 s) runs `inbox-brief.js`, which runs `companion inbox --brief` (3 s limit)
  and prints its first line only if it starts with `AI companion: `. `--brief`
  (`formatInboxBrief`) prints the unseen count and per-kind counts under fixed
  labels (`conflict resolution`, `background analysis`, `review request`,
  `digest`) and never a title, because a hook's output enters the session's
  context and titles are written by other people. It prints nothing when
  nothing is unseen, the service is down or anything fails, and always exits
  0. Turning the plugin off removes it.
- **Doctor.** `checkPluginPath` reads Claude Code's own files
  (`installed_plugins.json` and `settings.json` `enabledPlugins` under
  `$CLAUDE_CONFIG_DIR` or `~/.claude`, read-only; nothing prompts anyone to
  install the plugin) and reports whether Claude Code reaches the companion
  through `claude mcp add`, the plugin, or both (WARN).
- **`node install.js --plugin`** only prints the two `/plugin` lines (and the
  shared-repository variant when `updateSource.url` is https); it makes no
  network call and writes nothing, and exits before anything else in
  `install.js` runs.
- **Not verified live.** That Claude Code expands `${CLAUDE_PLUGIN_ROOT}` in
  the plugin's MCP `args` and the hook `command` is documented behaviour, not
  exercised in tests; a real `/plugin install` is the check.

## Developing

The extension and the background service run from `~/ai-dev-companion`,
not from your git checkout, so editing or rebuilding the checkout changes
nothing on its own. Run `node install.js` in your checkout to copy the
changes over (your settings are kept), then reload the extension from
`~/ai-dev-companion/chrome-extension`. To restart the installed
service after that (or after a config change):
`cd ~/ai-dev-companion/companion-service && npm run service:install`.

To run a development checkout alongside the installed copy instead, set a
different `port` in that checkout's `config.json`, then give its background
job its own name:
`AI_DEV_COMPANION_SERVICE_LABEL=com.example.ai-dev-companion-dev npm run service:install`.

Node 22.13 or newer is required (the history store uses the built-in
`node:sqlite`); `install.js` refuses older versions.

Tests: `npm test` in `companion-service` runs every unit test file; to run
one, use `node --test <file>`, e.g. `node --test setup.test.js`.

`server.ts` never runs `core/paths.js`'s `migrateStateDir()` from a dev
checkout — `paths.shouldMigrate({isStableInstall})` gates it to
`updater.cannotApplyReason() === null`, i.e. only the stable install at
`~/ai-dev-companion` (the same check `GET /update` uses to decide
whether this copy can self-update). A dev checkout running alongside a
live installed copy (see above) must never move — or half-move — that
copy's `~/.ai-dev-companion` out from under it. When skipped this way,
startup logs `[startup] OK - skipped state-dir migration (not the
installed copy)` and carries on normally — nothing else about starting
the service depends on the migration having run. Set
`AI_DEV_COMPANION_SKIP_MIGRATION=1` to skip it even for the stable
install (e.g. while deliberately inspecting `~/.bitbucket-ai-companion` by
hand); this logs the same skip line.

## Why "merge master in," not rebase

The original ask was rebase, but this team already resolves PR conflicts by
merging master in (see PR #5530's own history: `Merge branch 'master' ...
into cis-53543-cleanup-standard`). Merging means the push after resolution
is a plain `git push` — no `--force-with-lease`, and no risk to existing PR
review comments/approvals, which a rebase's history rewrite would disturb.

## Extending it — adding a new feature

Both sides of this project are built around a small plugin shape so
"resolve conflict" isn't hard-wired as the only thing this can do.

### Gating: two independent, optional conditions

Every feature in `chrome-extension/features/` (registered through `registry.js`) declares up to two
conditions deciding when its ✨-menu row appears. Either, or both, can be
omitted:

1. **`urlPattern`** — a `RegExp` tested against `location.href`. Absent
   means "every page this content script runs on" (see `manifest.json`'s
   `content_scripts.matches`). Capture groups (e.g. a ticket key, a PR id)
   are handed to `condition` as `ctx.match`.
2. **`condition(ctx)`** — an async predicate for anything a URL pattern
   alone can't express (page state, an API call). Returns a payload object
   (truthy) to list the feature — POSTed verbatim to `/features/<id>/start`
   — or `null`/`false` to hide it. Absent means "listed whenever
   `urlPattern` matched." Both absent means "always listed." A throwing
   `condition` just drops that one feature for the current refresh; it
   never affects the others.

`resolve-conflict`'s `urlPattern` matches a PR URL of the profile's git host and
its `condition` asks the host whether the PR conflicts; `create-jira-subtasks`'s `urlPattern`
matches a ticket URL and its `condition` checks the ticket's issue type;
`analyze-issue`'s `urlPattern` matches only the configured projects' keys and
its `condition` checks the issue type against the configured list. A feature with neither condition would
show up on every page — useful for something with no page-specific state
at all.

### Four shapes a feature can take

`chrome-extension/content.js` builds on one generic, reusable overlay-panel
primitive (`openOverlayPanel` — a shadow-DOM panel with a title/close
button, scrollable body, and pinned footer any feature can drop content
into) for any of three flows a feature's registry entry can pick. The
panel is **resizable** (drag the left edge; width persisted in
`chrome.storage.local`) and **collapsible** to a thin right-hand rail that
keeps the feature title and expands on click — shared by Resolve Conflict,
Create Subtasks, Analyze issue, and any future
caller of `openOverlayPanel`.

1. **Review a job that's already running** (`resolve-conflict`) — the
   feature starts immediately on click, the ✨ button tracks its progress
   (`progressSteps`), and once it reaches `awaiting-approval`/`failed` the
   panel shows `renderPanel(job)` with Approve/Discard actions.
2. **Collect input, then run to completion** (`create-jira-subtasks`) — the
   feature declares `renderStartForm(payload)` instead; clicking it opens
   the same panel with that form (see `openComposePanel` in `content.js`),
   and submitting POSTs straight to `/start`, which does the whole job
   before responding — no polling, no separate approve step. There's
   always a Close button next to the action button for backing out.
3. **Run to completion with nothing to review afterward**
   (`review-in-editor`) — the feature declares `oneShot: true` instead of
   either of the above; clicking it POSTs straight to `/start` (see
   `content.js`'s `runOneShot`) and the result — `job.result.summary` on
   success, `job.error` on failure — shows directly in the ✨ popover, with
   a **Show Details** button for a failure long enough to need it. No job
   is ever stored: unlike shape 1, there's no approve/reject click to ever
   stop tracking it, so storing one would hijack the FAB on that page
   forever the moment the job finished on its own.
4. **Report something, with nothing to approve**
   (`analyze-issue`, or a pack's reports) — shape 1 with `readOnly: true`
   added. The job is still tracked and polled and the panel still
   auto-opens on `awaiting-approval` with `renderPanel(job)`, but the
   footer gets a single **Done** button instead of Approve/Discard
   (analyze-issue also offers **Re-analyze** via `reanalyzeLabel`, and
   **Add as comment** via `postCommentLabel` → compose Markdown from
   `commentDraftMd`, then companion `POST .../post-comment` with `{ body }`),
   because a report has nothing to apply and nothing to clean up. Done
   clears the stored job locally (so a finished report can't hijack the
   FAB, the same trap shape 3 avoids differently) and makes no server
   call — so the feature's `approve`/`reject` only guard against ever
   being called, as in shapes 2 and 3. Table rows / analysis travel on
   `job.data`, which is free-form, rather than `job.result` (fixed at
   `{ summary, files }`).

Any of the four shapes above can also declare **`footerActions`** — a
generic hook for "one more button in the footer that isn't Approve/Discard
or Done/Re-analyze," so a feature-specific action (Resolve Conflict's
**Refresh diff**, Analyze issue's **Continue in Claude Code**) doesn't need
its own bespoke footer-rendering path. `footerActions?: [{id, label,
when(job) -> boolean, run(job, api) -> Promise<job|void>}]`.
`content.js`'s `showPanel` renders one button per entry whose `when(job)`
is true, in *either* footer shape (the readOnly Done/Re-analyze/Add-as-
comment row, or the Approve/Discard row — including a `failed` job, where
Discard already shows but Approve doesn't; that row's guard is
`showApprove || showReject || activeFooterActions.length > 0` so a
footerAction is never stranded on a status neither of those two cover).
Clicking calls `run(job, api)` with `api = { send(type, extra),
rerender(job), close(), showError(msg) }`:
- `send(type, extra)` POSTs a `background.js` message with
  `featureId`/`jobId`/`pageUrl` already filled in (`extra` merges over
  those — e.g. `{ body: { resume: true } }` for Continue in Claude Code).
- `rerender(job)` redraws the whole panel (body **and** footer) from a new
  job — for an action whose result can change what the footer should show
  next, e.g. Refresh diff flipping `failed` → `awaiting-approval`.
- `close()` closes the panel; `showError(msg)` shows `msg` in the same
  error box Approve/Discard use (`textContent` only — never render
  server-supplied text as HTML).

A button click disables itself, clears the error box, awaits `run`, and on
a rejected promise shows `"<label> failed:\n\n<message>"` via
`showError` as a fallback — an action that calls `api.showError` itself
inside `run` (Continue in Claude Code does, to phrase its own message)
never reaches that fallback.

### Adding one

- **Service side**: implement the `Feature` interface in
  `companion-service/core/jobs.ts` (`start`/`approve`/`reject`, each also
  given a `FeatureContext` carrying the relayed browser-session cookie and
  the request body) as a new file under `companion-service/features/<name>/`,
  using whatever `companion-service/core/*` helpers you need (`worktree.ts`
  for a disposable per-job git workspace, `review-worktree.js` for a
  persistent per-repo one instead, `claude.ts` to run Claude headlessly,
  `editor.ts` to open a path in the user's chosen editor, `diff.ts` for
  per-file diffs, `atlassian.ts` for authenticated Jira/Confluence REST
  calls — or none of them, if the feature needs none of that). A feature that runs to completion inside
  `start()` (shapes 2 and 3 above) doesn't need a real `approve`/`reject`
  — see `create-jira-subtasks`' and `review-in-editor`'s versions, which
  just guard against ever being called.
- **Declare it**: write `companion-service/features/<id>/feature.js` and list
  it in a pack (`packs/builtin.js`, or another pack file named in
  `environment.js`'s `packs`; see `core/packs.js` for the full spec shape).
  It holds the setup `descriptor` (below), `factory: () => require("./index").createXFeature`
  (a thunk, so `setup.js`, which runs without a build, never loads the
  implementation, and a disabled feature's module is never loaded), and
  optionally `mcpTools` (the catalog entries it owns), `scopeKey`
  (`"bitbucket-pr"`, `"jira-issue"` or a function), `persist: false`
  (jobs are not written to disk) and `history` (`readOnly`, `milestone`,
  `eventOnly`). The registry, `server.ts`, the MCP catalog, scope keys, job
  persistence and history recording all read these from the loaded packs.
- **Make it selectable in setup**: the `descriptor` in that file — `id` (matching the
  `Feature`'s own id), `label`/`description` for the setup wizard's
  enable/disable prompt, optional `enabledByDefault: false` to ship the
  feature listed in setup but off until the user opts in (omit or `true`
  keeps today's behavior: fresh-install default on, and soft-migrate via
  `knownFeatures` auto-enables it for existing installs), `requiredChecks`
  (names from `core/prereqs.js`'s `CHECKS_BY_NAME` — empty if the feature
  needs nothing beyond Node), `needsRepos: true` if it works on a PR's
  local clone (the service then resolves the clone before starting it and,
  if none is found, answers 409 `repo-not-found` so the extension can
  offer to choose or clone it), and `promptSetup(rl, helpers,
  existingConfig)` returning whatever partial config your feature needs
  (called only when a user actually enables it). The descriptors, gathered by
  `core/feature-registry.js` from the loaded packs, are the single source of
  truth setup.js *and* `server.ts` both drive themselves off of; nothing in
  either file's prompting/startup-check/routing logic needs to change.
- **Extension side**: add `chrome-extension/features/<id>.js` calling
  `PaiRegistry.register({...})` (name it in the feature's `extension: { script, order }`
  — `order` is its row's place in the ✨ menu — and `core/extension-manifest.js` adds it to
  the manifest's script list) with the same `id`, a `site` (`"git"`, `"issues"` or `"ci"`:
  the kind of server the feature belongs to, so it isn't offered on a page of another
  configured server), a `menuLabel`, the two gating conditions above, and one of
  `renderPanel(job)` (shape 1 — return a bare `Node`, or
  `{ node, getApprovalPayload }` if Approve needs to send something other
  than an empty body), `renderStartForm(payload)` (shape 2 — return
  `{ node, getPayload }`; `getPayload()` may throw to block submission with
  a message), or `oneShot: true` (shape 3 — nothing else to declare;
  `content.js`'s `runOneShot` handles the whole click-to-result flow
  generically). Optionally add `footerActions` (see above) for any extra
  footer button beyond Approve/Discard/Done. `content.js` loops over this
  array — and already filters it against whatever the service reports as
  enabled (via `GET /features`), so a disabled feature's row just doesn't
  appear; nothing about that filtering needs to change either.

**Actions, deps and the footer (Phase 6).** A feature can declare
`actions: { name: async (job, ctx) => … }`, served at
`POST /features/<id>/:jobId/actions/<name>` (404 for an unknown job, a job of
another feature, or an unknown action; errors go through `sendError`; the
extension reaches it with the background message `feature-action`). Factories
receive `FeatureDeps { history }`, so the history opens before the features
are built. The footer API gains `action(name, body)` and
`startFeature(featureId, payload)`, `when(job, env)` gets
`env.enabledFeatureIds`, and a feature can set `rejectLabel` for its
Discard button (Ticket to PR uses "Stop tracking").

## Security notes

- The companion service binds to `127.0.0.1` only — it is not reachable
  from the network.
- **The Host header is checked on every route**, not just `/mcp`: only
  `127.0.0.1:<port>` and `localhost:<port>` are answered, anything else gets
  a 403, so a web page can't reach the service by pointing its own
  hostname at 127.0.0.1 (DNS rebinding).
- Every request must carry an `X-Companion-Secret` header matching
  `sharedSecret` in `config.json`; anything else gets a 403. Extension calls
  are still relayed through `background.js` (the MV3 service worker) rather
  than made directly from `content.js` — a content-script `fetch()` would
  carry the *page's* origin (`https://bitbucket.example.com`), which
  isn't useful for identifying the extension either way, and keeping the
  one place that knows the secret out of content-script code (which shares
  more of its execution context with the page) is the more conservative
  choice.
  - **Why not check the `Origin` header**, which would seem like the
    obvious approach (an earlier version of this did exactly that): this
    extension declares `http://127.0.0.1/*` in its manifest's
    `host_permissions`, and Chrome treats a fetch from an extension to a
    host covered by its own `host_permissions` as CORS-exempt — no
    preflight, no `Origin` header sent at all. This was confirmed by
    driving the real loaded extension's service worker (via Playwright) and
    inspecting the request the companion service actually received:
    `sec-fetch-site: none` and no `origin` header. An Origin-based allowlist
    silently rejects every legitimate call in this setup, which is exactly
    the `origin not allowed: (none)` error this was fixed in response to.
- Claude runs headlessly with `--permission-mode bypassPermissions` (see
  `companion-service/core/claude.ts`), meaning it can edit files and run
  bash commands without an interactive approval prompt. Every call passes
  one of four **named policies** from `core/claude-args.js`'s
  `CLAUDE_POLICIES`, rather than a hand-listed `tools` array per call site:
  - **`readOnly`** (Analyze issue) — `Read`, `Glob`, `Grep`, `WebFetch`,
    `WebSearch`, and `git log`/`blame`/`show` via `Bash`; no `Edit`/`Write`,
    no general `Bash`. It never pushes and has no Approve step.
  - **`worktreeWrite`** (Resolve Conflict) — `Read`, `Edit`, `Write`,
    `Bash`, `Glob`, `Grep`, scoped by `cwd` to a disposable worktree
    (`~/.ai-dev-companion/worktrees/...`) that is never pushed to the
    real branch until you click **Approve & Push** in the review panel.
    Clicking **Discard** (or just closing the tab and never approving)
    throws it away.
  - **`worktreeWriteNarrowBash`** (Address review comments) — the same
    `Read`/`Edit`/`Write`/`Glob`/`Grep` grant and worktree confinement as
    `worktreeWrite`, but `Bash` is narrowed to `git diff`/`git status` plus
    one `Bash(<cmd>:*)` rule per configured
    `addressReviewComments.checkCommands` entry
    (`checkCommandRules`/`normalizeCheckCommands`) — and, unlike a bare
    `--allowedTools` grant, actually enforced: see **The Bash guard**
    below.
  - **`noTools`** — no tools at all (e.g. analyze-issue's repo-pick prompt,
    which is told to answer in text only).

  `readOnly` additionally wires in `core/mcp-guard.js` as a `PreToolUse`
  hook (passed via `--settings`, matcher `mcp__.*`): for every MCP tool
  call — including ones from a server Claude only discovers via
  `~/.claude.json` at run time, not ones a feature explicitly named —
  the hook classifies the tool name (`core/mcp-tool-classifier.js`) and
  denies anything whose action isn't a recognized read verb (`get`,
  `list`, `search`, `query`, `read`, `grep`, `glob`, `find`, `download`,
  `view`, `fetch`, `ask`, optionally prefixed by a service namespace like
  `jira_get_issue`). The hook **fails closed**: Claude Code only treats
  hook exit code 2 as blocking, so any unexpected failure in the hook
  itself (a parse error, the classifier module failing to load, malformed
  stdin) is caught and denies with exit 2, rather than risking exit 0
  (which Claude Code would treat as "allow") on a code path nobody
  anticipated. `runClaude()` refuses to start a `readOnly` run at all if
  `dist/core/mcp-guard.js` is missing, rather than silently running that
  policy with no guard; `npm run doctor` also reports whether that script
  is present, plus the protected-path rule count below. Analyze issue also
  keeps its own hand-maintained `MUTATING_MCP_TOOLS`
  deny list (`features/analyze-issue/plan.js`, passed as
  `--disallowedTools`) alongside the guard, as defense in depth — the two
  are not code-shared, so keep them conceptually aligned by hand if either
  changes.
  **None of the above governs the resumed session** — `POST
  /jobs/:jobId/open-in-claude-code`'s `{resume: true}` handoff (see
  Session capture and the resume route). `claude --resume` there runs
  interactively with the user's own Claude Code settings, no `--settings`
  guard hook or sandbox, and no `--disallowedTools`, so this
  policy/guard/deny-list/sandbox section describes the automatic,
  headless analysis run only — not anything the user does after clicking
  Continue in Claude Code.
  All three policies also add `protectedPathRules()` (`core/claude-args.js`)
  to `--disallowedTools`: `credentials.key`, `credentials.enc`,
  `config.json` and `~/.claude.json` are denied to `Read`, as are the
  common places a developer's own credentials live (`~/.ssh`, `~/.aws`,
  `~/.netrc`, `~/.gnupg`, `~/.config/gh`, `~/.npmrc`,
  `~/.docker/config.json`, `~/.kube`, and the legacy
  `~/.bitbucket-ai-companion` state dir — `SECRET_HOME_DIRS`/
  `SECRET_HOME_FILES`), and the whole `companion-service/` tree is denied
  to `Edit` — regardless of policy.
  Those rules alone never governed `Bash`, so any policy granting it
  (`readOnly`'s scoped `git log`/`blame`/`show`, `worktreeWrite`'s
  unscoped `Bash`) is additionally run under Claude Code's own Bash
  sandbox (`--settings`'s `sandbox` key, merged into the same
  `--settings` object as the `readOnly` MCP guard hook above — there is
  never more than one `--settings` flag): `core/claude-args.js`'s
  `isSandboxedPolicy()` decides which policies need it (any that grant
  `Bash` or `Bash(...)` — `noTools` never does), and `sandboxSettings()`
  shapes it with `enabled: true`, `allowUnsandboxedCommands: false` (a
  command the sandbox can't mediate fails rather than running
  unsandboxed) and `failIfUnavailable: true` (if the OS sandbox mechanism
  itself can't start, the run fails instead of silently running Bash
  unsandboxed). `filesystem.denyRead` is the same protected paths as
  `protectedPathRules()`'s `Read` rules, just as plain absolute paths rather than the
  `//abs` permission-rule form (`protectedSandboxPaths()`) — this closes
  a real gap: without it, `worktreeWrite`'s `Bash` grant could `cat`/`cp`
  a protected file that `Read` alone would have denied.

  `readOnly` has `denyCwdWrite: true` on its `CLAUDE_POLICIES` entry, which
  makes `buildClaudeArgs` add `cwd` to `filesystem.denyWrite` by itself
  (its Bash grant has no legitimate reason to write anywhere, e.g.
  `git log --output=<file>`) — this lives on the policy, not a
  per-policy-name `if` in `core/claude.ts`, so any future Bash-granting
  policy that wants the same default just sets the flag on itself.
  `worktreeWrite` (Resolve Conflict) sets `filesystem.allowWrite` to the
  worktree's git dir (`gitDir` = `git rev-parse --absolute-git-dir`, run
  inside the worktree) alongside the sandbox's own default allow of `cwd`
  — git keeps a worktree's index/HEAD in the *main* repo's
  `.git/worktrees/<id>`, outside the worktree checkout itself, so
  `git add`/`git commit` there need that path allow-written explicitly.
  Live-verified: Claude Code's sandbox already default-allows writes
  under the git working tree it detects `cwd` belongs to (a loose object
  written straight to the *main* repo's `.git/objects/`, well outside
  both `cwd` and the explicit `allowWrite`, succeeded with no `allowWrite`
  change needed) — this is exactly why `git add -A`, which writes objects
  there, works with only `gitDir` explicitly allow-written; a write to an
  unrelated directory named nowhere in `allowWrite` still fails, and
  Claude's own denial message names the sandbox's actual default allow
  list (the worktree, a couple of Claude-owned temp dirs, nothing else).
  The sandbox works on macOS (Seatbelt), Linux and WSL2 (bubblewrap +
  socat, both required on PATH — `npm run doctor` checks and reports a
  fix); it is not supported on native Windows, where any Claude run that
  needs Bash will fail (also reported by `npm run doctor`).
  - **Sandbox escape via the worktree's own git metadata, and the fix.**
    `allowWrite: [gitDir]` above is necessarily broad — but three files
    under `gitDir` (`commondir`, `gitdir`, `config.worktree`) and the
    worktree's own `.git` gitlink FILE (inside `cwd`, so writable by
    default) each decide how git itself behaves on ITS OWN next
    invocation (`core.hooksPath`, `core.fsmonitor`, or simply pointing
    git somewhere else entirely) — exactly what the service's own next
    `git add -A`/`git write-tree`/etc (`stageAll`, `renderReview`) would
    then run under. `sandboxDenyWrite` in `features/resolve-conflict/index.ts`
    denies all four explicitly on the Bash sandbox, live-verified to win
    over the broader `allowWrite` (an attempted rewrite of
    `<gitDir>/gitdir` via a Bash redirect was denied); the `editRoot` hook
    below denies the same four paths again for the Edit/Write tools
    specifically, since they're outside `editRoot` (`gitDir`) or touch a
    `.git` segment (the gitlink). As a fallback
    for whatever that misses, `core/worktree-integrity.js` snapshots all
    four right before Claude runs and compares right after; if anything
    changed, the job is marked `failed` with "Claude modified this
    worktree's git metadata, so it was not trusted. Discard the job."
    and — critically — **no git command runs in or against that worktree
    ever again**: `approve()`/`refreshDiff()` refuse outright, and
    discarding removes the worktree directory with plain `fs.rmSync`,
    never `git worktree remove` (which would itself read the
    now-untrusted `gitdir` file from the trusted main repo). This
    deliberately leaves the main repo's own `.git/worktrees/<id>`
    registration behind rather than risk touching it with git at all — a
    small, inert leftover, safe to clean up by hand.
  - **`Edit`/`Write` are not covered by the Bash sandbox at all** — its
    `filesystem` rules only ever mediate Bash. Live-verified: with
    `sandbox.enabled` and `filesystem.denyWrite` set, Claude's own Write
    tool still wrote a file outside the worktree with no denial at all.
    A spike tried moving `worktreeWrite` from `--permission-mode
    bypassPermissions` to `acceptEdits` (on the theory that an edit
    outside the working directory needs an interactive prompt `-p` can't
    answer, and so would be denied) — live-verified NOT true for this
    Claude Code version: under `acceptEdits`, a Write outside the
    worktree was silently auto-accepted, identically to
    `bypassPermissions`. So `worktreeWrite` stays on `bypassPermissions`
    (`readOnly` and `noTools` have no `Edit`/`Write` grant at all, so
    this doesn't apply to them).
    - **First attempt (reverted): plain `--disallowedTools` permission-rule
      strings** (`Edit(//<home>/**)`, `Write(//<home>/**)`, etc). These
      are glob-like string patterns with NO filesystem resolution — live-
      verified to be a real regression in production: once the worktree
      lives under `$HOME` (as it does there,
      `~/.ai-dev-companion/worktrees/...`, not under `/tmp` as this
      task's own first live checks happened to use), a broad enough
      `Edit(//<home>/**)` deny ALSO matches paths INSIDE the worktree
      itself, denying Claude's own Edit calls on the very files it needs
      to edit (`permission_denials: ["Edit","Edit"]`; Claude fell back to
      `sed` via Bash instead). Dropped entirely once the fix below covers
      the same ground correctly.
    - **Fix: a fail-closed PreToolUse hook, same pattern as the MCP
      guard.** `core/edit-guard.js` (+ pure `core/edit-path-policy.js`,
      unit-tested with temp dirs), wired in via `buildClaudeArgs`'s
      `editRoot` option, matcher `Edit|Write|MultiEdit|NotebookEdit`.
      Reads the hook JSON's `tool_input.file_path` (or `notebook_path`
      for `NotebookEdit`), resolves it to a real absolute path one
      component at a time with `lstat` (`resolvePathSafely`) — a symlink
      component is `realpath`'d, `..` is resolved physically from the
      real directory it's in, and the first component that doesn't exist
      yet ends the walk, so a not-yet-created file (a `Write` target)
      still resolves — and allows the call only if that real path is
      strictly inside the allowed root AND doesn't touch a `.git` path
      segment anywhere (covers the root's own gitlink file and anything
      nested under a `.git` directory). It also denies: any symlink in the
      path that can't be resolved (dangling, a loop); the final component
      being a symlink at all, dangling or live; and an existing target
      with `nlink > 1` (a hardlink may share its inode with a file outside
      the root). An earlier version walked up with `fs.existsSync`, which
      follows symlinks — so a dangling `<root>/link -> ../outside/new.txt`
      looked like a brand-new file inside the root, and a `Write` through
      it landed outside. The allowed root is passed as a third,
      shell-quoted argv entry on the hook command itself (after the node
      binary and the script path), computed once by
      `features/resolve-conflict/index.ts` as
      `fs.realpathSync(worktree.dir)`. Same fail-closed contract as
      `core/mcp-guard.js`: malformed/empty stdin denies (not crashes),
      and any internal error (missing policy module, no root argument)
      denies AND exits 2. All the hooks (MCP guard, edit guard, read
      guard, bash guard) share one `PreToolUse` array in the same
      `--settings` object — Claude Code runs every matching entry, and
      there is still never more than one `--settings` flag.
    - **Reads are confined too: the read guard.** `core/read-guard.js`
      (matcher `Read|Glob|Grep`, same fail-closed contract, same root as
      the edit guard) is wired in for both worktree policies
      (`readGuard: true`; `buildClaudeArgs` refuses such a policy without
      `editRoot` and the script, and `runClaude` refuses to spawn if the
      script is missing). Without it, `Read`/`Glob`/`Grep` could open any
      file the user can — the `Read(...)` deny rules above only cover the
      files they name — and the PR content Claude works from is untrusted.
      It checks `Read`'s `file_path` and `Glob`/`Grep`'s optional `path`
      (absent means the cwd, i.e. the worktree; relative resolves against
      it) with the same symlink-safe resolution, and denies a `Glob`
      `pattern` that is absolute or has a `..` segment. It deliberately
      skips the edit-only rules: reading inside `.git` or a hardlinked
      file is fine. It governs those three tools only: Resolve Conflict's
      unscoped `Bash` can still read outside the worktree wherever the
      sandbox's `denyRead` doesn't reach (the protected paths above), and
      Address review comments' Bash is held to `git diff`/`git status`
      (minus the flags below) and its configured checks.
    - **Live-verified with the worktree placed under a real `$HOME`
      subdirectory** (the exact shape that exposed the regression above):
      (A) an Edit inside the worktree succeeds with `permission_denials:
      []`; (B) a Bash write into the main repo's `.git/hooks` is still
      blocked by the sandbox; (C) a Write to a sibling of the worktree,
      also under `$HOME` but outside it, is denied
      (`permission_denials: ["Write"]`, file not created); (D) a Write to
      `<worktree>/.git` (the gitlink) is denied the same way, and the
      gitlink's content on disk is confirmed unchanged afterward. Re-run
      (`claude-haiku-4-5`) after the symlink and read-guard fixes: (E) a
      `Write` through a committed dangling symlink
      `link -> ../outside-new.txt` is denied (`["Write"]`, file absent);
      (F) a `Read` of a file in a `$HOME` sibling directory is denied
      (`["Read"]`, contents not returned), as is a `Grep` whose `path` is
      that directory; (G) a `Read` inside the worktree still works.
    - `worktreeGitEditDenyRules()` (the old `--disallowedTools`-based
      helper) was removed outright rather than kept narrower — the hook's
      "outside root" check already covers what its `<repoPath>/.git/**`
      and `<gitDir>/**` rules covered (both are outside `editRoot`, the
      worktree itself), and its "`.git` segment" check already covers
      `<worktreeDir>/.git`; keeping a redundant, less-precise mechanism
      alongside the hook would just be a second place to keep in sync (or
      to regress) for no added coverage.
  - **Sandboxed builds/tests can't write outside the worktree.** A
    resolve-conflict run that tries a quick build or test step (step 4 of
    `features/resolve-conflict/prompt.ts`'s prompt) can only write inside
    the worktree — a step that needs a global cache (`~/.npm`,
    `~/.gradle`, etc) is expected to fail for that reason alone, and
    isn't itself a sign the conflict resolution is wrong. The prompt
    says so explicitly.
  - **The confinement stack for a worktree job**, top to bottom: the Bash
    sandbox above, with its `denyRead`/`denyWrite`; the `PreToolUse` edit
    guard, resolving every `Edit`/`Write`/`MultiEdit`/`NotebookEdit`
    target to its real absolute path and allowing it only strictly inside
    the worktree with no `.git` path segment anywhere, in any case, and
    never through a symlink or a hardlink; the `PreToolUse` read guard,
    confining `Read`/`Glob`/`Grep` to the worktree the same way; a
    `PreToolUse` bash guard restricting `Bash` itself to an allowlist of
    command prefixes with no shell metacharacters allowed at all; the
    `core/worktree-integrity.js` snapshot of the worktree's own git
    metadata, taken as soon as the worktree exists — before the merge
    (Resolve Conflict) or before Claude runs at all (Address review
    comments) — compared right after Claude runs, and compared again
    before the git that **Refresh diff** and **Approve** run (both
    features — a resumed Claude Code session can change the worktree
    after the first compare; both also check before discarding); and, at
    Approve, pushing only the tree fingerprint the
    panel actually showed you, on a `HEAD` the job recognizes as its own
    (`core/reviewed-push.ts` — see **How each feature works** above for
    both features' exact rules). The snapshot is cached in memory and
    persisted to `~/.ai-dev-companion/jobs/<id>.integrity.json`
    (`core/integrity-baselines.js` — stateDir, outside both the sandbox's
    and the edit guard's write reach, and never inside the worktree), so
    it survives a service restart on the installed copy. In a dev
    checkout it's kept in memory only (see **Jobs across restarts**), so
    after a restart there the job fails closed. **Refresh diff**, **Approve** and
    **Discard** all fail closed when it's missing: the job is marked
    untrusted rather than trusted by default. That post-run compare only
    means something once Claude has actually exited, so a service
    shutdown kills the headless Claude's whole process group first, and
    after a crash **Refresh diff** and **Approve** refuse, before any git
    or integrity check, while the Claude recorded in `data.claudePid` is
    still alive (see **Jobs across restarts**). The resumed Continue-in-
    Claude-Code session is unconfined, as noted above, so these files
    aren't a boundary against it — only against everything that has to
    go through the sandboxed run itself.
    **Resolve Conflict gets five of the six, not all six: the bash
    guard is exclusive to Address review comments.** Resolve Conflict
    runs under `worktreeWrite`, which grants unscoped `Bash` with
    `bashGuard: false` (`core/claude-args.js`'s `CLAUDE_POLICIES`) — there
    is no command allowlist at all, so its Bash is bounded only by the
    sandbox's filesystem rules (`denyRead`/`denyWrite`/`allowWrite`),
    broad on purpose, since resolving a conflict may need to run whatever
    build or test commands the prompt tries. `worktreeWriteNarrowBash`
    (Address review comments) is the one policy with `bashGuard: true`,
    and the bash guard is what turns "no command allowlist" into
    "`git diff`/`git status` plus configured check-command prefixes,
    nothing else" — on top of the same sandbox both policies get.
  - **The Bash guard, and why `--allowedTools` alone isn't enough to
    narrow `Bash` itself.** Live-verified (`claude` 2.1.281, `-p
    --permission-mode bypassPermissions`): an `--allowedTools` entry like
    `Bash(git diff:*)` only *pre-approves* matching calls — it restricts
    nothing else. `touch probe-made.txt && echo MADE` ran with **zero**
    denials under a policy whose `--allowedTools` never mentioned `touch`
    or `&&`. That's why `core/bash-guard.js` exists: wired in as a
    `PreToolUse` hook (matcher `Bash`) whenever a policy sets `bashGuard:
    true` (today, only `worktreeWriteNarrowBash`), it's what actually
    says no. `core/bash-command-policy.js`'s `isBashCommandAllowed` denies
    any command that isn't *exactly* one of the allowed prefixes (or one
    of them followed by a space and arguments), with no shell
    metacharacter anywhere (`; & | \` $ ( ) < >`, a bare backslash, or a
    line break) — so an allowed prefix can never be chained, piped,
    redirected, or extended into a second command. The allowed prefixes
    are derived from the very same `Bash(<prefix>:*)` rules
    `--allowedTools` pre-approves (`prefixesFromBashRules`), so the two
    can never disagree. `git diff` is additionally refused with
    `--no-index` (compares any two paths on disk — a read the read guard
    can't see, since it's Bash), `--output` (writes a file) or
    `--ext-diff` (runs an external program), anywhere in its arguments,
    in `--opt=value` or quoted form, or abbreviated. Same fail-closed
    contract as the MCP and edit guards: malformed stdin or a missing
    argument denies **and** exits 2.
    - **The boundary this doesn't move: a configured check command still
      runs your repo's own code, which Claude can edit first.**
      `addressReviewComments.checkCommands` (e.g. `"npm test"`) becomes
      both an `--allowedTools` rule and a bash-guard prefix, but the guard
      only checks the command *string* — it has no idea whether `npm
      test` still does what it did a minute ago, and Claude was just
      given `Edit`/`Write` on this very worktree. A bare prefix is also
      wider than it looks: `"npm"` (instead of `"npm test"`) also allows
      `npm exec ...`, since the guard's rule is "the prefix, or the
      prefix plus a space and anything else." The sandbox — not the bash
      guard, not the command name — is the real boundary here: it's what
      stops that command from reading or writing anywhere outside the
      worktree, regardless of what it turns out to run. The README's
      Address review comments section says this in one line too.
  - **Commit hooks run unsandboxed, at Approve.** Everything above governs
    Claude's own run; `git commit` at **Approve** (both features) runs as
    the companion service itself, outside Claude's sandbox entirely —
    Resolve Conflict skips hooks for its merge commit (`--no-verify`), but
    Address review comments commits new PR code with hooks **on**, on
    purpose (that's what a repo's pre-commit checks are for). A hook is
    still arbitrary code running with your own permissions the moment you
    click Approve — and it may execute *any* file the change touches (a
    test it runs, a module a config imports), not only hook files. The
    review panel points at the likeliest places: any changed file on a
    path that commonly runs at commit or CI time (`.husky/`, `.githooks/`,
    `.git-hooks/`, `.github/workflows/`, `.yarn/`, `package.json`,
    `.pre-commit-config.yaml`, `Jenkinsfile`, `Makefile`, `lefthook.yml`,
    `.npmrc`, `.yarnrc*`, and lint-staged, ESLint, Prettier, Babel, Jest
    and commitlint configs — `chrome-extension/features/address-review-comments.js`'s
    `isRiskyChangePath`) gets a banner above the diff naming it. That list
    is best-effort, and the banner says so: no banner doesn't mean a
    change is safe, so the whole diff still needs reviewing. The pre-push
    tree check (`prePushError`) only guarantees what gets *pushed* is what
    you reviewed; it can't undo what a hook did on your machine.
- Push credentials are whatever your existing clone already uses (SSH
  agent or HTTPS credential helper) — the service doesn't manage or store
  any separate secret.
- **Jira features relay your browser session rather than storing a
  credential.** `background.js` reads the page's cookies via
  `chrome.cookies` (the only API that can see `HttpOnly` session cookies
  like a Jira `JSESSIONID` — `document.cookie` in a content script cannot)
  and sends them to the companion service as `X-Relay-Cookie` /
  `X-Relay-Origin` headers, alongside the same `X-Companion-Secret` every
  other request needs. `companion-service/core/atlassian.ts` only ever
  uses that cookie against a site whose base URL origin matches
  `X-Relay-Origin` exactly, and only as the *first* of two attempts — if
  Jira rejects it (expired session, or the service running under a
  different login than the browser), it falls back to a configured
  `jira.apiToken`, and fails with an actionable message (shown right in the
  review panel) telling you to add one if neither works.
  - **Since v0.9.0 the relayed cookie is also cached, which makes this
    more far-reaching than a same-origin `fetch(..., {credentials:
    "include"})` from the content script.** The session vault (see
    **Session vault**) keeps the last cookie relayed for each configured
    Jira, Jenkins and Bitbucket origin in memory for up to
    `sessionCache.ttlMinutes` (default 30), and exactly those origins,
    matched exactly. A cached cookie can be used by any holder of
    `mcp.token`, and Claude Code and Cursor keep that token in plain text
    in `~/.claude.json` and `~/.cursor/mcp.json` (owner-only in practice).
    The blast radius is bounded: the MCP tools are read-only, or queue a
    job that needs a click in Chrome (below). It is still your Jira,
    Jenkins and Bitbucket session, readable by anything that has the token
    for as long as the cache holds it. `sessionCache.ttlMinutes: 0` turns
    the vault off, and the heartbeat that keeps it warm is off by default.
  - **Never logged, never written to disk.** A cookie, `mcp.token` and
    `sharedSecret` are never logged, returned by `/settings` or put in an
    error message. The vault is a `Map` in memory; the token's only copy on
    disk is in `credentials.enc` (plus the client configs above). `/mcp`
    logs only the tool name, outcome and duration. `doctor` compares tokens
    without printing them.
- **`/mcp` has its own trust boundary.**
  - **A separate token.** `mcp.token` is accepted only on `/mcp`, and the
    extension's `X-Companion-Secret` is not accepted there at all, not even
    compared. So a leaked `~/.claude.json` can call the MCP tools but can't
    drive any of the extension's routes (start, approve, push, settings).
  - **Setup exposure.** During `claude mcp add`, at setup and on rotation,
    the token is briefly visible in that process's arguments (the command
    takes it as `--header`; there is no other way to pass it), and Claude
    Code and Cursor store it in plain text. If you think it leaked, rotate
    it in Settings; Claude Code and Cursor are re-registered
    automatically. The CLI's own error output is redacted before it is
    shown.
  - **Host check, then token, then body.** Only `127.0.0.1:<port>` and
    `localhost:<port>` are answered, which stops a web page from reaching
    `/mcp` by pointing its own hostname at 127.0.0.1 (DNS rebinding). Both
    checks run before the body is parsed.
  - **No MCP tool pushes or posts.** They read, run analyze-issue under the
    `readOnly` policy, or queue a job.
  - **The pending-start click.** Resolve Conflict and Address review
    comments, which give Claude `Bash`, can only be queued. They start via
    `POST /jobs/:jobId/start`, behind the extension's secret, which terminal
    Claude doesn't have. Approve and push remain separate clicks after
    that.
  - **The `ai-companion` deny.** The MCP guard denies every
    `mcp__ai-companion__*` call and analyze-issue doesn't offer that server
    to the run, so a headless Claude run can't call back into the
    companion, and Claude working on untrusted ticket or PR text can't
    reach the vault through it.
- **Selected text and build URLs are untrusted.** Open in editor takes text
  from a web page: it is capped at 8 KB, parsed into at most 20
  locations, and each must match a file listed by `git ls-files` inside a
  configured repo root, so no path outside those repos is opened. The
  editor is launched with an argv array, never a shell string. Diagnose
  re-validates the build URL on the service and uses only the rebuilt URL;
  the terminal command and prompt are built from argv words. Both routes
  sit behind the Host guard and the secret. The optional `riskFacts` feed
  is validated (size, types, no control characters) before it can reach a
  prompt.
- **Ticket and commit text is untrusted (Ticket to PR).** Summaries,
  analyses and commit messages can carry anything. The PR-draft run uses the
  `noTools` policy with the text inline, so a poisoned ticket has no tool to
  send data out with; pushing, opening the PR, the Jira link and the
  transition happen only on the **Create PR** click. The Start fix prompt
  fences the analysis as untrusted notes and is a single argv word that
  `core/terminal.ts` quotes; every git and terminal process is started with an
  argv array, never a shell string. Branch names are built from
  `[a-z0-9-]` slugs and re-checked with `isSafeBranchName` (and git's own
  check). A persisted job file is untrusted too, so the worktree folder is
  recomputed from config before anything runs there. Error text shown to the
  panel has URL credentials and tokens masked, and the history stores only
  ids, paths, branch names, titles and URLs.
- **Background work (Phase 7).** A watcher's run has no click behind it, so
  it gets no way out: Analyze issue runs under `readOnlyBackground` with no
  WebFetch/WebSearch, no MCP servers and no links; Resolve Conflict runs
  sandboxed in its disposable worktree and can't push (only a person's Approve
  does); the review watcher only fetches. The `background` flag is set only by
  the service's own watcher, never from a request. Ticket and PR text is
  untrusted in the triage prompt (fenced, flattened, forged fences stripped)
  and the on-prem answer can only veto a run the rules allow. The proxy key
  lives in `credentials.enc` and only the on-prem models are ever sent
  anything; the state files (`scheduler.json`, `notifications.json`) hold ids,
  keys, clipped titles, https links and counts, never credentials, and error
  text passes through `redactSecrets`. Every new route sits behind the Host
  guard and the secret check, and no new MCP tool writes.
- **Similar items and the plugin (Phase 8).** Text sent for embedding is
  already-stored, clipped, secret-masked history text and goes only to the
  on-prem model (the allowed-models list, https on an allowed host suffix,
  `chooseTier("embed")`). The similar block in a prompt is untrusted data:
  fenced, flattened, labelled with where it came from, limited to validated
  ticket keys, never a tool, and unable to widen a background run's policy.
  `find_similar` and `companion similar` clean the text they return the same
  way. The plugin carries no credential (it runs `companion mcp-stdio`, which
  reads `mcp.token` itself); the SessionStart hook puts counts and fixed
  labels into a session, never a title.

## Known limitations / open items

- **Open in editor and Diagnose are macOS-only** (they use `open` and a
  terminal window). Chrome may cut a long selection. A file name that
  matches several files fails with a list, and only a Bitbucket page's repo
  breaks ties (no Jenkins-job-to-repo mapping).
- **Background mode is macOS-only.** It's built on launchd, which other
  platforms don't have (`service.js` refuses anything but macOS). Elsewhere,
  setup builds the service but you start it with `npm start` in a terminal (or
  under your own systemd/pm2/etc.). A cross-platform service is deliberately
  not built until a teammate on Linux or Windows needs it. It would take: a
  pure `core/service-manager.js` generating a systemd user unit
  (`~/.config/systemd/user/ai-dev-companion.service`, `systemctl --user
  enable --now`, optionally `loginctl enable-linger`) and a Windows Task
  Scheduler logon task (`schtasks /Create /SC ONLOGON`, no admin rights);
  `service.js` and doctor learning both; "Open in Claude Code" and Diagnose
  falling back to copying the command where there is no known terminal;
  features that need Claude's Bash sandbox (not available on native Windows)
  staying disabled there with a clear message; and a live check by that
  teammate.
- **Some jobs live in memory only.** Resolve Conflict, Analyze issue, Create
  Subtasks and Ticket to PR jobs survive a restart on the installed copy (see
  **Jobs across restarts**): reload the PR page and click **Refresh diff**.
  Review in editor rebuilds its jobs from its live session records
  (`restoreLiveSessions` in `features/review-in-editor/index.ts`). Ticket workspace and Morning digest are one-shot reads kept in memory
  (`NOT_PERSISTED_FEATURES` in `core/job-files.js`): click again after a
  restart. A dev checkout (`npm run dev`) never persists jobs. If you restart
  the service while a job with a worktree is running or awaiting review and the
  job was not persisted, its worktree is left on disk for manual cleanup
  (`~/.ai-dev-companion/worktrees/<repo>/<job-id>/`,
  `git worktree remove --force`); the extension detects the resulting 404 and
  falls back gracefully.
- **"Conflict -> push" time is approximate** (job start to outcome) until
  the Phase 7 watcher can see the push.
- **Optional host permissions for other hosts are not built.** Pages on
  other Jira or Jenkins hosts than the configured ones still need the
  manifest's hosts.
- **"Open in Cursor" / "Open in Claude Code" are best-effort.** If Cursor
  isn't installed, that link just does nothing (harmless). "Open in Claude
  Code" is macOS-only (it shells out to `osascript` to open a real
  terminal) — on other platforms it returns a clear error instead of
  silently failing.
- **Reloading the extension doesn't refresh already-open tabs.** Chrome
  swaps out the code at `chrome://extensions`, but any Bitbucket/Jira tab
  that already had the old content script injected keeps running it — its
  `chrome.*` calls start throwing "Extension context invalidated" (visible
  in that tab's console) on every poll tick until the tab itself is
  refreshed. `content.js` detects this specific error and stops polling
  silently rather than spamming the console forever, but the only real fix
  is refreshing the tab (or closing it) after every reload during
  development.
- **Cloning from the browser can't answer git prompts.** "Clone it for me"
  runs `git clone` with `GIT_TERMINAL_PROMPT=0` and SSH in batch mode, so a
  clone that needs a password or passphrase fails, with the exact
  `git clone` command to run once in a terminal instead. Credentials already
  in the macOS keychain or an SSH agent work. "Choose folder…" needs macOS.
- **Ticket workspace and Ticket to PR (Phase 6).** The Bitbucket 9.4.16
  shapes of the default-branch, default-reviewers, build-status and
  PR-creation endpoints follow the documented API and have not been captured
  from a live instance (each degrades on a 404, but a different 200 shape
  would read as "no build" or "no reviewers"); re-check them with the live
  checks after a Bitbucket upgrade. Only same-repository PRs (no forks). The
  workspace looks at the first 30 repositories. Start fix needs a saved
  analysis (or the workspace's repository). Create PR fetches the base branch
  and pushes, so it needs the remote to be reachable without a prompt. The
  terminal hand-off is macOS-only, as before. `startFixToPrMs` is recorded but
  has no reader yet.
- **Background work (Phase 7).**
  - The LLM proxy's paths and auth are modelled on OpenAI-compatible
    documentation, and the dashboard (`/rest/api/1.0/dashboard/pull-requests`),
    `/merge` and Jira search shapes on the documented API; none was captured
    live. A different proxy path shows as detection "error" (the watchers keep
    working on rules); a different 200 shape reads as "nothing found", not an
    error.
  - A background analysis is saved to the shared analysis cache with no
    provenance, so a later click loads the weaker report (no MCP, no links)
    unless re-analysed.
  - Watchers see the first 20 PRs; forks are skipped; a conflict watcher needs
    a configured clone. There is no build-break watcher and no Slack.
  - The Chrome notification needs Chrome running; the extension has no unit
    tests, so it is checked live.
  - Config-based git hooks need a recent git (otherwise the fallback line is
    printed); the precheck's 3 seconds is best effort.
  - A pre-warmed Resolve Conflict spends budget and runs sandboxed without a
    click (never a push). Address review comments is not
    used by any of this.
  - **Live checks the user must do** (after `node install.js --yes`, a
    service restart and an extension reload; nothing here has been run
    against the real services):
    1. *Contracts, read-only first.* Turn on only `watchers.reviewRequests`:
       its next `scheduler:watcher.reviewRequests` row (`companion history
       --metrics`) should show `found` > 0 and no `needsLogin`, confirming the
       Bitbucket 9.4.16 dashboard shape; turn on `assignedBugs` to confirm Jira
       search; with a conflicted PR, `/merge`'s `conflicted` field.
    2. *LLM proxy.* Save a key: `companion doctor` and Settings should say
       "ready". If "error", fix `PATHS`/`headers()` in
       `core/llm-proxy-request.js` and capture one real chat and embed reply
       into `core/fixtures/llm-proxy/`. Remove the key: "no key", and the
       watchers still work. A model outside the list (`gpt-5.1`) is refused.
    3. *Watcher end to end.* `watchers.conflicts` with `intervalMinutes: 1` and
       a conflict on a test PR: a pre-warmed Resolve Conflict job, badge 1, one
       Chrome notification, the job shown on the PR, nothing pushed until
       Approve. With `budget.claudeRunsPerDay: 0`: an inbox item, no run.
    4. *Real Chrome notifications and the badge*, since the extension has no
       unit tests (batching, the urgent bypass, click opening the item).
    5. *Quiet hours and the digest.* A window around now holds notifications;
       a digest time of now + 2 minutes on a weekday posts "Morning digest"
       and takes over what was held.
    6. *Metrics.* After approving one pre-warmed job and opening one analysis
       from the inbox, `GET /history/metrics` has `prewarmed` with
       `usedFraction`.
    7. *A real `git push` with the hook installed alongside husky:*
       `companion hooks install`, husky still runs, a push touching a file in
       the facts prints a cited warning within about 3 s and proceeds, with the
       service stopped it is silent, and `COMPANION_SKIP=1 git push` prints
       nothing. Also check the exit code under a real push.
    8. *`companion doctor`'s new background lines* (syntax-checked and unit
       tested, not run against a real config).

- **The local history's threat model.** Personal, derived data only (ids,
  keys, clipped titles and excerpts, links, timings, vectors; never tokens,
  passwords or cookies). `~/.ai-dev-companion/history.db` and its WAL files
  are mode 0600 and outside the installed folder. Retention is
  `history.retentionDays` (default 180), pruned daily by the scheduler's
  maintenance. `companion forget` / `forget_item` delete an item with its
  links, facts, events and vector. Only the service writes it, and every Claude
  policy denies reading `history.db*` (`core/claude-args.js`). Anything running
  as the same OS user can still read it: as with Microsoft Recall's first
  version, it is plain SQLite, not encrypted. `redactSecrets`
  (`core/history-record.js`) masks Bearer/Basic values, `token=`/`password=`
  style pairs and URL credentials but does **not** mask GitHub `ghp_...`
  tokens or other bare token formats, so a secret of that shape in a ticket
  title would be stored and embedded as text.
- **The LLM proxy and the routing rules.** Optional. The key is in
  `credentials.enc`; only `llmProxy.allowedModels` (on-prem) and https
  base URLs on an allowed host suffix are used, both changeable only in `config.json`.
  `chooseTier` sends only triage, classify, condense and embed on-prem, and
  everything with tools, code changes, diagnosis and analysis to Claude. The
  proxy's chat and embeddings paths and the embedding vector size are modelled
  on OpenAI-compatible documentation, not captured live.
- **The session vault** (`core/session-vault.js`). Browser sessions relayed by
  the extension are held in memory only, for `sessionCache.ttlMinutes` (default
  30, 0 turns it off), only for the configured origins, and are lost on
  restart.
- **Watchers.** Off by default. At most 5 background Claude runs a day (the
  default), one at a time, none in quiet hours; nothing is pushed without a
  click; background runs have no MCP servers and no web tools.
- **The MCP token.** Separate from the extension's secret. It is stored in
  `credentials.enc`, and in plain text in `~/.claude.json` (by `claude mcp add`)
  and `~/.cursor/mcp.json` because those clients need it. The plugin path
  avoids the `~/.claude.json` copy if `claude mcp add` is removed. Rotate it in
  Settings.
- **Similar items (Phase 8).** The search is a brute-force scan: fine for a
  personal history, slower past tens of thousands of items. A
  `scheduler:history.embed` events row is written every 30 minutes even when
  it only says "skipped: no proxy". Doctor's "embedded" figure counts only
  real vectors (of the current model, with more than one number), not the
  placeholders. The prompt block can
  still carry a wrong earlier analysis (it is labelled, not filtered). An
  unchanged item can be re-embedded when its text is saved again, bounded by 48
  a run, and an embedding request is not aborted when its 8 s limit passes.
  Themes for review comments are not built (see **Similar items**). The
  plugin's `${CLAUDE_PLUGIN_ROOT}` expansion and a real `/plugin install` are
  checked live, not in tests.
- **Live checks the user must do for Phase 8** (after `node install.js --yes`,
  restarting the service and reloading the extension; none of this has been run
  against the real services):
  1. *Migration.* The service starts on the existing `history.db` (schema 1);
     the startup log says the local history is on; `companion doctor` shows
     "Similar past tickets: on, ... N of M ... embedded".
  2. *Without a proxy key.* Two minutes after start, `companion history
     --metrics` shows a `scheduler:history.embed` row whose metrics say
     `skipped: "no proxy"`; `companion similar <an analysed ticket>` answers
     "by shared words (no LLM proxy)".
  3. *LLM proxy embeddings.* With the key saved, within 30 minutes the embed row
     shows `embedded` > 0 and doctor's count rises, and `companion similar`
     says "by meaning and shared words". If the row says `failed` with an HTTP
     error, the embeddings path or body differs from `/v1/embeddings`: fix
     `PATHS.embed`/`buildEmbedRequest` in `core/llm-proxy-request.js` and
     capture one real embed reply into `core/fixtures/llm-proxy/`.
  4. *Similar block quality in a real analysis.* Re-analyse a ticket with a
     similar earlier one: the Claude run's prompt (the session transcript in
     `~/.claude/projects/...`) contains the `<similar-past-tickets>` block with
     the provenance labels and the matches are useful; turning the Settings
     checkbox off removes it.
  5. *The plugin.* `node install.js --plugin`, then in Claude Code
     `/plugin marketplace add ~/ai-dev-companion` and `/plugin install
     ai-companion@ai-dev-companion`. `/mcp` lists the plugin's
     `ai-companion` as connected (this confirms `${CLAUDE_PLUGIN_ROOT}`
     expansion in the MCP `args`). With the service stopped, the
     `companion-status` skill says "companion service not running" and a plugin
     tool call returns that error.
  6. *The SessionStart hook.* With an unseen inbox item, a new session starts
     with "AI companion: 1 new in your inbox (...)" (this confirms
     `${CLAUDE_PLUGIN_ROOT}` in the hook command); with nothing unseen, nothing
     is printed.
  7. *Doctor.* `companion doctor` warns that both paths are active until
     `claude mcp remove --scope user ai-companion`, then says "through the
     ai-companion plugin".
  8. *Measured benefit.* Fill README's table from `companion history --metrics`
     on the installed copy.

## Verification performed

- Companion service builds, typechecks, and its test suite passes cleanly
  (`npm run build`, `npm test`).
- Shared-secret auth confirmed **from the real extension's service worker**
  (via Playwright, not just curl): a fetch with the correct
  `X-Companion-Secret` gets `200` from `/features`; the wrong secret gets
  `403`.
- Ran full `resolve-conflict` jobs end-to-end against real, currently-open
  PRs on the real `sample-app` clone, including an **actual merge conflict**
  (not a no-op): Claude correctly resolved it, made a sensible judgment
  call between the two branches' changes, and the diff matched reality.
- **The full path through Approve & Push has been exercised for real** on
  a live PR — commit, push, and the PR's conflict badge clearing on
  Bitbucket afterward, all confirmed. `git add -A -- <files>` (not a
  blanket `-A`) is used specifically because a blanket `-A` was confirmed
  to stage the `node_modules` symlink that pre-commit hooks need (a
  gitignore directory pattern like `/node_modules/` does **not** match a
  symlink — confirmed the hard way).
- The side-by-side diff viewer, the progress checklist, and
  `chrome.storage.local` job persistence across a page refresh were all
  verified against real jobs, not just code review.
- **Create Subtasks was run end-to-end against a real ticket**
  (`CIS-83053`, a Story), not just unit-tested: submitting real rows
  created real Sub-tasks, confirmed afterward via
  `jira_get_issue` — correct parent, summary, and assignee on each,
  including one deliberately edited through the assignee search dropdown.
  Gating (Story/Epic only), the assignee-search endpoint, and the
  auto-close-and-reload-on-success behavior were all exercised in the
  actual browser, not simulated.
- **`companion doctor`, in the order `doctor.js`'s `main()` prints it:**
  prerequisites (Node), the settings file, credentials, MCP registration (with
  Phase 8's line for which way Claude Code reaches the companion), background
  work (watchers, quiet hours, digest), the LLM proxy, Phase 8's
  "Similar past tickets" line, the pre-push hook, the extension's connection
  settings, enabled features and their prerequisites,
  a pack's own lines, the shared test history, Ticket to PR and the repositories, the
  built service, the Claude Bash sandbox, and the service's health. The
  functions behind the new lines (`checkPluginPath`, `checkSimilar`) have unit
  tests, but the live pass against the real config is the orchestrator's after
  the last task (see the Phase 8 live checks above); it has not been run for
  this release.
