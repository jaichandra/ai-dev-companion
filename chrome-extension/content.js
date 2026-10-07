// Runs on every page this extension is scoped to (see manifest.json's
// content_scripts.matches — Bitbucket Server + Jira today). Shows a floating
// icon button (FAB) whenever at least one registered feature is currently
// applicable to this page, or a job for one is being tracked. Clicking it
// opens a flyout listing whichever features are applicable right now. Once a
// feature is started, the FAB switches to tracking that job instead: a
// checklist "pill" shows live progress, and the job is persisted to
// chrome.storage.local (keyed by feature id + page scope) so a page refresh
// resumes watching it rather than losing track of it — or worse, letting the
// user re-click and start a duplicate. All network calls to the companion
// service go through background.js (see the comment there for why).
(function () {
  // Every feature file has registered by now (the manifest loads them before this script).
  const FEATURES = PaiRegistry.all();
  let panelHost = null;
  let onPanelClosed = null; // called once when the current overlay panel closes
  let currentOptions = []; // [{feature, payload, match}], idle-mode flyout entries
  let savedResultFeatureIds = new Set(); // features whose saved result exists (green check, no highlight)
  let storedJobsByFeatureId = new Map(); // Maps feature.id -> {feature, jobKey, stored, job} for features with existing jobs
  let popoverOpen = false;
  let hasAutoOpenedOnResume = false;
  let repoChoiceOpenedForJobId = null; // popover opened once for this job's "Which repository?"
  let autoOpenPanelForJobId = null; // set by selectOption(); consumed once terminal
  // Set while a /start request is in flight (see renderStarting). The
  // periodic refreshAll must not redraw the popover then: the job isn't
  // stored yet, so it would show the feature menu again for a moment.
  let startingFeature = null;
  // Set while the popover shows a one-shot result, which has no stored job
  // to redraw it from: refreshAll leaves the popover alone until `until`
  // passes, or until the user dismisses it when `until` is null (failures).
  // Navigating to another page (these are single-page apps) releases it.
  let heldNotice = null;
  // {kind: "update" | "reload" | "running", label, info} while a new version
  // (or a mismatch between this extension and the service) is pending.
  let updateOffer = null;
  let updateCheckedAt = 0;
  const UPDATE_CHECK_INTERVAL_MS = 60 * 1000;
  const UPDATE_FRESH_ON_OPEN_MS = 15 * 1000;
  // The companion's inbox (GET /notifications): what the background watchers
  // found. Its unseen count shows on the ✨ badge, its newest items under
  // "Ready for you" in the menu, on every covered page.
  let inboxState = { unseen: 0, items: [] };
  let inboxCheckedAt = 0;
  const INBOX_FRESH_MS = 30 * 1000;
  const INBOX_MENU_ITEMS = 5;

  function sendToBackground(message) {
    return new Promise((resolve, reject) => {
      chrome.runtime.sendMessage(message, (response) => {
        if (chrome.runtime.lastError) {
          reject(new Error(chrome.runtime.lastError.message));
          return;
        }
        if (response && response.error) {
          const err = new Error(response.error);
          err.details = response.details;
          reject(err);
          return;
        }
        resolve(response);
      });
    });
  }

  // The configured Jenkins pipelines and Jira projects (GET /targets). Falls
  // back to the built-in defaults until the service answers, and keeps the last
  // good answer if it later can't be reached.
  let currentTargets = PaiTargets.DEFAULT_TARGETS;
  let targetsFetchedAt = 0;
  const TARGETS_TTL_MS = 10000;
  async function refreshTargets() {
    const now = Date.now();
    if (now - targetsFetchedAt < TARGETS_TTL_MS) return;
    targetsFetchedAt = now;
    try {
      const { targets } = await sendToBackground({ type: "get-targets" });
      if (PaiTargets.validTargets(targets)) {
        currentTargets = targets;
        PaiGit.configure(targets);
      }
    } catch {
      // Keep whatever we had.
    }
  }

  // Which features the *companion service* actually has registered — e.g.
  // a feature disabled in config.json (see companion-service's
  // enabledFeatures / core/feature-registry.js) never gets built into its
  // FEATURES array or given routes at all, regardless of what this file's
  // own AI_ASSISTANT_FEATURES array knows how to render. refreshAll's idle
  // pass filters against this so a disabled feature's menu row simply
  // never appears, rather than appearing and then failing confusingly
  // when clicked. Cached briefly (this is a local loopback call, but no
  // reason to make it on every single refresh tick) and fails *open* — a
  // lookup failure (e.g. the service is down) doesn't filter anything;
  // the action itself will surface its own clear "Could not reach the
  // companion service" error if that's really what's going on.
  let enabledFeatureIdsCache = null; // Set<string> | null
  let enabledFeatureIdsCacheAt = 0;
  const ENABLED_FEATURE_IDS_TTL_MS = 10000;
  async function getEnabledFeatureIds() {
    await refreshTargets();
    const now = Date.now();
    if (enabledFeatureIdsCache && now - enabledFeatureIdsCacheAt < ENABLED_FEATURE_IDS_TTL_MS) {
      return enabledFeatureIdsCache;
    }
    try {
      const { features } = await sendToBackground({ type: "list-features" });
      enabledFeatureIdsCache = new Set((features || []).map((f) => f.id));
      enabledFeatureIdsCacheAt = now;
    } catch {
      // Leave any previous cache (or null) as-is — see fail-open note above.
    }
    return enabledFeatureIdsCache;
  }

  // Cheap, synchronous, page-shape-independent bits of "where are we" — kept
  // separate from each feature's own urlPattern/condition (often
  // network-backed) so building this never has to wait on an API round trip.
  function buildPageCtx() {
    return {
      url: location.href,
      origin: location.origin,
      pathname: location.pathname,
      search: location.search,
      targets: currentTargets,
      // Asks the companion something on a feature's behalf (a `condition` can't reach the background script on
      // its own): `ctx.service("pr-status", { project, repo, prId })`. See git-host.js, background.js.
      service: (type, payload) => sendToBackground({ type, ...payload }),
    };
  }

  // A feature's urlPattern (if any) tested against the current page. Returns
  // { urlMatches, match } — match is the regex match array (so a feature's
  // condition can read capture groups, e.g. a ticket key or PR id) or null
  // when the feature has no urlPattern at all.
  function matchFeatureUrl(feature) {
    // A feature belongs to one kind of server ("git" | "issues" | "ci"); on a page of any other
    // configured server it isn't offered, even if the path happens to look right. The service says
    // which origin each kind has (GET /targets); until it answers there is nothing to compare.
    const kindOrigin = feature.site && currentTargets.sites ? currentTargets.sites[feature.site] : null;
    if (kindOrigin && kindOrigin !== location.origin) return { urlMatches: false, match: null };
    if (!feature.urlPattern) return { urlMatches: true, match: null };
    const pattern =
      typeof feature.urlPattern === "function" ? feature.urlPattern(currentTargets) : feature.urlPattern;
    const match = location.href.match(pattern);
    return { urlMatches: !!match, match };
  }

  // ---- Persisted "which job is this page tracking" (survives a refresh) ----
  //
  // Scoped by feature id (so a Bitbucket job and a Jira job never collide)
  // and by urlPattern match[0] when the feature has one (so e.g. a PR's
  // /pull-requests/123/diff and /pull-requests/123/overview tabs share one
  // key), falling back to the bare pathname for a feature with no
  // urlPattern. Deliberately never keyed on `condition` — a job must stay
  // trackable even after its own condition goes false (the conflict got
  // resolved; the subtasks now exist).
  function storageKey(feature, ctx, match) {
    const scope = match ? match[0] : ctx.pathname;
    return `pai:activeJob:${feature.id}:${ctx.origin}${scope}`;
  }
  function getStoredJob(key) {
    return new Promise((resolve) => {
      chrome.storage.local.get([key], (result) => resolve(result[key] || null));
    });
  }
  function setStoredJob(key, value) {
    return new Promise((resolve) => {
      chrome.storage.local.set({ [key]: value }, resolve);
    });
  }
  function clearStoredJob(key) {
    return new Promise((resolve) => {
      chrome.storage.local.remove([key], resolve);
    });
  }

  // ---- Adopting a job an MCP tool started (Task 6) ----
  //
  // An MCP tool (terminal Claude) can create a "pending-start" job for a
  // feature's scopeKey without this page's involvement at all — this
  // page has no stored jobKey for it yet, and won't unless it adopts one.
  // refreshAll's pass 1 below fills that gap: when a feature has no
  // stored job, it asks the service (GET /jobs/lookup) whether one
  // exists for this page's scopeKey, and if so starts tracking it exactly
  // like a browser-started job.
  //
  // Throttled per jobKey (module-level, survives across refreshAll ticks
  // but not a reload) so the 2s poll interval and the MutationObserver's
  // scheduleRefresh don't turn this into a lookup on every single tick.
  const mcpLookupThrottle = new Map(); // jobKey -> Date.now() of the last lookup
  const MCP_LOOKUP_THROTTLE_MS = 10000;

  // Which "pending-start" job ids have a Start/Dismiss request in flight
  // right now — jobId -> "start" | "dismiss". Module-level (not a local
  // in renderPendingStartActions) because that function's buttons are
  // rebuilt from scratch, freshly enabled, every time pollStoredJob's
  // ~2s poll calls renderPill again while the job is still
  // "pending-start" (the server doesn't flip it to "rejected" until
  // startFeatureJob has actually resolved) — a per-DOM-instance disabled
  // flag alone would let a poll tick hand the user a second, live,
  // enabled Start button mid-request. Consulted by renderPendingStartActions
  // to render the buttons already disabled/busy-labelled for a job this
  // map still lists.
  const pendingActionInFlight = new Map();

  // Which job ids this page has already adopted from a lookup — kept in
  // chrome.storage.local (not memory) so it survives a reload. It exists
  // so a job the user dismissed (dismiss-pending) or finished
  // (clearStoredJob, once approved/rejected) is never re-adopted by the
  // very next lookup: the service still has it (jobStore.lookup only
  // excludes approved/rejected, and dismissing turns a pending job
  // rejected, but a *started* one goes on existing under the tracking
  // jobKey until it too reaches a terminal status), so without this list
  // the next tick would just pull it straight back onto the FAB. Capped
  // at the newest 200 so a long-lived browser profile can't grow it
  // forever.
  const SEEN_MCP_JOBS_KEY = "pai:seenMcpJobs";
  const MAX_SEEN_MCP_JOBS = 200;

  function getSeenMcpJobs() {
    return new Promise((resolve) => {
      chrome.storage.local.get([SEEN_MCP_JOBS_KEY], (result) => resolve(result[SEEN_MCP_JOBS_KEY] || []));
    });
  }

  // Marks `jobId` seen unless it already was, returning whether it was
  // newly added — i.e. whether this page should go ahead and adopt it.
  async function adoptMcpJobIfUnseen(jobId) {
    const seen = await getSeenMcpJobs();
    if (seen.includes(jobId)) return false;
    const next = [...seen, jobId].slice(-MAX_SEEN_MCP_JOBS);
    await new Promise((resolve) => chrome.storage.local.set({ [SEEN_MCP_JOBS_KEY]: next }, resolve));
    return true;
  }

  // Asks the service for an MCP-started job on this feature/page, subject
  // to the throttle above. Any failure — the service is down, or a 400
  // (a malformed scopeKey; shouldn't happen given features.js's own
  // scopeKey builders, but this is a fire-and-forget background check,
  // not something worth surfacing) — just means "nothing to adopt this
  // round"; the next tick tries again.
  async function lookupMcpJob(feature, match, jobKey) {
    const now = Date.now();
    if (now - (mcpLookupThrottle.get(jobKey) || 0) < MCP_LOOKUP_THROTTLE_MS) return null;
    mcpLookupThrottle.set(jobKey, now);
    try {
      const scopeKey = feature.scopeKey(match);
      if (!scopeKey) return null;
      const { job } = await sendToBackground({ type: "lookup-job", scopeKey, featureId: feature.id });
      return job || null;
    } catch {
      return null;
    }
  }

  function formatElapsed(ms) {
    const totalSec = Math.max(0, Math.floor(ms / 1000));
    const m = Math.floor(totalSec / 60);
    const s = totalSec % 60;
    return `${m}:${String(s).padStart(2, "0")}`;
  }

  // ---- Floating action button + popover (flyout or progress pill) ----

  // FAB stays above the overlay panel (panel uses z-index one lower) so the
  // ✨ button remains clickable when a collapsed rail sits on the right.
  // When the panel is expanded the FAB is hidden; when collapsed it docks to
  // the rail as one visual unit (see .fab-wrap.rail-attached).
  const fabHost = document.createElement("div");
  fabHost.style.cssText = "position:fixed;top:0;left:0;width:0;height:0;z-index:2147483647;";
  document.body.appendChild(fabHost);
  // Shadow DOM so the host page's styles can't bleed into this (and vice
  // versa) — same isolation approach as the review panel below.
  const fabShadow = fabHost.attachShadow({ mode: "open" });

  // "closed" | "expanded" | "collapsed" — drives FAB show/hide + rail docking.
  let overlayUiState = "closed";
  // Last visibility request from feature/job refresh (independent of overlay).
  let fabWantedVisible = false;

  const fabStyle = document.createElement("style");
  fabStyle.textContent = `
    :host { all: initial; }
    .fab-wrap, .popover {
      --pai-accent:#0067c5; --pai-accent-dark:#0052a3; --pai-accent-bg:#f4fbff;
      --pai-text:#1c1c1c; --pai-muted:#6f6f6f; --pai-border:#e0e0e0;
    }
    .fab-wrap { position:fixed; bottom:28px; right:28px; }
    /* Dock ~¼ of the FAB over the 28px collapsed rail so they read as one unit. */
    .fab-wrap.rail-attached { right:14px; }
    .fab {
      position:relative; width:52px; height:52px; border-radius:50%; border:none;
      cursor:pointer; display:flex; align-items:center; justify-content:center;
      font-size:22px; background:linear-gradient(135deg,#0067c5,#1a82e2);
      box-shadow:0 6px 16px rgba(0,103,197,.38), 0 2px 4px rgba(9,30,66,.2), inset 0 0 0 2px rgba(255,255,255,.22);
      transition:transform .15s ease, box-shadow .3s ease, background .3s ease;
    }
    .fab:hover { transform:translateY(-2px); box-shadow:0 10px 22px rgba(0,103,197,.45), 0 2px 6px rgba(9,30,66,.22), inset 0 0 0 2px rgba(255,255,255,.28); }
    .fab:focus-visible { outline:none; box-shadow:0 0 0 3px #fff, 0 0 0 5px #0067c5; }
    .fab.busy { animation: pai-pulse-blue 1.1s ease-in-out infinite; }
    .fab.ready { background:linear-gradient(135deg,#00875A,#36B37E); animation: pai-pulse-green 1.4s ease-in-out infinite; }
    .fab.error { background:linear-gradient(135deg,#DE350B,#FF7452); animation: pai-pulse-red 1.4s ease-in-out infinite; }
    /* Collapsed rail: drop the colored halo; share the rail's left-edge shadow. */
    .fab-wrap.rail-attached .fab,
    .fab-wrap.rail-attached .fab.busy,
    .fab-wrap.rail-attached .fab.ready,
    .fab-wrap.rail-attached .fab.error {
      animation:none;
      box-shadow:-6px 0 18px rgba(9,30,66,.32), -2px 0 6px rgba(9,30,66,.18);
    }
    .fab-wrap.rail-attached .fab:hover { transform:scale(1.04); }
    @keyframes pai-pulse-blue {
      0%,100% { box-shadow:0 6px 16px rgba(0,103,197,.38), 0 2px 4px rgba(9,30,66,.2), 0 0 0 0 rgba(0,103,197,.45); }
      50%     { box-shadow:0 6px 16px rgba(0,103,197,.38), 0 2px 4px rgba(9,30,66,.2), 0 0 0 9px rgba(0,103,197,0); }
    }
    @keyframes pai-pulse-green {
      0%,100% { box-shadow:0 6px 16px rgba(0,135,90,.38), 0 2px 4px rgba(9,30,66,.2), 0 0 0 0 rgba(0,135,90,.45); }
      50%     { box-shadow:0 6px 16px rgba(0,135,90,.38), 0 2px 4px rgba(9,30,66,.2), 0 0 0 9px rgba(0,135,90,0); }
    }
    @keyframes pai-pulse-red {
      0%,100% { box-shadow:0 6px 16px rgba(222,53,11,.38), 0 2px 4px rgba(9,30,66,.2), 0 0 0 0 rgba(222,53,11,.45); }
      50%     { box-shadow:0 6px 16px rgba(222,53,11,.38), 0 2px 4px rgba(9,30,66,.2), 0 0 0 9px rgba(222,53,11,0); }
    }
    .fab-badge {
      position:absolute; top:-2px; right:-2px; width:14px; height:14px; border-radius:50%;
      background:#FF5630; border:2px solid #fff; box-sizing:border-box; display:none; box-shadow:0 1px 3px rgba(9,30,66,.3);
    }
    .fab-badge.update {
      background:#FFA500; animation: pai-pulse-update 2s ease-in-out infinite;
    }
    /* Update prompt beside the FAB, so an update is noticed without opening the menu. */
    .fab-update-chip {
      position:absolute; right:calc(100% + 10px); bottom:8px; display:none; align-items:center; gap:2px;
      background:#fff; border:1px solid #FFA500; border-radius:16px; padding:2px 4px 2px 12px;
      box-shadow:0 4px 12px rgba(9,30,66,.2); white-space:nowrap; font:600 13px/1.2 system-ui,sans-serif;
    }
    .fab-update-chip .chip-main {
      border:none; background:none; cursor:pointer; padding:5px 4px; font:inherit; color:#974F0C;
    }
    .fab-update-chip .chip-main:hover { text-decoration:underline; }
    .fab-update-chip .chip-close {
      border:none; background:none; cursor:pointer; padding:4px 8px; font:inherit; color:#6f6f6f; border-radius:12px;
    }
    .fab-update-chip .chip-close:hover { background:#f1f3f6; }
    @keyframes pai-pulse-update {
      0%,100% { transform: scale(1); opacity: 1; }
      50% { transform: scale(1.2); opacity: 0.8; }
    }
    .popover {
      position:fixed; bottom:92px; right:28px; width:300px; background:#fff;
      border:1px solid var(--pai-border); border-radius:8px; padding:6px;
      box-shadow:0 12px 32px rgba(9,30,66,.2), 0 2px 6px rgba(9,30,66,.1);
      font-family:-apple-system,BlinkMacSystemFont,Helvetica,Arial,sans-serif;
      display:none; box-sizing:border-box; color:var(--pai-text);
    }
    .popover.open { display:block; animation: pai-pop .14s ease-out; }
    @keyframes pai-pop { from { opacity:0; transform:translateY(6px); } to { opacity:1; transform:none; } }
    .popover-header {
      display:flex; align-items:center; gap:8px;
      font-size:11px; text-transform:uppercase; letter-spacing:.06em; color:var(--pai-muted);
      font-weight:600; padding:8px 4px 8px 10px; margin-bottom:4px; border-bottom:1px solid var(--pai-border);
    }
    .popover-header > span:first-child { flex:1 1 auto; min-width:0; }
    .popover-elapsed { font-size:11px; color:var(--pai-accent); font-weight:600; text-transform:none; letter-spacing:0; }
    .popover-gear {
      flex:0 0 auto; width:32px; height:32px; margin:-6px 0; padding:0; border:none;
      border-radius:4px; background:none; color:var(--pai-muted); cursor:pointer;
      display:flex; align-items:center; justify-content:center;
    }
    .popover-gear + .popover-gear { margin-left:-4px; }
    .popover-gear:hover { background:var(--pai-accent-bg); color:var(--pai-accent); }
    .popover-gear:focus-visible, .flyout-item:focus-visible { outline:none; box-shadow:0 0 0 2px rgba(0,103,197,.35); }
    .flyout-item {
      display:block; width:100%; text-align:left; padding:9px 10px 9px 12px; border:none;
      border-left:3px solid transparent; background:none; border-radius:4px; cursor:pointer;
      font:inherit; font-size:13px; font-weight:500; color:var(--pai-text); box-sizing:border-box;
    }
    .flyout-item:hover { background:var(--pai-accent-bg); border-left-color:var(--pai-accent); color:var(--pai-accent-dark); }
    .flyout-empty { padding:8px 12px 10px; font-size:12px; color:var(--pai-muted); }
    .flyout-update {
      color:var(--pai-accent-dark); font-weight:600; background:var(--pai-accent-bg);
      border-bottom:1px solid var(--pai-border); border-radius:4px; margin-bottom:4px;
    }
    .fab-badge.count {
      width:auto; min-width:18px; height:18px; padding:0 4px; top:-4px; right:-4px; border-radius:9px;
      font:700 11px/14px -apple-system,BlinkMacSystemFont,Helvetica,Arial,sans-serif; color:#fff; text-align:center;
    }
    .flyout-inbox {
      margin:2px 0 8px; padding:2px 0 4px; background:#f7f8fa; border:1px solid var(--pai-border);
      border-radius:6px;
    }
    .flyout-inbox-title {
      display:flex; align-items:center; justify-content:space-between; padding:6px 10px 4px;
      font-size:10.5px; font-weight:600; letter-spacing:.06em; text-transform:uppercase; color:var(--pai-muted);
    }
    .flyout-inbox-count {
      display:inline-block; min-width:16px; margin-left:6px; padding:0 5px; box-sizing:border-box; border-radius:8px;
      background:var(--pai-accent); color:#fff; font-size:10px; line-height:16px; text-align:center; letter-spacing:0;
    }
    .flyout-inbox-seen {
      border:none; background:none; color:var(--pai-accent-dark); font-size:11px; font-weight:500;
      text-transform:none; letter-spacing:0; cursor:pointer; padding:2px 4px; border-radius:3px;
    }
    .flyout-inbox-seen:hover { background:#fff; text-decoration:underline; }
    .flyout-inbox-item {
      font-size:11px; line-height:1.4; padding:6px 10px; color:#42526e; overflow-wrap:anywhere;
    }
    /* Clamp on an inner span: on the padded button itself, overflow clips at the padding
       edge and a third line shows through the bottom padding. */
    .flyout-inbox-text {
      display:-webkit-box; -webkit-line-clamp:2; -webkit-box-orient:vertical; overflow:hidden;
    }
    .flyout-inbox-item + .flyout-inbox-item { border-top:1px solid #eceef1; border-radius:0; }
    .flyout-inbox-item:hover { background:#fff; color:var(--pai-accent-dark); }
    .flyout-inbox-item.urgent { font-weight:600; color:#BF2600; border-left-color:#DE350B; }

    .pill-checklist { padding:2px 10px 10px; }
    .pill-step { display:flex; align-items:flex-start; gap:8px; padding:4px 0; font-size:12.5px; color:#42526e; }
    .pill-step-icon { width:16px; text-align:center; flex-shrink:0; line-height:1.4; }
    .pill-step-label {
      flex:1 1 auto; min-width:0; overflow-wrap:anywhere; line-height:1.4;
    }
    .pill-step.done .pill-step-icon { color:#00875A; }
    .pill-step.current .pill-step-label { color:#172b4d; font-weight:600; }
    .pill-step.current .pill-step-icon { display:inline-block; animation: pai-spin 1s linear infinite; }
    @keyframes pai-spin { from { transform:rotate(0deg); } to { transform:rotate(360deg); } }
    .pill-message { padding:4px 10px 10px; font-size:12.5px; color:#42526e; line-height:1.4; }
    .pill-message-error { color:#611a15; }
    .pill-action {
      display:block; width:calc(100% - 20px); margin:2px 10px 8px; padding:8px 10px;
      border:none; border-radius:4px; cursor:pointer; font-size:13px; font-weight:600;
      color:#fff; background:var(--pai-accent);
    }
    .pill-action:hover { background:var(--pai-accent-dark); }
    .pill-action.secondary { color:var(--pai-text); background:#fff; border:1px solid #a7a7a7; }
    .pill-action.secondary:hover { background:var(--pai-accent-bg); }
    .pill-action:disabled { opacity:.6; cursor:default; }
    .flyout-item.completed {
      background:#E3FCEF; border:1px solid #34B88A; border-left-width:3px; color:#172b4d;
    }
    .flyout-item.completed:before {
      content: '✓ ';
      color: #34B88A;
      font-weight: 700;
      margin-right: 4px;
    }
    .flyout-item.completed:hover { background:#CCF7E8; }
    .flyout-item.failed {
      background:#FFEBE6; border:1px solid #FF7452; border-left-width:3px; color:#172b4d;
    }
    .flyout-item.failed:before { content:'! '; color:#DE350B; font-weight:700; margin-right:4px; }
    .flyout-item.failed:hover { background:#FFD5CC; }
    .flyout-item.saved:before {
      content: '✓ ';
      color: #34B88A;
      font-weight: 700;
      margin-right: 4px;
    }
  `;
  fabShadow.appendChild(fabStyle);

  const fabWrap = document.createElement("div");
  fabWrap.className = "fab-wrap";
  fabWrap.style.display = "none"; // hidden until a feature is applicable (or tracking a job)

  const fabBtn = document.createElement("button");
  fabBtn.className = "fab";
  fabBtn.title = "AI Assistant";
  fabBtn.textContent = "✨";

  const fabBadge = document.createElement("div");
  fabBadge.className = "fab-badge";

  const updateChip = document.createElement("div");
  updateChip.className = "fab-update-chip";
  const updateChipMain = document.createElement("button");
  updateChipMain.className = "chip-main";
  const updateChipClose = document.createElement("button");
  updateChipClose.className = "chip-close";
  updateChipClose.textContent = "✕";
  updateChipClose.title = "Hide until the next page load";
  updateChip.append(updateChipMain, updateChipClose);
  // Labels the user hid with ✕; a new version has a new label, so it shows again.
  const dismissedUpdateLabels = new Set();
  updateChipMain.addEventListener("click", (event) => {
    event.stopPropagation();
    if (updateOffer) openUpdatePanel(updateOffer);
  });
  updateChipClose.addEventListener("click", (event) => {
    event.stopPropagation();
    if (updateOffer) dismissedUpdateLabels.add(updateOffer.label);
    applyUpdateChip();
  });

  fabWrap.append(fabBtn, fabBadge, updateChip);
  fabShadow.appendChild(fabWrap);

  const popover = document.createElement("div");
  popover.className = "popover";
  fabShadow.appendChild(popover);

  function setFabState(state) {
    fabBtn.classList.remove("busy", "ready", "error");
    if (state !== "idle") fabBtn.classList.add(state);
  }

  function applyPopoverVisibility() {
    popover.classList.toggle("open", popoverOpen);
  }

  function onOutsideClick(event) {
    // Clicks inside the shadow root are retargeted when observed from the
    // light DOM, so composedPath is needed to tell "inside the fab/popover"
    // apart from "elsewhere on the page".
    const path = event.composedPath ? event.composedPath() : [];
    if (path.includes(fabHost)) return;
    setPopoverOpen(false);
  }

  function setPopoverOpen(open) {
    popoverOpen = open;
    applyPopoverVisibility();
    applyUpdateChip();
    if (open) {
      document.addEventListener("click", onOutsideClick, true);
    } else {
      document.removeEventListener("click", onOutsideClick, true);
    }
  }

  fabBtn.addEventListener("click", (event) => {
    event.stopPropagation();
    setPopoverOpen(!popoverOpen);
    // Opening the menu re-asks about updates unless the answer is fresh.
    if (popoverOpen && Date.now() - updateCheckedAt > UPDATE_FRESH_ON_OPEN_MS) {
      updateCheckedAt = 0;
      scheduleRefresh();
    }
  });

  function applyFabChrome() {
    const railAttached = overlayUiState === "collapsed";
    fabWrap.classList.toggle("rail-attached", railAttached);
    // Expanded slideout owns the viewport edge — FAB must not be visible or clickable.
    if (overlayUiState === "expanded") {
      fabWrap.style.display = "none";
      setPopoverOpen(false);
      return;
    }
    // Collapsed rail or fully closed: show when features/jobs want the FAB.
    fabWrap.style.display = fabWantedVisible ? "block" : "none";
    if (!fabWantedVisible) setPopoverOpen(false);
  }

  function setOverlayUiState(next) {
    overlayUiState = next;
    applyFabChrome();
  }

  function setFabVisible(visible) {
    fabWantedVisible = visible;
    applyFabChrome();
  }

  // In both the menu's and a tracked job's popover header, so Settings is
  // reachable wherever the ✨ button shows, whichever features apply.
  const SVG_NS = "http://www.w3.org/2000/svg";
  function headerIcon(paths) {
    const svg = document.createElementNS(SVG_NS, "svg");
    svg.setAttribute("viewBox", "0 0 24 24");
    svg.setAttribute("width", "20");
    svg.setAttribute("height", "20");
    svg.setAttribute("fill", "none");
    svg.setAttribute("stroke", "currentColor");
    svg.setAttribute("stroke-width", "1.8");
    svg.setAttribute("stroke-linecap", "round");
    svg.setAttribute("stroke-linejoin", "round");
    svg.setAttribute("aria-hidden", "true");
    for (const d of paths) {
      const path = document.createElementNS(SVG_NS, "path");
      path.setAttribute("d", d);
      svg.appendChild(path);
    }
    return svg;
  }

  // Feather-style "sun" and "settings" glyphs.
  const ICON_DIGEST = [
    "M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8z",
    "M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4",
  ];
  const ICON_SETTINGS = [
    "M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6z",
    "M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z",
  ];

  function headerActionButton(feature, onClick) {
    const btn = document.createElement("button");
    btn.className = "popover-gear";
    btn.type = "button";
    btn.title = feature.menuLabel;
    btn.setAttribute("aria-label", feature.menuLabel);
    btn.appendChild(headerIcon(ICON_DIGEST));
    btn.addEventListener("click", (event) => {
      event.stopPropagation();
      onClick();
    });
    return btn;
  }

  function settingsButton() {
    const btn = document.createElement("button");
    btn.className = "popover-gear";
    btn.type = "button";
    btn.title = "Settings";
    btn.setAttribute("aria-label", "Settings");
    btn.appendChild(headerIcon(ICON_SETTINGS));
    btn.addEventListener("click", (event) => {
      event.stopPropagation();
      setPopoverOpen(false);
      openSettingsPanel();
    });
    return btn;
  }

  // ---- Idle mode: flyout of applicable features ----

  // What renderFlyout last built: { signature, header, bindings }. Kept so a
  // refresh that produces the same menu can leave those buttons alone —
  // rebuilding them is what used to eat clicks (see renderFlyout).
  let flyoutView = null;

  // Everything about the menu that changes its DOM. Payloads are excluded on
  // purpose: they ride on the bindings instead, so a fresher payload doesn't
  // cost a rebuild.
  function flyoutSignature() {
    return JSON.stringify([
      updateOffer ? updateOffer.label : null,
      currentOptions.map((o) => o.feature.id),
      inboxState.items.slice(0, INBOX_MENU_ITEMS).map((i) => i.id),
      // The "Ready for you (n)" heading shows this count, so a change rebuilds the menu.
      inboxState.unseen,
      // Also rebuild when the set of completed jobs changes
      Array.from(storedJobsByFeatureId.keys()).sort(),
      Array.from(savedResultFeatureIds).sort(),
    ]);
  }

  /** Reads the inbox at most every 30 s (a failure keeps the last answer). */
  async function refreshInbox(force = false) {
    if (!force && Date.now() - inboxCheckedAt < INBOX_FRESH_MS) return;
    inboxCheckedAt = Date.now();
    try {
      const view = await sendToBackground({ type: "notifications-get" });
      const items = Array.isArray(view?.items) ? view.items : [];
      inboxState = { unseen: Number.isInteger(view?.unseen) ? view.unseen : items.length, items };
    } catch {
      // The service may be down; the badge just keeps what it last knew.
    }
  }

  /** Shows the update prompt beside the FAB unless the menu is open (it lists the update itself) or it was hidden. */
  function applyUpdateChip() {
    const show = !!updateOffer && !popoverOpen && !dismissedUpdateLabels.has(updateOffer.label);
    if (show) updateChipMain.textContent = `⬆ ${updateOffer.label}`;
    updateChip.style.display = show ? "flex" : "none";
  }

  function applyFabBadge(showDot, hasUpdate) {
    applyUpdateChip();
    const n = inboxState.unseen;
    fabBadge.classList.toggle("count", n > 0);
    fabBadge.classList.toggle("update", n === 0 && hasUpdate);
    fabBadge.textContent = n > 0 ? (n > 99 ? "99+" : String(n)) : "";
    if (n > 0) {
      fabBadge.title = `${n} ready for you`;
    } else if (hasUpdate) {
      fabBadge.title = "Update available";
    } else {
      fabBadge.title = "";
    }
    fabBadge.style.display = n > 0 || showDot ? "block" : "none";
  }

  /** "Ready for you (N)": the newest inbox items, each opening its page. */
  function renderInboxSection() {
    const box = document.createElement("div");
    box.className = "flyout-inbox";
    const title = document.createElement("div");
    title.className = "flyout-inbox-title";
    const label = document.createElement("span");
    label.textContent = "Ready for you";
    const count = document.createElement("span");
    count.className = "flyout-inbox-count";
    count.textContent = String(inboxState.unseen);
    label.appendChild(count);
    const seenAll = document.createElement("button");
    seenAll.className = "flyout-inbox-seen";
    seenAll.type = "button";
    seenAll.textContent = "Mark all seen";
    seenAll.addEventListener("click", async (event) => {
      event.stopPropagation();
      await sendToBackground({ type: "notifications-seen", all: true }).catch(() => {});
      await refreshInbox(true);
      flyoutView = null;
      renderFlyout();
    });
    title.append(label, seenAll);
    box.appendChild(title);
    for (const entry of inboxState.items.slice(0, INBOX_MENU_ITEMS)) {
      const item = document.createElement("button");
      item.className = `flyout-item flyout-inbox-item${entry.urgent ? " urgent" : ""}`;
      item.type = "button";
      // Server text (PR titles, ticket summaries): textContent only.
      const text = document.createElement("span");
      text.className = "flyout-inbox-text";
      text.textContent = entry.title;
      item.appendChild(text);
      if (entry.body) item.title = entry.body;
      item.addEventListener("click", async () => {
        setPopoverOpen(false);
        await sendToBackground({ type: "notification-open", id: entry.id }).catch(() => {});
        await refreshInbox(true);
        let url = null;
        try {
          url = entry.url && new URL(entry.url).protocol === "https:" ? entry.url : null;
        } catch {
          url = null;
        }
        // The page's own ✨ picks up a pre-warmed job through /jobs/lookup.
        if (url && url !== location.href) window.open(url, "_blank", "noopener");
        else scheduleRefresh();
      });
      box.appendChild(item);
    }
    return box;
  }

  /** What clicking a menu entry does; shared by the list rows and the header shortcut. */
  function activateBinding(binding) {
    const { feature, payload } = binding.opt;
    const existingJob = binding.storedJobInfo;

    // If a feature has an existing completed job, open it instead of starting new
    if (existingJob && existingJob.job && !feature.renderStartForm && !feature.oneShot) {
      setPopoverOpen(false);
      showPanel(feature, existingJob.job, existingJob.jobKey);
      return;
    }

    if (feature.renderStartForm) {
      // A feature with renderStartForm needs input collected before
      // anything starts (e.g. create-jira-subtasks' subtask rows) —
      // that goes straight to the big overlay panel rather than this
      // small popover, which content.js's own periodic refresh
      // (scheduleRefresh -> refreshAll -> renderFlyout) can rewrite
      // out from under anything long-lived it holds. Closing the
      // popover here is a real close (a different panel takes over),
      // not the flicker the plain branch below avoids.
      setPopoverOpen(false);
      openComposePanel(feature, payload, buildPageCtx());
    } else if (feature.oneShot) {
      // Same "swap content in place, don't toggle visibility" reasoning
      // as the job-tracking branch below — see its comment. The
      // difference is what happens after: runOneShot never calls
      // selectOption/setStoredJob, since there's no job to track (see
      // features.js's oneShot doc comment for why that matters).
      renderStarting(feature);
      runOneShot(binding.opt);
    } else {
      // Deliberately NOT closing the popover here. It's already open
      // (the user just clicked something inside it) — closing
      // (display:none) and then reopening once /start responds, with
      // nothing re-rendered in between, is exactly what read as a
      // flicker: hidden for a beat, then briefly the *old* flyout list
      // again, then finally the checklist. Swapping content in place
      // (renderStarting, then selectOption's own refreshAll) never
      // toggles visibility at all.
      renderStarting(feature);
      selectOption(binding.opt);
    }
  }

  function renderFlyout() {
    // Refreshes run every couple of seconds — and on every DOM mutation of
    // these SPA pages — so this used to tear down and recreate the buttons
    // constantly. A rebuild landing between a mousedown and its mouseup
    // deletes the button being pressed, and the browser then dispatches the
    // click on a surviving ancestor rather than on the replacement button:
    // the click silently does nothing, and only the user's next one works.
    // Opening the menu schedules one of these refreshes (the update
    // re-check in the FAB handler), which is why it was the *first* click
    // that got eaten. So rebuild only when the menu really differs from
    // what's on screen; otherwise keep the buttons and rebind their options.
    //
    // The parentNode check is what makes "what's on screen" honest:
    // renderStarting, the job pill and the notices all replace the popover's
    // content wholesale, which detaches this header.
    const signature = flyoutSignature();
    if (flyoutView && flyoutView.signature === signature && flyoutView.header.parentNode === popover) {
      for (const opt of currentOptions) {
        const binding = flyoutView.bindings.get(opt.feature.id);
        if (binding) binding.opt = opt;
      }
      return;
    }

    const bindings = new Map();
    const pageOptions = currentOptions.filter((o) => !o.feature.global);
    popover.textContent = "";
    const header = document.createElement("div");
    header.className = "popover-header";
    const title = document.createElement("span");
    title.textContent = "AI Assistant";
    header.appendChild(title);
    // Page-independent features (feature.global) are shortcuts up here, not rows.
    for (const opt of currentOptions.filter((o) => o.feature.global)) {
      const binding = { opt, storedJobInfo: storedJobsByFeatureId.get(opt.feature.id) };
      bindings.set(opt.feature.id, binding);
      header.appendChild(headerActionButton(opt.feature, () => activateBinding(binding)));
    }
    header.appendChild(settingsButton());
    popover.appendChild(header);
    flyoutView = { signature, header, bindings };

    if (inboxState.unseen > 0 && inboxState.items.length > 0) popover.appendChild(renderInboxSection());

    if (updateOffer) {
      const item = document.createElement("button");
      item.className = "flyout-item flyout-update";
      item.textContent = `⬆ ${updateOffer.label}`;
      item.addEventListener("click", () => {
        setPopoverOpen(false);
        openUpdatePanel(updateOffer);
      });
      popover.appendChild(item);
      if (pageOptions.length === 0) return;
    }

    if (pageOptions.length === 0) {
      const empty = document.createElement("div");
      empty.className = "flyout-empty";
      empty.textContent = "No AI actions available for this page right now.";
      popover.appendChild(empty);
      return;
    }

    for (const opt of pageOptions) {
      const item = document.createElement("button");
      item.className = "flyout-item";
      item.textContent = opt.feature.menuLabel;

      // Check if this feature has an existing completed job
      const storedJobInfo = storedJobsByFeatureId.get(opt.feature.id);
      if (storedJobInfo && storedJobInfo.job) {
        // A failed run is not "done": green + ✓ would read as success.
        // quietCompleted: just the ✓ (the "saved" look), no green fill.
        const doneClass = opt.feature.quietCompleted ? "saved" : "completed";
        item.classList.add(storedJobInfo.job.status === "failed" ? "failed" : doneClass);
      }
      if (savedResultFeatureIds.has(opt.feature.id)) item.classList.add("saved");

      // Read through the binding, never the captured `opt`: this button
      // outlives the refresh that created it, and the one thing a refresh
      // can change without rebuilding is the payload behind it.
      const binding = { opt, storedJobInfo };
      bindings.set(opt.feature.id, binding);
      item.addEventListener("click", () => activateBinding(binding));
      popover.appendChild(item);
    }
  }

  // Shown the instant a flyout item is clicked, replacing its content in
  // place (see the click handler above for why this matters) — covers the
  // gap between the click and the first real status poll landing.
  function renderStarting(feature) {
    startingFeature = feature;
    heldNotice = null;
    popover.textContent = "";
    const header = document.createElement("div");
    header.className = "popover-header";
    const title = document.createElement("span");
    title.textContent = feature.menuLabel;
    header.appendChild(title);
    popover.appendChild(header);
    const msg = document.createElement("div");
    msg.className = "pill-message";
    msg.textContent = "Starting…";
    popover.appendChild(msg);
    setFabState("busy");
  }

  // ---- Job-tracking mode: progress checklist / result pill ----

  function renderChecklist(steps, progress) {
    const list = document.createElement("div");
    list.className = "pill-checklist";
    const currentIndex = progress ? steps.findIndex((s) => s.id === progress.stepId) : -1;
    steps.forEach((step, i) => {
      const row = document.createElement("div");
      row.className = "pill-step";
      const icon = document.createElement("span");
      icon.className = "pill-step-icon";
      const label = document.createElement("span");
      label.className = "pill-step-label";
      if (currentIndex !== -1 && i < currentIndex) {
        row.classList.add("done");
        icon.textContent = "✓";
        label.textContent = step.label;
      } else if (i === currentIndex) {
        row.classList.add("current");
        icon.textContent = "⟳";
        label.textContent = (progress && progress.label) || step.label;
      } else {
        icon.textContent = "○";
        label.textContent = step.label;
      }
      row.append(icon, label);
      list.appendChild(row);
    });
    return list;
  }

  // A feature whose work is a known, fixed sequence declares progressSteps
  // and gets the checklist above. One whose work is variable-length can't:
  // pre-deployment-stats examines "build 3 of up to 15" and only knows how
  // many when it stops, so there's no static list to check off. Those get a
  // single live row driven entirely by the server's own progress label —
  // which is where the detail lives anyway (see renderChecklist's use of
  // progress.label for the current step).
  function renderProgress(steps, progress) {
    if (steps && steps.length > 0) return renderChecklist(steps, progress);
    const list = document.createElement("div");
    list.className = "pill-checklist";
    const row = document.createElement("div");
    row.className = "pill-step current";
    const icon = document.createElement("span");
    icon.className = "pill-step-icon";
    icon.textContent = "⟳";
    const label = document.createElement("span");
    label.className = "pill-step-label";
    label.textContent = (progress && progress.label) || "Working…";
    row.append(icon, label);
    list.appendChild(row);
    return list;
  }

  function renderPill(feature, job, jobKey) {
    if (job.status === "running" && feature.repoChoiceAction && job.data && job.data.pendingChoice) {
      setFabState("ready"); // waiting for the user, not working
    } else if (job.status === "running" || job.status === "approving" || job.status === "rejecting") {
      setFabState("busy");
    } else if (job.status === "awaiting-approval" || job.status === "pending-start") {
      setFabState("ready");
    } else if (job.status === "failed") {
      setFabState("error");
    }

    popover.textContent = "";
    const header = document.createElement("div");
    header.className = "popover-header";
    const title = document.createElement("span");
    title.textContent = feature.menuLabel;
    const elapsed = document.createElement("span");
    elapsed.className = "popover-elapsed";
    elapsed.textContent = formatElapsed(Date.now() - job.createdAt);
    header.append(title, elapsed, settingsButton());
    popover.appendChild(header);

    if (job.status === "awaiting-approval") {
      const msg = document.createElement("div");
      msg.className = "pill-message";
      msg.textContent = job.result?.summary || "Ready for review.";
      popover.appendChild(msg);
      const reviewBtn = document.createElement("button");
      reviewBtn.className = "pill-action";
      reviewBtn.textContent = "Review";
      reviewBtn.addEventListener("click", () => {
        setPopoverOpen(false);
        showPanel(feature, job, jobKey);
      });
      popover.appendChild(reviewBtn);
    } else if (job.status === "failed") {
      const msg = document.createElement("div");
      msg.className = "pill-message pill-message-error";
      msg.textContent = job.error || "The job failed.";
      popover.appendChild(msg);
      const viewBtn = document.createElement("button");
      viewBtn.className = "pill-action";
      viewBtn.textContent = "View Details";
      viewBtn.addEventListener("click", () => {
        setPopoverOpen(false);
        showPanel(feature, job, jobKey);
      });
      popover.appendChild(viewBtn);
    } else if (job.status === "approving" || job.status === "rejecting") {
      const msg = document.createElement("div");
      msg.className = "pill-message";
      msg.textContent =
        job.status === "approving" ? "Applying…" : (job.progress && job.progress.label) || "Cleaning up…";
      popover.appendChild(msg);
    } else if (job.status === "pending-start") {
      const msg = document.createElement("div");
      msg.className = "pill-message";
      msg.textContent = `Claude Code asked to run ${feature.menuLabel} here. Nothing runs until you click Start.`;
      popover.appendChild(msg);
      const summaryLine = pendingStartSummary(job);
      if (summaryLine) {
        const summary = document.createElement("div");
        summary.className = "pill-message";
        summary.textContent = summaryLine;
        popover.appendChild(summary);
      }
      popover.appendChild(renderPendingStartActions(feature, job, jobKey));
    } else {
      popover.appendChild(renderProgress(feature.progressSteps, job.progress));
      if (feature.repoChoiceAction && job.status === "running" && job.data && job.data.pendingChoice) {
        popover.appendChild(renderRepoChoice(feature, job, jobKey));
        // The user is needed now (e.g. Start fix started from a panel that
        // has just closed): show the question once instead of waiting for a FAB click.
        if (!popoverOpen && repoChoiceOpenedForJobId !== job.id) {
          repoChoiceOpenedForJobId = job.id;
          hasAutoOpenedOnResume = true;
          setPopoverOpen(true);
        }
      }
      if (feature.cancellable && job.status === "running") {
        popover.appendChild(renderCancelButton(feature, job, jobKey));
      }
    }

    if (!popoverOpen && !hasAutoOpenedOnResume) {
      hasAutoOpenedOnResume = true;
      setPopoverOpen(true);
    }
  }

  // "Which repository?" — a running job that needs the user's word before it
  // goes on (job.data.pendingChoice, see analyze-issue): one button per repo it
  // offers, the suggested one first and highlighted, and "No repo". The answer
  // goes to the feature's named action; the service checks it against what it
  // offered.
  function renderRepoChoice(feature, job, jobKey) {
    const choice = job.data.pendingChoice;
    const box = document.createElement("div");
    box.className = "repo-choice";
    const question = document.createElement("div");
    question.className = "pill-message";
    question.textContent = choice.prompt
      ? choice.prompt
      : choice.suggested
      ? `Analyze in ${choice.suggested}? Nothing is checked out or analyzed until you answer.`
      : "Which repository does this ticket belong to? Nothing is checked out or analyzed until you answer.";
    box.appendChild(question);

    const buttons = [];
    async function answer(repoKey, btn) {
      buttons.forEach((b) => (b.disabled = true));
      btn.textContent = "Starting…";
      try {
        await sendToBackground({
          type: "feature-action",
          featureId: feature.id,
          jobId: job.id,
          action: feature.repoChoiceAction,
          body: { repoKey },
          pageUrl: location.href,
        });
        await refreshAll();
      } catch (err) {
        buttons.forEach((b) => (b.disabled = false));
        alert(`Could not send your answer: ${err.message}`);
      }
    }
    function addButton(label, detail, repoKey, primary) {
      const btn = document.createElement("button");
      btn.className = primary ? "pill-action" : "pill-action secondary";
      btn.textContent = detail ? `${label} — ${detail}` : label;
      btn.addEventListener("click", () => answer(repoKey, btn));
      buttons.push(btn);
      box.appendChild(btn);
    }
    (choice.options || []).forEach((opt) => {
      const suggested = opt.repoKey === choice.suggested;
      addButton(opt.repoKey, opt.reason, opt.repoKey, suggested);
      if (suggested) {
        const hint = document.createElement("div");
        hint.className = "pill-message";
        hint.style.cssText = "padding:0 10px 6px;font-size:12px;";
        hint.textContent = "Recommended. If you think it's not the right one, pick another below.";
        box.appendChild(hint);
      }
    });
    addButton(choice.noRepoLabel || "Analyze without a repo", "", null, false);
    return box;
  }

  // One summary line for a "pending-start" job's own start payload
  // (job.data.payload — see core/jobs.ts's createPending) so the pill
  // says *what* Claude Code asked to run here without opening a panel
  // first. The feature says how (its `describePendingStart(payload)`); one
  // that doesn't renders no summary line.
  function pendingStartSummary(job) {
    const feature = FEATURES.find((f) => f.id === job.featureId);
    return feature && feature.describePendingStart ? feature.describePendingStart(job.data?.payload) : null;
  }

  // The Start / Dismiss pair for a "pending-start" job — the one browser
  // click a write-capable feature needs before Claude Code's MCP tool
  // call actually runs anything (server.ts's /jobs/:jobId/start and
  // /jobs/:jobId/dismiss). Modelled on selectOption's own /start flow: a
  // repo-not-found error opens the same repo setup panel and retries once
  // it succeeds, rather than just failing.
  function renderPendingStartActions(feature, job, jobKey) {
    const startBtn = document.createElement("button");
    startBtn.className = "pill-action";
    const dismissBtn = document.createElement("button");
    dismissBtn.className = "pill-action secondary";

    // busy is "start" | "dismiss" | false — which action (if any) is in
    // flight for this job right now, driving both buttons' disabled
    // state and label together (whichever one is running, clicking the
    // other makes no sense either).
    function setBusy(busy) {
      startBtn.disabled = dismissBtn.disabled = !!busy;
      startBtn.textContent = busy === "start" ? "Starting…" : "Start";
      dismissBtn.textContent = busy === "dismiss" ? "Dismissing…" : "Dismiss";
    }
    // renderPill rebuilds this pair from scratch every ~2s while the job
    // stays "pending-start" (pollStoredJob's poll) — the server doesn't
    // flip it to "rejected" until startFeatureJob has actually resolved,
    // so a poll tick landing mid-request would otherwise hand the user a
    // second, freshly-enabled Start button to click. pendingActionInFlight
    // (module-level, keyed by job id, so it survives this rebuild) is
    // what a *fresh* instance checks here to come up already
    // disabled/busy instead.
    setBusy(pendingActionInFlight.get(job.id) || false);

    async function start() {
      pendingActionInFlight.set(job.id, "start");
      setBusy("start");
      try {
        const { job: newJob } = await sendToBackground({
          type: "start-pending",
          jobId: job.id,
          pageUrl: location.href,
        });
        await setStoredJob(jobKey, { jobId: newJob.id, featureId: feature.id });
        // Already tracked here, so a lookup must never re-adopt it if it
        // ends up failed or otherwise non-terminal.
        await adoptMcpJobIfUnseen(newJob.id);
        autoOpenPanelForJobId = newJob.id;
        await refreshAll();
      } catch (err) {
        if (isRepoNotFound(err)) {
          // Left disabled: the popover reopens showing this same pending
          // job, and start() — called again from here — re-marks it in
          // flight and picks up right where it left off.
          openRepoSetupPanel(feature, err.details, () => {
            setPopoverOpen(true);
            start();
          });
          return;
        }
        setBusy(false);
        alert(`${feature.menuLabel} failed: ${err.message}`);
      } finally {
        pendingActionInFlight.delete(job.id);
      }
    }

    async function dismiss() {
      pendingActionInFlight.set(job.id, "dismiss");
      setBusy("dismiss");
      try {
        await sendToBackground({ type: "dismiss-pending", jobId: job.id });
        await clearStoredJob(jobKey);
        await refreshAll();
      } catch (err) {
        setBusy(false);
        alert(`Dismiss failed: ${err.message}`);
      } finally {
        pendingActionInFlight.delete(job.id);
      }
    }

    startBtn.addEventListener("click", start);
    dismissBtn.addEventListener("click", dismiss);

    const actions = document.createDocumentFragment();
    actions.append(startBtn, dismissBtn);
    return actions;
  }

  // Stops a running job and has the service undo what it changed so far
  // (for resolve-conflict: abort the merge and delete its isolated
  // worktree — nothing was pushed; for analyze-issue: abort Claude).
  // The request returns once cleanup is done; meanwhile the polled pill
  // shows the job as "rejecting". Shown when feature.cancellable is true
  // — including readOnly features while status is "running".
  function renderCancelButton(feature, job, jobKey) {
    const btn = document.createElement("button");
    btn.className = "pill-action secondary";
    btn.textContent = "Cancel";
    btn.addEventListener("click", async () => {
      const detail =
        feature.cancelConfirm || "In-progress work will be discarded.";
      const ok = confirm(`Stop "${feature.menuLabel}"? ${detail}`);
      if (!ok) return;
      btn.disabled = true;
      btn.textContent = "Cancelling…";
      try {
        await sendToBackground({ type: "cancel", featureId: feature.id, jobId: job.id, pageUrl: location.href });
        await clearStoredJob(jobKey);
        setFabState("idle");
        await refreshAll();
      } catch (err) {
        btn.disabled = false;
        btn.textContent = "Cancel";
        alert(`Cancel failed: ${err.message}`);
      }
    });
    return btn;
  }

  // ---- Running a selected feature ----

  async function selectOption(opt) {
    const { feature, payload, match } = opt;
    const ctx = buildPageCtx();
    const jobKey = storageKey(feature, ctx, match);
    try {
      const { job: startedJob } = await sendToBackground({
        type: "start",
        featureId: feature.id,
        payload,
        pageUrl: ctx.url,
      });
      await setStoredJob(jobKey, { jobId: startedJob.id, featureId: feature.id });
      autoOpenPanelForJobId = startedJob.id;
      hasAutoOpenedOnResume = true; // we're about to show it ourselves below
      startingFeature = null;
      await refreshAll();
    } catch (err) {
      startingFeature = null;
      if (isRepoNotFound(err)) {
        openRepoSetupPanel(feature, err.details, () => {
          setPopoverOpen(true);
          renderStarting(feature);
          selectOption(opt);
        });
      } else {
        alert(`${feature.menuLabel} failed: ${err.message}`);
      }
      // No job was created — refresh so the popover moves on from the
      // "Starting…" placeholder instead of leaving it stuck there.
      await refreshAll();
    }
  }

  // ---- One-shot features: a single click that runs to completion, with
  // no job to track or approve afterward (e.g. review-in-editor — the
  // whole point is that you review the checkout in your own editor, not
  // inside the extension). Deliberately never calls setStoredJob: unlike
  // the job-tracking flow above, there's no approve/reject click to ever
  // clear it, and a job that reaches a terminal status on its own would
  // otherwise hijack the FAB on this page forever (see refreshAll's pass
  // 1 and renderPill's fallback branch). Modelled on openComposePanel's
  // submit handler for the same "/start always resolves even when the
  // *job* itself failed" trap — see the comment there.

  async function runOneShot(opt) {
    const { feature, payload } = opt;
    if (feature.renderOneShotPanel) return runOneShotInPanel(opt);
    try {
      const { job } = await sendToBackground({
        type: "start",
        featureId: feature.id,
        payload,
        pageUrl: location.href,
      });
      startingFeature = null;
      if (job.status !== "approved") {
        throw new Error(job.error || `Unexpected job status "${job.status}".`);
      }
      renderOneShotDone(feature, job.result?.summary || "Done.");
    } catch (err) {
      startingFeature = null;
      if (isRepoNotFound(err)) {
        setFabState("idle");
        openRepoSetupPanel(feature, err.details, () => {
          setPopoverOpen(true);
          renderStarting(feature);
          runOneShot(opt);
        });
        return;
      }
      renderOneShotFailed(feature, err.message);
    }
  }

  // A oneShot feature with renderOneShotPanel (see features.js) reports in
  // the overlay panel from the click on, rather than in the popover: the
  // periodic refresh redraws the popover, but the panel stays until the
  // user closes it. Closed before /start answers, the result falls back
  // to the popover so it isn't lost.
  async function runOneShotInPanel(opt) {
    const { feature, payload } = opt;
    setPopoverOpen(false);
    const { body } = openOverlayPanel(feature.menuLabel);
    const thisPanel = panelHost;
    const render = (state) => {
      if (panelHost !== thisPanel) return false;
      body.textContent = "";
      body.appendChild(feature.renderOneShotPanel({ payload, ...state }));
      return true;
    };
    render({ status: "running" });
    try {
      const { job } = await sendToBackground({
        type: "start",
        featureId: feature.id,
        payload,
        pageUrl: location.href,
      });
      startingFeature = null;
      if (job.status !== "approved") {
        throw new Error(job.error || `Unexpected job status "${job.status}".`);
      }
      if (render({ status: "done", job })) {
        setFabState("idle");
        scheduleRefresh();
        followOneShotSession(feature, payload, job, render, thisPanel);
      } else {
        renderOneShotDone(feature, job.result?.summary || "Done.");
      }
    } catch (err) {
      startingFeature = null;
      if (isRepoNotFound(err)) {
        setFabState("idle");
        openRepoSetupPanel(feature, err.details, () => {
          renderStarting(feature);
          runOneShotInPanel(opt);
        });
        return;
      }
      // Refused because something the service started earlier is still
      // live (details.existing); the panel offers what to do about it and
      // starts again with the payload that choice needs.
      const conflict = err.details?.existing ? err.details : null;
      if (
        conflict &&
        render({
          status: "conflict",
          conflict,
          startWith: (nextPayload) => runOneShotInPanel({ ...opt, payload: nextPayload }),
        })
      ) {
        setFabState("idle");
        scheduleRefresh();
        return;
      }
      if (render({ status: "failed", error: err.message })) {
        setFabState("idle");
        scheduleRefresh();
      } else {
        renderOneShotFailed(feature, err.message);
      }
    }
  }

  const ONE_SHOT_POLL_MS = 3000;
  const LIVE_SESSION_STATES = ["starting", "running"];

  // Where an open one-shot panel's job is remembered, so a reload reopens
  // it (restoreOneShotPanels). Deliberately not storageKey's activeJob
  // prefix: refreshAll's pass 1 would treat it as a tracked job and let it
  // take over the FAB.
  function oneShotPanelKey(feature) {
    const { match } = matchFeatureUrl(feature);
    return `pai:oneShotPanel:${feature.id}:${location.origin}${match ? match[0] : location.pathname}`;
  }

  // For a finished one-shot job whose review carries on outside the
  // service (job.result.review.tracked): polls GET /status for its live
  // `session` while the panel is open and the session is live, and gives
  // the panel a stopSession() for its Cancel button. The panel is
  // remembered until closed.
  function followOneShotSession(feature, payload, job, render, thisPanel) {
    const key = oneShotPanelKey(feature);
    void setStoredJob(key, { jobId: job.id, payload });
    onPanelClosed = () => void clearStoredJob(key);
    if (!job.result?.review?.tracked) return;

    // /start's own result (e.g. review-in-editor's "switched to the running
    // review") outlives the stored job's, which later answers carry.
    const { result } = job;
    let stopping = false;
    let stopError = null;
    let shown = null;
    const session = () => job.session || { state: "starting" };
    const show = () => {
      const next = JSON.stringify([session(), stopping, stopError]);
      if (next === shown) return;
      shown = next;
      render({ status: "done", job, session: session(), stopping, stopError, stopSession });
    };
    async function stopSession() {
      stopping = true;
      stopError = null;
      show();
      try {
        const next = await sendToBackground({
          type: "cancel",
          featureId: feature.id,
          jobId: job.id,
          pageUrl: location.href,
        });
        job = { ...next.job, result };
      } catch (err) {
        stopError = err.message;
      }
      stopping = false;
      show();
    }
    async function poll() {
      if (panelHost !== thisPanel || !LIVE_SESSION_STATES.includes(session().state)) return;
      try {
        const next = await sendToBackground({ type: "status", jobId: job.id });
        job = { ...next.job, result };
      } catch {
        // The service may be restarting; the next poll tries again.
      }
      if (panelHost !== thisPanel) return;
      show();
      setTimeout(poll, ONE_SHOT_POLL_MS);
    }
    show();
    void poll();
  }

  // Reopens a one-shot panel that was open when the page was reloaded, if
  // the service still knows its job.
  async function restoreOneShotPanels() {
    for (const feature of FEATURES) {
      if (!feature.oneShot || !feature.renderOneShotPanel || !matchFeatureUrl(feature).urlMatches) continue;
      const key = oneShotPanelKey(feature);
      const stored = await getStoredJob(key);
      if (!stored) continue;
      let job;
      try {
        ({ job } = await sendToBackground({ type: "status", jobId: stored.jobId }));
      } catch (err) {
        // Only forget the panel when the service says the job is gone —
        // not when it's merely unreachable (see isJobGone).
        if (isJobGone(err)) await clearStoredJob(key);
        continue;
      }
      if (job.status !== "approved" || panelHost) continue;
      const { body } = openOverlayPanel(feature.menuLabel);
      const thisPanel = panelHost;
      const render = (state) => {
        if (panelHost !== thisPanel) return false;
        body.textContent = "";
        body.appendChild(feature.renderOneShotPanel({ payload: stored.payload, ...state }));
        return true;
      };
      render({ status: "done", job });
      followOneShotSession(feature, stored.payload, job, render, thisPanel);
      return;
    }
  }

  // ---- First use of a repo with no local clone found ----
  //
  // The companion service answers a start for such a repo with a 409
  // "repo-not-found". Rather than ending in an error, offer the two ways to
  // fix it right here — pick the existing clone's folder (a native picker
  // the service opens, since the browser can't see file paths) or have the
  // service clone it — then retry the original action. The overlay panel
  // rather than the popover because the periodic refresh redraws the
  // popover, and cloning can take minutes.

  function isRepoNotFound(err) {
    return err && err.details && err.details.code === "repo-not-found";
  }

  function openRepoSetupPanel(feature, details, retry) {
    const { project, repo } = details;
    const key = `${project}/${repo}`;
    setPopoverOpen(false);
    const { body, footer, close } = openOverlayPanel(`Set up ${key}`);
    const thisPanel = panelHost;

    const wrap = document.createElement("div");
    wrap.className = "repo-setup";
    const intro = document.createElement("p");
    intro.textContent =
      `${feature.menuLabel} works on your local clone of ${key}, and none was found on this computer.`;
    const options = document.createElement("p");
    options.textContent =
      "If you've already cloned it somewhere, choose its folder. Otherwise the assistant can clone it " +
      "next to your other repos — large repos can take a few minutes the first time. Either way it's " +
      "remembered, so you're only asked once.";
    const status = document.createElement("p");
    status.className = "status";
    wrap.append(intro, options, status);
    body.appendChild(wrap);

    const errorBox = document.createElement("pre");
    errorBox.className = "action-error";
    errorBox.style.display = "none";

    const chooseBtn = document.createElement("button");
    chooseBtn.className = "approve";
    chooseBtn.textContent = "Choose folder…";
    const cloneBtn = document.createElement("button");
    cloneBtn.className = "approve";
    cloneBtn.textContent = "Clone it for me";
    const cancelBtn = document.createElement("button");
    cancelBtn.className = "cancel";
    cancelBtn.textContent = "Cancel";
    cancelBtn.addEventListener("click", close);

    function setBusy(busy, message) {
      chooseBtn.disabled = cloneBtn.disabled = busy;
      status.textContent = message || "";
      if (busy) errorBox.style.display = "none";
    }
    function showError(message) {
      setBusy(false);
      errorBox.textContent = message;
      errorBox.style.display = "block";
    }
    function succeed() {
      close();
      retry();
    }

    chooseBtn.addEventListener("click", async () => {
      setBusy(true, "A folder picker has opened — if you don't see it, check behind this window.");
      try {
        await sendToBackground({ type: "choose-repo", project, repo });
        succeed();
      } catch (err) {
        if (err.details && err.details.code === "cancelled") setBusy(false);
        else showError(err.message);
      }
    });

    cloneBtn.addEventListener("click", async () => {
      setBusy(true, `Starting to clone ${key}…`);
      try {
        let { job } = await sendToBackground({ type: "clone-repo", project, repo, pageUrl: location.href });
        while (job.status === "running") {
          await new Promise((resolve) => setTimeout(resolve, 1500));
          if (panelHost !== thisPanel) return; // closed — the clone carries on server-side
          ({ job } = await sendToBackground({ type: "status", jobId: job.id }));
          if (job.progress) status.textContent = job.progress.label;
        }
        if (job.status !== "approved") throw new Error(job.error || "Cloning failed.");
        succeed();
      } catch (err) {
        showError(err.message);
      }
    });

    const actions = document.createElement("div");
    actions.className = "actions";
    actions.append(chooseBtn, cloneBtn, cancelBtn);
    footer.append(actions, errorBox);
    footer.style.display = "block";
  }

  function renderOneShotHeader(feature) {
    popover.textContent = "";
    const header = document.createElement("div");
    header.className = "popover-header";
    const title = document.createElement("span");
    title.textContent = feature.menuLabel;
    header.appendChild(title);
    popover.appendChild(header);
  }

  function renderOneShotDone(feature, summary) {
    renderOneShotHeader(feature);
    const msg = document.createElement("div");
    msg.className = "pill-message";
    msg.textContent = summary;
    popover.appendChild(msg);
    setFabState("idle");
    // Auto-dismiss back to the idle flyout rather than leaving a stale
    // success message showing indefinitely — scheduleRefresh re-evaluates
    // which features currently apply and rebuilds the flyout from scratch.
    heldNotice = { until: Date.now() + 4000, url: location.href };
    setTimeout(scheduleRefresh, 4000);
  }

  function isNoticeHeld() {
    if (!heldNotice) return false;
    const expired = heldNotice.until !== null && Date.now() >= heldNotice.until;
    if (expired || heldNotice.url !== location.href) {
      heldNotice = null;
      setFabState("idle");
    }
    return !!heldNotice;
  }

  function renderOneShotFailed(feature, message) {
    renderOneShotHeader(feature);
    const msg = document.createElement("div");
    msg.className = "pill-message pill-message-error";
    msg.textContent = message;
    popover.appendChild(msg);
    const detailsBtn = document.createElement("button");
    detailsBtn.className = "pill-action";
    detailsBtn.textContent = "Show Details";
    detailsBtn.addEventListener("click", () => {
      setPopoverOpen(false);
      showOneShotError(feature, message);
    });
    const dismissBtn = document.createElement("button");
    dismissBtn.className = "pill-action secondary";
    dismissBtn.textContent = "Dismiss";
    dismissBtn.addEventListener("click", () => {
      heldNotice = null;
      setFabState("idle");
      refreshAll();
    });
    popover.append(detailsBtn, dismissBtn);
    setFabState("error");
    // Stays, even across the periodic refresh and closing the popover,
    // until Dismiss.
    heldNotice = { until: null, url: location.href };
  }

  // A dedicated overlay for a one-shot failure's full message (a
  // dirty-worktree file list, a branch-checked-out-elsewhere path, etc.
  // can run several lines) — same reasoning as showPanel's own errorBox: a
  // native alert() truncates/wraps long text badly.
  function showOneShotError(feature, message) {
    const { body, footer, close } = openOverlayPanel(feature.menuLabel);
    const errorBox = document.createElement("pre");
    errorBox.className = "action-error";
    errorBox.textContent = message;
    body.appendChild(errorBox);

    const closeBtn = document.createElement("button");
    closeBtn.className = "cancel";
    closeBtn.textContent = "Close";
    closeBtn.addEventListener("click", close);
    const actions = document.createElement("div");
    actions.className = "actions";
    actions.appendChild(closeBtn);
    footer.appendChild(actions);
    footer.style.display = "block";
  }

  // ---- Review panel ----

  let removeEscapeListener = null;

  function closePanel() {
    if (removeEscapeListener) {
      removeEscapeListener();
      removeEscapeListener = null;
    }
    if (panelHost) {
      panelHost.remove();
      panelHost = null;
    }
    const closed = onPanelClosed;
    onPanelClosed = null;
    if (closed) closed();
    // Fully closed — restore default FAB (flyout / job tracking as before).
    setOverlayUiState("closed");
  }

  // Shared overlay size prefs — used by every feature that opens
  // openOverlayPanel (resolve-conflict, create-jira-subtasks,
  // pre-deployment-stats, analyze-issue, …).
  const OVERLAY_WIDTH_KEY = "pai:overlayPanelWidth";
  const OVERLAY_COLLAPSED_KEY = "pai:overlayPanelCollapsed";
  const OVERLAY_MIN_WIDTH = 320;
  // Thin rail; FAB docks against it when collapsed (see .fab-wrap.rail-attached).
  // Chrome buttons shrink when collapsed to fit this strip.
  const OVERLAY_COLLAPSED_WIDTH = 28;
  const OVERLAY_DEFAULT_WIDTH_VW = 50;

  function getOverlayPrefs() {
    return new Promise((resolve) => {
      chrome.storage.local.get([OVERLAY_WIDTH_KEY, OVERLAY_COLLAPSED_KEY], (result) => {
        resolve({
          width: typeof result[OVERLAY_WIDTH_KEY] === "number" ? result[OVERLAY_WIDTH_KEY] : null,
          collapsed: result[OVERLAY_COLLAPSED_KEY] === true,
        });
      });
    });
  }

  function setOverlayPref(key, value) {
    return new Promise((resolve) => {
      chrome.storage.local.set({ [key]: value }, resolve);
    });
  }

  function clampOverlayWidth(px) {
    const max = Math.max(OVERLAY_MIN_WIDTH, Math.floor(window.innerWidth * 0.92));
    return Math.min(max, Math.max(OVERLAY_MIN_WIDTH, Math.round(px)));
  }

  // Generic overlay panel chrome — a wide right-hand shadow-DOM overlay with
  // a title + close button, a scrollable body, and a pinned (non-scrolling)
  // footer. Knows nothing about jobs, forms, or any particular feature: it
  // just hands back empty `body`/`footer` containers for the caller to fill.
  // Two call sites build on this — showPanel() (reviewing an
  // already-started job, e.g. resolve-conflict's diff) and
  // openComposePanel() (collecting input *before* a job exists, e.g.
  // create-jira-subtasks' subtask rows) — so any future feature needing
  // its own overlay UI has this to build on too.
  //
  // Width is draggable (left-edge handle) and persisted in
  // chrome.storage.local; a collapse control shrinks the panel to a thin
  // full-height rail that keeps the title and an expand control. The FAB
  // hides while expanded and docks to the rail when collapsed. Opening
  // always starts expanded — the collapsed pref is only applied when the
  // user hits the toggle.
  function openOverlayPanel(title) {
    closePanel();

    panelHost = document.createElement("div");
    // One below the FAB (2147483647) so a collapsed rail never steals FAB clicks.
    panelHost.style.cssText =
      "position:fixed;top:0;right:0;bottom:0;height:auto;z-index:2147483646;box-sizing:border-box;";
    document.body.appendChild(panelHost);
    // Menu / start-job / auto-open always lands expanded → hide FAB immediately.
    setOverlayUiState("expanded");
    // Shadow DOM so the host page's styles can't bleed into the panel (and
    // vice versa) — this is a self-contained overlay, not a themed part of
    // the page.
    const shadow = panelHost.attachShadow({ mode: "open" });

    const style = document.createElement("style");
    style.textContent = `
      :host { all: initial; }
      .panel {
        background:#fff; height:100%; box-sizing:border-box;
        box-shadow:-8px 0 28px rgba(9,30,66,.22), -1px 0 0 #e0e0e0;
        font-family:-apple-system,BlinkMacSystemFont,Helvetica,Arial,sans-serif;
        font-size:14px; color:#1c1c1c; position:relative;
        display:flex; flex-direction:column;
        pointer-events:auto;
      }
      .panel.collapsed .panel-body,
      .panel.collapsed .panel-footer,
      .panel.collapsed .resize-handle { display:none !important; }
      /* Strong left-edge shadow + hairline so the thin rail reads on white pages.
         FAB uses the same shadow language when .rail-attached. */
      .panel.collapsed {
        align-items:center;
        border-left:1px solid rgba(9,30,66,.22);
        box-shadow:-6px 0 18px rgba(9,30,66,.32), -2px 0 6px rgba(9,30,66,.18);
      }
      .panel-rail {
        display:none; flex:1 1 auto; writing-mode:vertical-rl;
        transform:rotate(180deg); align-items:center; justify-content:center;
        gap:12px; padding:12px 0; cursor:pointer; user-select:none;
        color:#42526e; font-size:12px; font-weight:600; letter-spacing:.02em;
      }
      .panel.collapsed .panel-rail { display:flex; }
      .panel-rail-title {
        overflow:hidden; text-overflow:ellipsis; white-space:nowrap;
        max-height:calc(100% - 80px);
      }
      .resize-handle {
        position:absolute; top:0; left:0; width:6px; height:100%;
        cursor:ew-resize; z-index:2;
      }
      .resize-handle:hover, .resize-handle.dragging {
        background:rgba(0,103,197,.18);
      }
      /* Dotted grip at mid-height so the edge reads as draggable before
         the user hovers it — the handle itself is invisible until then. */
      .resize-handle::after {
        content:""; position:absolute; top:50%; left:50%; width:2px; height:28px;
        transform:translate(-50%,-50%);
        background:radial-gradient(circle, #a5adba 1px, transparent 1px) 0 0 / 2px 6px repeat-y;
      }
      .resize-handle:hover::after, .resize-handle.dragging::after {
        background:radial-gradient(circle, #0052a3 1px, transparent 1px) 0 0 / 2px 6px repeat-y;
      }
      /* Only the body scrolls — Approve/Discard stay pinned in a footer
         below it so a long diff can never push them out of view.
         Flex column so compose mode can grow the textarea between the
         heading and the footer; report mode keeps natural height so
         overflow-y on .panel-body still scrolls tall content. */
      .panel-body {
        flex:1 1 auto; overflow-y:auto; padding:20px; min-height:0;
        display:flex; flex-direction:column;
      }
      .panel-body > h2 { flex:0 0 auto; }
      .panel-body-content { min-width:0; }
      /* Compose: fill between heading and footer; overflow visible so the
         native textarea resize grip is not clipped by the scrollport. */
      .panel-body.compose-mode { overflow:visible; }
      .panel-body.compose-mode .panel-body-content {
        flex:1 1 auto; min-height:0; display:flex; flex-direction:column;
      }
      .panel-footer {
        flex:0 0 auto; padding:14px 20px 16px; border-top:1px solid #e0e0e0;
        background:#fafafa;
      }
      h2 { margin:0 0 14px; font-size:18px; font-weight:600; color:#1c1c1c; padding-right:72px; }
      summary { cursor:pointer; font-weight:600; margin:12px 0 4px; }

      /* "Open in editor" widget (built by open-in-editor.js) */
      .open-in-editor {
        background:#f4fbff; border:1px solid #cfe2f5; border-radius:6px;
        padding:10px 12px; margin-bottom:16px;
      }
      .open-in-editor-label { font-size:12.5px; color:#42526e; margin-bottom:6px; }
      .open-in-editor-path {
        display:block; font-size:11.5px; font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
        color:#0052a3; word-break:break-all; margin-bottom:8px;
      }
      .open-in-editor-actions { display:flex; gap:8px; }
      .open-in-editor-btn {
        display:inline-block; padding:6px 12px; border:none; border-radius:5px;
        font-size:12.5px; font-weight:600; cursor:pointer; text-decoration:none;
        background:#0067c5; color:#fff;
      }
      .open-in-editor-btn-secondary { background:#fff; color:#0052a3; border:1px solid #b3d4f2; }
      .open-in-editor-actions { flex-wrap:wrap; }

      /* review-in-editor's summary panel (features.js's renderOneShotPanel) */
      .review-summary .report-headline { font-size:15px; line-height:1.4; overflow-wrap:anywhere; }
      .review-pr-id { color:#0052a3; margin-right:6px; }
      .review-branches { display:flex; flex-wrap:wrap; align-items:center; gap:6px; margin:8px 0 10px; }
      .branch-chip {
        display:inline-block; padding:3px 9px; border-radius:12px; font-size:12px; font-weight:600;
        font-family:ui-monospace,SFMono-Regular,Menlo,monospace; overflow-wrap:anywhere;
        background:#e3fcef; color:#006644; border:1px solid #abf5d1;
      }
      .branch-chip.target { background:#fffae6; color:#974F0C; border-color:#ffe380; }
      .review-arrow { color:#7a869a; font-weight:700; }
      .review-status {
        display:flex; align-items:center; gap:8px; flex-wrap:wrap;
        margin:16px 0 10px; padding-top:14px; border-top:1px solid #e2e6ea;
      }
      .review-status-text { font-size:13px; color:#42526e; overflow-wrap:anywhere; }
      .review-actions { margin:0 0 14px; }
      .review-hint { margin:0 0 14px; font-size:13px; line-height:1.5; color:#42526e; }
      .review-hint .branch-chip { padding:1px 7px; }
      .review-switched-warn { color:#974F0C; }
      .review-facts {
        display:grid; grid-template-columns:max-content 1fr; gap:6px 14px; margin:0 0 12px; font-size:13px;
      }
      .review-facts dt { color:#7a869a; }
      .review-facts dd { margin:0; color:#172b4d; font-weight:600; }
      .review-stash {
        padding:10px 12px; border-radius:6px; background:#fffae6; border:1px solid #ffe380;
        font-size:12.5px; line-height:1.45; color:#172b4d;
      }
      .review-stash p { margin:0 0 6px; }
      .review-stash code {
        display:block; margin-bottom:6px; font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
        font-size:12px; overflow-wrap:anywhere;
      }
      .review-stash ul { margin:0; padding-left:18px; }

      /* resolve-conflict's optional per-file note (features.js's
         renderPanel) — same callout look as .review-stash above. */
      .file-diff-note {
        padding:8px 12px; border-radius:6px; background:#fffae6; border:1px solid #ffe380;
        font-size:12.5px; line-height:1.45; color:#172b4d; margin:0 0 8px;
      }

      /* Side-by-side diff viewer (built by diff-viewer.js) */
      .diff-viewer {
        border:1px solid #e2e6ea; border-radius:4px; margin-bottom:8px;
        max-height:480px; overflow-y:auto;
      }
      .diff-hunk-header {
        background:#f1f3f6; color:#5c6b7a; font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
        font-size:11px; padding:4px 8px; border-top:1px solid #e2e6ea;
      }
      .diff-hunk-header:first-child { border-top:none; }
      .diff-table-scroll { overflow-x:auto; }
      table.diff-table {
        border-collapse:collapse; width:100%; font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
        font-size:12px; line-height:1.5;
      }
      table.diff-table td { padding:0 8px; white-space:pre; vertical-align:top; }
      td.diff-linenum {
        width:1%; min-width:32px; text-align:right; color:#8a97a6; background:#fafbfc;
        user-select:none; border-right:1px solid #edf0f3;
      }
      td.diff-cell { width:49%; }
      td.diff-ctx { background:#fff; }
      td.diff-add { background:#e6ffec; }
      td.diff-del { background:#ffebe9; }
      td.diff-empty { background:#fafbfc; }
      pre.diff-raw-fallback {
        white-space:pre-wrap; font-size:12px; line-height:1.4; background:#f6f8fa;
        padding:8px; margin:0; border-radius:4px;
      }

      /* create-jira-subtasks' add-a-row form (generic enough for any
         future feature that needs a repeatable row of inputs) */
      .subtask-row-form { margin-bottom:12px; }
      .subtask-row { display:flex; gap:8px; align-items:center; margin-bottom:8px; }
      .subtask-row input {
        width:100%; box-sizing:border-box; padding:7px 9px; font-size:13px;
        border:1px solid #dfe1e6; border-radius:4px;
      }
      /* Assignee text ("Display Name (ssoid)") tends to run longer than a
         subtask name, so it gets the bigger share; min-width:0 on both so
         a flex child's intrinsic content width can't force the row wider
         than the panel instead of actually shrinking. */
      .subtask-row > input:first-child { flex:1 1 0; min-width:0; }
      .subtask-row-remove {
        flex:0 0 auto; width:26px; height:26px; border:none; border-radius:4px;
        background:#f4fbff; color:#42526e; cursor:pointer; font-size:15px; line-height:1;
      }
      .subtask-add-row {
        padding:7px 12px; border:1px dashed #b3d4f2; border-radius:6px; background:#fff;
        color:#0052a3; font-size:13px; font-weight:600; cursor:pointer;
      }
      .subtask-add-row:disabled { opacity:.5; cursor:default; }

      /* Assignee autocomplete (Jira's own assignable-user search, see
         features.js's attachAssigneeAutocomplete) */
      .assignee-field { position:relative; flex:0.75 1 0; min-width:0; }
      .assignee-suggestions {
        position:absolute; top:calc(100% + 2px); left:0; right:0; z-index:1;
        background:#fff; border:1px solid #dfe1e6; border-radius:6px;
        box-shadow:0 4px 12px rgba(0,0,0,.18); max-height:260px; overflow-y:auto;
      }
      .assignee-suggestion {
        display:flex; align-items:baseline; justify-content:space-between; gap:10px;
        padding:7px 10px; font-size:12.5px; cursor:pointer; border-bottom:1px solid #f0f1f4;
      }
      .assignee-suggestion:last-child { border-bottom:none; }
      .assignee-suggestion:hover, .assignee-suggestion.active { background:#f4fbff; }
      .assignee-suggestion-name {
        color:#172b4d; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; min-width:0;
      }
      .assignee-suggestion-id {
        color:#7a869a; flex:0 0 auto; font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:11.5px;
      }

      /* Neutral data table — deliberately NOT .diff-table, whose
         monospace white-space:pre cells are tuned for code and would wrap
         prose badly. Used by pre-deployment-stats' build table; generic
         enough for any future feature reporting rows of data. Wrapped in
         .data-table-scroll so a wide table scrolls itself instead of
         widening the panel. */
      .data-table-scroll { overflow-x:auto; margin-bottom:12px; }
      table.data-table {
        border-collapse:collapse; width:100%; font-size:12.5px; line-height:1.45;
      }
      table.data-table th {
        text-align:left; padding:7px 10px; font-size:11px; font-weight:600;
        text-transform:uppercase; letter-spacing:.04em; color:#7a869a;
        background:#fafbfc; border-bottom:1px solid #dfe1e6; white-space:nowrap;
      }
      table.data-table td {
        padding:8px 10px; border-bottom:1px solid #edf0f3; vertical-align:top; color:#172b4d;
      }
      table.data-table tr:last-child td { border-bottom:none; }
      table.data-table tr.recommended td { background:#f4fbff; }
      table.data-table th.numeric { text-align:center; }
      table.data-table td.numeric { text-align:center; font-variant-numeric:tabular-nums; }
      .data-table a { color:#0052a3; text-decoration:none; font-weight:600; }
      .data-table a:hover { text-decoration:underline; }
      .data-muted { color:#7a869a; }

      /* address-review-comments' per-comment table (features.js's
         renderPanel) — comment/note prose wraps instead of stretching the
         table, and the reply column holds a checkbox plus the server's
         read-only reply text. */
      .review-comments-table td.comment-text { max-width:260px; white-space:pre-wrap; }
      .reply-toggle { display:flex; align-items:flex-start; gap:6px; font-weight:600; cursor:pointer; }
      .reply-toggle input[type="checkbox"] { margin:2px 0 0; }
      pre.reply-text {
        margin:6px 0 0; padding:0; font-family:inherit; white-space:pre-wrap;
        font-size:12px; line-height:1.4; color:#42526e;
      }

      /* address-review-comments' per-comment diff row (features.js's
         renderPanel) — a second <tr>, right under its comment row, whose
         one cell spans every column and holds a collapsed .row-detail
         disclosure. The comment row above drops its own bottom border
         (see tr.review-comment-has-diff) so the pair reads as one block;
         the indent plus tinted background on this row is what still
         tells it apart from an ordinary data row, with no border of its
         own fighting the row above. */
      table.review-comments-table tr.review-comment-has-diff > td { border-bottom:none; }
      tr.review-comment-diff-row > td {
        padding:0 10px 10px 30px; background:#fafbfc; border-bottom:1px solid #edf0f3;
      }

      /* Warns that the diff below touches a hook/CI/script path — approve
         runs the commit with hooks on and unsandboxed, so this needs a
         stronger tone than the yellow .file-diff-note callout. */
      .risky-paths-banner {
        margin:0 0 14px; padding:10px 12px; border-radius:6px; background:#fff5f5;
        border:1px solid #ffbdbd; font-size:12.5px; line-height:1.5; color:#611a15;
      }
      .risky-paths-banner p { margin:0 0 6px; font-weight:600; }
      .risky-paths-banner ul { margin:0; padding-left:18px; }
      .risky-paths-banner li {
        font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:12px; overflow-wrap:anywhere;
      }

      /* Status badges, on the same palette as the FAB states. */
      .status-badge {
        display:inline-block; padding:2px 8px; border-radius:10px; font-size:11px;
        font-weight:700; white-space:nowrap;
      }
      .status-badge.ok { background:#e3fcef; color:#00875A; }
      .status-badge.bad { background:#ffebe6; color:#DE350B; }
      .status-badge.warn { background:#fffae6; color:#974F0C; }
      .status-badge.busy { background:#f4fbff; color:#0052a3; }
      .status-badge.neutral { background:#f1f3f6; color:#5c6b7a; }

      /* Per-row failing-test detail — collapsed by default so a build with
         20-odd failures doesn't bury the table it belongs to. */
      .row-detail { margin-top:6px; }
      .row-detail > summary {
        cursor:pointer; color:#0052a3; font-size:11.5px; font-weight:600; list-style:none;
      }
      .row-detail > summary::-webkit-details-marker { display:none; }
      .row-detail > summary::before { content:"▸ "; }
      .row-detail[open] > summary::before { content:"▾ "; }
      .row-detail ul { margin:6px 0 0; padding-left:16px; }
      .row-detail li { margin-bottom:4px; font-size:11.5px; line-height:1.4; color:#42526e; }
      .row-detail .test-name { overflow-wrap:anywhere; }
      .test-origin { color:#7a869a; font-size:11.5px; line-height:1.35; }
      /* A failure with no origin build to name — styled as a word rather
         than a number so the Origin column still reads as an answer. */
      .origin-intermittent { color:#974F0C; font-weight:600; }

      /* Morning digest (renderDigestPanel in features.js): headline, then one
         card per section; each row carries a tone stripe. */
      .digest-headline {
        margin:0 0 16px; padding:14px 16px; border:1px solid #cfe2f5; border-left:3px solid #0067c5;
        border-radius:4px; background:#f4fbff; font-size:15px; font-weight:600; line-height:1.4;
        color:#1c1c1c; overflow-wrap:anywhere;
      }
      .digest-section {
        margin:0 0 16px; border:1px solid #e0e0e0; border-radius:4px; background:#fff; overflow:hidden;
      }
      .digest-section > .report-headline {
        display:flex; align-items:center; justify-content:space-between; gap:8px; margin:0;
        padding:10px 14px; background:#fafafa; border-bottom:1px solid #e0e0e0;
        font-size:11px; font-weight:600; letter-spacing:.06em; text-transform:uppercase; color:#6f6f6f;
      }
      .digest-count {
        min-width:20px; padding:0 6px; box-sizing:border-box; border-radius:10px; background:#e8e8e8;
        font-size:11px; line-height:20px; text-align:center; letter-spacing:0; color:#525252;
      }
      .digest-row {
        margin:0; padding:10px 14px 10px 11px; border-left:3px solid transparent;
        font-size:13px; line-height:1.5; color:#1c1c1c; overflow-wrap:anywhere;
      }
      .digest-row + .digest-row { border-top:1px solid #f0f0f0; }
      .digest-row.bad { border-left-color:#DE350B; }
      .digest-row.warn { border-left-color:#FF991F; }
      .digest-row.busy { border-left-color:#0067c5; }
      .digest-row.ok { border-left-color:#36B37E; }
      .digest-row .status-badge { margin-right:8px; vertical-align:1px; }
      .digest-row a { color:#0052a3; text-decoration:none; }
      .digest-row a:hover { text-decoration:underline; }
      .digest-more {
        display:block; width:100%; margin:0; padding:9px 14px; border:none; border-top:1px solid #f0f0f0;
        background:#fafafa; color:#0052a3; font:inherit; font-size:12.5px; font-weight:600;
        text-align:left; cursor:pointer;
      }
      .digest-more:hover { background:#f4fbff; text-decoration:underline; }
      .digest-when { margin:0; font-size:12px; color:#6f6f6f; }
      .report-meta { font-size:12px; color:#7a869a; margin-bottom:14px; line-height:1.5; }
      .report-headline { font-size:14px; font-weight:600; color:#172b4d; margin-bottom:6px; }

      .error { color:#c00; font-weight:600; }
      .repo-setup p { margin:0 0 12px; line-height:1.5; }
      .repo-setup .status { color:#0067c5; font-size:13px; overflow-wrap:anywhere; }
      /* Update panel: headline + cards reuse the digest styles above. */
      .update-version { margin:0 0 4px; font-size:12px; font-weight:600; color:#0052a3; }
      .update-notes { margin:0; padding-left:18px; }
      .update-notes li { margin:0 0 4px; font-size:13px; line-height:1.45; color:#1c1c1c; }
      .repo-setup .update-status { margin:0 0 16px; padding:10px 14px 10px 11px; border-left:3px solid #0067c5; background:#f4fbff; }
      .repo-setup .update-status:empty { display:none; }
      .actions { display:flex; gap:8px; flex-wrap:wrap; }
      .actions button {
        padding:8px 16px; border:none; border-radius:4px; cursor:pointer;
        font-size:13px; color:#fff;
      }
      .approve { background:#0067c5; }
      .approve:hover:not(:disabled) { background:#0052a3; }
      .actions button:focus-visible { outline:none; box-shadow:0 0 0 2px #fff, 0 0 0 4px #0067c5; }
      .approve:disabled, .reject:disabled, .cancel:disabled, .post-comment:disabled { opacity:.6; cursor:default; }
      .reject { background:#de350b; }
      .actions button.post-comment {
        background:#fff; color:#0052a3; border:1px solid #b3d4f2;
      }
      .comment-compose {
        display:flex; flex-direction:column; gap:8px;
        flex:1 1 auto; min-height:0; width:100%;
      }
      .comment-compose-hint {
        flex:0 0 auto; font-size:12px; color:#7a869a; line-height:1.45; margin:0;
      }
      /* flex:1 fills the slideout; after mount JS locks an explicit height
         so native resize:vertical can shrink/grow (flex-grow otherwise
         fights the resize handle and snaps height back). */
      .comment-compose textarea {
        flex:1 1 auto; min-height:0; width:100%; box-sizing:border-box;
        font-family:ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
        font-size:12px; line-height:1.45; padding:10px; border:1px solid #dfe1e6;
        border-radius:4px; resize:vertical; overflow:auto; color:#172b4d; background:#fff;
      }
      .comment-compose textarea.compose-sized {
        flex:0 0 auto;
      }
      .comment-compose textarea:focus {
        outline:none; border-color:#0052a3; box-shadow:0 0 0 2px rgba(0,103,197,.15);
      }
      /* Neutral "close without acting" button — distinct from the
         destructive-looking .reject (which discards a job server-side)
         and the primary .approve; a plain close has no such side effect.
         ".actions button" above sets color:#fff for approve/reject's
         colored backgrounds — needs "button.cancel" here, not just
         ".cancel", to actually out-specificity that and not end up with
         invisible white-on-white text. */
      .actions button.cancel { background:#fff; color:#1c1c1c; border:1px solid #a7a7a7; }
      .actions button.cancel:hover { background:#f4fbff; }
      pre.action-error {
        margin-top:12px; max-height:280px; overflow:auto; white-space:pre-wrap;
        font-size:12px; line-height:1.4; background:#fff5f5; border:1px solid #ffbdbd;
        color:#611a15; padding:10px; border-radius:4px;
      }
      .action-ok {
        margin-top:12px; font-size:13px; line-height:1.4; color:#00875A; font-weight:600;
      }
      /* Settings panel (built by settings-panel.js): a searchable form with a
         left-hand page list, following the Settings design review. */
      .panel-body.settings-mode { padding:0; overflow:hidden; }
      .panel-body.settings-mode > h2 { padding:16px 72px 14px 20px; margin:0; border-bottom:1px solid #e0e0e0; }
      .settings-loading { margin:0; padding:20px 24px; font-size:13px; line-height:1.45; color:#6f6f6f; }
      .panel-body.settings-mode .panel-body-content {
        flex:1 1 auto; min-height:0; display:flex; flex-direction:column;
      }
      .panel-body.settings-mode .settings-banner { margin:0 20px 12px; }
      .settings-form {
        flex:1 1 auto; min-height:0; display:flex; flex-direction:column;
        container-type:inline-size; color:#1c1c1c;
      }
      .settings-form [hidden], .settings-form .gated-off, .settings-form .is-off { display:none !important; }
      .settings-search-row { padding:12px 20px; border-bottom:1px solid #e0e0e0; }
      .settings-search {
        width:100%; max-width:420px; box-sizing:border-box; height:32px; padding:0 8px;
        font:inherit; font-size:13px; color:#1c1c1c; border:1px solid #a7a7a7; border-radius:4px; background:#fff;
      }
      .settings-search:focus, .settings-input:focus {
        outline:none; border-color:#0067c5; box-shadow:0 0 0 2px rgba(0,103,197,.2);
      }
      .settings-shell { flex:1 1 auto; min-height:0; display:flex; }
      .settings-nav {
        width:176px; flex:none; box-sizing:border-box; padding:12px 8px; overflow:auto;
        border-right:1px solid #e0e0e0; display:flex; flex-direction:column; gap:2px;
      }
      .settings-nav-item {
        display:flex; align-items:center; gap:8px; padding:8px 10px; cursor:pointer;
        border-left:3px solid transparent; color:#1c1c1c;
      }
      .settings-nav-item:hover { background:#f4fbff; }
      .settings-nav-item.on { border-left-color:#0067c5; background:#f4fbff; color:#0067c5; }
      .settings-nav-text { flex:1; min-width:0; }
      .settings-nav-title { font-size:13px; font-weight:600; }
      .settings-nav-sub { margin-top:1px; font-size:11.5px; color:#6f6f6f; }
      .settings-nav-dot { width:8px; height:8px; flex:none; border-radius:50%; background:#0067c5; }
      .settings-main { flex:1; min-width:0; overflow:auto; padding:20px 24px; }
      .settings-page { display:none; flex-direction:column; gap:16px; }
      .settings-page.on, .searching .settings-page { display:flex; }
      .settings-page-title { font-size:18px; font-weight:600; color:#1c1c1c; }
      .settings-page-intro { margin-top:4px; max-width:640px; font-size:13px; line-height:1.45; color:#6f6f6f; }
      .searching .settings-page-head, .searching .settings-block-head, .searching .settings-page .settings-empty { display:none; }
      .settings-results { margin:0 0 16px; font-size:16px; font-weight:600; }
      .settings-section {
        margin:0; padding:16px 20px; border:1px solid #e0e0e0; background:#fff;
        display:flex; flex-direction:column; gap:14px;
      }
      .settings-path { display:none; font-size:12px; color:#6f6f6f; }
      .searching .settings-path, .searching .settings-feature-path { display:block; }
      .settings-block-title-row { display:flex; align-items:center; gap:10px; flex-wrap:wrap; }
      .settings-section-title { font-size:15px; font-weight:600; color:#1c1c1c; }
      .settings-block-action { margin-left:auto; display:flex; align-items:center; gap:8px; }
      .settings-note { margin-top:4px; max-width:680px; font-size:12.5px; line-height:1.45; color:#6f6f6f; }
      .settings-used-by { margin-top:6px; font-size:12px; color:#6f6f6f; }
      .settings-empty { padding:16px 0; font-size:13px; color:#6f6f6f; }
      .settings-hint { display:block; margin:0; font-size:12px; line-height:1.45; color:#6f6f6f; }
      .settings-list { margin:0 0 8px; padding-left:18px; }
      .settings-list li { margin-bottom:4px; }
      .settings-form code {
        font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:12px; padding:1px 4px; background:#f5f5f5; border-radius:3px; color:#1c1c1c;
      }
      .settings-field { display:block; }
      .settings-field .settings-hint { margin-top:4px; max-width:640px; }
      .settings-label { display:block; font-size:13px; font-weight:600; color:#1c1c1c; margin-bottom:4px; }
      .settings-input {
        width:100%; max-width:520px; box-sizing:border-box; min-height:32px; padding:0 8px; font-size:13px;
        color:#1c1c1c; border:1px solid #a7a7a7; border-radius:4px; background:#fff; font-family:inherit;
      }
      select.settings-input { max-width:420px; }
      textarea.settings-input {
        max-width:none; padding:6px 8px; resize:vertical;
        font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:12.5px;
      }
      .settings-input[readonly], .settings-input:disabled { background:#f5f5f5; color:#6f6f6f; border-color:#e0e0e0; }
      .settings-input.invalid { border-color:#de350b; }
      .settings-input.settings-narrow { width:104px; }
      .settings-row { display:flex; flex-wrap:wrap; align-items:center; gap:8px; }
      .settings-row-text { font-size:13px; font-weight:600; color:#1c1c1c; }
      .settings-row .settings-time { width:140px; }
      .settings-row-hint { margin:4px 0 0; }
      .settings-indent { padding-left:48px; margin-top:-6px; }
      .settings-toggle { display:flex; align-items:flex-start; gap:12px; cursor:pointer; }
      .settings-toggle-text { display:flex; flex-direction:column; gap:2px; min-width:0; }
      .settings-toggle-text .settings-label { margin:0; }
      .settings-switch {
        appearance:none; -webkit-appearance:none; flex:none; margin:1px 0 0; width:36px; height:20px;
        border-radius:10px; background:#a7a7a7; position:relative; cursor:pointer; transition:background .12s;
      }
      .settings-switch::after {
        content:""; position:absolute; top:2px; left:2px; width:16px; height:16px; border-radius:50%;
        background:#fff; transition:transform .12s; box-shadow:0 1px 2px rgba(0,0,0,.3);
      }
      .settings-switch:checked { background:#0067c5; }
      .settings-switch:checked::after { transform:translateX(16px); }
      .settings-switch:focus-visible { outline:2px solid #0067c5; outline-offset:2px; }
      .settings-switch:disabled { opacity:.5; cursor:default; }
      .field-error { margin-top:4px; font-size:12px; line-height:1.4; color:#bf2600; white-space:pre-wrap; }
      .settings-link-button {
        border:none; background:none; padding:4px 8px; font:inherit; font-size:13px; font-weight:600;
        color:#0067c5; cursor:pointer; white-space:nowrap;
      }
      .settings-link-button:hover { background:#f4fbff; color:#1e4a93; }
      .settings-link { font-size:13px; font-weight:600; color:#0067c5; }
      .settings-btn {
        padding:4px 12px; font:inherit; font-size:13px; font-weight:600; color:#0067c5; cursor:pointer;
        background:#fff; border:1px solid #0067c5; border-radius:4px; min-height:28px;
      }
      .settings-btn:hover:not(:disabled) { background:#f4fbff; }
      .settings-btn:disabled { color:#a7a7a7; border-color:#e0e0e0; cursor:default; }
      .settings-btn.go { color:#fff; background:#00875A; border-color:#00875A; }
      .settings-btn.go:hover:not(:disabled) { background:#006644; }
      .settings-btn.go:disabled { color:#fff; opacity:.6; }
      .settings-ok { color:#00875A; font-weight:600; }
      .settings-advanced > summary { margin:0 0 8px; font-size:13px; font-weight:600; color:#0067c5; cursor:pointer; }
      .settings-features { border:1px solid #e0e0e0; background:#fff; }
      .settings-feature { border-bottom:1px solid #e0e0e0; }
      .settings-feature:last-child { border-bottom:none; }
      .settings-feature-head { display:flex; align-items:center; gap:12px; padding:12px 16px; }
      .settings-feature-label { flex:1; min-width:0; display:flex; align-items:flex-start; gap:16px; cursor:pointer; }
      .settings-feature-label .settings-switch { margin-top:2px; }
      .settings-feature-text { min-width:0; }
      .settings-feature-title { font-size:14px; font-weight:600; color:#1c1c1c; }
      .settings-feature-title .status-badge { margin-left:4px; vertical-align:1px; }
      .settings-feature-desc { margin-top:2px; font-size:12.5px; line-height:1.45; color:#6f6f6f; }
      .settings-feature-problem { margin-top:4px; font-size:12px; line-height:1.4; color:#974f0c; }
      .settings-feature-path { display:none; padding:8px 16px 0; font-size:12px; color:#6f6f6f; }
      .settings-feature-options { display:none; padding:12px 16px 20px 68px; background:#f5f5f5; flex-direction:column; gap:16px; }
      .settings-feature.open .settings-feature-options, .searching .settings-feature.force-open .settings-feature-options { display:flex; }
      .searching .settings-feature:not(.force-open) .settings-feature-options { display:none; }
      .searching .settings-feature.force-open .settings-feature-options { padding-left:16px; }
      .settings-repos.collapsed .settings-repo:nth-child(n+6) { display:none; }
      .settings-repo { margin:0; }
      .settings-repo + .settings-repo { margin-top:8px; }
      .settings-repo-line { display:flex; gap:8px; align-items:center; }
      .settings-repo-key { flex:0.6 1 0; min-width:0; }
      .settings-repo-path {
        flex:1 1 0; min-width:0; font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:12px;
      }
      .settings-repo-line .status-badge { flex:0 0 auto; }
      .settings-repos-more { align-self:flex-start; padding-left:0; }
      .settings-found > summary { margin:0 0 6px; font-size:13px; color:#0067c5; cursor:pointer; }
      .settings-found-item {
        display:flex; gap:10px; align-items:center; justify-content:space-between;
        padding:6px 0; border-bottom:1px solid #e0e0e0;
      }
      .settings-found-text { min-width:0; display:flex; flex-direction:column; }
      .settings-found-key { font-size:12.5px; font-weight:600; color:#1c1c1c; }
      .settings-found-path {
        font-size:11.5px; color:#6f6f6f; overflow:hidden; text-overflow:ellipsis; white-space:nowrap;
        font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
      }
      @container (max-width: 560px) {
        .settings-shell { flex-direction:column; }
        .settings-nav {
          width:auto; flex-direction:row; padding:8px 12px; overflow-x:auto; overflow-y:hidden;
          border-right:none; border-bottom:1px solid #e0e0e0;
        }
        .settings-nav-item { border-left:none; border-bottom:3px solid transparent; white-space:nowrap; }
        .settings-nav-item.on { border-bottom-color:#0067c5; }
        .settings-nav-sub { display:none; }
        .settings-main { padding:16px; }
        .settings-feature-options { padding-left:16px; }
        .settings-indent { padding-left:0; }
      }
      .settings-banner {
        position:sticky; top:0; z-index:3; flex:0 0 auto; margin:0 0 18px; padding:12px 14px; border-radius:6px;
        background:#fff0b3; border:1px solid #ff991f; border-left:6px solid #ff991f;
        box-shadow:0 2px 8px rgba(9,30,66,.25); font-size:13.5px; line-height:1.5; color:#172b4d;
      }
      .settings-banner-title { font-size:14px; font-weight:700; color:#974f0c; }
      .settings-banner p { margin:0 0 8px; }
      .settings-banner p:last-child { margin-bottom:0; }
      .settings-banner code, .settings-warning code {
        font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:12px; overflow-wrap:anywhere;
      }
      .settings-banner button {
        padding:6px 12px; border:none; border-radius:4px; background:#0067c5; color:#fff;
        font-size:12.5px; font-weight:600; cursor:pointer;
      }
      .settings-banner button:disabled { opacity:.6; cursor:default; }
      .settings-foot-row { display:flex; align-items:center; gap:12px; flex-wrap:wrap; }
      .settings-foot-row .actions { margin:0; }
      .settings-status { margin:0; font-size:13px; color:#6f6f6f; }
      .settings-status.ok { color:#00875A; font-weight:600; }
      .settings-status.warn {
        padding:8px 10px; border-radius:4px; background:#fff0b3; border-left:4px solid #ff991f;
        color:#974f0c; font-weight:700;
      }
      .settings-status:empty { display:none; }
      .settings-warning {
        margin-top:12px; padding:10px 12px; border-radius:4px; background:#fffae6;
        border:1px solid #ffe380; font-size:12.5px; line-height:1.45; color:#172b4d;
      }
      .settings-warning ul { margin:6px 0 10px; padding-left:18px; }
      .settings-warning li { margin-bottom:4px; }

      .panel-chrome {
        position:absolute; top:10px; right:12px; display:flex; gap:2px; z-index:3;
      }
      .panel-chrome button {
        cursor:pointer; border:none; background:none; font-size:18px; line-height:1;
        color:#6f6f6f; width:28px; height:28px; border-radius:4px; padding:0;
      }
      .panel-chrome button:hover { background:#f4fbff; color:#0067c5; }
      .panel.collapsed .panel-chrome { flex-direction:column; top:4px; right:2px; gap:0; }
      .panel.collapsed .panel-chrome button { width:24px; height:24px; font-size:15px; }
    `;
    shadow.appendChild(style);

    const panel = document.createElement("div");
    panel.className = "panel";

    // Jira (and Bitbucket) bind page-wide single-key shortcuts (e.g. "a" to
    // assign, "e" to edit) on `document`. A keystroke inside this shadow
    // root still bubbles out past the shadow boundary, but gets
    // *retargeted*: a page-level listener sees event.target as panelHost
    // (a plain <div>), not the actual <input> being typed into — so Jira's
    // "am I typing in a field?" check fails and it treats every keystroke
    // as a shortcut, preventing it from ever reaching the input. Stopping
    // propagation at the shadow boundary keeps every keystroke inside this
    // panel from ever being seen by the host page at all.
    for (const type of ["keydown", "keyup", "keypress"]) {
      panel.addEventListener(type, (e) => e.stopPropagation());
    }

    let expandedWidthPx = clampOverlayWidth((window.innerWidth * OVERLAY_DEFAULT_WIDTH_VW) / 100);
    let collapsed = false;

    function applyHostWidth() {
      if (!panelHost) return;
      panelHost.style.width = collapsed ? `${OVERLAY_COLLAPSED_WIDTH}px` : `${expandedWidthPx}px`;
      panelHost.style.bottom = "0";
    }

    function setCollapsed(next) {
      collapsed = next;
      panel.classList.toggle("collapsed", collapsed);
      // ▸ when open (collapse toward right edge); ◂ when rail (expand left).
      collapseBtn.textContent = collapsed ? "◂" : "▸";
      const label = collapsed ? "Expand panel" : "Collapse panel";
      collapseBtn.title = label;
      collapseBtn.setAttribute("aria-label", label);
      applyHostWidth();
      // Expanded → hide FAB; collapsed → dock FAB to the rail as one unit.
      setOverlayUiState(collapsed ? "collapsed" : "expanded");
      void setOverlayPref(OVERLAY_COLLAPSED_KEY, collapsed);
    }

    const chrome = document.createElement("div");
    chrome.className = "panel-chrome";

    const collapseBtn = document.createElement("button");
    collapseBtn.type = "button";
    collapseBtn.textContent = "▸";
    collapseBtn.title = "Collapse panel";
    collapseBtn.setAttribute("aria-label", "Collapse panel");
    collapseBtn.addEventListener("click", () => setCollapsed(!collapsed));

    const closeBtn = document.createElement("button");
    closeBtn.type = "button";
    closeBtn.textContent = "×";
    closeBtn.title = "Close";
    closeBtn.setAttribute("aria-label", "Close");
    closeBtn.addEventListener("click", closePanel);
    chrome.append(collapseBtn, closeBtn);
    panel.appendChild(chrome);

    const resizeHandle = document.createElement("div");
    resizeHandle.className = "resize-handle";
    resizeHandle.title = "Drag to resize";
    panel.appendChild(resizeHandle);

    let dragging = false;
    resizeHandle.addEventListener("mousedown", (e) => {
      if (collapsed) return;
      e.preventDefault();
      dragging = true;
      resizeHandle.classList.add("dragging");
      const onMove = (ev) => {
        if (!dragging) return;
        expandedWidthPx = clampOverlayWidth(window.innerWidth - ev.clientX);
        applyHostWidth();
      };
      const onUp = () => {
        if (!dragging) return;
        dragging = false;
        resizeHandle.classList.remove("dragging");
        document.removeEventListener("mousemove", onMove);
        document.removeEventListener("mouseup", onUp);
        void setOverlayPref(OVERLAY_WIDTH_KEY, expandedWidthPx);
      };
      document.addEventListener("mousemove", onMove);
      document.addEventListener("mouseup", onUp);
    });

    // Collapsed rail: click (or keyboard) expands again; title kept so the
    // user still knows which job/feature the strip belongs to.
    const rail = document.createElement("div");
    rail.className = "panel-rail";
    rail.title = "Expand panel";
    rail.setAttribute("role", "button");
    rail.tabIndex = 0;
    const railTitle = document.createElement("span");
    railTitle.className = "panel-rail-title";
    railTitle.textContent = title;
    rail.appendChild(railTitle);
    const expandRail = () => {
      if (collapsed) setCollapsed(false);
    };
    rail.addEventListener("click", expandRail);
    rail.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        expandRail();
      }
    });
    panel.appendChild(rail);

    // Everything scrollable (heading + caller-supplied content) lives in
    // here; the footer is a separate, non-scrolling strip below it (see the
    // .panel/.panel-body/.panel-footer CSS above) so action buttons are
    // always visible instead of scrolling out of view behind a long diff.
    const panelBody = document.createElement("div");
    panelBody.className = "panel-body";
    panel.appendChild(panelBody);

    const heading = document.createElement("h2");
    heading.textContent = title;
    panelBody.appendChild(heading);

    const body = document.createElement("div");
    body.className = "panel-body-content";
    panelBody.appendChild(body);

    // Empty and invisible until a caller appends something and shows it —
    // not every panel has footer actions (e.g. resolve-conflict's panel
    // once a job is merely "running", with nothing to approve/reject yet).
    const footer = document.createElement("div");
    footer.className = "panel-footer";
    footer.style.display = "none";
    panel.appendChild(footer);

    shadow.appendChild(panel);

    // Menu / start-job / auto-open always lands expanded. Restoring a prior
    // collapsed pref here made the panel snap shut on the async prefs paint
    // after the user had just chosen a feature. Persist expanded so a later
    // remount (or the prefs callback) cannot re-collapse until the user
    // hits the toggle. Width still comes from storage.
    void setOverlayPref(OVERLAY_COLLAPSED_KEY, false);
    void getOverlayPrefs().then((prefs) => {
      if (!panelHost) return;
      if (prefs.width != null) expandedWidthPx = clampOverlayWidth(prefs.width);
      applyHostWidth();
    });
    applyHostWidth();

    // Escape closes the expanded panel. Capture phase on document so it runs
    // before the panel's own keydown stopPropagation (and the host page's
    // shortcuts); skipped when collapsed or another handler already used it.
    if (removeEscapeListener) removeEscapeListener();
    const onEscape = (e) => {
      if (e.key !== "Escape" || e.defaultPrevented || collapsed) return;
      e.stopPropagation();
      closePanel();
    };
    document.addEventListener("keydown", onEscape, true);
    removeEscapeListener = () => document.removeEventListener("keydown", onEscape, true);

    return { body, footer, close: closePanel };
  }

  // ---- Review panel: an already-started job (resolve-conflict's diff, or
  // create-jira-subtasks' result after it's run) ----

  function showPanel(feature, job, jobKey) {
    const { body, footer } = openOverlayPanel(feature.menuLabel);

    // Draws the whole panel (body + footer) for `job`. Called once below,
    // and again by a footerAction's api.rerender(job) — e.g. resolve-
    // conflict's Refresh diff, whose job can come back awaiting-approval
    // *or* failed, which changes whether Approve shows — so both halves
    // are redrawn together rather than patched in place. `job` here
    // shadows showPanel's own parameter on purpose: every reference below
    // was already written against "the current job", which is exactly
    // what a rerender needs it to mean.
    function renderJob(job) {
      body.textContent = "";
      footer.textContent = "";
      footer.style.display = "none";

      // renderPanel returns either a bare Node, or { node, getApprovalPayload }
      // when Approve needs to send something other than an empty body (e.g.
      // a future feature's in-panel edits before committing).
      // The second argument lets a panel's own links run the feature's
      // actions (e.g. the ticket workspace's worktree rows); footerApi and
      // showActionError are defined below and only used once clicked.
      const rendered = feature.renderPanel(job, {
        action: (name, body) => footerApi.action(name, body),
        showError: (text) => showActionError(text),
      });
      const isWrapped = !(rendered instanceof Node);
      const bodyNode = isWrapped ? rendered.node : rendered;
      const getApprovalPayload = isWrapped ? rendered.getApprovalPayload : undefined;
      body.appendChild(bodyNode);

      // A failed Approve/Discard can carry a lot of text (e.g. a full lint
      // report from a pre-commit hook) — a native alert() truncates/wraps
      // that badly, so failures render here instead: scrollable, monospace,
      // and left on screen so the user can actually read it. Also used by
      // read-only "Add as comment" failures.
      const errorBox = document.createElement("pre");
      errorBox.className = "action-error";
      errorBox.style.display = "none";
      const okBox = document.createElement("div");
      okBox.className = "action-ok";
      okBox.style.display = "none";
      function showActionError(text) {
        okBox.style.display = "none";
        errorBox.textContent = text;
        errorBox.style.display = "block";
      }
      function showActionOk(text) {
        errorBox.style.display = "none";
        okBox.textContent = text;
        okBox.style.display = "block";
      }

      // footerActions (features.js's doc comment on the field has the
      // full contract): send() fills in featureId/jobId/pageUrl the same
      // way Approve/Discard already do above, so an action only has to
      // name the message type and any extra fields (e.g. Continue in
      // Claude Code's { body: { resume: true } }). rerender() redraws
      // this whole panel from a fresh job — needed by e.g. Refresh diff,
      // whose job can come back awaiting-approval *or* failed, which
      // changes whether Approve shows.
      const footerApi = {
        send(type, extra) {
          return sendToBackground({
            type,
            featureId: feature.id,
            jobId: job.id,
            pageUrl: location.href,
            ...extra,
          });
        },
        // A named action of this feature on this job (Feature.actions in
        // companion-service/core/jobs.ts). Resolves with the route's answer.
        action(name, body) {
          return sendToBackground({
            type: "feature-action",
            featureId: feature.id,
            jobId: job.id,
            pageUrl: location.href,
            action: name,
            body,
          });
        },
        // Starts ANOTHER feature's job from this panel (e.g. the analysis
        // report's Start fix starting ticket-to-pr) and hands the page over
        // to it: this panel's job stops being tracked, the new one is
        // stored under its own feature's key, and its panel opens when it
        // is ready — the same path Re-analyze takes.
        async startFeature(featureId, payload) {
          const target = FEATURES.find((f) => f.id === featureId);
          if (!target) throw new Error(`Unknown feature "${featureId}".`);
          const { urlMatches, match } = matchFeatureUrl(target);
          if (!urlMatches) throw new Error(`${target.menuLabel || featureId} isn't available on this page.`);
          const { job: startedJob } = await sendToBackground({
            type: "start",
            featureId,
            payload,
            pageUrl: location.href,
          });
          await clearStoredJob(jobKey);
          await setStoredJob(storageKey(target, buildPageCtx(), match), { jobId: startedJob.id, featureId });
          autoOpenPanelForJobId = startedJob.id;
          closePanel();
          setTimeout(scheduleRefresh, 300);
          return startedJob;
        },
        rerender: renderJob,
        close: closePanel,
        showError: showActionError,
      };
      // when(job, env): env.enabledFeatureIds is the service's enabled set
      // (a Set, or null before the first /features answer), so an action
      // that starts another feature can hide itself when that one is off.
      const footerEnv = { enabledFeatureIds: enabledFeatureIdsCache };
      const activeFooterActions = (feature.footerActions || []).filter((action) => action.when(job, footerEnv));
      function appendFooterActionButtons(container) {
        for (const action of activeFooterActions) {
          const btn = document.createElement("button");
          btn.className = "cancel";
          btn.textContent = action.label;
          if (action.hint) btn.title = action.hint;
          btn.addEventListener("click", async () => {
            btn.disabled = true;
            errorBox.style.display = "none";
            try {
              await action.run(job, footerApi);
            } catch (err) {
              // A well-behaved action already called api.showError itself
              // (see analyze-issue's Continue in Claude Code); this is the
              // fallback for one that just let the rejection propagate
              // (Refresh diff's api.send, if the request itself fails).
              showActionError(`${action.label} failed:\n\n${err.message}`);
            } finally {
              btn.disabled = false;
            }
          });
          container.appendChild(btn);
        }
      }

      // A read-only report (pre-deployment-stats) has nothing to apply and
      // nothing to clean up, so Approve/Discard would both be lies — it gets
      // a single Done button instead (plus optional Re-analyze when the
      // feature sets reanalyzeLabel). Done still has to clear the stored
      // job: without it the finished job keeps hijacking the FAB on this
      // page (see refreshAll's pass 1), which is the same trap oneShot
      // avoids by never storing one at all. Purely local — the server has no
      // state for this job beyond the in-memory report.
      if (feature.readOnly) {
        let reanalyzeBtn = null;
        let postCommentBtn = null;
        const doneBtn = document.createElement("button");
        doneBtn.className = "approve";
        doneBtn.textContent = "Done";
        doneBtn.addEventListener("click", async () => {
          doneBtn.disabled = true;
          if (reanalyzeBtn) reanalyzeBtn.disabled = true;
          if (postCommentBtn) postCommentBtn.disabled = true;
          await clearStoredJob(jobKey);
          closePanel();
          setTimeout(scheduleRefresh, 300);
        });
        const actions = document.createElement("div");
        actions.className = "actions";
        actions.appendChild(doneBtn);

        // Optional secondary restart: clear this job and POST /start again
        // with force: true so the companion skips its on-disk cache. Same
        // poll + auto-open path as a fresh menu click (selectOption).
        if (
          feature.reanalyzeLabel &&
          (job.status === "awaiting-approval" || job.status === "failed") &&
          typeof job.data?.issueKey === "string" &&
          job.data.issueKey
        ) {
          reanalyzeBtn = document.createElement("button");
          reanalyzeBtn.className = "cancel";
          reanalyzeBtn.textContent = feature.reanalyzeLabel;
          reanalyzeBtn.addEventListener("click", async () => {
            reanalyzeBtn.disabled = true;
            doneBtn.disabled = true;
            if (postCommentBtn) postCommentBtn.disabled = true;
            try {
              await clearStoredJob(jobKey);
              const { job: startedJob } = await sendToBackground({
                type: "start",
                featureId: feature.id,
                payload: { issueKey: job.data.issueKey, force: true },
                pageUrl: location.href,
              });
              await setStoredJob(jobKey, { jobId: startedJob.id, featureId: feature.id });
              autoOpenPanelForJobId = startedJob.id;
              closePanel();
              setTimeout(scheduleRefresh, 300);
            } catch (err) {
              reanalyzeBtn.disabled = false;
              doneBtn.disabled = false;
              if (postCommentBtn) postCommentBtn.disabled = false;
              alert(`${feature.reanalyzeLabel} failed: ${err.message}`);
            }
          });
          actions.appendChild(reanalyzeBtn);
        }

        // Edit-before-post: "Add as comment" opens a Markdown compose view
        // (draft from job.data.commentDraftMd). Post comment sends { body }
        // to the companion; Back returns to the report without posting.
        const analysis = job.data?.analysis;
        const hasAnalysisContent =
          analysis &&
          typeof analysis === "object" &&
          (analysis.tldr ||
            analysis.raw ||
            (Array.isArray(analysis.nextSteps) && analysis.nextSteps.length > 0) ||
            (Array.isArray(analysis.hypotheses) && analysis.hypotheses.length > 0));
        let commentPosted = false;
        if (
          feature.postCommentLabel &&
          job.status === "awaiting-approval" &&
          hasAnalysisContent
        ) {
          postCommentBtn = document.createElement("button");
          postCommentBtn.className = "post-comment";
          postCommentBtn.textContent = feature.postCommentLabel;

          const reportNode = bodyNode;
          let composeRoot = null;
          let composeTextarea = null;
          // The Haiku-summarized draft, fetched once per panel on the first
          // click: Back and re-opening reuse it (edits are not kept).
          let summaryDraft = null;
          let summaryFailed = "";
          let summarizing = false;

          const panelBodyEl = body.parentElement;

          function showReportFooter() {
            actions.style.display = "flex";
            if (panelBodyEl) panelBodyEl.classList.remove("compose-mode");
            if (composeRoot && composeRoot.parentNode) composeRoot.remove();
            composeRoot = null;
            composeTextarea = null;
            if (reportNode.style.display === "none") reportNode.style.display = "";
            if (!reportNode.parentNode) body.appendChild(reportNode);
          }

          // Flex-grow fills the panel, but then the browser's resize handle
          // cannot change height (layout snaps back). Lock the filled size
          // as an explicit height so resize:vertical works.
          function lockComposeTextareaHeight() {
            if (!composeTextarea || !composeRoot) return;
            const filled = composeTextarea.offsetHeight;
            if (filled < 1) return;
            composeTextarea.style.height = `${filled}px`;
            composeTextarea.classList.add("compose-sized");
          }

          function enterComposeMode() {
            errorBox.style.display = "none";
            okBox.style.display = "none";
            const draft =
              typeof job.data?.commentDraftMd === "string" && job.data.commentDraftMd
                ? job.data.commentDraftMd
                : "";

            reportNode.style.display = "none";
            actions.style.display = "none";
            if (panelBodyEl) panelBodyEl.classList.add("compose-mode");

            composeRoot = document.createElement("div");
            composeRoot.className = "comment-compose";
            const hint = document.createElement("p");
            hint.className = "comment-compose-hint";
            const hintDone =
              "Edit the Markdown comment, then post it to the ticket. Back returns to the report without posting.";
            hint.textContent = summaryFailed
              ? `Could not summarize (${summaryFailed}); showing the full analysis. ${hintDone}`
              : hintDone;
            composeTextarea = document.createElement("textarea");
            composeTextarea.setAttribute("aria-label", "Comment draft");
            composeTextarea.value = summaryDraft !== null ? summaryDraft : draft;
            composeRoot.appendChild(hint);
            composeRoot.appendChild(composeTextarea);
            body.appendChild(composeRoot);

            const composeActions = document.createElement("div");
            composeActions.className = "actions";
            const postBtn = document.createElement("button");
            postBtn.className = "approve";
            postBtn.textContent = "Post comment";
            const backBtn = document.createElement("button");
            backBtn.className = "cancel";
            backBtn.textContent = "Back";

            backBtn.addEventListener("click", () => {
              composeActions.remove();
              showReportFooter();
              footer.insertBefore(actions, okBox);
            });

            postBtn.addEventListener("click", async () => {
              const markdown = composeTextarea.value;
              if (!markdown.trim()) {
                showActionError("Comment body is empty.");
                return;
              }
              postBtn.disabled = true;
              backBtn.disabled = true;
              doneBtn.disabled = true;
              if (reanalyzeBtn) reanalyzeBtn.disabled = true;
              errorBox.style.display = "none";
              okBox.style.display = "none";
              postBtn.textContent = "Posting…";
              try {
                await sendToBackground({
                  type: "post-comment",
                  featureId: feature.id,
                  jobId: job.id,
                  body: markdown,
                  pageUrl: location.href,
                });
                commentPosted = true;
                composeActions.remove();
                showReportFooter();
                footer.insertBefore(actions, okBox);
                showActionOk("Comment added to the ticket.");
                postCommentBtn.disabled = true;
                postCommentBtn.textContent = feature.postCommentLabel;
                doneBtn.disabled = false;
                if (reanalyzeBtn) reanalyzeBtn.disabled = false;
              } catch (err) {
                showActionError(`Post comment failed:\n\n${err.message}`);
                postBtn.disabled = false;
                backBtn.disabled = false;
                doneBtn.disabled = false;
                if (reanalyzeBtn) reanalyzeBtn.disabled = false;
                postBtn.textContent = "Post comment";
              }
            });

            composeActions.appendChild(postBtn);
            composeActions.appendChild(backBtn);
            footer.insertBefore(composeActions, okBox);

            // Two RAFs: first applies flex fill, second reads the filled
            // height and locks it so the native resize grip can take over.
            requestAnimationFrame(() => {
              requestAnimationFrame(() => {
                lockComposeTextareaHeight();
                composeTextarea.focus();
              });
            });
          }

          // The panel stays on the report while Haiku writes the short
          // draft (a visible notice in the footer); the compose view only
          // opens once it is ready. If summarizing fails, compose opens
          // with the full analysis instead.
          postCommentBtn.addEventListener("click", async () => {
            if (commentPosted || summarizing) return;
            if (summaryDraft !== null || summaryFailed) {
              enterComposeMode();
              return;
            }
            summarizing = true;
            postCommentBtn.disabled = true;
            doneBtn.disabled = true;
            if (reanalyzeBtn) reanalyzeBtn.disabled = true;
            errorBox.style.display = "none";
            showActionOk("Summarizing the analysis for the comment…");
            try {
              const res = await sendToBackground({
                type: "feature-action",
                featureId: feature.id,
                jobId: job.id,
                action: "summarize-comment",
                pageUrl: location.href,
              });
              if (!res || typeof res.draft !== "string" || !res.draft.trim()) {
                throw new Error("empty summary");
              }
              summaryDraft = res.draft;
            } catch (err) {
              summaryFailed = err.message || "unknown error";
            }
            summarizing = false;
            postCommentBtn.disabled = false;
            doneBtn.disabled = false;
            if (reanalyzeBtn) reanalyzeBtn.disabled = false;
            okBox.style.display = "none";
            enterComposeMode();
          });
          actions.appendChild(postCommentBtn);
        }

        appendFooterActionButtons(actions);
        footer.appendChild(actions);
        footer.appendChild(okBox);
        footer.appendChild(errorBox);
        footer.style.display = "block";
        return;
      }

      const showApprove = job.status === "awaiting-approval" && !feature.hideApprove;
      const showReject = job.status === "awaiting-approval" || job.status === "failed";
      if (showApprove || showReject || activeFooterActions.length > 0) {
        const actions = document.createElement("div");
        actions.className = "actions";

        if (showApprove) {
          const approveLabel = feature.approveLabel || "Approve & Push";
          const approveBtn = document.createElement("button");
          approveBtn.className = "approve";
          approveBtn.textContent = approveLabel;
          approveBtn.addEventListener("click", async () => {
            approveBtn.disabled = true;
            const rejectBtn = actions.querySelector(".reject");
            if (rejectBtn) rejectBtn.disabled = true;
            approveBtn.textContent = "Working…";
            errorBox.style.display = "none";
            try {
              const approvalBody = getApprovalPayload ? getApprovalPayload() : undefined;
              const { job: updatedJob } = await sendToBackground({
                type: "approve",
                featureId: feature.id,
                jobId: job.id,
                payload: approvalBody,
                pageUrl: location.href,
              });
              await clearStoredJob(jobKey);
              // Stay open and re-render from the finished job in place of the
              // action buttons, rather than closing immediately and losing
              // that information.
              actions.remove();
              const doneRendered = feature.renderPanel(updatedJob);
              const doneNode = doneRendered instanceof Node ? doneRendered : doneRendered.node;
              bodyNode.replaceWith(doneNode);
              setTimeout(scheduleRefresh, 1000);
            } catch (err) {
              // The server may have already moved this job to a new
              // status before the error reached us (e.g. resolve-
              // conflict's "the worktree changed since you reviewed it"
              // fingerprint mismatch lands the job on "failed"). Re-fetch
              // and re-render from that job rather than just re-enabling
              // these buttons — otherwise Approve stays clickable and just
              // fails the same way again, instead of the panel showing
              // the failed state (Approve hidden, Refresh diff/Discard
              // available).
              try {
                const { job: freshJob } = await sendToBackground({
                  type: "status",
                  featureId: feature.id,
                  jobId: job.id,
                  pageUrl: location.href,
                });
                if (freshJob.status !== job.status) {
                  renderJob(freshJob);
                  return;
                }
              } catch {
                // The refetch itself failed — nothing fresher to render;
                // fall through to the generic re-enable below.
              }
              showActionError(`${approveLabel} failed:\n\n${err.message}`);
              approveBtn.disabled = false;
              approveBtn.textContent = approveLabel;
              if (rejectBtn) rejectBtn.disabled = false;
            }
          });
          actions.appendChild(approveBtn);
        }

        if (showReject) {
          const rejectBtn = document.createElement("button");
          rejectBtn.className = "reject";
          rejectBtn.textContent = feature.rejectLabel || "Discard";
          rejectBtn.addEventListener("click", async () => {
            rejectBtn.disabled = true;
            errorBox.style.display = "none";
            try {
              await sendToBackground({
                type: "reject",
                featureId: feature.id,
                jobId: job.id,
                pageUrl: location.href,
              });
              await clearStoredJob(jobKey);
              closePanel();
              setTimeout(scheduleRefresh, 300);
            } catch (err) {
              showActionError(`${feature.rejectLabel || "Discard"} failed:\n\n${err.message}`);
              rejectBtn.disabled = false;
            }
          });
          actions.appendChild(rejectBtn);
        }

        appendFooterActionButtons(actions);
        footer.appendChild(actions);
        footer.appendChild(errorBox);
        footer.style.display = "block";
      }
    }

    renderJob(job);
  }

  // ---- Compose panel: collect input *before* a job exists (e.g.
  // create-jira-subtasks' subtask rows), then start it directly ----

  function openComposePanel(feature, payload, ctx) {
    const { body, footer, close } = openOverlayPanel(feature.menuLabel);

    const { node, getPayload } = feature.renderStartForm(payload);
    body.appendChild(node);

    const errorBox = document.createElement("pre");
    errorBox.className = "action-error";
    errorBox.style.display = "none";
    function showActionError(text) {
      errorBox.textContent = text;
      errorBox.style.display = "block";
    }

    const submitLabel = feature.startLabel || "Submit";
    const submitBtn = document.createElement("button");
    submitBtn.className = "approve";
    submitBtn.textContent = submitLabel;

    const cancelBtn = document.createElement("button");
    cancelBtn.className = "cancel";
    cancelBtn.textContent = "Close";
    cancelBtn.addEventListener("click", close);

    submitBtn.addEventListener("click", async () => {
      let finalPayload;
      try {
        finalPayload = { ...payload, ...getPayload() };
      } catch (err) {
        // A validation error from getPayload() (e.g. a blank field) — same
        // display path as a failed request below, just no request made.
        showActionError(err.message);
        return;
      }
      submitBtn.disabled = true;
      cancelBtn.disabled = true;
      submitBtn.textContent = "Working…";
      errorBox.style.display = "none";
      try {
        const { job } = await sendToBackground({
          type: "start",
          featureId: feature.id,
          payload: finalPayload,
          pageUrl: ctx.url,
        });
        // This kind of feature runs to completion inside /start (no
        // separate approve step, no polling — see create-jira-subtasks), so
        // the response here already carries the finished job — but /start
        // always resolves the request itself (HTTP 200) even when the
        // *job* failed (e.g. an invalid assignee), so that has to be
        // checked explicitly rather than assumed from "the call didn't
        // throw." Anything short of "approved" is treated as a failure:
        // stay open, show the message, let the user fix and retry.
        if (job.status !== "approved") {
          throw new Error(job.error || `Unexpected job status "${job.status}".`);
        }
        // Success: close immediately and reload the page so the host
        // page's own UI (e.g. Jira's subtask list) reflects what was just
        // created, rather than leaving the user looking at a stale view.
        close();
        location.reload();
      } catch (err) {
        showActionError(`${submitLabel} failed:\n\n${err.message}`);
        submitBtn.disabled = false;
        cancelBtn.disabled = false;
        submitBtn.textContent = submitLabel;
      }
    });

    const actions = document.createElement("div");
    actions.className = "actions";
    actions.append(submitBtn, cancelBtn);
    footer.appendChild(actions);
    footer.appendChild(errorBox);
    footer.style.display = "block";
  }

  // ---- Detection / polling loop ----

  async function pollStoredJob(feature, jobKey, stored) {
    let job;
    try {
      const res = await sendToBackground({ type: "status", jobId: stored.jobId });
      job = res.job;
    } catch (err) {
      if (isContextInvalidated(err)) throw err;
      if (isJobGone(err)) {
        // The service answered and doesn't know this job (discarded
        // elsewhere, pruned, or a feature that keeps jobs in memory only)
        // — stop tracking it rather than watching a dead job forever.
        await clearStoredJob(jobKey);
        return false;
      }
      // Couldn't reach the service — most likely it's restarting (an
      // install or update), and since 0.8.1 it brings review jobs back.
      // Keep tracking; the next poll tries again.
      return true;
    }
    if (job.status === "rejected") {
      // Cancelled or rejected (possibly from another tab) — nothing left to show.
      await clearStoredJob(jobKey);
      return false;
    }

    if (
      job.id === autoOpenPanelForJobId &&
      (job.status === "awaiting-approval" || job.status === "failed")
    ) {
      autoOpenPanelForJobId = null;
      setPopoverOpen(false);
      showPanel(feature, job, jobKey);
    } else {
      renderPill(feature, job, jobKey);
    }
    return true;
  }

  // Reloading the extension at chrome://extensions doesn't touch an
  // already-open tab's already-injected content script: its chrome.*
  // references just go dead. Any chrome.storage/chrome.runtime call then
  // throws this exact message — reliably, on every single poll tick,
  // forever, until the tab itself is reloaded. Nothing about that is this
  // page's fault or worth surfacing every 800ms-2s, so it's the one error
  // shutdownAndStop() treats as "stop polling silently" rather than letting
  // it become an infinite stream of uncaught console errors.
  function isContextInvalidated(err) {
    return !!err && typeof err.message === "string" && err.message.includes("Extension context invalidated");
  }

  // True only when the service itself answered that it has no such job
  // (GET /status's 404). An unreachable service (no HTTP status at all —
  // see background.js's callService) is not "gone": a restart is expected
  // to bring the job back.
  function isJobGone(err) {
    return err?.details?.status === 404;
  }

  let stopped = false;
  let pollIntervalId = null;
  let domObserver = null;

  function shutdownAndStop() {
    stopped = true;
    setFabVisible(false);
    closePanel();
    if (domObserver) domObserver.disconnect();
    if (pollIntervalId) clearInterval(pollIntervalId);
  }

  // ---- Updating the extension and the companion service together ----
  //
  // The service knows the latest version at the install source (see
  // companion-service/core/updater.js). An update offer appears at the top
  // of the menu, and the ✨ button shows even on pages with no feature, when
  // a newer version exists or when this extension and the service differ
  // (one was updated without the other).

  function compareVersions(a, b) {
    const parse = (v) =>
      String(v || "")
        .split(/[-+]/)[0]
        .split(".")
        .map((n) => parseInt(n, 10) || 0);
    const pa = parse(a);
    const pb = parse(b);
    for (let i = 0; i < Math.max(pa.length, pb.length); i++) {
      const d = (pa[i] || 0) - (pb[i] || 0);
      if (d !== 0) return d > 0 ? 1 : -1;
    }
    return 0;
  }

  function computeUpdateOffer(info, extensionVersion) {
    if (!info || !info.current) return null;
    const base = { info, extensionVersion };
    const run = info.update;
    if (run && run.state === "running") {
      return { ...base, kind: "running", label: `Updating to v${run.to || info.latest || "…"}…` };
    }
    if (run && run.state === "done" && run.needsRestart && compareVersions(info.current, run.to) < 0) {
      return { ...base, kind: "restart", label: `Restart the companion service to finish v${run.to}` };
    }
    if (info.available) return { ...base, kind: "update", label: `Update to v${info.latest}` };
    const cmp = compareVersions(info.current, extensionVersion);
    if (cmp > 0) return { ...base, kind: "reload", label: `Reload extension to v${info.current}` };
    if (cmp < 0) return { ...base, kind: "update", label: `Update companion service to v${extensionVersion}` };
    return null;
  }

  async function refreshUpdateOffer(force = false) {
    if (!force && Date.now() - updateCheckedAt < UPDATE_CHECK_INTERVAL_MS) return;
    updateCheckedAt = Date.now();
    try {
      const { info, extensionVersion } = await sendToBackground({ type: "update-info" });
      updateOffer = computeUpdateOffer(info, extensionVersion);
      // A service that has just started hasn't checked the install source
      // yet (its first check runs ~15s in), so "no update" means "don't know
      // yet": ask again in a few seconds rather than a full interval.
      if (info && !info.checkedAt) updateCheckedAt = Date.now() - UPDATE_CHECK_INTERVAL_MS + 5000;
    } catch (err) {
      if (isContextInvalidated(err)) throw err;
      // A service too old to have /update, or not running: nothing to offer.
      updateOffer = null;
    }
  }

  // The new release's CHANGELOG.md entries newer than this install, as the
  // service reports them (GET /update's whatsNew). Null when there are none,
  // including from a service too old to send them.
  function renderWhatsNew(entries) {
    const valid = (Array.isArray(entries) ? entries : []).filter(
      (e) => e && typeof e.version === "string" && Array.isArray(e.notes) && e.notes.length > 0,
    );
    if (valid.length === 0) return null;
    const section = document.createElement("div");
    section.className = "digest-section";
    const heading = document.createElement("h4");
    heading.className = "report-headline";
    heading.textContent = "What's new";
    section.appendChild(heading);
    for (const entry of valid) {
      const row = document.createElement("div");
      row.className = "digest-row ok";
      const version = document.createElement("div");
      version.className = "update-version";
      version.textContent = `v${entry.version}`;
      const list = document.createElement("ul");
      list.className = "update-notes";
      for (const note of entry.notes) {
        const item = document.createElement("li");
        item.textContent = String(note);
        list.appendChild(item);
      }
      row.append(version, list);
      section.appendChild(row);
    }
    return section;
  }

  function openUpdatePanel(offer) {
    const { info, extensionVersion } = offer;
    const { body, footer, close } = openOverlayPanel(`Update ${chrome.runtime.getManifest().name}`);
    const thisPanel = panelHost;

    const wrap = document.createElement("div");
    wrap.className = "repo-setup";
    const target = info.latest || extensionVersion;
    const headline = document.createElement("div");
    headline.className = "digest-headline";
    // What to do next, in the card under the headline.
    let nextTitle = "What happens";
    let nextText;
    if (offer.kind === "reload") {
      headline.textContent = `Reload the extension to finish updating to v${info.current}`;
      nextTitle = "Next step";
      nextText =
        'Click "Reload extension and page". ' +
        "If it doesn't reload, open chrome://extensions and click the reload (↻) icon on " + chrome.runtime.getManifest().name + ", then refresh this page.";
    } else if (offer.kind === "restart") {
      headline.textContent = `v${info.update.to} is installed — restart the companion service`;
      nextTitle = "Next step";
      nextText =
        "The service isn't running in the background, so restart it yourself " +
        "(cd ~/ai-dev-companion/companion-service && npm start), then click \"Reload extension and page\". " +
        "If it doesn't reload, open chrome://extensions and click the reload (↻) icon on " + chrome.runtime.getManifest().name + ", then refresh this page.";
    } else {
      headline.textContent = `Version ${target} is available`;
      nextText =
        "Updating installs the new version in ~/ai-dev-companion, keeps all your settings, restarts " +
        "the companion service and reloads the extension and this page automatically — about a minute. " +
        "Refresh any other tabs this extension runs on afterwards, since Chrome only reloads this one.";
    }

    const card = (title, rows) => {
      const section = document.createElement("div");
      section.className = "digest-section";
      const head = document.createElement("h4");
      head.className = "report-headline";
      head.textContent = title;
      section.appendChild(head);
      for (const { text, tone, badge } of rows) {
        const row = document.createElement("div");
        row.className = `digest-row${tone ? ` ${tone}` : ""}`;
        if (badge) {
          const b = document.createElement("span");
          b.className = `status-badge ${tone || "neutral"}`;
          b.textContent = badge;
          row.appendChild(b);
        }
        row.appendChild(document.createTextNode(text));
        section.appendChild(row);
      }
      return section;
    };

    const versionRows = [
      { text: `Extension v${extensionVersion}` },
      { text: `Companion service v${info.current}` },
    ];
    if (offer.kind === "update" || offer.kind === "running") {
      versionRows.push({ text: `Latest v${target}`, tone: "ok", badge: "New" });
    }
    const status = document.createElement("p");
    status.className = "status update-status";
    wrap.append(headline, card("Installed versions", versionRows));
    if (offer.kind === "update" || offer.kind === "running") {
      const whatsNew = renderWhatsNew(info.whatsNew);
      if (whatsNew) wrap.appendChild(whatsNew);
    }
    wrap.append(card(nextTitle, [{ text: nextText, tone: "busy" }]), status);
    body.appendChild(wrap);

    const errorBox = document.createElement("pre");
    errorBox.className = "action-error";
    errorBox.style.display = "none";

    const updateBtn = document.createElement("button");
    updateBtn.className = "approve";
    updateBtn.textContent = "Update now";
    const reloadBtn = document.createElement("button");
    reloadBtn.className = "approve";
    reloadBtn.textContent = "Reload extension and page";
    const cancelBtn = document.createElement("button");
    cancelBtn.className = "cancel";
    cancelBtn.textContent = "Not now";
    cancelBtn.addEventListener("click", close);

    function showError(message) {
      updateBtn.disabled = false;
      status.textContent = "";
      errorBox.textContent = message;
      errorBox.style.display = "block";
    }

    function reloadEverything() {
      status.textContent = "Reloading the extension…";
      updateBtn.disabled = reloadBtn.disabled = true;
      sendToBackground({ type: "reload-extension" }).catch(() => {});
      // This page's content script dies with the old extension; reloading
      // the page loads the new one.
      setTimeout(() => location.reload(), 1500);
    }

    async function watchUpdate() {
      updateBtn.disabled = true;
      errorBox.style.display = "none";
      const deadline = Date.now() + 10 * 60 * 1000;
      while (Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, 2000));
        if (panelHost !== thisPanel) return; // closed — the update carries on regardless
        let res;
        try {
          res = await sendToBackground({ type: "update-info" });
        } catch (err) {
          if (isContextInvalidated(err)) return;
          status.textContent = "Restarting the companion service…";
          continue;
        }
        const run = res.info.update;
        if (!run || run.state === "running") {
          status.textContent = (run && run.label) || "Starting the update…";
        } else if (run.state === "failed") {
          showError(run.error || "The update failed.");
          return;
        } else if (run.needsRestart) {
          status.textContent =
            `${run.label} It isn't running in the background, so restart it yourself ` +
            "(cd ~/ai-dev-companion/companion-service && npm start), then click \"Reload extension and page\". " +
        "If it doesn't reload, open chrome://extensions and click the reload (↻) icon on " + chrome.runtime.getManifest().name + ", then refresh this page.";
          reloadBtn.style.display = "";
          return;
        } else if (compareVersions(res.info.current, run.to) >= 0) {
          reloadEverything();
          return;
        } else {
          status.textContent = "Restarting the companion service…";
        }
      }
      showError(
        "The update is taking longer than expected. Its log is in ~/.ai-dev-companion/update.log. " +
          `To update by hand, run:\n${info.manualCommand}`,
      );
    }

    updateBtn.addEventListener("click", async () => {
      updateBtn.disabled = true;
      status.textContent = "Starting the update…";
      try {
        await sendToBackground({ type: "update-apply" });
        watchUpdate();
      } catch (err) {
        showError(`${err.message}\n\nTo update by hand, run:\n${info.manualCommand}`);
      }
    });
    reloadBtn.addEventListener("click", reloadEverything);

    const actions = document.createElement("div");
    actions.className = "actions";
    if (offer.kind === "reload" || offer.kind === "restart") {
      actions.append(reloadBtn, cancelBtn);
    } else {
      reloadBtn.style.display = "none";
      actions.append(updateBtn, reloadBtn, cancelBtn);
      if (!info.canApply) {
        showError(`${info.cannotApplyReason} Update by hand instead:\n${info.manualCommand}`);
        updateBtn.disabled = true;
      }
    }
    footer.append(actions, errorBox);
    footer.style.display = "block";
    if (offer.kind === "running") watchUpdate();
  }

  // ---- Settings (⚙ in the popover header) ----
  //
  // Reviewing and changing the companion service's settings without
  // re-running its terminal setup — settings-panel.js builds the form;
  // companion-service/core/settings.js decides what may change. Everything
  // but the enabled feature set applies as soon as it's saved; that one
  // needs the service restarted, which it can do itself under launchd.

  function openSettingsPanel() {
    const { body, footer, close } = openOverlayPanel("Settings");
    const thisPanel = panelHost;
    // Two scrolling panes (page list, page) fill the panel instead of one scrolling column.
    body.parentElement.classList.add("settings-mode");
    let view = null;
    let form = null;

    const loading = document.createElement("p");
    loading.className = "settings-loading";
    loading.textContent = "Loading your settings…";
    body.appendChild(loading);

    const status = document.createElement("p");
    status.className = "settings-status";
    const errorBox = document.createElement("pre");
    errorBox.className = "action-error";
    errorBox.style.display = "none";
    const warningBox = document.createElement("div");
    warningBox.className = "settings-warning";
    warningBox.style.display = "none";

    const saveBtn = document.createElement("button");
    saveBtn.className = "approve";
    saveBtn.textContent = "Save changes";
    saveBtn.disabled = true;
    const discardBtn = document.createElement("button");
    discardBtn.className = "cancel";
    discardBtn.textContent = "Discard";
    discardBtn.disabled = true;
    discardBtn.addEventListener("click", () => {
      clearMessages();
      render(view, form ? form.currentPage() : undefined);
    });
    const actions = document.createElement("div");
    actions.className = "actions";
    actions.append(saveBtn, discardBtn);
    const footRow = document.createElement("div");
    footRow.className = "settings-foot-row";
    footRow.append(actions, status);
    footer.append(footRow, warningBox, errorBox);
    footer.style.display = "block";
    // Until the form reports a change there is nothing to save or discard.
    let dirtyCount = 0;
    function onDirty(count) {
      dirtyCount = count;
      saveBtn.disabled = count === 0 || saveBtn.textContent !== "Save changes";
      discardBtn.disabled = count === 0;
      if (count > 0) setStatus(`${count} unsaved ${count === 1 ? "change" : "changes"}`);
      else if (status.textContent.includes("unsaved")) setStatus("");
    }

    function setStatus(text, ok = false, warn = false) {
      status.textContent = text;
      status.classList.toggle("ok", ok);
      status.classList.toggle("warn", warn);
    }
    function showError(text) {
      setStatus("");
      errorBox.textContent = text;
      errorBox.style.display = "block";
    }
    function clearMessages() {
      setStatus("");
      errorBox.style.display = "none";
      warningBox.style.display = "none";
    }
    function featureLabels(ids) {
      return ids.map((id) => (view.features.find((f) => f.id === id) || { label: id }).label).join(", ");
    }

    function restartBanner(restart) {
      if (!restart.required) return null;
      const banner = document.createElement("div");
      banner.className = "settings-banner";
      const changes = [];
      if (restart.added.length > 0) changes.push(`turn on ${featureLabels(restart.added)}`);
      if (restart.removed.length > 0) changes.push(`turn off ${featureLabels(restart.removed)}`);
      const title = document.createElement("p");
      title.className = "settings-banner-title";
      title.textContent = "⚠ Restart needed — your change isn't active yet";
      banner.appendChild(title);
      const what = document.createElement("p");
      what.textContent = `Restart the companion service to ${changes.join(" and ")}. Everything else is already live.`;
      banner.appendChild(what);
      if (restart.canRestart) {
        const btn = document.createElement("button");
        btn.type = "button";
        btn.textContent = "Restart now";
        btn.addEventListener("click", () => restartService(btn));
        const p = document.createElement("p");
        p.appendChild(btn);
        banner.appendChild(p);
      } else {
        const how = document.createElement("p");
        how.append("It isn't running in the background, so restart it yourself: stop it, then run ");
        const code = document.createElement("code");
        code.textContent = restart.manualCommand;
        how.appendChild(code);
        banner.appendChild(how);
      }
      return banner;
    }

    function render(nextView, page) {
      view = nextView;
      body.textContent = "";
      const banner = restartBanner(view.restart);
      if (banner) body.appendChild(banner);
      form = window.renderSettingsForm(view, { page });
      form.onDirtyChange = onDirty;
      body.appendChild(form.node);
      onDirty(0);
    }

    function showWarnings(warnings) {
      warningBox.textContent = "";
      const intro = document.createElement("div");
      intro.textContent = "Some features you turned on can't run on this machine yet:";
      const list = document.createElement("ul");
      for (const w of warnings) {
        const item = document.createElement("li");
        item.textContent = `${w.features.join(", ")}: ${w.message}`;
        list.appendChild(item);
      }
      const hint = document.createElement("div");
      hint.textContent = "Fix that and save again, or save anyway — those features will fail with this message until it's fixed.";
      const anyway = document.createElement("button");
      anyway.type = "button";
      anyway.className = "cancel";
      anyway.textContent = "Save anyway";
      anyway.addEventListener("click", () => save(true));
      const row = document.createElement("div");
      row.className = "actions";
      row.style.marginTop = "8px";
      row.appendChild(anyway);
      warningBox.append(intro, list, hint, row);
      warningBox.style.display = "block";
    }

    async function save(acknowledgeWarnings = false) {
      clearMessages();
      let update;
      try {
        update = form.getUpdate();
      } catch (err) {
        showError(err.message);
        return;
      }
      saveBtn.disabled = true;
      saveBtn.textContent = "Saving…";
      discardBtn.disabled = true;
      try {
        const { view: saved } = await sendToBackground({
          type: "settings-save",
          settings: { ...update, acknowledgeWarnings },
        });
        if (panelHost !== thisPanel) return;
        render(saved, form.currentPage());
        // A changed base URL means the manifest's host list changed (the service rewrote it); Chrome
        // only reads it when the extension reloads.
        const reload = saved.extension && saved.extension.reloadNeeded;
        const reloadNote = " Also reload the extension at chrome://extensions so it covers the new server address.";
        if (saved.restart.required) {
          setStatus(
            `Saved — click “Restart now” in the notice at the top to activate the new feature settings.${reload ? reloadNote : ""}`,
            false,
            true,
          );
          body.scrollTop = 0;
        } else if (reload) {
          setStatus(`Saved —${reloadNote.trimEnd()}`, false, true);
        } else if (saved.extension && saved.extension.error) {
          setStatus(
            `Saved, but the extension's host list couldn't be updated (${saved.extension.error}). Run \`npm run setup\` in the companion folder, then reload the extension.`,
            false,
            true,
          );
        } else {
          setStatus("Saved — your changes are live.", true);
        }
      } catch (err) {
        const details = err.details || {};
        if (details.code === "invalid-settings") {
          const unmatched = form.showErrors(details.errors || []);
          showError(unmatched.length > 0 ? unmatched.join("\n") : "Some settings need fixing — see the highlighted fields.");
        } else if (details.code === "prerequisites-failing") {
          showWarnings(details.warnings || []);
        } else {
          showError(err.message);
        }
      } finally {
        saveBtn.textContent = "Save changes";
        saveBtn.disabled = !form || dirtyCount === 0;
        discardBtn.disabled = !form || dirtyCount === 0;
      }
    }
    saveBtn.addEventListener("click", () => save(false));

    async function restartService(btn, force = false) {
      clearMessages();
      btn.disabled = saveBtn.disabled = true;
      setStatus("Restarting the companion service…");
      const startedBefore = view.service.startedAt;
      try {
        await sendToBackground({ type: "settings-restart", force });
      } catch (err) {
        btn.disabled = saveBtn.disabled = false;
        if (err.details && err.details.code === "jobs-running") {
          setStatus("");
          if (confirm(`${err.message} Restart anyway?`)) restartService(btn, true);
          return;
        }
        showError(err.message);
        return;
      }
      const deadline = Date.now() + 60 * 1000;
      while (Date.now() < deadline) {
        await new Promise((resolve) => setTimeout(resolve, 1500));
        if (panelHost !== thisPanel) return;
        try {
          const { view: next } = await sendToBackground({ type: "settings-get" });
          if (next.service.startedAt === startedBefore) continue;
          enabledFeatureIdsCache = null;
          render(next);
          setStatus("Restarted — your changes are live.", true);
          scheduleRefresh();
          return;
        } catch (err) {
          if (isContextInvalidated(err)) return;
        }
      }
      btn.disabled = saveBtn.disabled = false;
      showError(
        "The companion service didn't come back within a minute. Run `npm run doctor` in the " +
          "companion-service folder to see why.",
      );
    }

    sendToBackground({ type: "settings-get" })
      .then(({ view: loaded }) => {
        if (panelHost === thisPanel) render(loaded);
      })
      .catch((err) => {
        if (panelHost !== thisPanel) return;
        loading.textContent = "";
        showError(err.message);
      });
  }

  async function refreshAll() {
    if (stopped || startingFeature || isNoticeHeld()) return;
    try {
      // Pass 1 matches URLs too, so the configured targets must be current
      // before it (TTL-guarded, so this is cheap on most ticks).
      await refreshTargets();
      const ctx = buildPageCtx();

      // Pass 1: check for tracked jobs. For COMPLETED jobs (awaiting-approval
      // or failed), collect them to display as highlighted menu items. For
      // RUNNING jobs, use the old behavior (show the job pill with progress).
      storedJobsByFeatureId.clear();
      const savedIds = new Set();
      for (const feature of FEATURES) {
        if (!feature.savedResultIssueKey) continue;
        const { urlMatches, match } = matchFeatureUrl(feature);
        if (!urlMatches) continue;
        try {
          const res = await sendToBackground({ type: feature.savedResultMessage || "analysis-saved", issueKey: feature.savedResultIssueKey(match) });
          if (res && res.exists) {
            // A saved result can be out of date (Summarize comments: the
            // thread has grown); only a current one earns the check.
            const current = feature.savedResultIsCurrent ? await feature.savedResultIsCurrent({ ...ctx, match }, res) : true;
            if (current) savedIds.add(feature.id);
          }
        } catch (err) {
          if (isContextInvalidated(err)) throw err;
          // service down: no check this round
        }
      }
      savedResultFeatureIds = savedIds;
      for (const feature of FEATURES) {
        const { urlMatches, match } = matchFeatureUrl(feature);
        if (!urlMatches) continue;
        const jobKey = storageKey(feature, ctx, match);
        let stored = await getStoredJob(jobKey);
        // No job of this page's own yet — see if an MCP tool already
        // started (or asked to start) one for it (see the lookupMcpJob
        // comment above).
        if (!stored && feature.scopeKey) {
          const job = await lookupMcpJob(feature, match, jobKey);
          if (job && (await adoptMcpJobIfUnseen(job.id))) {
            stored = { jobId: job.id, featureId: feature.id };
            await setStoredJob(jobKey, stored);
          }
        }
        if (!stored) continue;

        // Poll the job to check its status
        let job;
        try {
          const res = await sendToBackground({ type: "status", jobId: stored.jobId });
          job = res.job;
        } catch (err) {
          if (isContextInvalidated(err)) throw err;
          if (isJobGone(err)) {
            // Job is gone, clear it and continue
            await clearStoredJob(jobKey);
            continue;
          }
          // Service is unreachable but job might come back, use old behavior
          const handled = await pollStoredJob(feature, jobKey, stored);
          if (handled) return;
          continue;
        }

        if (job.status === "rejected") {
          // Cancelled or rejected — nothing left to show
          await clearStoredJob(jobKey);
          continue;
        }

        // For running/pending jobs, show the job pill (old behavior)
        if (job.status === "running" || job.status === "approving" || job.status === "rejecting" ||
            job.status === "pending-start") {
          setFabVisible(true);
          if (job.id === autoOpenPanelForJobId &&
              (job.status === "awaiting-approval" || job.status === "failed")) {
            autoOpenPanelForJobId = null;
            setPopoverOpen(false);
            showPanel(feature, job, jobKey);
          } else {
            renderPill(feature, job, jobKey);
          }
          return;
        }

        // For completed jobs (awaiting-approval, failed), collect them to show
        // as highlighted menu items instead of taking over the popover
        if (job.status === "awaiting-approval" || job.status === "failed") {
          if (feature.alwaysFresh) {
            // Never a saved result (see features.js): open the panel the
            // moment the run we just started finishes, else forget it so
            // the menu item stays plain and the next click re-runs.
            if (job.id === autoOpenPanelForJobId) {
              autoOpenPanelForJobId = null;
              setPopoverOpen(false);
              showPanel(feature, job, jobKey);
              setFabVisible(true);
              return;
            }
            await clearStoredJob(jobKey);
            continue;
          }
          storedJobsByFeatureId.set(feature.id, { feature, jobKey, stored, job });
          setFabVisible(true);
          // The run the user just started has finished: open its panel now
          // (the check in the running branch above can never see a finished
          // job). The job stays stored, so the menu keeps its green row for
          // reopening later.
          if (job.id === autoOpenPanelForJobId) {
            autoOpenPanelForJobId = null;
            setPopoverOpen(false);
            showPanel(feature, job, jobKey);
            return;
          }
        }
      }

      // Pass 2: no tracked job — evaluate which features currently apply here.
      setFabState("idle");
      const enabledIds = await getEnabledFeatureIds();
      const results = [];
      for (const feature of FEATURES) {
        if (enabledIds && !enabledIds.has(feature.id)) continue; // disabled server-side
        const { urlMatches, match } = matchFeatureUrl(feature);
        if (!urlMatches) continue;
        try {
          const payload = feature.condition ? await feature.condition({ ...ctx, match }) : true;
          if (payload) {
            results.push({ feature, payload: payload === true ? {} : payload, match });
          }
        } catch {
          // A condition failure (e.g. an API hiccup) just means "don't list
          // this feature this round" — never let one feature's error affect
          // the others.
        }
      }
      await refreshUpdateOffer();
      await refreshInbox();
      // A menu item may have been clicked (or its one-shot result shown)
      // while the conditions above were being evaluated.
      if (startingFeature || isNoticeHeld()) return;
      currentOptions = results;
      applyFabBadge(results.length > 0 || !!updateOffer, !!updateOffer);
      setFabVisible(results.length > 0 || !!updateOffer || inboxState.unseen > 0);
      renderFlyout();
    } catch (err) {
      if (isContextInvalidated(err)) {
        shutdownAndStop();
        return;
      }
      throw err;
    }
  }

  // These pages (Bitbucket, Jira) are single-page apps: navigating between
  // items or toggling tabs mutates the DOM without a full page load.
  // Debounce so a burst of mutations doesn't fire a burst of
  // host-page/companion-service calls. The 2s fallback interval doubles as
  // the job-progress poll rate.
  let refreshScheduled = false;
  function scheduleRefresh() {
    if (stopped || refreshScheduled) return;
    refreshScheduled = true;
    setTimeout(() => {
      refreshScheduled = false;
      refreshAll();
    }, 800);
  }

  scheduleRefresh();
  void restoreOneShotPanels().catch(() => {});
  domObserver = new MutationObserver(scheduleRefresh);
  domObserver.observe(document.body, { childList: true, subtree: true });
  pollIntervalId = setInterval(scheduleRefresh, 2000);
})();
