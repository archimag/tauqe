# 6. Structured Code Review

The **Review** workspace (`Ctrl+3`) transforms code review from an informal chat conversation into a structured, verifiable engineering discipline.

---

## 6.1 The Core Dilemma: Generation Speed vs. Human Cognitive Bandwidth

Modern AI models can synthesize code orders of magnitude faster than a human engineer can realistically read, understand, and verify line-by-line. 

At that speed, requiring the developer to manually inspect every generated diff hunk ceases to be a meaningful safety mechanism. It turns into a cognitive bottleneck and induces **review fatigue**, where the human inevitably starts rubber-stamping changes without genuine comprehension.

Conversely, letting the model generate hundreds of lines unsupervised without independent verification leads to catastrophic architectural drift, subtle logic regressions, and security vulnerabilities.

### The Structured AI Code Review Paradigm
TAUQE resolves this dilemma by treating code review as an **independent, structured verification loop**:

```text
       AI Implementation (Develop)
                   ↓
       Independent AI Review (Review)
                   ↓
          Structured Findings
                   ↓
       Human Engineering Judgment (Triage / Discussion)
                   ↓
       Isolated Autonomous Execution (Fail-Fast Verification)
```

The fundamental philosophy is:
> **Not “AI writes code and nobody reads it”, but “AI writes code, an independent AI inspects it, and the human evaluates the evidence.”**

The developer does not need to read every repetitive line of generated boilerplate. Instead, their mental bandwidth is elevated to where it is most valuable: evaluating **engineering consequences** exposed by the audit — architectural trade-offs, correctness hazards, boundary edge-cases, missing tests, and security debt.

### The Two-Phase Lifecycle
Review is deliberately split into two independent phases that never share a model context:

1. **Analytical audit (Reviewer role, read-only).** An isolated reviewer inspects the selected files and emits structured findings. The audit cannot modify files: it runs in the dedicated **Reviewer** role and produces evidence, not code.
2. **Finding resolution (Coder role, isolated execution).** Each approved finding is resolved in its own bounded turn — either singly or as a fail-fast batch — with pre-flight build verification, the target file pre-loaded as editable, and intra-role escalation along the coder ladder.

The phases stay decoupled: the audit never reuses the Develop conversation, and the fix phase never escalates into the Architect role.

---

## 6.2 Architectural Rationale: Complete Isolation of Review

When you trigger a review in TAUQE, the review model does **not** receive the ongoing Develop conversation history, nor does it receive the Tree-sitter Repo Map. It is fed solely the pure content of the selected files.

### Why Review Must Be Isolated:
1. **Elimination of Confirmation Bias (Sycophancy):**
   If the review model shares conversation history with the development model, it inherits the generative narrative, rationalizations, and false assumptions established during code generation. It tends to agree with prior decisions ("looks good to me"). An isolated model approaches the files with zero preconceptions.
2. **Context Window Hygiene & Token Economics:**
   Omitting hundreds of conversational history turns and global repo maps keeps the prompt compact, fast, and cost-effective, allowing higher-capability audit models to concentrate their full attention budget on the code itself.
3. **Model Heterogeneity:**
   The audit runs in the dedicated **Reviewer** role, a read-only analytical mandate. TAUQE resolves it to the configured `reviewer` model unless a specific model is chosen for the run, so an analytical reasoning model can audit while a fast coding model drives Develop.

### Selecting the Audited Files: Target Paths without Context Mutation

By default a review audits the files already in context. The launch dialog also accepts an optional **target directory or glob pattern** (for example `src/**/*.rs`), carried on the request as `ReviewStartParams::path_pattern`.

The server resolves the pattern against the repository's tracked files and **merges** the matched paths with the files already in context, de-duplicated, for that run only. This deliberately leaves the **User Context untouched**: the system never clears or rewrites the developer's layer, so the audited file set can widen for a single review while the developer keeps exclusive control of what stays loaded. Because matched files are read directly from the working tree, a scoped review can cover code the developer never explicitly added.

---

## 6.3 Session Storage, UUIDv7 Identity, and the ReviewIssue Model

Every review session is a first-class, versioned engineering artifact that persists across workspace restarts. The domain deliberately models a finding as a `ReviewIssue` that separates the defect from its remediation, instead of burying both in one free-form body.

### Session Storage and UUIDv7 Identity

- **Directory structure.** Sessions are stored as independent JSON files in `.tauqe/reviews/{uuid7}.json`, one file per session. Monolithic single-file state dumps are prohibited so one session can never corrupt another.
- **Chronological identity (UUIDv7).** A session's `id` is a UUIDv7: the leading 48 bits encode the creation instant as a Unix millisecond timestamp, and the remaining bits carry cryptographic randomness. This makes identities **lexicographically time-ordered** and **collision-resistant**: a directory listing sorted by id reproduces creation order, and two sessions created in the same millisecond can never collide or race onto the same file.
- **Active session pointer.** `.tauqe/reviews/active` records the most recently inspected session, so a client reconnect restores the developer's last context. It is a UI pointer only: it is never consulted to steer a mutation, so review edits remain strictly scoped to the session the caller names.
- **Auto-migration.** A legacy single-file dump (`.tauqe/reviews/latest.json`) is migrated into per-session files exactly once at daemon startup, keeping ordinary reads free of write side effects.

### Session Metadata (`ReviewSession`)

- `id`: the session's UUIDv7 identity.
- `title`: a concise, human-readable session title synthesized by the reviewer model.
- `created_at`: the exact Unix instant (seconds) the session was produced.
- `model`: the `ModelRef` that actually performed the audit.
- `description`: the reviewer's high-level executive summary; falls back to the user prompt when the model produced none.
- `user_prompt`: the raw audit instructions and focus directives.
- `target_files`: the repository-relative paths that were audited.
- `items`: the discrete structured findings.
- `raw_markdown`: the untouched model payload, preserved for audit.

### Finding Fields (`ReviewIssue`)

- `id`: unique numeric identifier within the session.
- `title`: concise title of the finding.
- `model`: optional model selection or role override (`Option<ModelSelection>`).
- `type`: the classified problem category (`IssueType`).
- `status`: the finding lifecycle status (see 6.4).
- `description`: the technical defect — what is wrong, why it occurs, and its root cause.
- `recommendations`: an ordered list of concrete, actionable remediation steps.
- `files`: repository-relative paths to modify when resolving the finding.
- `readonly_files`: immutable contracts, interfaces, or invariants the fix must rely on but never modify.
- `plan`: an optional embedded decomposition plan (`Vec<PlanItem>`) produced by the Architect (see 6.9).
- `note`: optional resolution notes or skip rationale.

Legacy on-disk shapes stay readable: `severity` is accepted for `type`, `body` for `description`, `subplan` for `plan`, and `resolution_notes` for `note`, so sessions written before the rename load without manual editing.

### Problem Typing (`IssueType`)

Findings are classified into five actionable categories, replacing a shallow severity ladder with a category that drives triage:

- `bug`: incorrect behavior, panic, data corruption, or resource leak.
- `security`: a security flaw or unsafe boundary.
- `architecture`: a structural or runtime design risk that must be decomposed before implementation.
- `performance`: a costly hot path or wasted resources.
- `quality`: maintainability, readability, or dead-code cleanup.

The non-actionable `info` severity is never exposed to the reviewer. The model emits `quality`, which projects onto the closest domain variant and renders as `[QUAL]`, so the audit vocabulary stays aligned with the five categories an engineer can actually act on.

### Why Split the Defect from its Remediation

An undivided finding body conflates the diagnosis with the cure. When the description and the remediation share one field, a model can dilute a precise root cause with generic advice, and the human cannot tell whether the defect was truly understood. Splitting them enforces two independent statements — `description` states *what is wrong and why*, `recommendations` state *the ordered steps that fix it* — which gives the developer a clean acceptance criterion and the Coder a bounded mandate.

### Why `readonly_files`

Some defects can only be fixed safely against an immutable boundary: a trait definition, a public interface, a serialization format, or an architectural invariant. `readonly_files` names those protected artifacts explicitly, so a fix relies on their guarantees instead of silently changing a contract the rest of the system depends on. A protected contract is context, never a target.

---

## 6.4 Finding Lifecycle and the Refine Status

Each finding undergoes a formal lifecycle. Status is consciously chosen through the modal picker (`t` or `s`), and is strictly separated from execution (`x` / `Enter`):

```text
   [ DISCUSSION ] ──(approve via 't')──► [ TODO ]
         │                                  │
         ▼                                  ▼
   [ REJECTED ]                      [ IN_PROGRESS ]
   (won't fix /                           │
    false positive)              ┌────────┴────────┐
                                 ▼                 ▼
                             [ FIXED ]         [ SKIPPED ]
                            (verified)   (already resolved /
                                          not reproducible)

   Refinement loop (Architect owns What, Coder owns How):
   [ TODO ] ──('t')──► [ REFINE ] ──(Architect decomposition 'R')──► [ TODO ] + plan
```

1. **DISCUSSION:**
   Default initial status for newly parsed findings. Represents findings that require human triage, feasibility assessment, or architectural discussion. Findings in `DISCUSSION` **cannot** be executed autonomously until explicitly reviewed and approved.
2. **TODO:**
   Approved findings ready for autonomous resolution.
3. **IN_PROGRESS:**
   Actively being resolved by the model in an isolated turn.
4. **REFINE:**
   The finding is broader than a single bounded turn can resolve cleanly and requires architectural decomposition before any code is written. It is the direct expression of the harness's core rule: **the Architect owns *What*, the Coder owns *How*.** A `REFINE` finding is a *What* that has not yet been decomposed into executable *How* steps, so neither the single-finding executor nor the batch runner may start it. Pressing `R` dispatches the Architect to decompose it into an embedded plan (see 6.9); once the plan exists, the finding is moved to `TODO` and becomes executable by the Coder.
5. **FIXED:**
   Successfully resolved, staged in Git, and verified by deterministic toolchain gates (`cargo check`, linters, tests). A concise summary of the applied change is captured automatically in `note`, so the essence of the fix stays readable in the view without opening the Git diff.
6. **SKIPPED:**
   Bypassed during execution because the finding was already addressed by earlier changes in the batch, rendered obsolete by prior refactorings, or verified as a non-reproducible false positive during active inspection. The model's or developer's justification is retained in `note`.
7. **REJECTED:**
   Dismissed as false positive, intentional design trade-off, or non-actionable.

### Progress Counting

Session progress is `fixed / total`, and `total` counts only **actionable** findings. `SKIPPED` and `REJECTED` are terminal, non-actionable outcomes, so both are excluded from the denominator. This mirrors how a plan excludes `CANCELLED` leaves from its progress metric (`Plan::count_stats`). A session whose remaining findings are all resolved therefore reports `fixed == total` (a full progress bar) instead of a fraction that can never reach one; a session with no actionable findings reports `0/0`.

Press `t` or `s` on any finding to open the interactive status picker modal, supporting fast numeric selection (`1` Discussion, `2` Todo, `3` InProgress, `4` Refine, `5` Fixed, `6` Skipped, `7` Rejected).

---

## 6.5 Interactive Discussion (`d`) in Structured Output Mode

When evaluating complex findings, the developer does not have to accept or reject them blindly. Pressing `d` on a finding or session header opens the **Discussion Modal**:

- **Focused Scope:** Discussions target either a specific finding (`[rev-xxx] #id`) or the entire session scope.
- **Structured Output Protocol:** Discussions run in Develop using a dedicated structured prompt, so the model reasons about trade-offs through a typed schema rather than ad-hoc prose.
- **Focused Editing in the REPL:** Unlike a plan discussion, a focus on a finding keeps the file-edit capability. The developer keeps the full finding context and can ask the model to apply the fix directly: edits are staged, verified, and committed exactly as in an ordinary Develop turn. Discussion and resolution therefore share one anchored dialogue without a screen switch or a separate execution request.
- **Protected Contracts in the Prompt:** Every file listed in the finding's `readonly_files` is presented to the model as an immutable contract it must rely on but never modify.
- **Proposed Status Changes:** When a discussion suggests that a finding's status should change, the model explains the proposed modification in conversational text; the developer applies the change through the review interface. Status remains under explicit developer control, so a discussion never silently rewrites the persisted review state.
- **Quick Focus (`f`):** Pressing `f` on a finding or session binds the active discussion focus to it directly (`discussion/set_focus`) without leaving the Review view or typing a prompt; the focused row then shows a `[FOCUS]` badge (see 2.8.7).

---

## 6.6 Isolated Execution & Server-Side Resilient Batch Runner (`x` / `Enter` / `X`)

Findings approved as `TODO` can be executed directly from the Review workspace without conversational context pollution. The client sends one `review/execute` request with an optional `review_id` and optional `item_id`. When `review_id` is omitted, the server executes actionable findings across all review sessions sequentially. The server owns the queue and every status transition.

**Why the server owns the batch.** Marking a finding `FIXED` and advancing to the next finding must be one atomic decision tied to the verified result of the turn. If the TUI drove the queue, a disconnect would lose the batch and clients could disagree about the review state. The TUI is a thin view: it starts the scope and renders `review/updated`, `review/listChanged`, `turn/phase`, and `model/*` events, then receives a single typed `review/batch_finished` event carrying the aggregate status, finding counters, and elapsed time once the supervisor terminates.

**Refine Is Never Executed Autonomously.** A finding in `REFINE` status has not yet been decomposed into executable steps, so the server refuses to start it: an explicit request returns `ITEM_IN_REFINE`, and a batch never selects one because only `TODO` and `IN_PROGRESS` findings are actionable. This enforces the Architect/Coder boundary at the orchestration layer — code is never written against an undecomposed *What*.

**Mandatory Pre-Flight Verification.** Before executing any finding, a single one or a whole batch, the server runs the exact same toolchain pipeline (`VerifyTarget::All`) that the core runs after each fixing turn. If the repository does not build or its tests fail, execution is blocked synchronously before any LLM request and before any status change, at zero token cost. The server returns `PREFLIGHT_VERIFICATION_FAILED` with the full compiler or test output and leaves the finding statuses untouched. Repositories without a detected toolchain skip this gate transparently.

### 1. Single Finding Execution (`x` or `Enter` on a finding)
- The server validates that the finding is in `TODO` or `IN_PROGRESS` status. Findings in `DISCUSSION`, `REFINE`, `REJECTED`, `SKIPPED`, or already `FIXED` are refused.
- A confirmation popup displays finding details and associated target files.
- The server marks the finding `IN_PROGRESS`, pre-loads all files in `item.files` as editable into the `auto` context layer, and formats an isolated execution prompt containing the finding body, target paths, and directives.
- The model requests additional context via `<context_request>` if needed, edits are validated in staging, and toolchain gates verify the build.
- **Completion Contract:** The finding transitions to `FIXED` only if the turn succeeds **and** actually applies verified code changes to disk. The turn's edit summary is written to `note` when the finding has none, so the essence of the fix is preserved alongside the status.
- **Managed Skip (`<skip>`):** If the model determines that the finding was already addressed by preceding edits or is non-reproducible, it emits a typed protocol outcome tag: `<skip reason="Concise reason">Detailed explanation</skip>` (or self-closing `<skip reason="..." />`). The server transitions the finding to `SKIPPED`, persists the explanation to `item.note`, and concludes the turn cleanly without requiring code modifications.

### 2. Batch Execution (`x` on a session header)
- The server evaluates the whole session: if any finding remains in `DISCUSSION`, the batch is refused to enforce human triage. `REFINE` findings are not actionable and are simply excluded from the queue.
- All findings in `TODO` or `IN_PROGRESS` form an ordered queue. A confirmation dialog shows the queue before it starts.
- **Sequential Execution and Uninterrupted Skips:** When a finding emits `<skip>`, the server records its `note`, transitions it to `SKIPPED`, and immediately proceeds to the next finding in the queue without interrupting the batch.
- **Failure Classification: Domain Skip versus Infrastructure Abort:** Findings execute sequentially, and a failure no longer stops the queue unconditionally. The server classifies the cause and reacts to each category differently:
  - **Infrastructure failure — abort the batch.** A provider or network error (HTTP 401/429/500/502/504), a request timeout, a missing API key, or an explicit user cancellation (`Esc` / `Ctrl+C`). The fault is external to the finding, so any further step would hit the same wall; the server stops the batch immediately and reports the reason.
  - **Domain failure — skip and continue.** A problem attributable to this finding: unhealed compiler errors, a patch that cannot be applied, exhausted retries, a turn that produces no applied code changes without emitting `<skip>`, or a semantic `<blocked>` outcome from the model. The server rolls the failed turn's file changes back through the Git workflow checkpoint, transitions the finding to `SKIPPED`, records the diagnostic reason (the toolchain error or block explanation) in `note`, and moves on to the next finding in the queue without interruption.
  - Findings already `FIXED` or `SKIPPED` earlier in the batch remain recorded; they are not rolled back. Because a domain failure resolves to `SKIPPED` rather than a stopped queue, one hard finding no longer blocks the remaining independent findings — the developer inspects the skip rationale in the view and decides how to proceed.

### 3. Cross-Session Batch Execution (`X` / `Shift+X`)
Pressing `X` (`Shift+X`) anywhere in the Review workspace initiates sequential batch execution across **all** review sessions:
- **Human-in-the-Loop Precondition:** The server inspects all sessions for actionable items. If all findings across all sessions remain in `DISCUSSION`, execution is refused with an explicit guidance prompt to triage and approve items (`t`) first.
- **Aggregated Confirmation:** When actionable findings exist, an aggregated confirmation dialog shows the total number of findings and participating sessions before starting.
- **Ordered Sequential Execution:** Actionable findings across sessions are queued in `{session_id}:{item_id}` order. The server switches the active session pointer (`.tauqe/reviews/active`) as it moves across sessions, pre-loads target files, runs toolchain gates, applies the same domain-skip and infrastructure-abort classification, and terminates with a single aggregated `review/batch_finished` event (`owner_id: "all"`).

### 4. Multi-File Findings and Elimination of Line Number Drift
Earlier review systems bound each finding to a single `file_path` and a fragile `line_range`. In practice, line numbers drift rapidly across consecutive edits in a batch, inducing model hallucinations and patch application failures. Moreover, architectural and interface bugs frequently span multiple files (for example, a trait definition, concrete implementations, and caller sites).

TAUQE addresses this through two structural invariants:
1. **Multi-File Path Association (`files: Vec<String>`):** Findings declare a clean list of related file paths without line coordinates, harmonizing with `PlanItem.files`. Legacy review files containing `file_path` are automatically migrated on read to `files`.
2. **Batch Context Preloading:** Before invoking the coder model for a step, the server preloads **all** paths in `item.files` into the `auto` context layer as editable. The model receives all relevant files immediately on turn 0, eliminating discovery latency.

---

## 6.7 Architectural Triage via Structured Discussion (`d`)

In accordance with TAUQE's dual-contour architecture, review findings are not dumped indiscriminately into the general Develop conversation context via checkboxes:
- **Zero Prompt Pollution:** General Develop turns remain clean and focused on user-specified objectives without carrying stale review findings in system prompt headers.
- **Dedicated Discussion Contour (`d`):** Whenever an architectural finding needs triage, feasibility inquiry, or refinement, pressing `d` launches a focused Structured Discussion targeting specifically the chosen finding or entire audit session.
- **Developer-Applied Status Changes:** During discussion, the developer decides whether a finding advances to `TODO` or is dismissed as `REJECTED`; the model explains proposed status changes in its response, and the developer applies them through the interface.
- **Focused Execution (`x` / `Enter`):** Approved findings are executed in isolated turns with deterministic toolchain verification.
- **Architect Decomposition (`R`):** A finding broader than one bounded turn is routed to `REFINE` and decomposed into an embedded plan via the Architect role (see sections 6.4 and 6.9).
- **View Filtering (`F`):** Press `F` (Shift+F) in Review to filter out closed findings (`FIXED`, `SKIPPED`, and `REJECTED`), keeping the view focused strictly on outstanding engineering debt.

---

## 6.8 Finding Model Assignment and Coder Ladder Escalation

A finding can carry a model assignment: a semantic role (`scribe`, `coder`, `architect`, `primary`, `reviewer`) or an explicit model reference. It is shown as a badge next to the status. Simple findings can then run on a cheap model, while hard ones use a stronger one. A finding without an assignment runs on the **Coder** role, never on whatever model happens to be selected in Develop.

Fixing a finding is intra-role escalation only. When `auto_level_up = true` in the `[models]` section of `tauqe.toml`, a finding that fails verification does not stop the queue immediately. TAUQE first rolls the working tree back to the pre-step checkpoint, then restarts the finding on the next rung of the coder ladder (`CoderTier`, each rung pairing a model with a reasoning effort). Files already found during discovery stay in context. Only when the ladder is exhausted does the domain-failure policy from section 6.6 apply: the tree stays rolled back, the finding transitions to `SKIPPED` with the escalation diagnostic recorded in `note`, and the batch continues with the next finding.

Escalation never crosses the role boundary: a coder failure is answered by a stronger coder, never by promoting the turn into the Architect role. The audit model is chosen per run in the launch dialog; escalation affects only fix execution, never the audit itself.

---

## 6.9 Embedded Subplans: Architect Decomposition and Safe Partial Execution

A finding can be broader than one bounded turn can resolve cleanly. Pressing `R` on a selected finding dispatches it to the **Architect** role (`review/refineItem`), which decomposes it into an **embedded plan** stored directly on `ReviewIssue::plan`. Decomposition is the concrete mechanism behind the `REFINE` status: it turns an undecomposed *What* into executable *How* steps.

### Why Reuse the Plan Engine

The embedded plan reuses the plan domain verbatim: each sub-step is a `PlanItem` with the same status vocabulary, dependency evaluation, and leaf-file normalization as a full implementation plan. Finding decomposition and plan refinement are conceptually the same operation — both call the Architect — so they share the horizon-synthesis logic. Reusing the engine keeps a finding's sub-step progress consistent with a plan's progress and avoids a parallel scheduling implementation.

### Architect Decomposition (`R`)

- The Architect inspects the finding, its body, and its target files, and may request additional context across bounded discovery rounds, exactly as other Architect operations do.
- A schema-enforced structured response either yields a decomposed plan or reports a semantic `<blocked>` outcome.
- The synthesized horizon is merged with the finding's existing **Done foundation**: every sub-step already marked `Done` is preserved verbatim, colliding identifiers are dropped deterministically, and the merged tree is validated before it reaches disk. A plan that would introduce a dependency cycle is rejected.
- Decomposition alone writes no code; it produces the plan that makes the finding executable. The finding is then moved to `TODO` (see 6.4) and executed by the Coder.
- **Recorded Architect Provenance and Replan Escalation.** Each decomposition records the Architect configuration that produced the subplan on the finding (`subplan_architect`: model plus optional reasoning depth) and renders it as a compact `[ARC model effort]` badge next to the title. When a sub-step later fails with a domain failure, the replanning round reads that record and asks the `architect` ladder for the next rung, so the reprojected horizon is synthesized by a stronger Architect than the one whose horizon already failed. The new rung is recorded in turn, escalation saturates on the last rung, and a configuration absent from `tauqe.toml` restarts from the base rung instead of guessing a position.

### Architect Context Discovery (Shared with Plan Refinement)

Finding decomposition reuses the plan engine's Architect context mechanics, so a finding is never decomposed blind:

- **Deterministic Target Context Preloading:** The finding's target files (`item.files`) are loaded into the read-only auto context layer *before* the first generation round. The Architect opens on the real code of the finding rather than an empty context, and these preloaded files bypass the files token budget.
- **Architect Discovery Protocol (Contracts over Contours):** The Repo Map is blinkers, not a contract. Whenever a sub-step must interact with an existing struct, enum, trait, function, module, or external crate, the Architect must request the concrete source files via structured `needs_context` and return no steps for that round; a contract guessed from a contour line is a hard contract violation.
- **Robust Discovery Loop (`already_satisfied` feedback):** The discovery loop stops only when a round adds nothing new, reports nothing missing, and leaves no requested file already satisfied. A round that merely re-requests files already in context is not a dead end: the model still receives `<context_feedback>` and another analysis round, so re-listing an already-loaded file cannot exhaust the discovery budget and abort the decomposition with a spurious error.

### Execution and Safe Partial Progress

When a finding carries a plan, its execution expands each **leaf** sub-step into its own bounded turn — files preloaded as editable, with the same completion contract and coder-ladder escalation as a single finding. A sub-step that applies cleanly is persisted as `Done` immediately, so finished work survives any later interruption.

If a sub-step fails, blocks, or the turn is cancelled:

- Completed sub-steps **stay `Done`**; the working tree keeps every verified change, so the codebase remains functional.
- The stop reason is recorded in `note`.
- The finding transitions to `SKIPPED`, letting the developer inspect the cause and decide how to proceed without discarding the finished sub-steps.

This mirrors the safety principle the plan runner applies to a completed horizon: finished work is immutable, and a partial advance is recorded rather than thrown away.
