// The handler behind POST /features/<id>/:jobId/actions/:action (server.ts
// registers it per feature that declares `actions`): 404 for an unknown job,
// a job of another feature, or an action name the feature doesn't define
// (own properties only, so "constructor" or "__proto__" are unknown too);
// otherwise runs the action and answers with its result. Split out so the
// route's checks are testable without starting the server.
import type { Feature, FeatureContext, Job } from "./jobs";
import { jobStore } from "./jobs";

interface Req {
  params: Record<string, string>;
}
interface Res {
  status(code: number): Res;
  json(body: unknown): unknown;
}

export function featureActionsHandler(
  feature: Feature,
  helpers: {
    contextFor(req: never): FeatureContext;
    sendError(res: never, err: unknown): void;
    present(job: Job): Job;
  },
) {
  const actions = feature.actions || {};
  return async (req: Req, res: Res): Promise<void> => {
    const job = jobStore.get(req.params.jobId);
    if (!job || job.featureId !== feature.id) {
      res.status(404).json({ error: "unknown job id" });
      return;
    }
    const name = req.params.action;
    if (!Object.prototype.hasOwnProperty.call(actions, name)) {
      res.status(404).json({ error: `${feature.label} has no action "${String(name).slice(0, 40)}".` });
      return;
    }
    try {
      const result = await actions[name](job, helpers.contextFor(req as never));
      res.json(result && typeof result === "object" && (result as Job).id === job.id ? helpers.present(result as Job) : result);
    } catch (err) {
      helpers.sendError(res as never, err);
    }
  };
}
