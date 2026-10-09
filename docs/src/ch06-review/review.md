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

---

## 6.2 Architectural Rationale: Complete Isolation of Review

When you trigger a review in TAUQE, the review model does **not** receive the ongoing Develop conversation history, nor does it receive the Tree-sitter Repo Map. It is fed solely the pure content of the selected files.

### Why Review Must Be Isolated:
1. **Elimination of Confirmation Bias (Sycophancy):**
   If the review model shares conversation history with the development model, it inherits the generative narrative, rationalizations, and false assumptions established during code generation. It tends to agree with prior decisions ("looks good to me"). An isolated model approaches the files with zero preconceptions.
2. **Context Window Hygiene & Token Economics:**
   Omitting hundreds of conversational history turns and global repo maps keeps the prompt compact, fast, and cost-effective, allowing higher-capability audit models to concentrate their full attention budget on the code itself.
3. **Model Heterogeneity:**
   TAUQE allows selecting a completely different model for review (e.g. using an analytical reasoning model for auditing while using a fast coding model for Develop).

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

## 6.6 Isolated Execution & Fail-Fast Batch Runner (`e` / `Enter`)

Findings approved as `TODO` can be executed directly from the Review workspace without conversational context pollution:

### 1. Single Finding Execution (`e` or `Enter` on a finding)
- Validates that the finding is in `TODO` status (blocking execution if still in `DISCUSSION` or `REJECTED`).
- Opens an execution confirmation popup with finding details and file location.
- Formats an isolated model prompt containing only the target finding, file location, and directives.
- Model requests necessary context via `<context_request>`, stages patches in memory, and validates with toolchain gates.
- Upon passing all gates, the finding status is marked `FIXED`.

### 2. Batch Execution Queue (`e` on session header)
- Evaluates the entire review session: if any items remain in `DISCUSSION`, batch execution is blocked to enforce human triage.
- Sequences all remaining `TODO` findings into a deterministic batch execution queue.
- Opens a confirmation dialog detailing the queue order and findings to resolve.
- **Fail-Fast Policy:** Findings execute sequentially. If any step fails toolchain verification, encounters compiler errors, or is interrupted by the user (`Esc` / `Ctrl+C`):
  - Execution stops immediately.
  - The working tree is atomically rolled back to the pre-edit checkpoint (`git/undo`).
  - The offending item is reverted to `TODO`.
  - Subsequent items in the queue are aborted, preventing compounding errors.

---

## 6.7 Architectural Triage via Structured Discussion (`d`)

In accordance with TAUQE's dual-contour architecture, review findings are not dumped indiscriminately into the general Develop conversation context via checkboxes:
- **Zero Prompt Pollution:** General Develop turns remain clean and focused on user-specified objectives without carrying stale review findings in system prompt headers.
- **Dedicated Discussion Contour (`d`):** Whenever an architectural finding needs triage, feasibility inquiry, or refinement, pressing `d` launches a focused Structured Discussion targeting specifically the chosen finding or entire audit session.
- **Direct Status Approval:** During discussion, findings can be approved (`TODO`) or dismissed (`REJECTED`) programmatically via structured `review_update` responses.
- **Focused Execution (`e`):** Approved findings are executed in isolated turns with deterministic toolchain verification.
- **View Filtering (`f`):** Press `f` in Review to filter out closed findings (`FIXED` and `REJECTED`), keeping the view focused strictly on outstanding engineering debt.
