// Extension-side feature registry — the mirror of the companion service's
// Feature plugin interface (companion-service/core/jobs.ts). Adding a new
// AI feature later means adding an entry here (id/menuLabel/gating/
// renderPanel) plus its companion-service plugin; content.js never needs
// to change.
//
// Every entry is gated by up to two independent, optional conditions —
// content.js evaluates both and only lists the feature when both that are
// present pass:
//
//   urlPattern   RegExp (or (targets) => RegExp) tested against location.href. Absent => matches
//                every page this content script runs on (see manifest.json's
//                content_scripts.matches for what that actually is).
//                content.js exposes the match array's capture groups to
//                `condition` (and reuses it to scope stored-job tracking) as
//                ctx.match.
//   scopeKey(match) optional. Builds the exact same scopeKey string the
//                companion service tags this feature's jobs with
//                (companion-service/core/scope-key.js's scopeKeyFor) from
//                urlPattern's own match array. Only a feature an MCP tool
//                can start on Claude Code's behalf needs one — it's how
//                content.js's GET /jobs/lookup (Task 6) finds a job the
//                *service* already knows about for this page before any
//                stored jobKey exists here. Absent => this feature's jobs
//                are only ever adopted via a browser click's own stored job.
//   condition(ctx) async predicate. ctx is { url, origin, pathname, search,
//                match }. Return a truthy payload object to list this
//                feature (POSTed verbatim — or merged with renderStartForm's
//                getPayload() result, see below — to /features/<id>/start);
//                return null/false to hide it. `true` is shorthand for
//                "list it with payload {}".
//                Absent => listed whenever urlPattern matched (or always, if
//                urlPattern is also absent). A throwing condition just drops
//                this one feature for this refresh — see content.js's
//                refreshAll — it never affects other features.
//
// Other entry fields:
//   id            must match the companion-service feature id
//   menuLabel     text for this feature's row in the flyout menu
//   global        true => not tied to a page: shown as an icon shortcut in the menu header
//                (next to Settings) instead of a row in the page's list.
//   oneShot       optional boolean. When true, clicking the flyout row
//                POSTs straight to /start and shows the server's response
//                — a finished job's result.summary on success, or
//                job.error on failure — directly in the popover
//                (content.js's runOneShot). No job is ever stored: there's
//                no approve/reject step to end tracking with, and a job
//                that reaches a terminal status on its own would
//                otherwise hijack the FAB on this page forever (see
//                refreshAll's pass 1 and storageKey above). Mutually
//                exclusive with renderStartForm and with
//                renderPanel/progressSteps — see review-in-editor below
//                for the canonical example (a single click-and-done
//                action with nothing to review inside the extension
//                itself).
//   renderOneShotPanel(state) optional, oneShot features only. When
//                present, the click opens the overlay panel straight away
//                instead of reporting in the popover, and content.js
//                re-renders the panel body with this function's return
//                (a DOM node) as the job goes: state is { payload,
//                status: "running" } first, then { payload, status:
//                "done", job } or { payload, status: "failed", error }.
//                The panel stays open until the user closes it, and is
//                reopened after a reload while the service still has the
//                job (under its own storage key, not storageKey's, so it
//                never takes over the FAB). When job.result.review.tracked
//                is set, the job's work carries on outside the service:
//                content.js polls GET /status while the panel is open and
//                the session is live, re-rendering "done" with `session`
//                (the service's { state, exitCode }), `stopping`,
//                `stopError` and a `stopSession()` that POSTs cancel. A
//                start the service refuses because an earlier job's work
//                is still live (a 409 whose body has `existing`) renders
//                { payload, status: "conflict", conflict, startWith },
//                where conflict is that body and startWith(payload)
//                starts again in the panel. See review-in-editor below.
//   readOnly      optional boolean. For a feature whose panel *reports*
//                something rather than proposing a change to approve —
//                see pre-deployment-stats below. Everything else works
//                exactly like the polled renderPanel shape (the job is
//                tracked, progress shows in the popover, the panel
//                auto-opens on "awaiting-approval"); the only difference
//                is that content.js's showPanel gives the footer a single
//                Done button instead of Approve/Discard, since there's
//                nothing to apply and nothing to clean up. Done still
//                clears the stored job — a terminal job left stored would
//                hijack the FAB on this page (the same trap oneShot
//                sidesteps by never storing one) — but makes no server
//                call, so such a feature's approve()/reject() only ever
//                guard against being called at all.
//   approveLabel  optional; button text for the review panel's primary
//                action (defaults to "Approve & Push" — leave unset if that
//                fits). Only relevant to a feature using renderPanel's
//                getApprovalPayload path (see below) — unused by anything
//                today, kept as infra for a future feature that needs it.
//   rejectLabel   optional; text of the Discard button (defaults to
//                "Discard") — e.g. ticket-to-pr's "Stop tracking", whose
//                reject only stops tracking and deletes nothing.
//   renderStartForm(payload) optional -> { node, getPayload() }. When
//                present, clicking the flyout row opens the big overlay
//                panel (content.js's openComposePanel) with this form
//                instead of starting immediately — see create-jira-subtasks
//                below for the canonical example (a repeatable row of
//                inputs). getPayload() is called on submit; its return value
//                is merged over the detect/condition payload and POSTed to
//                /start. getPayload() may throw an Error (e.g. a blank
//                field) to block submission — content.js shows the message
//                and lets the user fix the form, nothing is sent. A
//                renderStartForm feature doesn't need renderPanel at all
//                (see below) unless it wants one: on success the compose
//                panel just closes and reloads the page; on failure it
//                shows job.error in the footer, not a re-render.
//   startLabel    optional; footer button text in compose mode (defaults to
//                "Submit").
//   progressSteps  ordered [{id, label}] matching the companion service's
//                job.progress.stepId values for this feature (see
//                companion-service/features/<id>/index.ts) — content.js
//                renders this as a checklist, checking off everything
//                before the current step and using the *server's* live
//                label (job.progress.label) for the current one, since
//                that can carry detail a static label can't (e.g. a file
//                count). Only meaningful for a feature that's polled after
//                starting (resolve-conflict) — a feature whose /start runs
//                to completion before responding (create-jira-subtasks)
//                has nothing to poll, so it has no progressSteps either.
//   quietCompleted optional; true to mark a finished, kept job on the menu with
//                just a check (like a saved result) instead of the green row.
//   alwaysFresh   optional; true for a read-only report whose finished job is
//                never kept as a green "completed" menu item: each click starts
//                a new run and the panel opens when it finishes (ticket-workspace
//                for live state; analyze-issue, whose result the companion saves).
//   savedResultIssueKey(match)  optional; with the companion's analysis cache,
//                gives the menu item a green check while a saved result exists.
//   savedResultMessage  optional; the background.js message that asks the
//                companion whether a saved result exists (default
//                "analysis-saved"; Summarize comments uses "summary-saved").
//   savedResultIsCurrent(ctx, saved)  optional, async; false when the saved
//                result is stale, so the menu item gets no green check.
//   cancellable   optional; true if the companion service implements
//                cancel() for this feature — the progress popover then
//                shows a Cancel button while the job is running. Works
//                with readOnly features too (Cancel while running;
//                Done-only on the finished panel — or Done + Re-analyze
//                when reanalyzeLabel is set).
//   cancelConfirm optional; second sentence in the Cancel confirm dialog
//                (defaults to a generic "in-progress work discarded"
//                line). Only used when cancellable is true.
//   reanalyzeLabel optional; for readOnly features whose companion start
//                accepts `{ force: true }` to skip a saved result. When
//                set, the finished panel footer adds this secondary
//                button next to Done; content.js clears the stored job
//                and POSTs /start again with force so the normal poll +
//                auto-open panel flow runs a fresh job.
//   postCommentLabel optional; for readOnly features whose companion
//                implements postComment (POST .../post-comment). When
//                set and the finished job has analysis content, the
//                footer adds this button (e.g. "Add as comment") next
//                to Done / Re-analyze. Clicking it opens an edit-before-
//                post compose view (Markdown draft from job.data.commentDraftMd);
//                Post comment sends { body } to the companion. Success/
//                error feedback shows in the panel footer — the job stays open.
//   renderPanel(job) required only for a feature that goes through the
//                review-panel flow (content.js's showPanel — a job that
//                gets tracked/polled and reaches "awaiting-approval" or
//                "failed", e.g. resolve-conflict). Returns either a DOM
//                node for the panel body, or { node, getApprovalPayload() }
//                when Approve needs to send something other than an empty
//                body (e.g. in-panel edits made before committing) —
//                content.js calls getApprovalPayload() (if present) when
//                Approve is clicked and POSTs its return value as the
//                approve request body.
//   footerActions  optional [{id, label, hint?, when(job, env) -> boolean, run(job, api)
//                -> Promise<job|void>}]. when is called as when(job, env); env is { enabledFeatureIds } (a Set
//                of the service's enabled feature ids, or null before the
//                first answer — null means unknown, so a when() that needs a
//                feature on treats it as on and lets the action report an error). content.js's showPanel renders one
//                button per entry whose when(job, env) is true, in *both* footer
//                shapes above (readOnly's Done/Re-analyze/Add-as-comment row,
//                and the Approve/Discard row — including a "failed" job,
//                where Discard already shows but Approve doesn't). Clicking
//                calls run(job, api); api is { send(type, extra), action(name, body),
//                startFeature(featureId, payload), rerender(job), close(),
//                showError(msg) }: send() POSTs a background.js
//                message with featureId/jobId/pageUrl already filled in (extra
//                is merged over those, e.g. { body: {...} }); action() POSTs
//                this feature's named action on this job (Feature.actions) and
//                resolves with the service's answer; startFeature() starts
//                another feature's job on this page (its urlPattern must
//                match), stops tracking this one, and opens the new job's
//                panel when it is ready; rerender(job)
//                redraws the whole panel (body and footer) from a new job,
//                for an action whose result can change what the footer should
//                show next (e.g. resolve-conflict's Refresh diff, below);
//                close() closes the panel; showError(msg) shows msg in the
//                same error box Approve/Discard use. An action that lets its
//                promise reject gets a generic "<label> failed: <message>"
//                in that same box.

(function (root) {
  const SITES = ["git", "issues", "ci"];
  const entries = [];

  // Also read from an entry: `settingsGroups` (which Settings groups — "jira", "jenkins", "git" (the profile's git host) —
  // the feature needs, so the panel shows a group only while one of its features is ticked) and
  // `describePendingStart(payload)` (one summary line for a job Claude Code queued for this feature).

  /** Adds a feature's entry. Every feature file calls this once, and every one must
   * be loaded before content.js, which reads the list once at startup (the manifest
   * lists them in menu order — the order the ✨ menu shows its rows). `site`
   * ("git" | "issues" | "ci") says which kind of server the feature belongs to, so
   * content.js can skip it on a page of another configured server. */
  function register(entry) {
    if (!entry || typeof entry.id !== "string" || !entry.id) {
      throw new Error("PaiRegistry.register: an entry needs an id");
    }
    if (entries.some((e) => e.id === entry.id)) {
      throw new Error(`PaiRegistry.register: "${entry.id}" is already registered`);
    }
    if (entry.settingsGroups !== undefined && !(Array.isArray(entry.settingsGroups) && entry.settingsGroups.every((g) => typeof g === "string"))) {
      throw new Error(`PaiRegistry.register: "${entry.id}" has settingsGroups that aren't a list of names`);
    }
    if (entry.site !== undefined && !SITES.includes(entry.site)) {
      throw new Error(`PaiRegistry.register: "${entry.id}" has site "${entry.site}", expected one of ${SITES.join(", ")}`);
    }
    entries.push(entry);
    return entry;
  }

  const api = { register, all: () => entries.slice(), SITES };
  if (typeof module !== "undefined" && module.exports) module.exports = api;
  else root.PaiRegistry = api;
})(typeof self !== "undefined" ? self : this);
