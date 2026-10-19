// "Diagnose this failed build": works out why a Jenkins build failed in a headless, read-only Claude run
// (the analyze-issue policy: read tools and the user's read-only MCP servers) and reports it as data for the
// panel (data.report) — through a pack's diagnose command when one is installed (e.g. a platform's own
// build-fix command, whose read-only behaviour is that command's own; its text is then put into the report's
// shape by a second, tool-less run), otherwise through a plain diagnosis prompt. The session is recorded on the
// job (data.claudeSession) so the panel's Continue in Claude Code can pick it up in a terminal. The build URL
// comes from a web page, so it is re-validated here and only the rebuilt URL is used. Nothing is posted or changed.
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { randomUUID } from "crypto";
import type { Config } from "../../config";
import { DEFAULT_SUMMARIZE_COMMENTS_MODEL } from "../../config";
import type { AuthContext } from "../../core/atlassian";
import { runClaude } from "../../core/claude";
import { ClaudeSession, Feature, FeatureContext, Job, JobStatusError, jobStore } from "../../core/jobs";
import { providersFor } from "../../core/providers";

/* eslint-disable @typescript-eslint/no-var-requires */
const { permissionModeFor } = require("../../core/permission-mode.js") as { permissionModeFor(featureId: string): "plan" | "auto" | "default" };
const paths = require("../../core/paths.js") as { stateDir(home?: string): string };
const packs = require("../../core/packs.js") as {
  diagnoseCommand(): { args(buildUrl: string): string[]; summary(build: { jobName: string; number: number }): string } | null;
};
const diagnosePrompt = require("../../core/diagnose-prompt.js") as {
  parseBuildUrl(input: string, baseUrl: string): { jobName: string; number: number; buildUrl: string } | null;
  buildBasicPrompt(o: { buildUrl: string; jobName: string; number: number; flakyLines?: string[] }): string;
  flakyContext(facts: unknown, testNames: string[]): string[];
  buildReportPrompt(o: { buildUrl: string; jobName: string; number: number; flakyLines?: string[] }): string;
  buildStructurePrompt(text: string): string;
  parseReport(text: string): Record<string, unknown>;
};
const analysisPlan = require("../analyze-issue/plan.js") as {
  MUTATING_MCP_TOOLS: string[];
  analysisRunOptions(
    background: boolean,
    readMcpServers: () => string[],
  ): { policy: "readOnly" | "readOnlyBackground"; mcpServers: string[]; extraAllowedTools: string[]; useUrls: boolean };
  readMcpServers(claudeJsonPath?: string): string[];
};
const { redactSecrets } = require("../../core/history-record.js") as { redactSecrets(text: string): string };
const riskFacts = require("../../core/risk-facts.js") as {
  shared: { get(url: string): Promise<{ ok: true; facts: unknown } | { ok: false; reason: string }> };
};
const testReport = require("../../core/jenkins-testreport.js") as {
  extractFailingCases(report: unknown): { normalizedName: string }[];
};
/* eslint-enable @typescript-eslint/no-var-requires */

const FLAKY_LOOKUP_BUDGET_MS = 8000;

function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T> {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("timed out")), ms);
    promise.then(
      (v) => { clearTimeout(timer); resolve(v); },
      (e) => { clearTimeout(timer); reject(e); },
    );
  });
}

/** Known-flaky lines for the build's failing tests, from the shared feed. Best effort: any problem → no lines. */
async function flakyLinesFor(
  config: Config,
  auth: AuthContext,
  build: { jobName: string; number: number },
): Promise<string[]> {
  const url = config.riskFacts?.url;
  if (!url || build.jobName.includes("/")) return []; // fetchTestReport addresses a top-level job only
  try {
    const facts = await riskFacts.shared.get(url);
    if (!facts.ok) return [];
    const report = await withTimeout(providersFor(config).ci.fetchTestReport(auth, build.jobName, build.number), FLAKY_LOOKUP_BUDGET_MS);
    if (!report) return [];
    return diagnosePrompt.flakyContext(facts.facts, testReport.extractFailingCases(report).map((c) => c.normalizedName));
  } catch {
    return [];
  }
}

export function createDiagnoseBuildFeature(config: Config): Feature {
  const running = new Map<string, { controller: AbortController; done: Promise<void> }>();
  // The latest job per build, so a second click on the same build reopens it instead of running Claude again.
  const latestByBuild = new Map<string, string>();

  async function run(jobId: string, build: { jobName: string; number: number; buildUrl: string }, ctx: FeatureContext, signal: AbortSignal): Promise<void> {
    jobStore.update(jobId, { progress: { stepId: "collect", label: "Reading the build…" } });
    const flakyLines = await flakyLinesFor(config, ctx.auth, build);
    if (signal.aborted) return;

    // A pack may offer a richer way to diagnose (a platform's own command); otherwise a plain read-only prompt.
    const richer = packs.diagnoseCommand();
    const runOptions = analysisPlan.analysisRunOptions(false, () => analysisPlan.readMcpServers());
    const safeName = build.jobName.replace(/[^A-Za-z0-9._-]+/g, "_");
    const cwd = path.join(paths.stateDir(), "sessions", `diagnose-${safeName}-${build.number}`);
    fs.mkdirSync(cwd, { recursive: true });
    const sessionId = randomUUID();
    // Stamped before the run so a run that fails partway still leaves a session Continue in Claude Code can open.
    jobStore.patchData(jobId, { claudeSession: { id: sessionId, cwd, permissionMode: "auto" } as ClaudeSession });

    jobStore.update(jobId, { progress: { stepId: "diagnose", label: "Diagnosing with Claude…" } });
    const result = await runClaude({
      cwd,
      prompt: richer ? richer.args(build.buildUrl)[0] : diagnosePrompt.buildReportPrompt({ ...build, flakyLines }),
      policy: runOptions.policy,
      extraAllowedTools: runOptions.extraAllowedTools,
      disallowedTools: analysisPlan.MUTATING_MCP_TOOLS,
      sessionId,
      signal,
      onProgress: (label) => jobStore.setProgressIfRunning(jobId, "diagnose", label),
    });
    if (signal.aborted) return;

    let text = result.text;
    if (richer) {
      // The command writes its own report; put it into the panel's shape with a small tool-less run.
      jobStore.update(jobId, { progress: { stepId: "format", label: "Preparing the report…" } });
      const scratch = fs.mkdtempSync(path.join(os.tmpdir(), "diagnose-build-"));
      try {
        const structured = await runClaude({
          cwd: scratch,
          prompt: diagnosePrompt.buildStructurePrompt(redactSecrets(text)),
          model: config.summarizeComments?.model || DEFAULT_SUMMARIZE_COMMENTS_MODEL,
          policy: "noTools",
          signal,
        });
        text = structured.text;
      } finally {
        fs.rmSync(scratch, { recursive: true, force: true });
      }
      if (signal.aborted) return;
    }
    const report = diagnosePrompt.parseReport(redactSecrets(text));
    jobStore.patchData(jobId, { claudeSession: { id: result.sessionId || sessionId, cwd, permissionMode: "auto" } as ClaudeSession });
    jobStore.update(jobId, {
      status: "awaiting-approval",
      progress: undefined,
      data: { ...(jobStore.get(jobId)?.data ?? {}), report, flakyLines, completedAt: new Date().toISOString() },
      result: { summary: `Diagnosed ${build.jobName} #${build.number}`, files: [] },
    });
  }

  return {
    id: "diagnose-build",
    label: "Diagnose failed builds",

    // Polled read-only shape (like summarize-comments): /start returns the running job; it lands in
    // awaiting-approval with data.report. A second start for the same build returns the job already
    // running or finished unless the payload says force.
    async start(payloadRaw: unknown, ctx: FeatureContext): Promise<Job> {
      const payload = (payloadRaw && typeof payloadRaw === "object" ? payloadRaw : {}) as { buildUrl?: unknown; force?: unknown };
      const build =
        typeof payload.buildUrl === "string"
          ? diagnosePrompt.parseBuildUrl(payload.buildUrl, providersFor(config).ci.baseUrl())
          : null;
      if (!build) {
        const job = jobStore.create("diagnose-build", {});
        jobStore.update(job.id, { status: "failed", error: "That doesn't look like a build page of your configured Jenkins." });
        return jobStore.get(job.id) ?? job;
      }
      const earlier = jobStore.get(latestByBuild.get(build.buildUrl) ?? "");
      if (payload.force !== true && earlier && (earlier.status === "running" || earlier.status === "awaiting-approval")) return earlier;

      const job = jobStore.create("diagnose-build", { buildUrl: build.buildUrl, jobName: build.jobName, number: build.number });
      latestByBuild.set(build.buildUrl, job.id);
      const controller = new AbortController();
      const done = run(job.id, build, ctx, controller.signal)
        .catch((err: Error) => {
          if (controller.signal.aborted) jobStore.update(job.id, { status: "rejected", progress: undefined });
          else jobStore.update(job.id, { status: "failed", progress: undefined, error: redactSecrets(err?.message || String(err)) });
        })
        .finally(() => running.delete(job.id));
      running.set(job.id, { controller, done });
      return job;
    },

    async cancel(job: Job): Promise<void> {
      const entry = running.get(job.id);
      if (!entry || job.status !== "running") throw new Error(`Cannot cancel job in status "${job.status}"`);
      jobStore.update(job.id, { status: "rejecting", progress: { stepId: "cancel", label: "Cancelling…" } });
      entry.controller.abort();
      await entry.done;
    },

    async approve(job: Job): Promise<void> {
      throw new JobStatusError(`Cannot approve job in status "${job.status}" — diagnose-build is a read-only report`);
    },

    async reject(job: Job): Promise<void> {
      if (job.status === "awaiting-approval" || job.status === "failed") {
        jobStore.update(job.id, { status: "rejected", progress: undefined });
        return;
      }
      throw new JobStatusError(`Cannot reject job in status "${job.status}"`);
    },
  };
}
