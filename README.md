# ai-dev-companion

A local companion service and a Chrome extension that add one-click AI actions to the pages you already use: resolve a pull request's merge conflicts with Claude, create several Jira subtasks at once, check out a PR for your editor's AI to review, analyze a ticket, diagnose a failed build, and a daily digest. The extension cannot run git or start processes, so all real work happens in the companion service on `127.0.0.1`. Nothing leaves your machine.

This repository is a **framework**. It knows how to do those things against a git host, an issue tracker and a CI server; a **distribution** tells it which ones, with its own settings and optional extra features. Providers today: **GitHub** (github.com and GitHub Enterprise) or Bitbucket Server/Data Center as the git host, Jira Server/Data Center for tickets, and Jenkins for builds. A profile names one git host.

## What is in here

| Folder | What it is |
|---|---|
| `crates/ai-dev-companion/` | The service, as a Rust library crate: jobs and worktrees, the Claude runner and its safety guards (the `guard` subcommand), the HTTP API and MCP server, local history, scheduler and watchers, setup wizard, `doctor`, the `companion` CLI. A distribution crate depends on it and supplies its profile and packs. |
| `chrome-extension/` | The extension: a feature registry, shared UI, the Settings panel. Its `manifest.json` is generated from the environment profile. |
| `plugin/` | A Claude Code plugin template (MCP bridge, session-start hook, skills). |
| `mods/companion-panes/` | A Claude Code mod: a Run pane (`/dev`) for dev servers and other commands, and `/shell` for a shell tab. Its own plugin in the marketplace. |
| `docs/DESIGN.md` | The technical design: architecture, security notes, how each feature and subsystem works, the extension guide, known limitations. |
| `scripts/` | `check.sh` (clippy and tests: the gate before a commit), `commit.sh`, `mark-ported.py`. |

## Building a distribution

A distribution is a separate repo that depends on this one and adds its own files:

1. **A distribution crate** that depends on `ai-dev-companion` (a git dependency with a branch, tag or rev) and has its own `main`: it installs its profile with `environment::init`, builds the pack registry (`packs::builtin::pack` plus its own packs) and calls `startup::run_service` or `companion_cli::run_cli`.
2. **An environment profile**, built with `environment_base::define_environment`: the sites (`jira`, `jenkins`, `bitbucket`: kind, provider, base URL), where updates come from, default Jira projects, branding, the names of any credentials its packs save, and which packs to load. The framework's placeholder profile (`environment::placeholder_profile`: GitHub at `github.com`, Jira and Jenkins at `example.com`) is what its own tests use; yours replaces it.
3. **Packs** (optional): a `packs::Pack` lists features and can add settings keys and whole Settings sections (with their extension page), an LLM for the watchers' triage and similar-ticket search (`llm::LlmBackend`), `/targets` data, borrowed-token sources, a diagnose command, doctor lines and an MCP deny list. A feature is a module with its implementation, a `feature.rs` declaration (setup descriptor, factory, MCP tools, scope-key rule, history flags) and an extension file that registers its ✨ row.
4. **An installer** (a distribution's own `install` subcommand): build the release binary, lay out the extension and plugin around it and mirror them into `~/<app slug>`, keeping settings.

See `CLAUDE.md` for the architecture and `crates/ai-dev-companion/src/packs.rs` for the pack contract.

## Using GitHub

Name a GitHub site (`provider: "github"`, `tokenOnly: true`) in the profile, as the placeholder profile does, and add a personal access token in Settings. The companion reads and writes pull requests, review threads (through GraphQL, so resolved and outdated threads are skipped), check runs and commit statuses with that token; the extension runs only on pull request pages and is never given github.com's cookies. Fork PRs, default reviewers and conversation-tab comments are not supported.

## Running the tests

```
scripts/check.sh   # cargo clippy --all-targets -D warnings, then cargo test
```

No Node is needed to build or test the service (the extension stays JavaScript; the plugin's two small scripts use the `node` that Claude Code already runs them with).

## Known limits

- Opening a terminal or an editor is macOS-only.
- The product identity (state folder `~/.ai-dev-companion`, install folder `~/ai-dev-companion`, launchd label, MCP server name `ai-companion`) is still fixed; making it part of the profile is future work.
- Provider-neutral issue types are not done: features read Jira's JSON for an issue.
- GitHub support was written against GitHub's documented API and hand-made fixtures, not a live account: compare the first real run with `crates/ai-dev-companion/tests/fixtures/github/`.
- One git host per profile: Bitbucket and GitHub side by side in one install is not supported.
- No license has been chosen yet.
