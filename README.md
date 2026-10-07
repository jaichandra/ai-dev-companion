# ai-dev-companion

A local companion service and a Chrome extension that add one-click AI actions to the pages you already use: resolve a pull request's merge conflicts with Claude, create several Jira subtasks at once, check out a PR for your editor's AI to review, analyze a ticket, diagnose a failed build, and a daily digest. The extension cannot run git or start processes, so all real work happens in the companion service on `127.0.0.1`. Nothing leaves your machine.

This repository is a **framework**. It knows how to do those things against a git host, an issue tracker and a CI server; a **distribution** tells it which ones, with its own settings and optional extra features. Providers today: **GitHub** (github.com and GitHub Enterprise) or Bitbucket Server/Data Center as the git host, Jira Server/Data Center for tickets, and Jenkins for builds. A profile names one git host.

## What is in here

| Folder | What it is |
|---|---|
| `companion-service/` | The Node service: jobs and worktrees, the Claude runner and its safety guards, the MCP server, local history, scheduler and watchers, setup wizard, `doctor`, the `companion` CLI. |
| `chrome-extension/` | The extension: a feature registry, shared UI, the Settings panel. Its `manifest.json` is generated from the environment profile. |
| `plugin/` | A Claude Code plugin template (MCP bridge, session-start hook, skills). |
| `docs/DESIGN.md` | The technical design: architecture, security notes, how each feature and subsystem works, the extension guide, known limitations. |
| `scripts/assemble.js` | Merges the framework with a distribution into the folder that runs and is installed. |
| `examples/minimal-env/` | An example distribution profile: GitHub, Jira and Jenkins, with its own test. |

## Building a distribution

A distribution is a separate repo that depends on this one and adds its own files:

1. **An environment profile**, `companion-service/environment.js`, written with `defineEnvironment` (`core/environment-base.js`): the sites (`jira`, `jenkins`, `bitbucket`: kind, provider, base URL), where updates come from, the LLM proxy defaults, default Jira projects, branding, and which packs to load. The framework ships a placeholder profile pointing at `example.com`; yours replaces it.
2. **Packs** (optional): a pack (`core/packs.js`) lists features and can add settings keys, `/targets` data, borrowed-token sources, a diagnose command, doctor lines and an MCP deny list. A feature is a folder with an `index.ts` implementation and a `feature.js` declaration (setup descriptor, factory, MCP tools, scope-key rule, history flags), plus an extension file that registers its ✨ row.
3. **Root files**: installer, README, changelog.
4. **Assemble**: `assemble({ frameworkDir, rootDir, rootSkip, overlays, dest })` from `scripts/assemble.js` lays the framework, your root files and your overlay folders into one tree. Install and test from that tree, so what you test is what you ship.

See `CLAUDE.md` for the architecture and `companion-service/core/packs.js` for the pack contract.

## Using GitHub

Name a GitHub site (`provider: "github"`, `tokenOnly: true`) in the profile, as `examples/minimal-env` does, and add a personal access token in Settings. The companion reads and writes pull requests, review threads (through GraphQL, so resolved and outdated threads are skipped), check runs and commit statuses with that token; the extension runs only on pull request pages and is never given github.com's cookies. See `examples/minimal-env/README.md` for what is and isn't supported (fork PRs, default reviewers, conversation-tab comments).

## Running the tests

```
cd companion-service
npm install
npm run build   # many tests run against dist/
npm test
cd .. && npm run test:example   # the same framework assembled with a GitHub profile (examples/minimal-env)
```

Node 22.13 or newer is required (the history store uses `node:sqlite`).

## Known limits

- Opening a terminal or an editor is macOS-only.
- The product identity (state folder `~/.ai-dev-companion`, install folder `~/ai-dev-companion`, launchd label, MCP server name `ai-companion`) is still fixed; making it part of the profile is future work.
- Provider-neutral issue types are not done: features read Jira's JSON for an issue.
- GitHub support was written against GitHub's documented API and hand-made fixtures, not a live account: compare the first real run with `core/fixtures/github/`.
- One git host per profile: Bitbucket and GitHub side by side in one install is not supported.
- No license has been chosen yet.
