import type { Config } from "../../config";
import { Feature, FeatureContext, Job, jobStore } from "../../core/jobs";
import { IssueTracker, providersFor } from "../../core/providers";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const plan = require("./plan.js") as {
  parseIssueKey(raw: string): { key: string; projectKey: string };
  assertSubtaskRows(body: unknown): Array<{ summary: string; assignee: string }>;
};

interface JobData {
  issueKey: string;
  projectKey: string;
  createdKeys: string[];
}

interface JiraIssue {
  fields?: {
    issuetype?: { name?: string };
    project?: { key?: string };
  };
}

export function createJiraSubtasksFeature(config: Config): Feature {
  return {
    id: "create-jira-subtasks",
    label: "Create Subtasks",

    // No separate approve step for this feature — the extension's form
    // already collected exactly what to create, so /start does the whole
    // thing (re-verify the ticket, create every row) and only returns once
    // the job has reached a terminal status. See core/jobs.ts's Feature
    // JSDoc: nothing in the contract requires an early return, and
    // server.ts's route already just awaits and returns whatever comes
    // back.
    async start(payloadRaw: unknown, ctx: FeatureContext): Promise<Job> {
      const p = payloadRaw as { issueKey?: unknown } | null;
      if (!p || typeof p.issueKey !== "string") {
        throw new Error("create-jira-subtasks payload must include issueKey");
      }
      const { key: issueKey } = plan.parseIssueKey(p.issueKey);
      const subtasks = plan.assertSubtaskRows(payloadRaw);

      const job = jobStore.create("create-jira-subtasks", {});
      await runJob(job.id, issueKey, subtasks, providersFor(config).issues, ctx);
      return jobStore.get(job.id) as Job;
    },

    async approve(job: Job): Promise<void> {
      // Jobs from this feature never reach "awaiting-approval" (see
      // start() above) — this can only ever be a caller bug.
      throw new Error(`Cannot approve job in status "${job.status}" — create-jira-subtasks has no approve step`);
    },

    async reject(job: Job): Promise<void> {
      throw new Error(`Cannot reject job in status "${job.status}" — create-jira-subtasks has no reject step`);
    },
  };
}

async function runJob(
  jobId: string,
  issueKey: string,
  subtasks: Array<{ summary: string; assignee: string }>,
  issues: IssueTracker,
  ctx: FeatureContext,
): Promise<void> {
  if (!issues.capabilities.has("subtasks")) {
    jobStore.update(jobId, { status: "failed", error: "This issue tracker can't create subtasks." });
    return;
  }
  let issue: JiraIssue;
  try {
    // Re-verified server-side (not just trusted from the extension's
    // client-side condition, which can be stale by the time this runs):
    // the ticket must exist and be a parent type the tracker allows. This same call also
    // proves auth works before anything is created.
    issue = (await issues.getIssueRaw(ctx.auth, issueKey, "issuetype,project")) as JiraIssue;
  } catch (err) {
    jobStore.update(jobId, { status: "failed", error: (err as Error).message });
    return;
  }

  const typeName = issue.fields?.issuetype?.name;
  if (!typeName || !issues.subtaskParentTypes.has(typeName)) {
    jobStore.update(jobId, {
      status: "failed",
      error: `${issueKey} is a ${typeName || "ticket"}, not a ${[...issues.subtaskParentTypes].join(" or ")} — subtasks can only be added to those.`,
    });
    return;
  }
  const projectKey = issue.fields?.project?.key || plan.parseIssueKey(issueKey).projectKey;

  // Jira has no "create N issues atomically" endpoint — each POST below is
  // its own commit — so `created` is declared outside the try/catch and
  // named in the error message on a partial failure, rather than lost,
  // so a retry isn't blind about what already exists.
  const created: string[] = [];
  try {
    for (const subtask of subtasks) {
      const createdIssue = await issues.createSubtask(ctx.auth, {
        projectKey,
        parentKey: issueKey,
        summary: subtask.summary,
        assignee: subtask.assignee,
      });
      if (createdIssue?.key) created.push(createdIssue.key);
    }
  } catch (err) {
    const partial =
      created.length > 0 ? ` The following were already created before this failure: ${created.join(", ")}.` : "";
    jobStore.update(jobId, {
      status: "failed",
      error: `${(err as Error).message}${partial}`,
      data: { issueKey, projectKey, createdKeys: created } as unknown as Record<string, unknown>,
    });
    return;
  }

  jobStore.update(jobId, {
    status: "approved",
    data: { issueKey, projectKey, createdKeys: created } as unknown as Record<string, unknown>,
    result: {
      summary: `Created ${created.length} subtask(s) on ${issueKey}: ${created.join(", ")}.`,
      files: [],
    },
  });
}
