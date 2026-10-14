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

---

## 6.3 Multi-Session Storage & Review Formats

All review sessions in TAUQE are first-class versioned engineering artifacts persisted across workspace restarts:
- **Directory Structure:** Saved in `.tauqe/reviews/` in JSON format (`rev-<timestamp>.json`).
- **Active Session Pointer:** `.tauqe/reviews/.active_review` tracks the latest inspected session across client reconnects.
- **Session Metadata:** Each session stores:
  - `id`: unique session identifier (`rev-<timestamp>`);
  - `title`: title derived from the review prompt or timestamp;
  - `created_at`: Unix timestamp of creation;
  - `model`: model reference utilized for auditing;
  - `description` / `user_prompt`: optional audit instructions and focus directives;
  - `target_files`: list of audited repository file paths;
  - `items`: collection of discrete structured review findings.

---

## 6.4 Finding Lifecycle and Status Progression

Each finding in a review session undergoes a formal lifecycle:

```text
   [ DISCUSSION ] ──(approve via 't' or 'd')──► [ TODO ]
         │                                         │
         ▼                                         ▼
   [ REJECTED ]                             [ IN_PROGRESS ]
   (won't fix /                             (under active execution)
    false positive)                                │
                                                   ▼
                                              [ FIXED ]
                                            (compiler verified)
```

1. **DISCUSSION:**
   Default initial status for newly parsed findings. Represents findings that require human triage, feasibility assessment, or architectural discussion. Findings in `DISCUSSION` **cannot** be executed autonomously until explicitly reviewed and approved.
2. **TODO:**
   Approved findings ready for autonomous resolution.
3. **IN_PROGRESS:**
   Actively being resolved by the model in an isolated turn.
4. **FIXED:**
   Successfully resolved, staged in Git, and verified by deterministic toolchain gates (`cargo check`, linters, tests).
5. **REJECTED:**
   Dismissed as false positive, intentional design trade-off, or non-actionable.

Press `t` or `s` on any finding to open the interactive status picker modal, supporting fast numeric selection (`1` Discussion, `2` Todo, `3` InProgress, `4` Fixed, `5` Rejected).

---

## 6.5 Interactive Discussion (`d`) in Structured Output Mode

When evaluating complex findings, the developer does not have to accept or reject them blindly. Pressing `d` on a finding or session header opens the **Discussion Modal**:

- **Focused Scope:** Discussions target either a specific finding (`[rev-xxx] #id`) or the entire session scope.
- **Structured Output Protocol:** Discussions run in Develop using a dedicated structured prompt. The model discusses trade-offs conceptually without generating unstructured code edits.
- **Automatic Status Synchronization:** If the developer approves or rejects items during discussion, the model emits structured `review_update` objects that update finding statuses directly in `.tauqe/reviews/` without manual editing.

---

## 6.6 Isolated Execution & Server-Side Fail-Fast Batch Runner (`x` / `Enter`)

Findings approved as `TODO` can be executed directly from the Review workspace without conversational context pollution. The client sends one `review/execute` request with `review_id` and an optional `item_id`. The server owns the queue and every status transition.

**Why the server owns the batch.** Marking a finding `FIXED` and advancing to the next finding must be one atomic decision tied to the verified result of the turn. If the TUI drove the queue, a disconnect would lose the batch and clients could disagree about the review state. The TUI is a thin view: it starts the scope and renders `review/updated`, `review/listChanged`, `turn/phase`, and `model/*` events, then receives a single typed `review/batch_finished` event carrying the aggregate status, finding counters, and elapsed time once the supervisor terminates.

**Mandatory Pre-Flight Verification.** Before executing any finding, a single one or a whole batch, the server runs the exact same toolchain pipeline (`VerifyTarget::All`) that the core runs after each fixing turn. If the repository does not build or its tests fail, execution is blocked synchronously before any LLM request and before any status change, at zero token cost. The server returns `PREFLIGHT_VERIFICATION_FAILED` with the full compiler or test output and leaves the finding statuses untouched. Repositories without a detected toolchain skip this gate transparently.

### 1. Single Finding Execution (`x` or `Enter` on a finding)
- The server validates that the finding is in `TODO` or `IN_PROGRESS` status. Findings in `DISCUSSION`, `REJECTED`, or already `FIXED` are refused.
- A confirmation popup shows the finding details and file location.
- The server marks the finding `IN_PROGRESS`, pre-loads its file as editable, and formats an isolated prompt containing only the target finding, its location, and directives.
- The model requests necessary context via `<context_request>`, edits are staged in memory, and toolchain gates validate them.
- The finding becomes `FIXED` only if the turn is successful **and** actually applied verified code changes to disk. Explanatory text alone, even with a terminal success tag, does not close a finding: resolving a finding is a code-modifying contract, not a discussion.

### 2. Batch Execution (`x` on a session header)
- The server evaluates the whole session: if any finding remains in `DISCUSSION`, the batch is refused to enforce human triage.
- All findings in `TODO` or `IN_PROGRESS` form an ordered queue. A confirmation dialog shows the queue before it starts.
- **Server-Side Fail-Fast Policy:** Findings execute sequentially. If a finding fails toolchain verification, hits compiler errors, ends with a terminal error tag, produces no applied code change, or is interrupted by the user (`Esc` / `Ctrl+C`):
  - The server stops the batch immediately and reports the reason.
  - The failed finding returns to `TODO`. File changes of the failed turn are undone by the Git workflow checkpoint.
  - Findings fixed earlier in the batch stay `FIXED`; they are not rolled back.
  - Subsequent findings are not started, which prevents compounding errors.

---

## 6.7 Architectural Triage via Structured Discussion (`d`)

In accordance with TAUQE's dual-contour architecture, review findings are not dumped indiscriminately into the general Develop conversation context via checkboxes:
- **Zero Prompt Pollution:** General Develop turns remain clean and focused on user-specified objectives without carrying stale review findings in system prompt headers.
- **Dedicated Discussion Contour (`d`):** Whenever an architectural finding needs triage, feasibility inquiry, or refinement, pressing `d` launches a focused Structured Discussion targeting specifically the chosen finding or entire audit session.
- **Direct Status Approval:** During discussion, findings can be approved (`TODO`) or dismissed (`REJECTED`) programmatically via structured `review_update` responses.
- **Focused Execution (`x` / `Enter`):** Approved findings are executed in isolated turns with deterministic toolchain verification.
- **View Filtering (`f`):** Press `f` in Review to filter out closed findings (`FIXED` and `REJECTED`), keeping the view focused strictly on outstanding engineering debt.

---

## 6.8 Finding Model Assignment and Coder Ladder Escalation

A finding can carry a model assignment: a semantic role (`scribe`, `coder`, `architect`, `primary`, `reviewer`) or an explicit model reference. It is shown as a badge next to the status. Simple findings can then run on a cheap model, while hard ones use a stronger one. A finding without an assignment runs on the **Coder** role, never on whatever model happens to be selected in Develop.

Fixing a finding is intra-role escalation only. When `auto_level_up = true` in the `[models]` section of `tauqe.toml`, a finding that fails verification does not stop the queue immediately. TAUQE first rolls the working tree back to the pre-step checkpoint, then restarts the finding on the next rung of the coder ladder (`CoderTier`, each rung pairing a model with a reasoning effort). Files already found during discovery stay in context. Only when the ladder is exhausted does the Fail-Fast policy from section 6.6 apply: the tree stays rolled back, the finding returns to `TODO`, and the batch stops.

Escalation never crosses the role boundary: a coder failure is answered by a stronger coder, never by promoting the turn into the Architect role. The audit model is chosen per run in the launch dialog; escalation affects only fix execution, never the audit itself.
