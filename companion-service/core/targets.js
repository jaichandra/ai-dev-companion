// Which Jira projects/issue types the features work on (the Jenkins pipelines
// live with their feature, in a pack).
// The defaults are the values that used to be hard-coded, so an install with
// no new config behaves exactly as before. Pure and dependency-light:
// settings.js validates edits with it, features read it at request time, and
// server.ts serves the effective values to the extension at GET /targets.
const prereqs = require("./prereqs.js");

const environment = require("../environment.js");
const DEFAULT_PROJECTS = environment.targets.projects;
// No type filter by default: any issue of a configured project counts; a list narrows it.
const DEFAULT_ISSUE_TYPES = environment.targets.issueTypes;
const MAX_PROJECTS = 20;
const MAX_ISSUE_TYPES = 10;
const ISSUE_TYPE_MAX = 40;
const PROJECT_KEY_RE = /^[A-Za-z][A-Za-z0-9]{0,19}$/;

const fail = (reason) => ({ ok: false, reason });

function validateProjects(value) {
  if (!Array.isArray(value) || value.length < 1 || value.length > MAX_PROJECTS) {
    return fail(`must be a list of 1 to ${MAX_PROJECTS} project keys`);
  }
  const out = [];
  for (const p of value) {
    if (typeof p !== "string" || !PROJECT_KEY_RE.test(p.trim())) return fail(`"${String(p).slice(0, 30)}" isn't a Jira project key`);
    const upper = p.trim().toUpperCase();
    if (!out.includes(upper)) out.push(upper);
  }
  return { ok: true, value: out };
}

function validateIssueTypes(value) {
  if (!Array.isArray(value) || value.length > MAX_ISSUE_TYPES) {
    return fail(`must be a list of at most ${MAX_ISSUE_TYPES} issue types (empty = any type)`);
  }
  const out = [];
  for (const t of value) {
    const name = typeof t === "string" ? t.trim() : "";
    if (!name || name.length > ISSUE_TYPE_MAX || /[\u0000-\u001f\u007f]/.test(name)) {
      return fail(`issue types must be 1 to ${ISSUE_TYPE_MAX} characters of plain text`);
    }
    if (!out.includes(name)) out.push(name);
  }
  return { ok: true, value: out };
}

function analyzeTargetsFrom(config) {
  const a = (config && config.analyzeIssue) || {};
  const projects = a.projects === undefined ? { ok: false } : validateProjects(a.projects);
  const issueTypes = a.issueTypes === undefined ? { ok: false } : validateIssueTypes(a.issueTypes);
  return {
    projects: projects.ok ? projects.value : DEFAULT_PROJECTS.slice(),
    issueTypes: issueTypes.ok ? issueTypes.value : DEFAULT_ISSUE_TYPES.slice(),
  };
}

/** "PROJ tickets", "CI/PROJ Bugs/Tasks" — for messages. */
function describeAnalyzeScope({ projects, issueTypes }) {
  return `${projects.join("/")} ${issueTypes.length ? issueTypes.map((t) => `${t}s`).join("/") : "tickets"}`;
}

const MAX_COMPONENTS = 50;
const REPO_REF_RE = /^[A-Za-z0-9][A-Za-z0-9._-]*\/[A-Za-z0-9][A-Za-z0-9._-]*$/;

/** Jira component name -> the "PROJECT/repo" key of a mapped clone (config.repos). */
function validateComponentRepoMap(value) {
  if (!value || typeof value !== "object" || Array.isArray(value)) {
    return fail("must be a list of `component = PROJECT/repo` lines");
  }
  const entries = Object.entries(value);
  if (entries.length > MAX_COMPONENTS) return fail(`can have at most ${MAX_COMPONENTS} components`);
  const out = {};
  for (const [component, repo] of entries) {
    const name = component.trim();
    if (!name || name.length > 60 || /[\u0000-\u001f\u007f]/.test(name)) {
      return fail("a component name must be 1 to 60 characters of plain text");
    }
    if (typeof repo !== "string" || !REPO_REF_RE.test(repo) || repo.includes("..")) {
      return fail(`"${String(repo).slice(0, 40)}" isn't a PROJECT/repo`);
    }
    out[name] = repo;
  }
  return { ok: true, value: out };
}

module.exports = {
  DEFAULT_PROJECTS,
  DEFAULT_ISSUE_TYPES,
  validateProjects,
  validateIssueTypes,
  validateComponentRepoMap,
  analyzeTargetsFrom,
  describeAnalyzeScope,
};
