// The single place that knows "what features exist and what does each one
// need to be configured/checked." Plain JS, not TypeScript — same reason
// as core/prereqs.js: setup.js requires this directly with zero build
// step, and server.ts requires it too (via commonjs interop) so the two
// never have to keep an id list in sync by hand.
//
// This file no longer lists the features itself: each one declares its own
// descriptor in features/<id>/feature.js, and a pack (packs/*.js, named by
// environment.js) lists them — see core/packs.js. Adding a feature means writing
// that folder (feature.js + index.ts), adding it to a pack, and adding its
// entry to the extension feature file (chrome-extension/features/<id>.js). Nothing in setup.js, server.ts or the
// startup checks changes, since they all drive themselves off this list.
//
// Each descriptor:
//   id             must match the companion-service Feature's own id and
//                  the extension feature file (chrome-extension/features/<id>.js)'s entry.
//   label          shown in the setup wizard's enable/disable prompt.
//   description    one line, shown next to the label so the prompt isn't
//                  just a bare id.
//   enabledByDefault  optional. Omit or true = today's behavior (fresh
//                  install defaults on; soft-migrate auto-enables when
//                  newly shipped). false = ship in the menu list but off
//                  until the user opts in (still recorded in
//                  knownFeatures so a later update doesn't flip it on).
//   summary        optional. A short line for the setup wizard's list (the full
//                  `description` is used elsewhere); falls back to its first sentence.
//   requiredChecks names from core/prereqs.js's CHECKS_BY_NAME — setup.js
//                  and server.ts each only run the *union* of these across
//                  every currently-*enabled* feature, so e.g. a Claude
//                  Code login is never required at all if no enabled
//                  feature actually calls Claude.
//   needsRepos     true if the feature works on local clones mapped in
//                  config.repos. setup.js asks for repos once, as a shared
//                  step, if any enabled feature needs them — rather than
//                  each such feature asking for the same list in turn.
//   async promptSetup(rl, helpers, existingConfig) -> partial config
//                  object to shallow-merge into the written config.json.
//                  Only ever called for a feature the user chose to
//                  enable (and, on a re-run, only if they also asked to
//                  update its settings — see setup.js). `helpers` is
//                  whatever interactive primitives setup.js itself uses
//                  (ask, select, log), passed in rather than required
//                  here, so this file never depends on setup.js (setup.js
//                  depends on this, not the other way around).
const { REVIEW_EDITORS } = require("./setup-helpers.js");
const packs = require("./packs.js");

// One descriptor per feature, in pack order. A feature's descriptor lives in its own
// folder (features/<id>/feature.js) and is listed in a pack (packs/*.js).
const ALL_DESCRIPTORS = packs.features().map((spec) => spec.descriptor);

/** The descriptors in play — ALL_DESCRIPTORS minus any marked
 * `disabled: true`. Everything else in this file and its consumers (setup,
 * Settings, MCP tool selection) reads this list, so a disabled feature
 * vanishes everywhere at once. A disabled id left in an existing
 * config.json's enabledFeatures is filtered out by the `known` checks below. */
const FEATURE_DESCRIPTORS = ALL_DESCRIPTORS.filter((d) => !d.disabled);

/** All descriptor ids, in declared order — used for knownFeatures
 * snapshots and for listing every selectable feature. Not the fresh-
 * install default enable set (see defaultEnabledFeatureIds). */
function allFeatureIds() {
  return FEATURE_DESCRIPTORS.map((d) => d.id);
}

/** Descriptor ids that should be on by default for a fresh install /
 * soft-migrate auto-enable — every descriptor except those with
 * `enabledByDefault: false`. */
function defaultEnabledFeatureIds() {
  return FEATURE_DESCRIPTORS.filter((d) => d.enabledByDefault !== false).map((d) => d.id);
}

/**
 * Soft-migrate an explicit `enabledFeatures` list when the product ships a
 * new descriptor. Returns null when nothing changed; otherwise
 * `{ enabledFeatures, knownFeatures }` ready to merge into config.json.
 *
 * Without this, an existing config that lists older features by id would
 * permanently hide every feature added later (the ✨ menu never shows the
 * row) until the user re-runs interactive setup and notices an unchecked
 * box. That bit every early adopter of analyze-issue.
 *
 * - No `enabledFeatures` field: nothing to migrate (runtime already treats
 *   that as "everything").
 * - No `knownFeatures` yet (legacy): any currently-known id missing from
 *   the enabled list is treated as newly shipped. Ones with
 *   `enabledByDefault: false` are recorded in knownFeatures only (not
 *   auto-enabled); omit/true still turn on. A feature the user had
 *   intentionally unchecked before this field existed gets re-enabled
 *   once (if default-on); after `knownFeatures` is written, interactive
 *   disables stick across updates.
 * - With `knownFeatures`: only ids absent from that snapshot are
 *   candidates for auto-enable (still gated by enabledByDefault); ids
 *   the user unchecked stay off.
 */
function migrateEnabledFeatures(config) {
  if (!Array.isArray(config?.enabledFeatures)) return null;

  const all = allFeatureIds();
  const allSet = new Set(all);
  const defaultOn = new Set(defaultEnabledFeatureIds());
  const knownPrior = Array.isArray(config.knownFeatures)
    ? config.knownFeatures
    : config.enabledFeatures;
  const knownPriorSet = new Set(knownPrior);

  const enabled = new Set(config.enabledFeatures.filter((id) => allSet.has(id)));
  for (const id of all) {
    // Newly shipped: always enter knownFeatures; only auto-enable when
    // the descriptor is default-on (omit/true).
    if (!knownPriorSet.has(id) && defaultOn.has(id)) enabled.add(id);
  }

  const enabledFeatures = all.filter((id) => enabled.has(id));
  const knownFeatures = all;

  const prevEnabled = config.enabledFeatures.filter((id) => allSet.has(id));
  const sameEnabled =
    prevEnabled.length === enabledFeatures.length &&
    enabledFeatures.every((id) => prevEnabled.includes(id));
  const sameKnown =
    Array.isArray(config.knownFeatures) &&
    config.knownFeatures.length === knownFeatures.length &&
    knownFeatures.every((id) => config.knownFeatures.includes(id));
  if (sameEnabled && sameKnown) return null;
  return { enabledFeatures, knownFeatures };
}

/** ids actually enabled by `config` — `config.enabledFeatures` if present,
 * else every known feature (legacy configs predating the field). Applies
 * migrateEnabledFeatures first so a list frozen before a new default-on
 * feature shipped still surfaces that feature; default-off new features
 * stay out of the menu until the user opts in. */
function enabledFeatureIds(config) {
  const migrated = migrateEnabledFeatures(config);
  const effective = migrated ? { ...config, ...migrated } : config;
  if (Array.isArray(effective?.enabledFeatures)) {
    const known = new Set(allFeatureIds());
    return effective.enabledFeatures.filter((id) => known.has(id));
  }
  return allFeatureIds();
}

/** Union of requiredChecks across exactly the descriptors whose id is in
 * `ids` — e.g. the checks setup.js/server.ts actually need to run for the
 * currently-enabled feature set. */
function requiredChecksFor(ids) {
  const idSet = new Set(ids);
  const names = new Set();
  for (const descriptor of FEATURE_DESCRIPTORS) {
    if (!idSet.has(descriptor.id)) continue;
    for (const name of descriptor.requiredChecks) names.add(name);
  }
  return [...names];
}

/** True if any descriptor whose id is in `ids` works on mapped repos. */
function needsRepos(ids) {
  return FEATURE_DESCRIPTORS.some((d) => ids.includes(d.id) && d.needsRepos);
}

/** Labels of the descriptors in `ids` that require check `checkName` —
 * what setup offers to switch off when that check can't be satisfied. */
function featuresRequiring(checkName, ids) {
  return FEATURE_DESCRIPTORS.filter((d) => ids.includes(d.id) && d.requiredChecks.includes(checkName));
}

module.exports = {
  REVIEW_EDITORS,
  FEATURE_DESCRIPTORS,
  allFeatureIds,
  defaultEnabledFeatureIds,
  migrateEnabledFeatures,
  enabledFeatureIds,
  requiredChecksFor,
  needsRepos,
  featuresRequiring,
};
