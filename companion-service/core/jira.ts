// Shared "how does a feature build a Jira SiteAuth" helper — Bearer token
// (SiteAuth's default authScheme), the same convention as core/bitbucket.ts's
// bitbucketSite. Moved out of features/analyze-issue/index.ts and
// features/create-jira-subtasks/index.ts (Task 10), where both had this
// exact function under the same name, so core/mcp.ts's generic jira_get_issue
// reader can share it too rather than growing a third copy.
import type { Config } from "../config";
import { DEFAULT_JIRA_BASE_URL } from "../config";
import { authedJson } from "./atlassian";
import type { AuthContext, SiteAuth } from "./atlassian";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const credentials = require("./credentials.js") as { getToken(name: string): string | undefined };
// eslint-disable-next-line @typescript-eslint/no-var-requires
const externalTokens = (require("./external-tokens.js") as { createExternalTokens(): { tokenFor(name: string, baseUrl: string): { token: string } | undefined } }).createExternalTokens();

/** Jira as an authenticated site. Bearer token (SiteAuth's default
 * authScheme) — Jira Server's REST convention. */
export function jiraSite(config: Config): SiteAuth {
  const baseUrl = config.jira?.baseUrl || DEFAULT_JIRA_BASE_URL;
  return {
    baseUrl,
    label: "Jira",
    configKey: "jira.apiToken",
    // Own saved token first, then the one another tool already stored for this host (core/external-tokens.js).
    apiToken: credentials.getToken("jira.apiToken") ?? externalTokens.tokenFor("jira.apiToken", baseUrl)?.token,
  };
}

// ---- Phase 6: the Jira half of Ticket to PR ----

// eslint-disable-next-line @typescript-eslint/no-var-requires
const workflow = require("./jira-workflow.js") as {
  normalizeIssueBasics(raw: unknown): IssueBasics | null;
  normalizeTransitions(raw: unknown): JiraTransition[];
  remoteLinkBody(link: { url: string; title: string }): unknown;
  normalizeSearchResults(raw: unknown, baseUrl: string): IssueSummary[];
};

/** A search hit (core/jira-workflow.js's normalizeSearchResults). */
export interface IssueSummary {
  key: string;
  summary: string | null;
  status: string | null;
  statusCategory: string | null;
  issueType: string | null;
  priority: string | null;
  updated: string | null;
  description: string | null;
  url: string | null;
}

const SEARCH_FIELDS = "summary,status,issuetype,priority,updated,description";

/** Up to `maxResults` (at most 50) issues matching `jql`. */
export async function searchIssues(site: SiteAuth, auth: AuthContext, jql: string, maxResults = 20): Promise<IssueSummary[]> {
  const query = new URLSearchParams({ jql, fields: SEARCH_FIELDS, maxResults: String(Math.max(1, Math.min(maxResults, 50))) });
  return workflow.normalizeSearchResults(await authedJson(site, auth, `/rest/api/2/search?${query}`), site.baseUrl);
}

export interface IssueBasics {
  key: string | null;
  summary: string | null;
  issueType: string | null;
  status: string | null;
}

export interface JiraTransition {
  id: string;
  name: string | null;
  to: string | null;
}

// Issue keys reach here already validated (features' parseIssueKey); the
// encodeURIComponent is belt and braces.
const issuePath = (key: string) => `/rest/api/2/issue/${encodeURIComponent(key)}`;

export async function getIssueBasics(site: SiteAuth, auth: AuthContext, key: string): Promise<IssueBasics> {
  const basics = workflow.normalizeIssueBasics(
    await authedJson(site, auth, `${issuePath(key)}?fields=summary,issuetype,status`),
  );
  if (!basics) throw new Error(`Jira didn't return ${key}.`);
  return basics;
}

/** Adds (or, by globalId, updates) the ticket's link to a PR. */
export async function addRemoteLink(
  site: SiteAuth,
  auth: AuthContext,
  key: string,
  link: { url: string; title: string },
): Promise<void> {
  await authedJson(site, auth, `${issuePath(key)}/remotelink`, { method: "POST", body: workflow.remoteLinkBody(link) });
}

export async function listTransitions(site: SiteAuth, auth: AuthContext, key: string): Promise<JiraTransition[]> {
  return workflow.normalizeTransitions(await authedJson(site, auth, `${issuePath(key)}/transitions`));
}

export async function transitionIssue(site: SiteAuth, auth: AuthContext, key: string, transitionId: string): Promise<void> {
  await authedJson(site, auth, `${issuePath(key)}/transitions`, { method: "POST", body: { transition: { id: transitionId } } });
}

/** The raw comment list of an issue (up to 1000, oldest first); features/summarize-comments/plan.js normalizes it. */
export async function getIssueComments(site: SiteAuth, auth: AuthContext, key: string): Promise<unknown> {
  return authedJson(site, auth, `${issuePath(key)}/comment?maxResults=1000&orderBy=created`);
}
