# Changelog

## 0.2.0

- GitHub as a git host (github.com and GitHub Enterprise): a provider with the same pull request, review-comment, build-status, dashboard and merge-status behaviour as the Bitbucket one, using a personal access token. Review threads come from GraphQL, so resolved and outdated threads are skipped.
- Token-only sites: a site can declare that its browser login is no use (GitHub's isn't for its API); a 403 or 429 on such a site is reported as a rate limit or a missing permission, not as a bad token.
- Pull request keys and addresses follow the profile's git host: `github:owner/repo#n` next to `bitbucket:PROJECT/repo#n`, GitHub's `/owner/repo/pull/n` pages, clone URLs without a `.git` suffix, and repository names with a leading dot (`.github`).
- `GET /prs/status`: the state, conflicts, branches and fork status of a pull request, read through the git provider and cached for a few seconds, for the extension to ask instead of calling the git host itself.
- Settings lists every site of the profile, not just Jira, Jenkins and Bitbucket, and shows the git host under its own name; a token-only site asks for a personal access token.
- The extension reads pull request pages through `PaiGit` (`git-host.js`): GitHub's `/owner/repo/pull/n` pages, asked of the companion; Bitbucket's unchanged. Resolve conflict, Address review comments and Review PR no longer assume Bitbucket, and skip fork PRs. Settings group `bitbucket` is now `git`.
- Sites can be `tokenOnly` (no host permission, no cookie relay) and can limit where the extension runs with `pageMatches` (GitHub: pull request pages only). `/targets` reports the git provider.
- `docs/DESIGN.md`: the technical design, written host-neutral; a new section on the environment profile, packs and distributions, and one on the Bitbucket and GitHub providers.
- `examples/minimal-env`: a GitHub + Jira + Jenkins profile with its own test (`npm run test:example`).

## 0.1.0

- First release of the framework as its own repository.
- The companion service: jobs and worktrees, the Claude runner and its guards, the MCP server, history, scheduler and watchers.
- The Chrome extension: feature registry, shared UI, settings panel and a manifest generated from the environment profile.
- The Claude Code plugin template. A distribution supplies an environment profile and packs for its own git host, issue tracker and CI.
