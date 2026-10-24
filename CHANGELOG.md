# Changelog

## Unreleased

- Update checks fetch only the version and changelog, not the whole release.
- Local access hardened: constant-time secret check, no CORS headers, owner-only `companion-config.js`.
- Review PR's prompt no longer names a `review-pr` skill; it uses whichever review skill you have.
- Review PR remembers its session and reviewed commit per PR, and reopens it when nothing changed.
- With new commits, Review PR starts a fresh session that reviews only `git diff <last reviewed>..HEAD`.
- Diagnose this failed build runs headless in a panel, with Continue in Claude Code and Re-diagnose.
- Ticket workspace, Analyze ticket, Summarize comments and the diagnosis use the AI Assistant design's cards.
- Address review comments and Review PR use the same cards; features can return `{ node, header }`.
- Regenerated the checked-in extension manifest (it lacked `icons.js`).
- Resolve merge conflicts and Address review comments ask what to do when the branch is checked out in your main clone.
- New workspace resolver (`core/workspace-resolver.js`) finds where to work on a branch without creating anything.
- Resolve merge conflicts and Address review comments reuse a clean linked worktree of the PR's branch.
- Rejecting a job restores a reused worktree; approving leaves it; it is never deleted.
- Review PR finds the branch in any worktree or clone before falling back to the shared review worktree.
- One Continue in Claude Code button replaces Discuss, Start fix and Resume in the report and Ticket workspace.
- Start fix reuses an existing checkout of the ticket's branch and asks before making a worktree or branch.
- The Ticket workspace is also offered on pull requests and CI builds.
- `companion worktrees` lists every worktree of your repos; `--prune` removes the tool's finished ones.
- Any linked worktree git has registered for a mapped repo can be used to resume a Claude session.
- New work-unit registry (`core/work-units.js`) records each branch's worktree, ticket, PR and Claude sessions.

## 0.2.0

- GitHub (and GitHub Enterprise) supported as a git host, using a personal access token.
- Token-only sites report a 403 or 429 as a rate limit or missing permission, not a bad token.
- Pull request keys, addresses and clone URLs now follow the profile's git host.
- New `GET /prs/status` returns a pull request's state, conflicts, branches and fork status.
- Settings lists every site of the profile and shows the git host under its own name.
- The extension reads pull request pages through `PaiGit` (`git-host.js`); Resolve conflict, Address review comments and Review PR skip fork PRs.
- Sites can be `tokenOnly` and limit where the extension runs with `pageMatches`.
- `docs/DESIGN.md`: the host-neutral technical design, with new profile, pack and provider sections.
- `examples/minimal-env`: a GitHub + Jira + Jenkins profile with its own test (`npm run test:example`).

## 0.1.0

- First release of the framework as its own repository.
- The companion service: jobs and worktrees, the Claude runner and its guards, the MCP server, history, scheduler and watchers.
- The Chrome extension: feature registry, shared UI, settings panel and a manifest generated from the environment profile.
- The Claude Code plugin template. A distribution supplies an environment profile and packs for its own git host, issue tracker and CI.
