// The `companion` command's behaviour, with every side effect injected
// (`deps`) so it is testable: talking to the service, printing, starting
// `claude`, checking the filesystem. bin/companion.js wires the real ones.
const { parseArgs } = require("./cli-args.js");
const fmt = require("./cli-format.js");
const resume = require("./resume.js");
const gitHook = require("./git-hook.js");

const USAGE = `Usage: companion <command>

  status                    Is the service running, and what is it doing?
  jobs [--feature <id>]     List the jobs it is tracking.
  history [<key>]           What it remembers about a ticket, PR or job (PROJ-12, CI/sample-app#7, job:<id>),
                            or, with no key, how long its jobs take. --search <words>, --days <n>, --metrics
                            (the timing table, even with a key).
  similar <key|words…>      Past tickets and PRs in the local history most like one (PROJ-12, CI/sample-app#7) or some words.
  resume <jobId|KEY>        Continue a job's Claude session in this terminal — or, for a ticket
                            (PROJ-1234), its saved session, else Claude Code in its worktree.
  forget <key> [--yes]      Delete one item from its local history.
  open <file:line…>         Open a stack-trace line in Cursor.
  inbox [--all] [--brief]   What the background watchers found for you (--all: seen items too;
                            --brief: one line, or nothing when there is nothing new or the service is down).
  digest                    Your digest: your PRs, reviews waiting on you, your tickets, what's ready.
  precheck [<file>…]        Check changed files against the shared test history. Advisory: it never blocks.
                            With --stdin it reads what git gives a pre-push hook. COMPANION_SKIP=1 skips it.
  hooks install|uninstall|status [<repo>]
                            The optional pre-push check, set in the repo's own git config (runs alongside husky).
  doctor                    Check the installation.
  mcp-stdio                 The companion's MCP tools over stdin/stdout, for the Claude Code plugin (not for typing).

Add --json to status, jobs, history, similar, inbox, digest and precheck for scripts.`;

const PRECHECK_LIMIT_MS = 3000;
// The service is down or the command isn't set up (no token / a refused one):
// as a hook, precheck stays silent about all of these.
const SERVICE_DOWN_RE = /isn't running|Couldn't reach the companion|No MCP token|refused the token/;

/** The files a precheck looks at, repo-relative: from git's pre-push stdin,
 * the named files, or (neither) the uncommitted changes. */
async function precheckFiles(flags, positional, deps, deadline) {
  const top = await deps.git(["rev-parse", "--show-toplevel"], deps.cwd);
  if (top.code !== 0) throw new Error("Run it inside a git repository.");
  const root = top.stdout.trim();
  const files = [];
  if (flags.stdin) {
    for (const update of gitHook.parsePrePushLines(await deps.readStdin())) {
      // git runs synchronously, so timers can't cut in: stop asking once the time is used up.
      if (deps.now() >= deadline) break;
      const args = gitHook.changedFilesArgs(update);
      if (!args) continue;
      const r = await deps.git(args, root);
      if (r.code === 0) files.push(...gitHook.parseNameList(r.stdout));
    }
  } else if (positional.length > 0) {
    for (const f of positional) files.push(deps.relative(root, deps.resolve(deps.cwd, f)));
  } else {
    const r = await deps.git(["diff", "--name-only", "HEAD"], root);
    if (r.code === 0) files.push(...gitHook.parseNameList(r.stdout));
  }
  return [...new Set(files)].slice(0, 500);
}

/** `companion precheck`: always exits 0. As a hook (--stdin) it's silent
 * unless it has a medium or high warning; it stops waiting for the service
 * after about 3 seconds (git and the service call each have their own short
 * timeout, so a stuck git process can run a little past that). */
async function precheck(flags, positional, deps, say) {
  if (deps.env.COMPANION_SKIP === "1") return 0;
  const config = deps.readConfig() || {};
  if (config.prePush && config.prePush.mode === "off") return 0;
  const hook = flags.stdin === true;
  const work = (async () => {
    const files = await precheckFiles(flags, positional, deps, deps.now() + PRECHECK_LIMIT_MS);
    if (files.length === 0) return hook ? null : { skipped: true, reason: "no changed files" };
    return deps.callTool("get_change_risk", { files });
  })();
  // A late failure (after the 3 seconds are up) must not surface as an unhandled rejection.
  work.catch(() => {});
  let result;
  try {
    result = await Promise.race([work, deps.sleep(PRECHECK_LIMIT_MS).then(() => ({ timedOut: true }))]);
  } catch (err) {
    if (!hook || !SERVICE_DOWN_RE.test(err && err.message)) deps.err(`companion precheck: ${fmt.oneLine(err && err.message ? err.message : err)}\n`);
    return 0;
  }
  if (!result) return 0;
  if (result.timedOut) {
    if (!hook) deps.err("companion precheck: no answer within 3 seconds — skipped.\n");
    return 0;
  }
  if (flags.json) {
    say(JSON.stringify(result, null, 2));
    return 0;
  }
  if (hook && (result.skipped || !["medium", "high"].includes(result.level))) return 0;
  deps.err(`${fmt.formatRisk(result)}\n`);
  return 0;
}

async function hooks(positional, deps, say, fail) {
  const action = positional[0];
  if (!["install", "uninstall", "status"].includes(action)) {
    deps.err(`hooks needs install, uninstall or status.\n\n${USAGE}\n`);
    return 2;
  }
  const top = await deps.git(["rev-parse", "--show-toplevel"], deps.resolve(deps.cwd, positional[1] || "."));
  if (top.code !== 0) return fail(`${positional[1] || deps.cwd} isn't inside a git repository.`);
  const repo = top.stdout.trim();
  if (action === "status") {
    const r = await deps.git(gitHook.statusArgs(repo), repo);
    say(r.code === 0 ? `The pre-push check is installed in ${repo}.` : `The pre-push check isn't installed in ${repo}.`);
    return 0;
  }
  if (action === "uninstall") {
    for (const args of gitHook.uninstallArgs(repo)) {
      const r = await deps.git(args, repo);
      // 5: the key wasn't set — nothing to remove.
      if (r.code !== 0 && r.code !== 5) return fail(`git config failed: ${r.stderr || r.stdout}`.trim());
    }
    say(`Removed the pre-push check from ${repo}.`);
    return 0;
  }
  const probeDir = deps.mkTemp();
  let supported = false;
  try {
    const steps = gitHook.probeSteps(probeDir);
    let last = { code: 1, stdout: "" };
    for (const args of steps) {
      // Isolated: the user's and system git config must not decide whether this git supports config hooks.
      last = await deps.git(args, probeDir, { isolated: true });
      if (last.code !== 0) break;
    }
    supported = last.code === 0 && last.stdout.includes(gitHook.PROBE_MARKER);
  } finally {
    deps.rmTemp(probeDir);
  }
  if (!supported) {
    say(gitHook.fallbackInstructions(repo));
    return 0;
  }
  for (const args of gitHook.installArgs(repo)) {
    const r = await deps.git(args, repo);
    if (r.code !== 0) return fail(`git config failed: ${r.stderr || r.stdout}`.trim());
  }
  say(
    `Installed the pre-push check in ${repo} (git config hook.${gitHook.HOOK_NAME}.*). It runs alongside husky and ` +
      "any other pre-push hook, never blocks a push, and COMPANION_SKIP=1 skips it once. `companion hooks uninstall` removes it.",
  );
  return 0;
}

const ACTIVE = ["running", "approving", "rejecting"];
const ISSUE_KEY_RE = /^[A-Za-z][A-Za-z0-9_]*-[1-9]\d*$/;
const PR_REF_RE = /^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+#[1-9]\d*$/;

/** `companion resume PROJ-1234`: the ticket's saved session if it can still
 * be resumed, else a fresh Claude Code in its worktree. The service already
 * filters what it reports, but a folder is only ever entered here if
 * `deps.cwdAllowed` (the same allow-list as a job's session folder) says so;
 * with no such dep nothing is. */
async function resumeTicket(issueKey, deps, say, fail) {
  const ws = await deps.callTool("ticket_workspace", { issueKey });
  const target = (ws && ws.resume) || {};
  const session = target.session;
  const allowed = (cwd) => typeof deps.cwdAllowed === "function" && deps.cwdAllowed(cwd) === true;
  if (session && resume.validateSession(session) && allowed(session.cwd)) {
    const transcript = resume.sessionTranscriptPath(deps.home, resume.realCwdForTranscript(session.cwd), session.id);
    if (deps.exists(session.cwd) && deps.exists(transcript)) {
      say(`Resuming the Claude session for ${issueKey} in ${session.cwd} …`);
      return deps.runClaude(resume.buildResumeArgs({ session }), session.cwd);
    }
  }
  if (typeof target.worktreeDir === "string" && allowed(target.worktreeDir) && deps.exists(target.worktreeDir)) {
    say(`No resumable Claude session for ${issueKey} — starting Claude Code in its worktree ${target.worktreeDir} …`);
    return deps.runClaude([], target.worktreeDir);
  }
  return fail(
    `Nothing to resume for ${issueKey}: no worktree or saved Claude session on this machine. ` +
      "Open the ticket in Chrome and use ✨ → Ticket workspace.",
  );
}

/** `companion precheck` is advisory and runs inside `git push`: whatever
 * happens (bad flag, odd reply, a closed stderr) it exits 0. */
async function main(argv, deps) {
  if (argv[0] !== "precheck") return run(argv, deps);
  const quiet = (write) => (text) => {
    try {
      write(text);
    } catch {
      // EPIPE and friends: nobody is listening, and the push goes on.
    }
  };
  try {
    await run(argv, { ...deps, out: quiet(deps.out), err: quiet(deps.err) });
  } catch {
    // Nothing here may hold a push up.
  }
  return 0;
}

async function run(argv, deps) {
  const parsed = parseArgs(argv);
  const say = (text) => deps.out(`${text}\n`);
  const fail = (message, code = 1) => {
    deps.err(`companion: ${fmt.oneLine(message)}\n`);
    return code;
  };
  if (parsed.errors.length > 0) {
    deps.err(`${parsed.errors.join("\n")}\n\n${USAGE}\n`);
    return 2;
  }
  const { flags, positional } = parsed;
  const print = (value, text) => say(flags.json ? JSON.stringify(value, null, 2) : text);

  try {
    switch (parsed.command) {
      case "help":
        say(USAGE);
        return 0;

      case "doctor":
        return await deps.runDoctor();

      case "status": {
        const jobs = await deps.callTool("list_jobs", {});
        print(jobs, fmt.formatStatus(jobs, deps.now(), deps.baseUrl));
        return 0;
      }

      case "jobs": {
        const args = flags.feature ? { featureId: flags.feature } : {};
        const jobs = await deps.callTool("list_jobs", args);
        print(jobs, fmt.formatJobs(jobs, deps.now()));
        return 0;
      }

      case "history": {
        const key = positional[0];
        if (key && !flags.search && !flags.metrics) {
          const detail = await deps.callTool("get_history", { key });
          print(detail, fmt.formatHistory(detail, deps.now()));
          return 0;
        }
        const args = {};
        if (flags.search) args.query = flags.search;
        if (flags.days !== undefined) {
          if (!/^\d+$/.test(String(flags.days)) || Number(flags.days) === 0) {
            deps.err(`--days needs a whole number of days, 1 or more.\n\n${USAGE}\n`);
            return 2;
          }
          args.days = Number(flags.days);
        }
        const data = await deps.callTool("list_history", args);
        print(data, fmt.formatMetrics(data));
        return 0;
      }

      case "similar": {
        if (positional.length === 0) {
          deps.err(`Similar to what? Give a key (PROJ-12, CI/sample-app#7) or some words.\n\n${USAGE}\n`);
          return 2;
        }
        const one = positional.length === 1 ? positional[0] : null;
        const isKey = one !== null && (ISSUE_KEY_RE.test(one) || PR_REF_RE.test(one) || /^[a-z]+:\S+$/.test(one));
        const args = isKey ? { key: one } : { text: positional.join(" ").slice(0, 200) };
        const r = await deps.callTool("find_similar", args);
        print(r, fmt.formatSimilar(r, isKey ? one : `"${positional.join(" ").slice(0, 60)}"`));
        return 0;
      }

      case "resume": {
        const jobId = positional[0];
        if (!jobId) {
          deps.err(`Which job or ticket? \`companion jobs\` lists jobs.\n\n${USAGE}\n`);
          return 2;
        }
        if (ISSUE_KEY_RE.test(jobId)) return await resumeTicket(jobId.toUpperCase(), deps, say, fail);
        const job = await deps.callTool("get_job", { jobId });
        if (ACTIVE.includes(job.status)) {
          return fail(`This job is still ${fmt.oneLine(job.status)} — wait for it to finish before resuming.`);
        }
        const session = job.data && job.data.claudeSession;
        if (!resume.validateSession(session)) return fail("This job has no resumable Claude Code session.");
        const transcript = resume.sessionTranscriptPath(deps.home, resume.realCwdForTranscript(session.cwd), session.id);
        if (!deps.exists(session.cwd) || !deps.exists(transcript)) {
          return fail("That Claude session has expired (its folder or transcript is gone).");
        }
        say(`Resuming the ${fmt.oneLine(job.featureId)} session in ${fmt.oneLine(session.cwd)} …`);
        return await deps.runClaude(resume.buildResumeArgs({ session }), session.cwd);
      }

      case "forget": {
        const key = positional[0];
        if (!key) {
          deps.err(`Forget which item? Give its key.\n\n${USAGE}\n`);
          return 2;
        }
        if (!flags.yes) {
          let detail = null;
          try {
            detail = await deps.callTool("get_history", { key });
          } catch (err) {
            if (!/Nothing is recorded/.test(err && err.message)) throw err;
          }
          if (!detail) return fail("Nothing is recorded under that key.");
          say(`Would delete ${fmt.oneLine(detail.item.key)} (${fmt.oneLine(detail.item.kind)})${detail.item.title ? ` "${fmt.oneLine(detail.item.title)}"` : ""} and ${detail.edges.length} link(s).`);
          say("Re-run with --yes to delete it. This can't be undone.");
          return 2;
        }
        const r = await deps.callTool("forget_item", { key });
        say(`Removed ${fmt.oneLine(r.items)} item(s) and ${fmt.oneLine(r.events)} timing event(s).`);
        return 0;
      }

      case "inbox": {
        if (flags.brief) {
          // For the Claude Code plugin's SessionStart hook: one line or
          // nothing, never an error, so a session start is never noisy.
          let brief = "";
          let timer;
          try {
            // Time-limited: a hung service must not hold a session start up.
            const limit = new Promise((_, reject) => {
              timer = setTimeout(() => reject(new Error("timeout")), Number.isFinite(deps.briefTimeoutMs) ? deps.briefTimeoutMs : 3000);
              if (timer.unref) timer.unref();
            });
            brief = fmt.formatInboxBrief(await Promise.race([deps.callTool("list_notifications", {}), limit]));
          } catch {
            brief = "";
          } finally {
            clearTimeout(timer);
          }
          if (brief) say(brief);
          return 0;
        }
        const r = await deps.callTool("list_notifications", flags.all ? { all: true } : {});
        print(r, fmt.formatInbox(r, deps.now()));
        return 0;
      }

      case "digest": {
        const d = await deps.callTool("get_digest", {});
        print(d, fmt.formatDigest(d));
        return 0;
      }

      case "precheck":
        return await precheck(flags, positional, deps, say);

      case "hooks":
        return await hooks(positional, deps, say, fail);

      case "open": {
        if (positional.length === 0) {
          deps.err(`Open what? Give a location such as src/app/login.ts:42.\n\n${USAGE}\n`);
          return 2;
        }
        const r = await deps.callTool("open_location", { text: positional.join(" ") });
        print(r, `Opened ${fmt.oneLine(r.repo)}: ${fmt.oneLine(r.path)}:${fmt.oneLine(r.line)}${r.column ? `:${fmt.oneLine(r.column)}` : ""} in ${fmt.oneLine(r.editor)}.`);
        return 0;
      }

      default:
        return fail(`Unhandled command "${parsed.command}".`, 2);
    }
  } catch (err) {
    return fail(err && err.message ? err.message : String(err));
  }
}

module.exports = { main, USAGE };
