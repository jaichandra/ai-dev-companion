// The Settings panel's form — the in-browser counterpart of the terminal
// setup wizard (companion-service/setup.js), prefilled from GET /settings.
// See companion-service/core/settings.js for what that returns and what
// PUT /settings accepts. This file only builds and reads the form;
// content.js's openSettingsPanel loads, saves and restarts.
(function () {
  const CHECK_LABELS = { git: "git", claudeCli: "Claude Code CLI", claudeAuth: "Claude Code login" };
  // Which features each group of settings belongs to ("jira", "jenkins", "git"): each
  // feature names its groups in its registry entry (`settingsGroups`). A group only shows while
  // one of its features is ticked. Read when the form is built, once every feature has registered.
  function usedByGroups() {
    const groups = {};
    for (const entry of PaiRegistry.all()) {
      for (const group of entry.settingsGroups || []) (groups[group] = groups[group] || []).push(entry.id);
    }
    return groups;
  }

  function el(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = text;
    return node;
  }

  function badge(kind, text) {
    return el("span", `status-badge ${kind}`, text);
  }

  function textInput(value, placeholder) {
    const input = el("input", "settings-input");
    input.type = "text";
    input.value = value || "";
    input.placeholder = placeholder || "";
    input.spellcheck = false;
    return input;
  }

  function textArea(value, placeholder, rows) {
    const input = el("textarea", "settings-input");
    input.value = value || "";
    input.placeholder = placeholder || "";
    input.rows = rows || 3;
    input.spellcheck = false;
    return input;
  }

  // "" → null, which the service reads as "restore the default".
  function parseList(text) {
    const items = text.split(",").map((s) => s.trim()).filter(Boolean);
    return items.length > 0 ? items : null;
  }
  function parsePipelines(text) {
    const lines = text.split("\n").map((l) => l.trim()).filter(Boolean);
    if (lines.length === 0) return null;
    // A line that isn't exactly three names is sent as-is-ish so the service's
    // validation names the problem next to this field.
    return lines.map((line) => {
      const parts = line.split(/[\s,]+/).filter(Boolean);
      return {
        helmJob: parts[0] || "",
        appJob: parts[1] || "",
        browserJob: parts.length > 3 ? parts.slice(2).join(" ") : parts[2] || "",
      };
    });
  }
  // "job, KEY=VALUE, KEY=VALUE" per line -> [{ job, match }]. A malformed pair
  // keeps its text as a (bad) parameter name so the service's validation
  // names the problem next to this field.
  function parseAutJobs(text) {
    const lines = text.split("\n").map((l) => l.trim()).filter(Boolean);
    return lines.map((line) => {
      const [job, ...pairs] = line.split(",").map((p) => p.trim()).filter(Boolean);
      const match = {};
      for (const pair of pairs) {
        const eq = pair.indexOf("=");
        if (eq < 0) match[pair] = "";
        else match[pair.slice(0, eq).trim()] = pair.slice(eq + 1).trim();
      }
      return { job: job || "", match };
    });
  }
  function parseComponentMap(text) {
    const lines = text.split("\n").map((l) => l.trim()).filter(Boolean);
    if (lines.length === 0) return null;
    const map = {};
    for (const line of lines) {
      const at = line.indexOf("=");
      // A line with no "=" keeps its whole text as the name and an empty repo,
      // so the service's validation flags it next to this field.
      map[(at === -1 ? line : line.slice(0, at)).trim()] = at === -1 ? "" : line.slice(at + 1).trim();
    }
    return map;
  }

  // The three background watchers (companion-service/core/watchers.js).
  const WATCHER_LABELS = {
    conflicts: ["Conflicts on your pull requests", "Prepares a resolution in the background. Nothing is pushed until you approve it.", "You'll see: “Conflict on PR…”, when a PR of yours conflicts at a new commit (urgent if already approved)."],
    assignedBugs: ["Bugs assigned to you", "Runs a read-only analysis of each newly assigned bug, once.", "You'll see: “Analysis on the way for KEY…” (or “Assigned to you: KEY…” if it couldn't be started), once per ticket."],
    reviewRequests: ["Review requests", "Fetches the PR's branch into your clone. It never checks anything out.", "You'll see: “Review requested: PR…”, when you're a reviewer who hasn't voted, again after each new push."],
  };

  /** "last ran 5 min ago", "needs login", … for a watcher's status line. */
  function watcherStatusText(last) {
    if (!last) return "Hasn't run yet.";
    const mins = Math.max(0, Math.round((Date.now() - last.at) / 60000));
    const when = mins < 1 ? "just now" : `${mins} min ago`;
    if (last.needsLogin) return `Needs login — open the site in Chrome (or save a token above). Last tried ${when}.`;
    if (last.outcome === "failed") return `Failed ${when}: ${last.error || "no details"}`;
    return `Last ran ${when}.`;
  }

  function numberInput(value, min, max) {
    const input = el("input", "settings-input");
    input.type = "number";
    input.min = String(min);
    input.max = String(max);
    input.value = String(value);
    return input;
  }

  /** A native time picker: "HH:MM" in, "HH:MM" out, "" when empty. */
  function timeInput(value) {
    const input = el("input", "settings-input");
    input.type = "time";
    input.classList.add("settings-time");
    input.step = "60";
    input.value = value || "";
    return input;
  }

  // The pages of the left-hand navigation, in order. `sub` is the small line
  // under each title; the Features one is rewritten live with the count.
  const PAGES = [
    { id: "features", title: "Features", sub: "", intro: "Choose what the ✨ menu offers. Open a feature to change its own options. Turning a feature on or off takes effect once the companion service restarts." },
    { id: "conn", title: "Code and accounts", sub: "Repos, Claude, Jira, Jenkins", intro: "Connections shared by several features. Jira, Jenkins and Bitbucket use your browser login unless you add a token." },
    { id: "auto", title: "Background work", sub: "Watchers, limits, quiet hours", intro: "Off unless you turn it on. Results appear under ✨ → Ready for you, as Chrome notifications and in `companion inbox`." },
    { id: "data", title: "Privacy and history", sub: "Stored on this machine only", intro: "What the assistant keeps on this machine. Nothing leaves your computer." },
    { id: "proxy", title: "LLM proxy", sub: "Optional", intro: "" },
    { id: "adv", title: "Advanced", sub: "MCP server, connection", intro: "Rarely changed." },
    { id: "about", title: "About", sub: "Version, support", intro: "" },
  ];

  function renderSettingsForm(view, opts = {}) {
    const { settings, features, checks, editors, repoStatus, suggestedRepos, defaults } = view;
    const branding = view.branding || {};
    const usedBy = usedByGroups();
    const background = view.background || {};
    const root = el("div", "settings-form");
    const fieldErrors = new Map();
    const groups = [];
    const pages = [];
    const pageById = new Map();
    let currentPage = "features";
    let built = false;

    // ---- Frame: search, left navigation, the page area ----
    const searchRow = el("div", "settings-search-row");
    const searchInput = el("input", "settings-search");
    searchInput.type = "search";
    searchInput.placeholder = "Search all settings";
    searchInput.spellcheck = false;
    searchRow.appendChild(searchInput);
    const shell = el("div", "settings-shell");
    const nav = el("nav", "settings-nav");
    const main = el("div", "settings-main");
    const results = el("div", "settings-results");
    results.hidden = true;
    main.appendChild(results);
    shell.append(nav, main);
    root.append(searchRow, shell);

    // The proxy page is named and described by the distribution's branding (GET /settings `branding`).
    function pageMeta(id) {
      const meta = PAGES.find((p) => p.id === id);
      const proxy = branding.llmProxy;
      if (id !== "proxy" || !proxy) return meta;
      return {
        ...meta,
        title: proxy.name,
        intro: `A free, ${proxy.operator}-hosted AI model service. Adding your key lets the companion do small jobs itself, keeps ticket and PR text inside ${proxy.operator}, and saves Claude for real work. Everything works without it.`,
      };
    }

    function page(id) {
      const meta = pageMeta(id);
      const node = el("div", "settings-page");
      node.dataset.id = id;
      const head = el("div", "settings-page-head");
      head.append(el("div", "settings-page-title", meta.title), el("div", "settings-page-intro", meta.intro));
      node.appendChild(head);
      main.appendChild(node);
      const navItem = el("div", "settings-nav-item");
      navItem.tabIndex = 0;
      navItem.setAttribute("role", "button");
      const text = el("div", "settings-nav-text");
      const subNode = el("div", "settings-nav-sub", meta.sub);
      text.append(el("div", "settings-nav-title", meta.title), subNode);
      const dot = el("span", "settings-nav-dot");
      dot.hidden = true;
      navItem.append(text, dot);
      const go = () => showPage(id);
      navItem.addEventListener("click", go);
      navItem.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          go();
        }
      });
      nav.appendChild(navItem);
      const pg = { id, title: meta.title, node, navItem, subNode, dot, blocks: [], empty: null };
      pages.push(pg);
      pageById.set(id, pg);
      return pg;
    }

    function showPage(id) {
      if (searchInput.value) {
        searchInput.value = "";
        applySearch();
      }
      currentPage = id;
      for (const pg of pages) {
        pg.node.classList.toggle("on", pg.id === id);
        pg.navItem.classList.toggle("on", pg.id === id);
      }
      main.scrollTop = 0;
    }

    function clearErrors() {
      for (const box of root.querySelectorAll(".field-error")) {
        box.textContent = "";
        box.style.display = "none";
      }
      for (const input of root.querySelectorAll(".invalid")) input.classList.remove("invalid");
    }

    function errorBox() {
      const box = el("div", "field-error");
      box.style.display = "none";
      return box;
    }

    /** Brings the page, feature options, "Advanced" group or hidden repo row that holds `node` into view. */
    function reveal(node) {
      if (!node || !node.closest) return;
      if (!built) return;
      const pg = node.closest(".settings-page");
      if (pg && (searchInput.value || !pg.classList.contains("on"))) showPage(pg.dataset.id);
      const feature = node.closest(".settings-feature");
      if (feature) {
        feature.classList.add("open");
        if (feature.syncToggle) feature.syncToggle();
      }
      const details = node.closest("details");
      if (details) details.open = true;
      const repos = node.closest(".settings-repos");
      if (repos) repos.classList.remove("collapsed");
    }

    function showError(box, message, input) {
      box.textContent = box.textContent ? `${box.textContent}\n${message}` : message;
      box.style.display = "block";
      if (input) input.classList.add("invalid");
      // An error inside a collapsed group must not stay hidden.
      reveal(input || box);
    }

    /** A bordered section of a page: a title, optional tag, note and "Used by" line, then its fields. */
    function block(pg, title, { usedBy, note, status } = {}) {
      const node = el("section", "settings-section");
      node.appendChild(el("div", "settings-path", `${pg.title} › ${title}`));
      const head = el("div", "settings-block-head");
      const titleRow = el("div", "settings-block-title-row");
      titleRow.appendChild(el("div", "settings-section-title", title));
      if (status) titleRow.appendChild(badge(status.kind, status.text));
      node.actionSlot = el("span", "settings-block-action");
      titleRow.appendChild(node.actionSlot);
      head.appendChild(titleRow);
      if (note) head.appendChild(el("div", "settings-note", note));
      if (usedBy) {
        const labels = features.filter((f) => usedBy.includes(f.id)).map((f) => f.label);
        if (labels.length > 0) head.appendChild(el("div", "settings-used-by", `Used by ${labels.join(", ")}`));
        groups.push({ node, usedBy });
      }
      node.appendChild(head);
      pg.node.appendChild(node);
      pg.blocks.push({ node });
      return node;
    }

    /** "Check every [ 15 ] minutes": a label, a narrow input and its unit on ONE line. */
    function inlineRow(parent, before, input, after, fieldName, hint) {
      const wrap = el("div", "settings-item");
      wrap.dataset.search = `${before} ${after || ""} ${hint || ""}`.toLowerCase();
      const row = el("div", "settings-row");
      row.appendChild(el("span", "settings-row-text", before));
      input.classList.add("settings-narrow");
      row.appendChild(input);
      if (after) row.appendChild(el("span", "settings-row-text", after));
      wrap.appendChild(row);
      if (hint) wrap.appendChild(el("p", "settings-hint settings-row-hint", hint));
      const box = errorBox();
      wrap.appendChild(box);
      parent.appendChild(wrap);
      if (fieldName) fieldErrors.set(fieldName, { box, input });
      return { wrap, row, box };
    }

    function field(parent, labelText, input, fieldName, hint) {
      const wrap = el("label", "settings-field settings-item");
      wrap.dataset.search = `${labelText} ${hint || ""}`.toLowerCase();
      wrap.appendChild(el("span", "settings-label", labelText));
      wrap.appendChild(input);
      if (hint) wrap.appendChild(el("span", "settings-hint", hint));
      const box = errorBox();
      wrap.appendChild(box);
      fieldErrors.set(fieldName, { box, input });
      parent.appendChild(wrap);
      return wrap;
    }

    /** A switch with its label and help beside it. `box` is the checkbox the caller reads. */
    function toggleRow(parent, labelText, box, hint, fieldName) {
      box.type = "checkbox";
      box.classList.add("settings-switch");
      const wrap = el("div", "settings-item");
      wrap.dataset.search = `${labelText} ${hint || ""}`.toLowerCase();
      const label = el("label", "settings-toggle");
      const text = el("span", "settings-toggle-text");
      text.appendChild(el("span", "settings-label", labelText));
      if (hint) text.appendChild(el("span", "settings-hint", hint));
      label.append(box, text);
      wrap.appendChild(label);
      const err = errorBox();
      wrap.appendChild(err);
      if (fieldName) fieldErrors.set(fieldName, { box: err, input: box });
      parent.appendChild(wrap);
      return wrap;
    }

    function tokenField(parent, labelText, fieldName, isSet, required = false) {
      const input = el("input", "settings-input");
      input.type = "password";
      input.autocomplete = "new-password";
      input.spellcheck = false;
      input.placeholder = isSet ? "Saved — leave blank to keep it" : required ? "Not set" : "Not set — your browser login is used";
      field(parent, labelText, input, `${fieldName}.apiToken`);
      const clear = el("input");
      if (isSet) {
        toggleRow(parent, "Remove the saved token", clear, null);
        clear.addEventListener("change", () => {
          input.disabled = clear.checked;
          if (clear.checked) input.value = "";
        });
      }
      return { input, clear };
    }

    const featurePage = page("features");
    const connPage = page("conn");
    const autoPage = settings.watchers ? page("auto") : null;
    const dataPage = page("data");
    const proxyPage = settings.llmProxy && branding.llmProxy ? page("proxy") : null;
    const advPage = page("adv");
    const aboutPage = page("about");

    // ---- Features ----
    // One row per feature: a switch, its name and status, and an "Options"
    // link that opens the settings only that feature uses.
    const featureList = el("div", "settings-features");
    featurePage.node.appendChild(featureList);
    featurePage.blocks.push({ node: featureList, custom: true });
    const featureRows = new Map();
    for (const feature of features) {
      const row = el("div", "settings-feature");
      const head = el("div", "settings-feature-head");
      const label = el("label", "settings-feature-label");
      const box = el("input", "settings-switch");
      box.type = "checkbox";
      box.checked = settings.enabledFeatures.includes(feature.id);
      const text = el("div", "settings-feature-text");
      const title = el("div", "settings-feature-title", feature.label);
      const failing = feature.requiredChecks.filter((name) => checks[name] && !checks[name].ok);
      title.appendChild(document.createTextNode(" "));
      title.appendChild(
        failing.length > 0
          ? badge("warn", `Needs ${failing.map((n) => CHECK_LABELS[n] || n).join(", ")}`)
          : badge("ok", "Ready"),
      );
      text.append(title, el("div", "settings-feature-desc", feature.description));
      for (const name of failing) text.appendChild(el("div", "settings-feature-problem", checks[name].message));
      label.append(box, text);
      const toggle = el("button", "settings-link-button", "Options");
      toggle.type = "button";
      toggle.hidden = true;
      row.syncToggle = () => {
        toggle.textContent = row.classList.contains("open") ? "Hide options" : toggle.dataset.closed || "Options";
      };
      toggle.addEventListener("click", () => {
        row.classList.toggle("open");
        row.syncToggle();
      });
      head.append(label, toggle);
      const optionsEl = el("div", "settings-feature-options");
      const path = el("div", "settings-feature-path", `Features › ${feature.label}`);
      row.append(head, path, optionsEl);
      featureList.appendChild(row);
      featureRows.set(feature.id, {
        feature,
        row,
        head,
        box,
        toggle,
        optionsEl,
        own: `${feature.label} ${feature.description}`.toLowerCase(),
      });
    }
    const featuresError = errorBox();
    featurePage.node.appendChild(featuresError);
    fieldErrors.set("enabledFeatures", { box: featuresError });

    /** Where a feature's own settings go. A feature this service doesn't list gets a detached box, so its values still round-trip. */
    function optionsOf(id) {
      const entry = featureRows.get(id);
      return entry ? entry.optionsEl : el("div");
    }

    // Review PR.
    const editorSelect = el("select", "settings-input");
    const detectedIds = editors.detected.map((e) => e.id);
    if (!settings.reviewEditor) editorSelect.appendChild(new Option("Pick automatically (first one installed)", ""));
    for (const editor of editors.supported) {
      const installed = detectedIds.includes(editor.id);
      if (!installed && editor.id !== settings.reviewEditor) continue;
      const label = installed ? editor.label : `${editor.label} — not found on this computer`;
      editorSelect.appendChild(new Option(label, editor.id, false, editor.id === settings.reviewEditor));
    }
    field(
      optionsOf("review-in-editor"),
      'Open "Review PR" checkouts in',
      editorSelect,
      "reviewEditor",
      detectedIds.length === 0 ? "No supported editor (Cursor or Claude Code) was found on this computer." : null,
    );

    // Diagnose failed builds: the optional risk-facts/v1 feed (core/risk-facts.js).
    // `settings.riskFacts` is absent from an older companion service.
    let riskUrlInput = null;
    if (settings.riskFacts) {
      riskUrlInput = textInput(settings.riskFacts.url, "https://…/risk-facts.json");
      field(
        optionsOf("diagnose-build"),
        "Known-flaky test feed URL",
        riskUrlInput,
        "riskFacts.url",
        "Optional. A risk-facts/v1 JSON feed of known-flaky tests; “Diagnose this failed build” shows what it says. Leave blank to turn it off.",
      );
    }

    // Pre-deployment build stats. What a blank field falls back to comes from the pack that owns it.
    const defaultPipeline = ((defaults.jenkinsExtras || {}).pipelines || [])[0];
    const defaultPipelineText = defaultPipeline
      ? [defaultPipeline.helmJob, defaultPipeline.appJob, defaultPipeline.browserJob].join(", ")
      : "helm job, sample-app job, browser job";
    const pipelinesInput = textArea(
      (settings.jenkins.pipelines || []).map((p) => [p.helmJob, p.appJob, p.browserJob].join(", ")).join("\n"),
      defaultPipelineText,
      3,
    );
    field(
      optionsOf("pre-deployment-stats"),
      "Pipelines",
      pipelinesInput,
      "jenkins.pipelines",
      `One per line: helm job, sample-app job, browser job. Leave blank for the default (${defaultPipelineText}).`,
    );

    const defaultAut = ((defaults.jenkinsExtras || {}).autJobs || [])[0];
    const defaultAutText = defaultAut ? [defaultAut.job, ...Object.entries(defaultAut.match || {}).map(([k, v]) => `${k}=${v}`)].join(", ") : "job, KEY=VALUE";
    const autJobsInput = textArea(
      (settings.jenkins.autJobs || [])
        .map((a) => [a.job, ...Object.entries(a.match || {}).map(([k, v]) => `${k}=${v}`)].join(", "))
        .join("\n"),
      defaultAutText,
      3,
    );
    field(
      optionsOf("pre-deployment-stats"),
      "Automation jobs",
      autJobsInput,
      "jenkins.autJobs",
      "One per line: job, then KEY=VALUE build parameters a run must carry to count, e.g. my_aut_job, BRANCH=master. Shown as a warning beside each build, never as a verdict. Clear all lines for none; this list is saved as-is (blank is not “default”).",
    );

    // Summarize comments (the model field only; absent from an older service's view).
    const summarizeModelInput = textInput(settings.summarizeComments?.model || "", "claude-haiku-4-5-20251001 (Haiku)");
    field(optionsOf("summarize-comments"), "Claude model for this feature", summarizeModelInput, "summarizeComments.model", "Blank = Haiku, which is plenty for a summary.");

    // Address review comments (the model field only; absent from an older service's view).
    const addressModelInput = textInput(settings.addressReviewComments?.model || "", "Default model");
    field(
      optionsOf("address-review-comments"),
      "Model for this feature",
      addressModelInput,
      "addressReviewComments.model",
      "Blank = the tool's own default. Runs in Cursor Agent when Cursor is your Review PR editor (use a Cursor model name, e.g. gpt-5), otherwise in Claude Code (a Claude model id).",
    );

    // Analyze a Jira issue.
    const analyzeOptions = optionsOf("analyze-issue");
    const analyzeView = settings.analyzeIssue;
    const analyzeModelInput = textInput(analyzeView.model, "Same as the Claude model on Code and accounts");
    field(analyzeOptions, "Claude model for this feature", analyzeModelInput, "analyzeIssue.model");
    // The target fields are absent from an older companion service's view.
    const defaultProjects = defaults.analyzeProjects || [];
    const projectsInput = textInput((analyzeView.projects || []).join(", "), defaultProjects.slice(0, 2).join(", ") || "PROJ");
    field(
      analyzeOptions,
      "Jira projects",
      projectsInput,
      "analyzeIssue.projects",
      `Comma-separated project keys. Blank = ${defaultProjects.length ? defaultProjects.join(", ") : "every project"}.`,
    );
    const typesInput = textInput((analyzeView.issueTypes || []).join(", "), "Any type");
    field(analyzeOptions, "Issue types", typesInput, "analyzeIssue.issueTypes", "Comma-separated to limit to some types (e.g. Bug, Customer Issue). Blank = any type.");
    const componentsInput = textArea(
      Object.entries(analyzeView.componentRepoMap || {})
        .map(([component, repo]) => `${component} = ${repo}`)
        .join("\n"),
      "Component = PROJECT/repo",
      3,
    );
    field(
      analyzeOptions,
      "Components → repos (learned automatically)",
      componentsInput,
      "analyzeIssue.componentRepoMap",
      "Filled in as tickets are analyzed. Edit or delete a line to correct it; add one (Jira component = PROJECT/repo) to set it yourself.",
    );
    // Similar past tickets (Phase 8): `settings.similar` is absent from an
    // older companion service, so the switch (and its key) is skipped then.
    let similarCheck = null;
    if (settings.similar) {
      similarCheck = el("input");
      similarCheck.checked = settings.similar.enabled !== false;
      toggleRow(
        analyzeOptions,
        "Add similar past tickets",
        similarCheck,
        "From this machine's history, marked as earlier AI output. With the LLM proxy key saved, matches by meaning (on-prem model); otherwise by shared words.",
        "similar.enabled",
      );
    }

    // Ticket to PR. `settings.ticketToPr` is absent from an older companion service.
    let reviewTransitionInput = null;
    let autoMoveCheck = null;
    if (settings.ticketToPr) {
      const ticketOptions = optionsOf("ticket-to-pr");
      reviewTransitionInput = textInput(settings.ticketToPr.reviewTransitionName, "In Review");
      field(
        ticketOptions,
        "Review transition",
        reviewTransitionInput,
        "ticketToPr.reviewTransitionName",
        "The Jira transition (or status) the ticket is moved with once its pull request is open. Skipped when the ticket has none by this name.",
      );
      autoMoveCheck = el("input");
      autoMoveCheck.checked = settings.ticketToPr.autoMoveToReview !== false;
      toggleRow(
        ticketOptions,
        "Move the ticket to review automatically",
        autoMoveCheck,
        "Checks every few minutes for a pull request from a fix you started, and moves the ticket when there is one. Off: use the Move ticket to review button.",
      );
    }

    // Morning digest.
    let digestBox = null;
    let digestTime = null;
    if (settings.digest) {
      const digestOptions = optionsOf("digest");
      digestBox = el("input");
      digestBox.checked = !!settings.digest.enabled;
      toggleRow(digestOptions, "Also post a digest on weekdays", digestBox, null, "digest.enabled");
      digestTime = timeInput(settings.digest.time || "08:30");
      inlineRow(digestOptions, "Post at", digestTime, "(your local time)", "digest.time");
      const syncDigest = () => {
        digestTime.disabled = !digestBox.checked;
      };
      digestBox.addEventListener("change", syncDigest);
      syncDigest();
    }

    for (const { toggle, optionsEl } of featureRows.values()) {
      const n = optionsEl.querySelectorAll("[data-search]").length;
      if (n === 0) continue;
      toggle.hidden = false;
      toggle.dataset.closed = n === 1 ? "Options" : `${n} options`;
      toggle.textContent = toggle.dataset.closed;
    }

    // ---- Code and accounts ----
    const repoSection = block(connPage, "Repositories", {
      usedBy: features.filter((f) => f.needsRepos).map((f) => f.id),
      note:
        'Local clones, as "PROJECT/repo" and folder. You don\'t have to list every repo — the first time you use ' +
        "one that's missing, you're offered to pick its folder or have it cloned.",
    });
    const repoCount = badge("ok", "");
    repoSection.actionSlot.append(repoCount);
    const repoRows = [];
    const repoList = el("div", "settings-repos");
    repoSection.appendChild(repoList);
    const REPOS_SHOWN = 5;
    const repoMore = el("button", "settings-link-button settings-repos-more");
    repoMore.type = "button";

    function syncRepoList() {
      repoCount.textContent = `${repoRows.length} ${repoRows.length === 1 ? "repository" : "repositories"}`;
      const collapsible = repoRows.length > REPOS_SHOWN;
      repoMore.hidden = !collapsible;
      if (!collapsible) repoList.classList.remove("collapsed");
      repoMore.textContent = repoList.classList.contains("collapsed") ? `Show all ${repoRows.length}` : "Show fewer";
    }
    repoMore.addEventListener("click", () => {
      repoList.classList.toggle("collapsed");
      syncRepoList();
    });

    function addRepoRow(key, clonePath, status) {
      const row = el("div", "settings-repo settings-item");
      row.dataset.search = `repositories ${key} ${clonePath}`.toLowerCase();
      const line = el("div", "settings-repo-line");
      const keyInput = textInput(key, "PROJECT/repo");
      keyInput.className += " settings-repo-key";
      const pathInput = textInput(clonePath, "/Users/you/gitviews/repo");
      pathInput.className += " settings-repo-path";
      const state = status
        ? badge(status.ok ? "ok" : "bad", status.ok ? "OK" : "Problem")
        : badge("neutral", "Checked on save");
      if (status && !status.ok) state.title = status.message;
      const remove = el("button", "subtask-row-remove", "×");
      remove.type = "button";
      remove.title = "Remove";
      const box = errorBox();
      if (status && !status.ok) showError(box, status.message);
      const entry = { row, keyInput, pathInput, box };
      remove.addEventListener("click", () => {
        if (baseline.get(keyInput) || baseline.get(pathInput)) removedRepos += 1;
        row.remove();
        repoRows.splice(repoRows.indexOf(entry), 1);
        syncRepoList();
        recompute();
      });
      for (const input of [keyInput, pathInput]) {
        input.addEventListener("input", () => {
          state.className = "status-badge neutral";
          state.textContent = "Checked on save";
          state.title = "";
        });
      }
      line.append(keyInput, pathInput, state, remove);
      row.append(line, box);
      repoList.appendChild(row);
      repoRows.push(entry);
      syncRepoList();
      return entry;
    }

    for (const [key, clonePath] of Object.entries(settings.repos)) addRepoRow(key, clonePath, repoStatus[key]);
    if (repoRows.length > REPOS_SHOWN) repoList.classList.add("collapsed");
    syncRepoList();
    repoSection.appendChild(repoMore);
    const addRepo = el("button", "settings-btn", "Add repository");
    addRepo.type = "button";
    addRepo.addEventListener("click", () => {
      repoList.classList.remove("collapsed");
      addRepoRow("", "", null).keyInput.focus();
      recompute();
    });
    repoSection.actionSlot.append(addRepo);

    if (suggestedRepos.length > 0) {
      const found = el("details", "settings-found");
      found.appendChild(el("summary", null, `${suggestedRepos.length} more clone(s) found on this computer`));
      for (const clone of suggestedRepos) {
        const item = el("div", "settings-found-item");
        const text = el("div", "settings-found-text");
        text.append(el("span", "settings-found-key", clone.key), el("span", "settings-found-path", clone.path));
        const add = el("button", "settings-btn", "Add");
        add.type = "button";
        add.addEventListener("click", () => {
          repoList.classList.remove("collapsed");
          addRepoRow(clone.key, clone.path, null);
          item.remove();
          recompute();
        });
        item.append(text, add);
        found.appendChild(item);
      }
      repoSection.appendChild(found);
    }

    const jiraSection = block(connPage, "Jira", {
      usedBy: usedBy.jira,
      status: settings.jira.apiTokenSet ? { kind: "ok", text: "API token saved" } : settings.jira.apiTokenExternal ? { kind: "ok", text: `Using token from ${settings.jira.apiTokenExternal}` } : { kind: "ok", text: "Using browser login" },
      note: "An API token is only needed as a fallback for when your browser login doesn't work.",
    });
    const jiraUrl = textInput(settings.jira.baseUrl, defaults.jiraBaseUrl);
    field(jiraSection, "Jira base URL", jiraUrl, "jira.baseUrl");
    const jiraToken = tokenField(jiraSection, "Jira API token", "jira", settings.jira.apiTokenSet);

    const jenkinsSection = block(connPage, "Jenkins", {
      usedBy: usedBy.jenkins,
      status: settings.jenkins.apiTokenSet ? { kind: "ok", text: "API token saved" } : settings.jenkins.apiTokenExternal ? { kind: "ok", text: `Using token from ${settings.jenkins.apiTokenExternal}` } : { kind: "ok", text: "Using browser login" },
      note: "A username and API token are only needed as a fallback for when your browser login doesn't work.",
    });
    const jenkinsUrl = textInput(settings.jenkins.baseUrl, defaults.jenkinsBaseUrl);
    field(jenkinsSection, "Jenkins base URL", jenkinsUrl, "jenkins.baseUrl");
    const jenkinsUser = textInput(settings.jenkins.username, "Leave blank to use your browser login");
    field(jenkinsSection, "Jenkins username", jenkinsUser, "jenkins.username");
    const jenkinsToken = tokenField(jenkinsSection, "Jenkins API token", "jenkins", settings.jenkins.apiTokenSet);

    // The git host is whichever site of kind "git" the profile names (Bitbucket, GitHub, ...): its own name, its own
    // settings key. GitHub's browser login is no use to the companion, so it works with a token only.
    const gitSite = (view.sites || []).find((s) => s.kind === "git") || { id: "bitbucket", label: "Bitbucket", tokenOnly: false };
    const gitView = settings[gitSite.id] || { baseUrl: "", apiTokenSet: false, apiTokenExternal: "" };
    const gitDefaultUrl = (defaults.siteBaseUrls && defaults.siteBaseUrls[gitSite.id]) || defaults.bitbucketBaseUrl || "";
    const gitSection = block(connPage, gitSite.label, {
      usedBy: usedBy.git,
      status: gitView.apiTokenSet
        ? { kind: "ok", text: "API token saved" }
        : gitView.apiTokenExternal
          ? { kind: "ok", text: `Using token from ${gitView.apiTokenExternal}` }
          : gitSite.tokenOnly
            ? { kind: "warn", text: "A personal access token is required" }
            : { kind: "ok", text: "Using browser login" },
      note: gitSite.tokenOnly
        ? `${gitSite.label} can only be used with a personal access token (the browser login doesn't work with its API). Create one with access to the repositories you work on — read and write for pull requests and contents — and paste it below.`
        : "An API token is only needed as a fallback for when your browser login doesn't work.",
    });
    const gitUrl = textInput(gitView.baseUrl, gitDefaultUrl);
    field(gitSection, `${gitSite.label} base URL`, gitUrl, `${gitSite.id}.baseUrl`, gitSite.tokenOnly ? "Blank for github.com; for GitHub Enterprise, the address you open in the browser (not the API address)." : undefined);
    const gitToken = tokenField(gitSection, `${gitSite.label} ${gitSite.tokenOnly ? "personal access token" : "API token"}`, gitSite.id, gitView.apiTokenSet, gitSite.tokenOnly);

    connPage.empty = el("div", "settings-empty", "Nothing to set up here until you turn on a feature that uses a repository, Claude, Jira, Jenkins or your git host.");
    connPage.node.appendChild(connPage.empty);

    // ---- Background work ----
    // Opt-in watchers, the daily Claude budget, quiet hours, notification
    // batching and the pre-push check. All read live by the service — no
    // restart. `settings.watchers` is absent from an older companion
    // service, so the whole page is skipped then.
    let backgroundUpdate = null;
    if (autoPage) {
      const scheduler = background.scheduler || {};

      const watchers = block(autoPage, "Watchers", {
        note: "Each watcher checks on a schedule and prepares work in the background. Nothing is pushed until you approve it.",
      });
      const watcherInputs = {};
      for (const [name, [label, hint, expect]] of Object.entries(WATCHER_LABELS)) {
        const saved = settings.watchers[name] || { enabled: false, intervalMinutes: 30 };
        const box = el("input");
        box.checked = !!saved.enabled;
        toggleRow(watchers, label, box, `${hint} ${expect}`, `watchers.${name}.enabled`);
        const interval = numberInput(saved.intervalMinutes, 1, 1440);
        const last = scheduler.watchers && scheduler.watchers[name] ? scheduler.watchers[name].last : null;
        const { wrap } = inlineRow(watchers, "Check every", interval, "minutes", `watchers.${name}.intervalMinutes`, saved.enabled ? watcherStatusText(last) : null);
        wrap.classList.add("settings-indent");
        wrap.dataset.search += ` ${label}`.toLowerCase();
        // The timer only matters while the watcher is on.
        const syncInterval = () => wrap.classList.toggle("is-off", !box.checked);
        box.addEventListener("change", syncInterval);
        syncInterval();
        watcherInputs[name] = { box, interval };
      }

      const limits = block(autoPage, "Limits and quiet time");
      const budgetInput = numberInput(settings.budget.claudeRunsPerDay, 0, 50);
      const used = scheduler.budget ? ` Used today: ${scheduler.budget.used} of ${scheduler.budget.limit}.` : "";
      inlineRow(limits, "Background Claude runs:", budgetInput, "per day", "budget.claudeRunsPerDay", `0 turns background Claude runs off.${used}`);

      const quiet = settings.scheduler.quietHours || { start: "", end: "" };
      const quietStart = timeInput(quiet.start);
      const quietEnd = timeInput(quiet.end);
      const quietRow = inlineRow(
        limits,
        "Quiet hours: from",
        quietStart,
        "to",
        "scheduler.quietHours",
        "Optional. No background runs and no notifications in this window; what's held goes into the digest. Leave both empty for none.",
      );
      quietEnd.classList.add("settings-narrow");
      quietRow.row.appendChild(quietEnd);
      const clearQuiet = el("button", "settings-link-button", "Clear");
      clearQuiet.type = "button";
      clearQuiet.addEventListener("click", () => {
        quietStart.value = "";
        quietEnd.value = "";
        recompute();
      });
      quietRow.row.appendChild(clearQuiet);
      fieldErrors.set("scheduler.quietHours.end", { box: quietRow.box, input: quietEnd });

      const notifyInput = numberInput(settings.notify.minIntervalMinutes, 0, 1440);
      inlineRow(limits, "Notify at most once every", notifyInput, "minutes", "notify.minIntervalMinutes", "Urgent ones (a conflict on an approved PR) come at once.");

      const pushCard = block(autoPage, "Pre-push check", {
        note:
          "When you run git push, warns you if the files in it have a history of test regressions, using the shared test-history feed " +
          "(set under Features › Diagnose failed builds). It only warns, never blocks the push, and stays silent for low risk or when the companion isn't running.",
      });
      const prePushSelect = el("select", "settings-input");
      for (const [label, value] of [
        ["Warn (never blocks the push)", "warn"],
        ["Off", "off"],
      ]) {
        prePushSelect.appendChild(new Option(label, value, false, settings.prePush.mode === value));
      }
      field(pushCard, "Mode", prePushSelect, "prePush.mode", "Warn shows the warning on risky pushes. Off turns it off in every repository.");
      const steps = el("div", "settings-item");
      steps.dataset.search = "pre-push check set up install hook companion hooks install COMPANION_SKIP skip once repository terminal";
      steps.appendChild(el("div", "settings-label", "Set up and skip"));
      const list = el("ol", "settings-hint settings-list");
      for (const parts of [
        ["Per repository, once: open a terminal in that repository's root folder and run ", ["code", "companion hooks install"], ". It adds the hook to that repo's own git config and keeps any existing hook, such as husky."],
        ["Check or remove it with ", ["code", "companion hooks status"], " or ", ["code", "companion hooks uninstall"], " in the same folder, or pass a path: ", ["code", "companion hooks install /path/to/repo"], "."],
        ["Skip it for a single push by putting the variable in front of the command, in the terminal where you push: ", ["code", "COMPANION_SKIP=1 git push"], ". It is not a setting to save here."],
      ]) {
        const li = el("li");
        for (const part of parts) {
          if (typeof part === "string") li.appendChild(document.createTextNode(part));
          else li.appendChild(el(part[0], null, part[1]));
        }
        list.appendChild(li);
      }
      steps.appendChild(list);
      pushCard.appendChild(steps);
      backgroundUpdate = () => ({
        watchers: Object.fromEntries(
          Object.entries(watcherInputs).map(([name, { box, interval }]) => [name, { enabled: box.checked, intervalMinutes: Number(interval.value) }]),
        ),
        budget: { claudeRunsPerDay: Number(budgetInput.value) },
        scheduler: {
          quietHours: quietStart.value.trim() || quietEnd.value.trim() ? { start: quietStart.value.trim(), end: quietEnd.value.trim() } : null,
        },
        notify: { minIntervalMinutes: Number(notifyInput.value) },
        prePush: { mode: prePushSelect.value },
      });
    }

    // ---- Privacy and history ----
    // Feeds the companion's in-memory session vault (core/session-vault.js)
    // so terminal Claude, talking to the companion as an MCP server, can
    // reuse this browser's Jira/Jenkins/Bitbucket logins. Not gated to a
    // feature — it's used by the MCP server, not the ✨ menu.
    const sessionSection = block(dataPage, "Browser sessions");
    const sessionTtlInput = el("input", "settings-input");
    sessionTtlInput.type = "number";
    sessionTtlInput.min = "0";
    sessionTtlInput.max = "1440";
    // `settings.sessionCache` is absent from an older companion service —
    // fall back to the section defaults rather than throwing.
    const sessionCache = settings.sessionCache || { ttlMinutes: 30, heartbeat: false };
    sessionTtlInput.value = String(sessionCache.ttlMinutes);
    sessionTtlInput.classList.add("settings-narrow");
    field(
      sessionSection,
      "Keep sessions for (minutes)",
      sessionTtlInput,
      "sessionCache.ttlMinutes",
      "0 turns this off. Sessions are kept in memory only, never on disk.",
    );
    const heartbeatCheck = el("input");
    heartbeatCheck.checked = !!sessionCache.heartbeat;
    toggleRow(
      sessionSection,
      "Keep browser sessions warm (checks every 5 minutes)",
      heartbeatCheck,
      "Lets Claude Code in your terminal use your Jira, Jenkins and Bitbucket logins while Chrome is open. Off by default.",
      "sessionCache.heartbeat",
    );

    // How long the companion's SQLite history (history.db) keeps rows.
    // `settings.history` is absent from an older companion service, so the
    // section (and its key in getUpdate) is skipped then.
    let historyDaysInput = null;
    if (settings.history) {
      const historySection = block(dataPage, "Local history");
      historyDaysInput = el("input", "settings-input settings-narrow");
      historyDaysInput.type = "number";
      historyDaysInput.min = "7";
      historyDaysInput.max = "3650";
      historyDaysInput.value = String(settings.history.retentionDays);
      field(
        historySection,
        "Keep history for (days)",
        historyDaysInput,
        "history.retentionDays",
        "Older runs are removed from history.db on this machine. Nothing leaves your computer.",
      );
    }

    // ---- Data folder ----
    // Everything the assistant stores on this machine (job worktrees, analysis
    // clones, history). The size comes from the service; Purge clears only
    // the rebuildable checkouts (core/data-folder.js). Skipped against an
    // older service that has no /data-folder route.
    const dataFolderSection = block(dataPage, "Data folder", {
      note: "Where worktrees, analysis checkouts and history are kept. Purging removes the checkouts only; history, settings and logins stay.",
    });
    const formatBytes = (n) => {
      if (n < 1024) return `${n} B`;
      const units = ["KB", "MB", "GB", "TB"];
      let v = n / 1024;
      let i = 0;
      while (v >= 1024 && i < units.length - 1) {
        v /= 1024;
        i++;
      }
      return `${v.toFixed(v < 10 ? 1 : 0)} ${units[i]}`;
    };
    const dataRow = (label, valueNode) => {
      const wrap = el("div", "settings-item");
      wrap.dataset.search = `${label} data folder worktrees purge size disk`.toLowerCase();
      const row = el("div", "settings-row");
      row.append(el("span", "settings-row-text", label), valueNode);
      wrap.appendChild(row);
      dataFolderSection.appendChild(wrap);
    };
    const dataPath = el("span", "settings-row-text", "Checking…");
    const dataSize = el("span", "settings-row-text", "Checking…");
    const dataPurgeable = el("span", "settings-row-text", "");
    dataRow("Location", dataPath);
    dataRow("Total size", dataSize);
    dataRow("Can be purged", dataPurgeable);
    const dataActions = el("div", "settings-row");
    const openFolder = el("button", "settings-link-button", "Open folder");
    const purgeData = el("button", "settings-link-button", "Purge data");
    purgeData.disabled = true;
    const dataMessage = el("div", "settings-note", "");
    dataActions.append(openFolder, purgeData);
    dataFolderSection.append(dataActions, dataMessage);
    const showUsage = (usage) => {
      dataPath.textContent = usage.path;
      dataSize.textContent = formatBytes(usage.totalBytes);
      dataPurgeable.textContent = formatBytes(usage.purgeableBytes);
      purgeData.disabled = usage.purgeableBytes === 0;
    };
    const dataFailure = (response) => (chrome.runtime.lastError ? chrome.runtime.lastError.message : response && response.error);
    chrome.runtime.sendMessage({ type: "data-folder-get" }, (response) => {
      const failure = dataFailure(response);
      if (failure || !response || !response.usage) {
        dataPath.textContent = "Not available";
        dataSize.textContent = "Not available";
        dataMessage.textContent = failure ? String(failure) : "";
        return;
      }
      showUsage(response.usage);
    });
    openFolder.addEventListener("click", () => {
      dataMessage.textContent = "";
      chrome.runtime.sendMessage({ type: "data-folder-open" }, (response) => {
        const failure = dataFailure(response);
        if (failure) dataMessage.textContent = `Couldn't open the folder: ${failure}`;
      });
    });
    purgeData.addEventListener("click", () => {
      if (
        !confirm(
          "Delete all worktrees and analysis checkouts?\n\nHistory, settings and logins are kept. Results still waiting for your approval and any resumable sessions in those checkouts will be lost. They can be re-run.",
        )
      ) {
        return;
      }
      purgeData.disabled = true;
      dataMessage.textContent = "Purging…";
      chrome.runtime.sendMessage({ type: "data-folder-purge" }, (response) => {
        const failure = dataFailure(response);
        if (failure) {
          dataMessage.textContent = `Couldn't purge: ${failure}`;
          purgeData.disabled = false;
          return;
        }
        dataMessage.textContent = `Freed ${formatBytes(response.freedBytes || 0)}.`;
        if (response.usage) showUsage(response.usage);
      });
    });

    // ---- LLM proxy (named by the distribution's branding) ----
    // The proxy's on-prem models (companion-service/core/llm-proxy.ts), used for
    // the watchers' quick "worth a Claude run?" triage. Optional: without a
    // key everything works on rule-based filters. Only the on-prem models the
    // service allows are offered; the key is shown only as saved or not.
    let llmUpdate = null;
    if (proxyPage) {
      const p = settings.llmProxy;
      const state = background.llmProxy ? background.llmProxy.label : "not checked";
      const signIn = block(proxyPage, "Sign-in", {
        status: { kind: /ready/i.test(state) ? "ok" : "neutral", text: state.charAt(0).toUpperCase() + state.slice(1) },
        note: "Your key is stored encrypted on this machine and never shown again.",
      });
      let keyPage = null;
      try {
        keyPage = new URL(p.keyPageUrl).protocol === "https:" ? p.keyPageUrl : null;
      } catch {
        keyPage = null;
      }
      if (keyPage) {
        const link = el("a", "settings-link", `Get your key on the ${branding.llmProxy.name} site`);
        link.href = keyPage;
        link.target = "_blank";
        link.rel = "noopener noreferrer";
        signIn.appendChild(link);
      }
      const keyInput = el("input", "settings-input");
      keyInput.type = "password";
      keyInput.autocomplete = "new-password";
      keyInput.spellcheck = false;
      keyInput.placeholder = p.apiKeySet ? "Saved — leave blank to keep it" : "Not set";
      field(signIn, "API key", keyInput, "llmProxy.apiKey", "Paste the key you created on the proxy site. Status should turn to “ready” after you save.");
      const clearKey = el("input");
      if (p.apiKeySet) {
        toggleRow(signIn, "Remove the saved key", clearKey, null);
        clearKey.addEventListener("change", () => {
          keyInput.disabled = clearKey.checked;
          if (clearKey.checked) keyInput.value = "";
        });
      }
      const userInput = textInput(p.user, "your sign-on name (blank: your computer's user name)");
      field(signIn, "Sign-on user name", userInput, "llmProxy.user", "The proxy asks who is calling. Leave blank to use your computer's user name.");
      const agentInput = textInput(p.userAgent, p.defaultUserAgent);
      field(
        signIn,
        "Client name (User-Agent)",
        agentInput,
        "llmProxy.userAgent",
        "The proxy ties each key to the application it was created for (QwenCode, Cline, OpenCode…) and checks the client name on every request. This tool isn't in that list, so if the status says your key is for another application, enter that application's name here (for example QwenCode/1.0) — or ask the proxy team to add one for this tool. Blank uses the default.",
      );
      const advanced = el("details", "settings-advanced");
      advanced.appendChild(el("summary", null, "Advanced"));
      signIn.appendChild(advanced);
      const baseInput = textInput(p.baseUrl, p.defaultBaseUrl);
      field(advanced, "API URL", baseInput, "llmProxy.baseUrl", "Blank for the default.");

      const modelsCard = block(proxyPage, "Models", { note: `Only ${branding.llmProxy.operator} on-prem models are listed.` });
      const modelSelect = (current, models) => {
        const select = el("select", "settings-input");
        for (const m of models) select.appendChild(new Option(m, m, false, m === current));
        return select;
      };
      const chatSelect = modelSelect(p.chatModel, p.chatModels || p.allowedModels);
      field(modelsCard, "Chat model", chatSelect, "llmProxy.chatModel", "Used for the watchers' quick checks.");
      const embedSelect = modelSelect(p.embeddingModel, p.embeddingModels || p.allowedModels);
      field(modelsCard, "Embedding model", embedSelect, "llmProxy.embeddingModel", "Used to match similar past tickets by meaning.");

      const what = block(proxyPage, "What the proxy does");
      const gains = el("ul", "settings-hint settings-list");
      for (const line of [
        `Background watchers: before spending one of your daily Claude runs, a ${branding.llmProxy.operator} model checks quickly whether a new conflict, bug or review request is worth it. Without a key, simple built-in rules decide, so a few more runs may be wasted.`,
        "Similar past tickets: finds earlier tickets that mean the same thing even when the words differ, and gives them to Claude when it analyses a ticket. Without a key, matching uses shared words only.",
      ]) {
        gains.appendChild(el("li", null, line));
      }
      const gainsItem = el("div", "settings-item");
      gainsItem.dataset.search = "what the proxy does background watchers similar past tickets outside ai vendors";
      gainsItem.append(
        gains,
        el("p", "settings-hint", `It never sends anything to outside AI vendors: only ${branding.llmProxy.operator}'s own on-prem models are allowed, and only over https to a ${branding.llmProxy.operator} address.`),
      );
      what.appendChild(gainsItem);

      llmUpdate = () => ({
        llmProxy: {
          baseUrl: baseInput.value.trim(),
          user: userInput.value.trim(),
          userAgent: agentInput.value.trim(),
          chatModel: chatSelect.value,
          embeddingModel: embedSelect.value,
          apiKey: keyInput.value.trim(),
          clearApiKey: clearKey.checked,
        },
      });
    }

    // ---- Advanced: MCP server, connection ----
    // The companion's own MCP server (companion-service/core/mcp.ts), which
    // terminal Claude Code / Cursor call with a token of its own — never
    // shown here, only whether one is saved. Not gated to a feature, same
    // as Browser sessions above. `settings.mcp` is absent from an older
    // companion service, so this section (and its key in getUpdate) is
    // skipped then rather than sending a setting it would reject.
    if (settings.mcp) {
      const mcpSection = block(advPage, "MCP server");
      const tokenItem = el("div", "settings-item");
      tokenItem.dataset.search = "mcp token rotate";
      const tokenStatus = el("p", "settings-hint", `Token: ${settings.mcp.tokenSet ? "set" : "not set"}`);
      tokenItem.appendChild(tokenStatus);
      const rotate = el("button", "settings-btn", "Rotate token");
      rotate.type = "button";
      const rotateResult = el("div", "settings-hint");
      const rotateError = errorBox();
      rotate.addEventListener("click", () => {
        if (
          !confirm(
            "Rotate the MCP token? Claude Code and Cursor are re-registered automatically; other clients need the new token.",
          )
        ) {
          return;
        }
        rotate.disabled = true;
        rotateResult.textContent = "";
        rotateError.textContent = "";
        rotateError.style.display = "none";
        // Saves only the rotation, not the rest of the form — nothing else
        // on this panel is touched by it.
        chrome.runtime.sendMessage({ type: "settings-save", settings: { mcp: { rotateToken: true } } }, (response) => {
          rotate.disabled = false;
          const failure = chrome.runtime.lastError ? chrome.runtime.lastError.message : response && response.error;
          if (failure) {
            showError(rotateError, failure);
            return;
          }
          const saved = (response && response.view) || {};
          tokenStatus.textContent = `Token: ${saved.settings?.mcp?.tokenSet ? "set" : "not set"}`;
          const lines = ["Token rotated."];
          const registration = saved.mcpRegistration;
          if (registration) {
            for (const [client, label] of [
              ["claude", "Claude Code"],
              ["cursor", "Cursor"],
            ]) {
              if (registration[client]) lines.push(`${label}: ${registration[client].message}`);
            }
          }
          rotateResult.textContent = lines.join("\n");
          rotateResult.style.whiteSpace = "pre-line";
        });
      });
      tokenItem.append(rotate, rotateResult, rotateError);
      mcpSection.appendChild(tokenItem);
    }

    // ---- Connection (read-only) ----
    const connection = block(advPage, "Connection", {
      note:
        "The extension reaches the companion service on this port using a shared secret, so neither can be " +
        "changed from here — that would disconnect this panel. To change them, re-run `npm run setup` in the " +
        "companion-service folder.",
    });
    const portInput = textInput(String(settings.port));
    portInput.readOnly = true;
    portInput.classList.add("settings-narrow");
    field(connection, "Port", portInput, "port");

    // ---- About ----
    const SUPPORT_EMAIL = branding.supportEmail || "";
    const about = block(aboutPage, "AI Dev Companion");
    let version = "unknown";
    try {
      version = chrome.runtime.getManifest().version;
    } catch {
      // Not running as an extension page; leave the version as unknown.
    }
    const aboutItem = (label, value) => {
      const wrap = el("div", "settings-item");
      wrap.dataset.search = `${label} ${typeof value === "string" ? value : SUPPORT_EMAIL}`.toLowerCase();
      const row = el("div", "settings-row");
      row.append(el("span", "settings-row-text", label), typeof value === "string" ? el("span", "settings-row-text", value) : value);
      wrap.appendChild(row);
      about.appendChild(wrap);
    };
    aboutItem("Version", version);
    if (SUPPORT_EMAIL) {
      const mail = el("a", "settings-link", SUPPORT_EMAIL);
      mail.href = `mailto:${SUPPORT_EMAIL}?subject=${encodeURIComponent(`AI Dev Companion ${version} support`)}`;
      aboutItem("Support", mail);
    }

    // ---- Check for updates ----
    const updateBtn = el("button", "settings-btn", "Check for updates");
    updateBtn.type = "button";
    const updateStatus = el("span", "settings-row-text", "");
    const updateCell = el("span", "settings-row-text");
    updateCell.append(updateStatus, " ", updateBtn);
    aboutItem("Updates", updateCell);

    const bgMessage = (message) =>
      new Promise((resolve, reject) => {
        chrome.runtime.sendMessage(message, (response) => {
          const failure = chrome.runtime.lastError ? chrome.runtime.lastError.message : response && response.error;
          if (failure) reject(new Error(String(failure)));
          else resolve(response);
        });
      });
    let offeredLatest = null;
    const checkForUpdates = async () => {
      updateBtn.disabled = true;
      updateBtn.classList.remove("go");
      updateStatus.classList.remove("settings-ok");
      updateStatus.textContent = "Checking…";
      try {
        const { info } = await bgMessage({ type: "update-info", refresh: true });
        const run = info.update;
        if (run && run.state === "running") {
          updateStatus.textContent = "An update is already running…";
        } else if (info.available) {
          offeredLatest = info.latest;
          if (!info.canApply) {
            updateStatus.textContent = `v${info.latest} is available. ${info.cannotApplyReason} Update by hand: ${info.manualCommand}`;
          } else {
            updateStatus.textContent = `v${info.latest} is available.`;
            updateStatus.classList.add("settings-ok");
            updateBtn.classList.add("go");
            updateBtn.textContent = `Update to v${info.latest}`;
          }
        } else if (info.error) {
          // A failed check keeps the previous answer, so "up to date" would be a guess.
          offeredLatest = null;
          updateStatus.textContent = `Couldn't check for updates: ${info.error}`;
        } else {
          offeredLatest = null;
          updateBtn.textContent = "Check for updates";
          updateStatus.textContent = `You're up to date (v${info.current}).`;
        }
      } catch (err) {
        updateStatus.textContent = `Couldn't check: ${err.message}`;
      }
      updateBtn.disabled = false;
    };
    const applyUpdate = async () => {
      updateBtn.disabled = true;
      updateStatus.classList.remove("settings-ok");
      updateStatus.textContent = "Starting the update…";
      try {
        await bgMessage({ type: "update-apply" });
        const deadline = Date.now() + 10 * 60 * 1000;
        while (Date.now() < deadline) {
          await new Promise((resolve) => setTimeout(resolve, 2000));
          let res;
          try {
            res = await bgMessage({ type: "update-info" });
          } catch {
            updateStatus.textContent = "Restarting the companion service…";
            continue;
          }
          const run = res.info.update;
          if (!run || run.state === "running") {
            updateStatus.textContent = (run && run.label) || "Updating…";
          } else if (run.state === "failed") {
            updateStatus.textContent = run.error || "The update failed.";
            break;
          } else if (run.needsRestart) {
            updateStatus.textContent = `${run.label} Restart the companion service yourself (cd ~/ai-dev-companion/companion-service && npm start), then reload the extension.`;
            return;
          } else {
            updateStatus.textContent =
              `Updated to v${res.info.current}. Click "Reload extension and page" to finish. ` +
              "If it doesn't reload, open chrome://extensions and click the reload (↻) icon on AI Dev Companion. " +
              "Refresh any other open Bitbucket, Jira or Jenkins tabs too.";
            updateBtn.textContent = "Reload extension and page";
            updateBtn.classList.add("go");
            updateStatus.classList.add("settings-ok");
            updateBtn.disabled = false;
            offeredLatest = null;
            updateBtn.onclick = () => {
              updateBtn.disabled = true;
              updateStatus.textContent = "Reloading the extension…";
              bgMessage({ type: "reload-extension" }).catch(() => {});
              setTimeout(() => location.reload(), 1500);
            };
            return;
          }
        }
        if (updateStatus.textContent === "Updating…") updateStatus.textContent = "The update is taking longer than expected; see ~/.ai-dev-companion/update.log.";
      } catch (err) {
        updateStatus.textContent = `Update failed: ${err.message}`;
      }
      updateBtn.disabled = false;
    };
    updateBtn.onclick = () => (offeredLatest ? applyUpdate() : checkForUpdates());

    function selectedFeatureIds() {
      return features.filter((f) => featureRows.get(f.id).box.checked).map((f) => f.id);
    }

    /** Hides a group while none of the features that use it is ticked. */
    function updateGroups() {
      const enabled = selectedFeatureIds();
      for (const g of groups) g.node.classList.toggle("gated-off", !g.usedBy.some((id) => enabled.includes(id)));
      for (const pg of pages) {
        if (pg.empty) pg.empty.hidden = pg.blocks.some((b) => !b.node.classList.contains("gated-off"));
      }
    }

    // ---- Search ----
    // Every input lives in the DOM all the time; searching just shows the
    // ones whose label or help text matches, each under its page › section.
    function applySearch() {
      const q = searchInput.value.trim().toLowerCase();
      const searching = q !== "";
      root.classList.toggle("searching", searching);
      let total = 0;
      for (const pg of pages) {
        let pageCount = 0;
        for (const b of pg.blocks) {
          if (b.custom) {
            let n = 0;
            for (const { row, head, optionsEl, own } of featureRows.values()) {
              const ownMatch = searching && own.includes(q);
              let optionMatches = 0;
              for (const it of optionsEl.querySelectorAll("[data-search]")) {
                const m = !searching || it.dataset.search.includes(q);
                it.hidden = !m;
                if (searching && m) optionMatches += 1;
              }
              row.hidden = searching && !ownMatch && optionMatches === 0;
              head.hidden = searching && !ownMatch;
              row.classList.toggle("force-open", searching && optionMatches > 0);
              if (searching) n += (ownMatch ? 1 : 0) + optionMatches;
            }
            b.node.hidden = searching && n === 0;
            pageCount += n;
            continue;
          }
          const gatedOff = b.node.classList.contains("gated-off");
          let n = 0;
          for (const it of b.node.querySelectorAll("[data-search]")) {
            const m = !searching || (!gatedOff && it.dataset.search.includes(q));
            it.hidden = !m;
            if (searching && m) n += 1;
          }
          b.node.hidden = searching && n === 0;
          pageCount += n;
        }
        pg.node.hidden = searching && pageCount === 0;
        total += pageCount;
      }
      for (const pg of pages) pg.navItem.classList.toggle("on", !searching && pg.id === currentPage);
      results.hidden = !searching;
      results.textContent = searching
        ? total === 0
          ? `No settings match “${searchInput.value.trim()}”`
          : `${total} ${total === 1 ? "setting" : "settings"} matching “${searchInput.value.trim()}”`
        : "";
      if (!searching) main.scrollTop = 0;
    }
    searchInput.addEventListener("input", applySearch);
    searchInput.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && searchInput.value) {
        e.stopPropagation();
        searchInput.value = "";
        applySearch();
      }
    });

    // ---- Unsaved-change tracking ----
    // A control is dirty when it differs from what it held when the form was
    // built; a row added afterwards starts from "". Removing a saved repo
    // row counts as a change to its page.
    const baseline = new Map();
    let removedRepos = 0;
    const valueOf = (c) => (c.type === "checkbox" ? String(c.checked) : c.value);
    const controlsOf = (node) => node.querySelectorAll("input, select, textarea");
    let lastCount = -1;
    let scheduled = false;

    function recompute() {
      const featureCount = features.filter((f) => featureRows.get(f.id).box.checked).length;
      featurePage.subNode.textContent = `${featureCount} of ${features.length} on`;
      let count = 0;
      for (const pg of pages) {
        let n = pg.id === "conn" ? removedRepos : 0;
        for (const c of controlsOf(pg.node)) {
          const base = baseline.has(c) ? baseline.get(c) : c.type === "checkbox" ? "false" : "";
          if (valueOf(c) !== base) n += 1;
        }
        pg.dot.hidden = n === 0;
        count += n;
      }
      if (count !== lastCount) {
        lastCount = count;
        if (form.onDirtyChange) form.onDirtyChange(count);
      }
    }
    function scheduleRecompute() {
      if (scheduled) return;
      scheduled = true;
      setTimeout(() => {
        scheduled = false;
        recompute();
      }, 0);
    }
    main.addEventListener("input", scheduleRecompute);
    main.addEventListener("change", scheduleRecompute);
    main.addEventListener("click", scheduleRecompute);

    for (const box of [...featureRows.values()].map((r) => r.box)) box.addEventListener("change", updateGroups);
    updateGroups();

    function getUpdate() {
      clearErrors();
      const repos = {};
      let incomplete = false;
      for (const { keyInput, pathInput, box } of repoRows) {
        const key = keyInput.value.trim();
        const clonePath = pathInput.value.trim();
        if (!key && !clonePath) continue;
        if (!key || !clonePath) {
          showError(box, "Needs both a PROJECT/repo and a folder.", key ? pathInput : keyInput);
          incomplete = true;
          continue;
        }
        repos[key] = clonePath;
      }
      if (incomplete) throw new Error("Some repositories are missing a name or a folder.");
      return {
        enabledFeatures: selectedFeatureIds(),
        repos,
        reviewEditor: editorSelect.value || null,
        jira: { baseUrl: jiraUrl.value, apiToken: jiraToken.input.value, clearApiToken: jiraToken.clear.checked },
        jenkins: {
          baseUrl: jenkinsUrl.value,
          username: jenkinsUser.value,
          apiToken: jenkinsToken.input.value,
          clearApiToken: jenkinsToken.clear.checked,
          pipelines: parsePipelines(pipelinesInput.value),
          autJobs: parseAutJobs(autJobsInput.value),
        },
        [gitSite.id]: {
          baseUrl: gitUrl.value,
          apiToken: gitToken.input.value,
          clearApiToken: gitToken.clear.checked,
        },
        analyzeIssue: {
          model: analyzeModelInput.value,
          projects: parseList(projectsInput.value),
          issueTypes: parseList(typesInput.value),
          componentRepoMap: parseComponentMap(componentsInput.value),
        },
        ...(settings.summarizeComments ? { summarizeComments: { model: summarizeModelInput.value } } : {}),
        ...(settings.addressReviewComments ? { addressReviewComments: { model: addressModelInput.value } } : {}),
        sessionCache: { ttlMinutes: Number(sessionTtlInput.value), heartbeat: heartbeatCheck.checked },
        ...(historyDaysInput ? { history: { retentionDays: Number(historyDaysInput.value) } } : {}),
        ...(similarCheck ? { similar: { enabled: similarCheck.checked } } : {}),
        ...(riskUrlInput ? { riskFacts: { url: riskUrlInput.value.trim() } } : {}),
        ...(reviewTransitionInput ? { ticketToPr: { reviewTransitionName: reviewTransitionInput.value.trim(), ...(autoMoveCheck ? { autoMoveToReview: autoMoveCheck.checked } : {}) } } : {}),
        ...(digestBox ? { digest: { enabled: digestBox.checked, time: digestTime.value.trim() || "08:30" } } : {}),
        ...(backgroundUpdate ? backgroundUpdate() : {}),
        ...(llmUpdate ? llmUpdate() : {}),
      };
    }

    /** Marks each server-side error on its field; returns the messages that
     * don't belong to any field shown here. */
    function showErrors(errors) {
      clearErrors();
      const unmatched = [];
      for (const { field: name, message } of errors) {
        const target = fieldErrors.get(name);
        if (target) {
          showError(target.box, message, target.input);
          continue;
        }
        const repo = name.startsWith("repos.") && repoRows.find((r) => r.keyInput.value.trim() === name.slice(6));
        if (repo) {
          showError(repo.box, message, repo.pathInput);
          continue;
        }
        unmatched.push(name ? `${name} ${message}` : message);
      }
      const first = root.querySelector(".field-error[style*='block']");
      if (first) first.scrollIntoView({ block: "center", behavior: "smooth" });
      return unmatched;
    }

    const form = { node: root, getUpdate, showErrors, onDirtyChange: null, currentPage: () => currentPage };
    for (const c of controlsOf(root)) if (c !== searchInput) baseline.set(c, valueOf(c));
    built = true;
    showPage(pageById.has(opts.page) ? opts.page : "features");
    recompute();
    return form;
  }

  window.renderSettingsForm = renderSettingsForm;
})();
