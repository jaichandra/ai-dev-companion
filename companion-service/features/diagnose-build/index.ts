// "Diagnose this failed build": opens Claude Code in a terminal to work out
// why a Jenkins build failed — through a pack's diagnose command when one is
// installed (e.g. a platform's own build-fix command, whose read-only
// behaviour is that command's own), otherwise a plain diagnosis in
// plan (read-only) mode. The build URL
// comes from a web page, so it is re-validated here and only the rebuilt URL
// is used. The diagnosis is not meant to post or change anything.
import * as fs from "fs";
import * as path from "path";
import type { Config } from "../../config";
import type { AuthContext } from "../../core/atlassian";
import { Feature, FeatureContext, Job, jobStore } from "../../core/jobs";
import { providersFor } from "../../core/providers";
import { openClaudeCodeInTerminal } from "../../core/terminal";
import type { HeaderSegment } from "../../core/terminal";

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
};
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
  return {
    id: "diagnose-build",
    label: "Diagnose failed builds",

    async start(payloadRaw: unknown, ctx: FeatureContext): Promise<Job> {
      const job = jobStore.create("diagnose-build", {});
      try {
        const payload = (payloadRaw && typeof payloadRaw === "object" ? payloadRaw : {}) as { buildUrl?: unknown };
        const build =
          typeof payload.buildUrl === "string"
            ? diagnosePrompt.parseBuildUrl(payload.buildUrl, providersFor(config).ci.baseUrl())
            : null;
        if (!build) throw new Error("That doesn't look like a build page of your configured Jenkins.");

        // A pack may offer a richer way to diagnose (a platform's own command); otherwise a plain read-only session.
        const richer = packs.diagnoseCommand();
        const flakyLines = await flakyLinesFor(config, ctx.auth, build);
        const args = richer
          ? richer.args(build.buildUrl)
          : ["--permission-mode", permissionModeFor("diagnose-build"), diagnosePrompt.buildBasicPrompt({ ...build, flakyLines })];

        const safeName = build.jobName.replace(/[^A-Za-z0-9._-]+/g, "_");
        const cwd = path.join(paths.stateDir(), "sessions", `diagnose-${safeName}-${build.number}`);
        fs.mkdirSync(cwd, { recursive: true });

        const header: HeaderSegment[][] = [
          [{ text: `Diagnosing ${build.jobName} #${build.number}`, style: "title" }],
          [{ text: build.buildUrl, style: "link" }],
        ];
        if (flakyLines.length > 0) {
          header.push([{ text: "Known flaky in the shared test history:", style: "label" }]);
          for (const line of flakyLines) header.push([{ text: line.replace(/^- /, "  "), style: "muted" }]);
        }
        await openClaudeCodeInTerminal(cwd, args, header);

        const summary = richer
          ? richer.summary(build)
          : `Opened Claude Code to diagnose ${build.jobName} #${build.number} (a plain diagnosis).`;
        jobStore.update(job.id, { status: "approved", result: { summary, files: [] } });
      } catch (err) {
        jobStore.update(job.id, { status: "failed", error: (err as Error)?.message || String(err) });
      }
      return jobStore.get(job.id) ?? job;
    },

    async approve(job: Job): Promise<void> {
      throw new Error(`Cannot approve job in status "${job.status}" — diagnose-build only opens a terminal`);
    },

    async reject(job: Job): Promise<void> {
      throw new Error(`Cannot reject job in status "${job.status}" — diagnose-build only opens a terminal`);
    },
  };
}
