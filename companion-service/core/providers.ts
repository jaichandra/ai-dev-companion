// The seam between features and the external systems they work with. A
// feature asks for `providers.git`, `providers.issues` or `providers.ci` and
// never imports a concrete client, so a different git host, tracker or CI
// server is a new implementation of one of these interfaces rather than a change
// to every feature.
//
// Which implementation backs each role comes from the environment profile
// (environment.js `sites`: each has a `kind` and a `provider` id). The
// implementations below wrap the existing clients (core/bitbucket.ts,
// core/jira.ts, core/jenkins.ts) and bind them to the live `config`, so a
// base URL or token saved from Settings applies on the next call — nothing is
// cached across calls except the provider objects themselves.
//
// Optional methods are declared by `capabilities`; a feature checks the set
// instead of assuming, so a provider without (say) default reviewers degrades
// the feature rather than breaking it.
import { AuthContext, authedJson } from "./atlassian";
import { Config } from "../config";
import * as bitbucket from "./bitbucket";
import * as github from "./github";
import * as jenkins from "./jenkins";
import * as jira from "./jira";
// eslint-disable-next-line @typescript-eslint/no-var-requires
const environment = require("../environment.js") as {
  sites: Array<{ id: string; kind: SiteKind; provider: string }>;
  issues: { subtaskTypeName: string; subtaskParentTypes: string[] };
};

export type SiteKind = "git" | "issues" | "ci";

export type { DashboardPullRequest, PullRequestSummary, BuildSummary } from "./bitbucket";
export type { IssueBasics, IssueSummary, JiraTransition } from "./jira";
export type { JenkinsTestReport } from "./jenkins";

/** Optional things a git host may or may not offer. */
export type GitCapability = "defaultReviewers" | "buildStatus" | "dashboard" | "mergeStatus";
/** Optional things an issue tracker may or may not offer. */
export type IssueCapability = "transitions" | "remoteLinks" | "linkedPullRequests" | "subtasks";

interface ProviderBase {
  /** The implementation's id, as named in the profile (`environment.sites[].provider`). */
  readonly id: string;
  /** The configured base URL, no trailing slash. Read live from config. */
  baseUrl(): string;
  /** Whether a token is configured (saved, or borrowed from another tool). */
  hasToken(): boolean;
}

export interface GitHost extends ProviderBase {
  readonly capabilities: ReadonlySet<GitCapability>;
  /** The web address of a pull request. */
  prUrl(project: string, repo: string, prId: number): string;
  whoAmI(auth: AuthContext): Promise<string | null>;
  getPullRequest(auth: AuthContext, project: string, repo: string, prId: number): Promise<Record<string, unknown> | null>;
  listActivities(auth: AuthContext, project: string, repo: string, prId: number): Promise<Record<string, unknown>[]>;
  replyToComment(auth: AuthContext, project: string, repo: string, prId: number, parentId: number, text: string): Promise<unknown>;
  getDefaultBranch(auth: AuthContext, project: string, repo: string): Promise<string | null>;
  listBranchPullRequests(auth: AuthContext, project: string, repo: string, branch: string): Promise<bitbucket.PullRequestSummary[]>;
  createPullRequest(
    auth: AuthContext,
    args: { project: string; repo: string; title: string; description: string; fromBranch: string; toBranch: string; reviewers: string[] },
  ): Promise<bitbucket.PullRequestSummary>;
  /** With the "defaultReviewers" capability. */
  getRepositoryId(auth: AuthContext, project: string, repo: string): Promise<number>;
  getDefaultReviewers(
    auth: AuthContext,
    project: string,
    repo: string,
    ids: { sourceRepoId: number; targetRepoId: number; sourceBranch: string; targetBranch: string },
  ): Promise<string[]>;
  /** With the "buildStatus" capability. */
  getCommitBuildStatus(auth: AuthContext, project: string, repo: string, sha: string): Promise<bitbucket.BuildSummary | null>;
  /** With the "dashboard" capability. */
  listDashboardPullRequests(auth: AuthContext, role: "AUTHOR" | "REVIEWER"): Promise<bitbucket.DashboardPullRequest[]>;
  /** With the "mergeStatus" capability. */
  getMergeStatus(
    auth: AuthContext,
    project: string,
    repo: string,
    prId: number,
  ): Promise<{ conflicted: boolean | null; canMerge: boolean | null; outcome: string | null }>;
}

export interface IssueTracker extends ProviderBase {
  readonly capabilities: ReadonlySet<IssueCapability>;
  /** The web address of an issue. */
  issueUrl(key: string): string;
  /** Issue types a subtask can be added under. */
  readonly subtaskParentTypes: ReadonlySet<string>;
  searchIssues(auth: AuthContext, query: string, maxResults?: number): Promise<jira.IssueSummary[]>;
  getIssueBasics(auth: AuthContext, key: string): Promise<jira.IssueBasics>;
  /** The tracker's own JSON for an issue with the named fields (comma-separated). Callers still
   * read the provider's shape; normalizing it is future work. */
  getIssueRaw(auth: AuthContext, key: string, fields: string): Promise<unknown>;
  /** The tracker's own JSON for the issue's comments, oldest first. */
  getIssueComments(auth: AuthContext, key: string): Promise<unknown>;
  /** Posts a comment already in the tracker's markup; returns its id when known. */
  addComment(auth: AuthContext, key: string, body: string): Promise<{ id?: string }>;
  /** With the "transitions" capability. */
  listTransitions(auth: AuthContext, key: string): Promise<jira.JiraTransition[]>;
  transitionIssue(auth: AuthContext, key: string, transitionId: string): Promise<void>;
  /** With the "remoteLinks" capability. */
  addRemoteLink(auth: AuthContext, key: string, link: { url: string; title: string }): Promise<void>;
  /** With the "remoteLinks" capability: the tracker's own JSON for the issue's links to other systems. */
  getRemoteLinksRaw(auth: AuthContext, key: string): Promise<unknown>;
  /** With the "linkedPullRequests" capability: the tracker's own JSON for the PRs linked to an issue id. */
  getLinkedPullRequestsRaw(auth: AuthContext, issueId: string): Promise<unknown>;
  /** With the "subtasks" capability. `assignee` is a username; the provider turns it into
   * whatever reference its API wants (Jira Server: `{name}`, Jira Cloud: `{accountId}`). */
  createSubtask(
    auth: AuthContext,
    args: { projectKey: string; parentKey: string; summary: string; assignee: string },
  ): Promise<{ key?: string }>;
}

export interface CiServer extends ProviderBase {
  fetchTestReport(auth: AuthContext, job: string, buildNumber: number): ReturnType<typeof jenkins.fetchTestReport>;
}

export interface Providers {
  git: GitHost;
  issues: IssueTracker;
  ci: CiServer;
}

/** Builds a provider for `config`; `siteId` is the profile site it serves (the key its settings live under). */
type Factory<T> = (config: Config, siteId: string) => T;

// ---- GitHub and GitHub Enterprise ----

/** Maps GitHub onto the git-host interface: owner = project, repo name = repo, PR number = prId. Token-only
 * (a personal access token); GitHub has no default reviewers, so those come back empty. */
export function githubProvider(config: Config, siteId: string): GitHost {
  const site = () => github.githubSite(config, siteId);
  return {
    id: "github",
    capabilities: new Set<GitCapability>(["buildStatus", "dashboard", "mergeStatus"]),
    baseUrl: () => site().web,
    hasToken: () => !!site().rest.apiToken,
    prUrl: (owner, repo, prId) => `${site().web}/${owner}/${repo}/pull/${prId}`,
    whoAmI: (auth) => github.whoAmI(site(), auth),
    getPullRequest: (auth, owner, repo, prId) => github.getPullRequest(site(), auth, owner, repo, prId),
    listActivities: (auth, owner, repo, prId) => github.listActivities(site(), auth, owner, repo, prId),
    replyToComment: (auth, owner, repo, prId, parentId, text) => github.replyToComment(site(), auth, owner, repo, prId, parentId, text),
    getDefaultBranch: (auth, owner, repo) => github.getDefaultBranch(site(), auth, owner, repo),
    listBranchPullRequests: (auth, owner, repo, branch) => github.listBranchPullRequests(site(), auth, owner, repo, branch),
    createPullRequest: (auth, args) => github.createPullRequest(site(), auth, args),
    getRepositoryId: (auth, owner, repo) => github.getRepositoryId(site(), auth, owner, repo),
    getDefaultReviewers: () => github.getDefaultReviewers(),
    getCommitBuildStatus: (auth, owner, repo, sha) => github.getCommitBuildStatus(site(), auth, owner, repo, sha),
    listDashboardPullRequests: (auth, role) => github.listDashboardPullRequests(site(), auth, role),
    getMergeStatus: (auth, owner, repo, prId) => github.getMergeStatus(site(), auth, owner, repo, prId),
  };
}

// ---- Bitbucket Server / Data Center ----

export function bitbucketDcProvider(config: Config): GitHost {
  const site = () => bitbucket.bitbucketSite(config);
  return {
    id: "bitbucket-dc",
    capabilities: new Set<GitCapability>(["defaultReviewers", "buildStatus", "dashboard", "mergeStatus"]),
    baseUrl: () => site().baseUrl,
    hasToken: () => !!site().apiToken,
    prUrl: (project, repo, prId) => `${site().baseUrl}/projects/${project}/repos/${repo}/pull-requests/${prId}`,
    whoAmI: (auth) => bitbucket.whoAmI(site(), auth),
    getPullRequest: (auth, project, repo, prId) => bitbucket.getPullRequest(site(), auth, project, repo, prId),
    listActivities: (auth, project, repo, prId) => bitbucket.listActivities(site(), auth, project, repo, prId),
    replyToComment: (auth, project, repo, prId, parentId, text) =>
      bitbucket.replyToComment(site(), auth, project, repo, prId, parentId, text),
    getDefaultBranch: (auth, project, repo) => bitbucket.getDefaultBranch(site(), auth, project, repo),
    listBranchPullRequests: (auth, project, repo, branch) => bitbucket.listBranchPullRequests(site(), auth, project, repo, branch),
    createPullRequest: (auth, args) => bitbucket.createPullRequest(site(), auth, args),
    getRepositoryId: (auth, project, repo) => bitbucket.getRepositoryId(site(), auth, project, repo),
    getDefaultReviewers: (auth, project, repo, ids) => bitbucket.getDefaultReviewers(site(), auth, project, repo, ids),
    getCommitBuildStatus: (auth, project, repo, sha) => bitbucket.getCommitBuildStatus(site(), auth, project, repo, sha),
    listDashboardPullRequests: (auth, role) => bitbucket.listDashboardPullRequests(site(), auth, role),
    getMergeStatus: (auth, project, repo, prId) => bitbucket.getMergeStatus(site(), auth, project, repo, prId),
  };
}

// ---- Jira Server / Data Center ----

export function jiraDcProvider(config: Config): IssueTracker {
  const site = () => jira.jiraSite(config);
  const issuePath = (key: string) => `/rest/api/2/issue/${encodeURIComponent(key)}`;
  return {
    id: "jira-dc",
    capabilities: new Set<IssueCapability>(["transitions", "remoteLinks", "linkedPullRequests", "subtasks"]),
    subtaskParentTypes: new Set(environment.issues.subtaskParentTypes),
    baseUrl: () => site().baseUrl,
    hasToken: () => !!site().apiToken,
    issueUrl: (key) => `${site().baseUrl.replace(/\/+$/, "")}/browse/${key}`,
    searchIssues: (auth, query, maxResults) => jira.searchIssues(site(), auth, query, maxResults),
    getIssueBasics: (auth, key) => jira.getIssueBasics(site(), auth, key),
    getIssueRaw: (auth, key, fields) => authedJson(site(), auth, `${issuePath(key)}?fields=${fields}`),
    getIssueComments: (auth, key) => jira.getIssueComments(site(), auth, key),
    addComment: async (auth, key, body) => {
      const created = (await authedJson(site(), auth, `${issuePath(key)}/comment`, { method: "POST", body: { body } })) as { id?: string | number };
      return { id: created?.id !== undefined ? String(created.id) : undefined };
    },
    listTransitions: (auth, key) => jira.listTransitions(site(), auth, key),
    transitionIssue: (auth, key, id) => jira.transitionIssue(site(), auth, key, id),
    addRemoteLink: (auth, key, link) => jira.addRemoteLink(site(), auth, key, link),
    getRemoteLinksRaw: (auth, key) => authedJson(site(), auth, `${issuePath(key)}/remotelink`),
    // applicationType=stash is the Bitbucket Server link in Jira's development panel.
    getLinkedPullRequestsRaw: (auth, issueId) =>
      authedJson(site(), auth, `/rest/dev-status/latest/issue/detail?issueId=${issueId}&applicationType=stash&dataType=pullrequest`),
    createSubtask: async (auth, { projectKey, parentKey, summary, assignee }) =>
      (await authedJson(site(), auth, "/rest/api/2/issue", {
        method: "POST",
        body: {
          fields: {
            project: { key: projectKey },
            parent: { key: parentKey },
            summary,
            issuetype: { name: environment.issues.subtaskTypeName },
            assignee: { name: assignee },
          },
        },
      })) as { key?: string },
  };
}

// ---- Jenkins ----

export function jenkinsProvider(config: Config): CiServer {
  const site = () => jenkins.jenkinsSite(config);
  return {
    id: "jenkins",
    baseUrl: () => site().baseUrl,
    hasToken: () => !!site().apiToken,
    fetchTestReport: (auth, job, buildNumber) => jenkins.fetchTestReport(site(), auth, job, buildNumber),
  };
}

/** Implementations by role and provider id — the ids `environment.sites[].provider` names. */
export const PROVIDER_FACTORIES: { git: Record<string, Factory<GitHost>>; issues: Record<string, Factory<IssueTracker>>; ci: Record<string, Factory<CiServer>> } = {
  git: { "bitbucket-dc": bitbucketDcProvider, github: githubProvider },
  issues: { "jira-dc": jiraDcProvider },
  ci: { jenkins: jenkinsProvider },
};

function pick<T>(kind: SiteKind, table: Record<string, Factory<T>>, config: Config): T {
  const site = environment.sites.find((s) => s.kind === kind);
  if (!site) throw new Error(`The environment profile has no "${kind}" site.`);
  const factory = table[site.provider];
  if (!factory) throw new Error(`No ${kind} provider named "${site.provider}" (known: ${Object.keys(table).join(", ")}).`);
  return factory(config, site.id);
}

export function createProviders(config: Config): Providers {
  return {
    git: pick("git", PROVIDER_FACTORIES.git, config),
    issues: pick("issues", PROVIDER_FACTORIES.issues, config),
    ci: pick("ci", PROVIDER_FACTORIES.ci, config),
  };
}

const memo = new WeakMap<Config, Providers>();

/** The providers for `config`, built once per config object. `config` is read live through each call,
 * so a Settings change is picked up without rebuilding. */
export function providersFor(config: Config): Providers {
  let providers = memo.get(config);
  if (!providers) {
    providers = createProviders(config);
    memo.set(config, providers);
  }
  return providers;
}
