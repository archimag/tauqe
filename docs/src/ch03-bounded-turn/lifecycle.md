# 3. The Autonomous Bounded Turn

Instead of an unbounded background agent loop, TAUQE operates as a **finite-state turn machine**.

---

## 3.1 The Turn Lifecycle

```text
               User Prompt
                    ↓
        ┌─► [Discovery Phase] ──── (requests missing files via <context_request>
        │           ↓               or system docs via <doc_request>)
        │   [Proposal Generation] ─ (generates minimal coherent patches)
        │           ↓
        │   [In-Memory Staging] ── (validates patches in memory with fuzzy matching)
        │       ├── Match Error → [Patch Retry Loop] (up to N retries)
        │       └── Success → Apply to disk
        │           ↓
        │   [Toolchain Verification] ── (runs cargo check / npm test / tsc)
        │       ├── Compiler Errors → [Verification Healing Loop] ────┐
        │       │                                                     │
        │       └── Clean Build                                       │
        │           ↓                                                 │
        └────── Rollback on Failure / Commit to Git ◄─────────────────┘
```

---

- **Discovery & Exploration Phase:** The model inspects the task and navigates the Tree-sitter repository map to locate missing symbols and type signatures, or requests internal system documentation via `<doc_request topic="...">`. If required files are missing from context, the system autonomously fetches them into the `auto` layer and proceeds to the next round. Throughout multi-round discovery, the model retains intermediate findings and working hypotheses in the ephemeral Turn Scratchpad. Code generation is deferred until the model has sufficient clarity to avoid blind speculation.

The five phases of the turn are surfaced to the interface as `TurnPhase` stream events (`Discovery`, `Proposal`, `Staging`, `Verification`, and `Healing`), so the footer pipeline can render which stage is currently active. `Discovery`, `Staging`, and `Healing` carry a `round` counter and a `max_rounds` bound because they may repeat, whereas `Proposal` is emitted for every model interaction and `Verification` runs once per applied proposal.

---

## 3.2 In-Memory Staging & Patch Retry

Modifications never touch disk unverified:
- **Virtual Staging:** Search/replace hunks, new files, renames, and deletions are staged in memory against the current file tree.
- **Fuzzy Indentation Resilience:** Indentation shifts, tab vs. space variances, and trailing whitespace discrepancies are resolved deterministically without human intervention.
- **Patch Retry Loop:** From time to time, models generate diffs that cannot be reliably anchored to existing source files due to stale context or ambiguous surrounding lines. When a match error occurs, TAUQE does not abandon the turn or corrupt the file system:
  1. Already validated and staged files are preserved in memory.
  2. The failing file edits and precise matching errors are formatted into an error report.
  3. The model is called again in a dedicated retry round to re-emit only the failing hunks, repeating until all files stage cleanly or the retry limit is exhausted.

---

## 3.3 Verification & Self-Healing

- Once staging converges, edits are atomically applied to disk.
- TAUQE executes project-specific toolchain checks (`cargo check`, `npm test`).
- If compilation fails, compiler diagnostics are parsed and returned to the model for an immediate **Self-Healing Loop**, fixing syntax and type errors automatically before concluding the turn.

### Verification Healing and Coder Ladder Escalation

When verification errors occur, the model enters a targeted healing cycle:
1. Compiler error messages and diagnostic line spans are parsed and formatted into a targeted healing prompt.
2. Missing error paths are automatically staged into the context working set.
3. The prompt is presented to the active role (typically `Coder`).
4. **Escalation along the Coder Ladder:** When a bounded turn concludes with an unresolved failure — an error result from staging or verification, or a proposal that yielded no valid edits — superficial syntactic adjustments are often insufficient because the failure stems from a deeper contradiction or a missing dependency. When `auto_level_up` is enabled in configuration, the harness advances the turn to the next rung of the configured **Coder Ladder** instead of leaving the `Coder` role. Escalation is strictly intra-role, so the `What` versus `How` separation between `Coder` and `Architect` is never violated.

The Coder Ladder is an ordered list of rungs, each pairing a model with an optional reasoning depth. Escalation advances one rung at a time and treats a rung as an advance whenever either the model or the reasoning depth changes, so recovery can first deepen reasoning on the same model and only then switch to a stronger or more specialized one. Before advancing, the failed turn's transaction is rolled back and the transient `auto` context is pruned, so every rung starts from a clean pre-turn state. The ladder saturates on its final rung, so escalation always terminates deterministically. A terminal `<blocked>` report short-circuits escalation entirely: no rung is consumed and the failure is reported instead of retried.

The ladder is configured polymorphically under `[models].coder` in `tauqe.toml`: a bare model string or a `{ model, effort }` table yields a one-rung ladder, while an ordered array of such entries expresses an explicit multi-rung ladder. Plan step reprompting uses the exact same ladder, as described in §7.3.3.

---

## 3.4 Turn Scratchpad: Ephemeral Working Memory

During multi-round discovery and verification healing cycles, maintaining cognitive focus and retaining intermediate findings is essential. Without a dedicated mechanism, a model can lose track of what has already been checked or why specific files were requested.

However, recording intermediate reasoning or scratch notes into persistent session history would pollute context, cause token bloat, and introduce noise into subsequent turns. TAUQE resolves this with the **Turn Scratchpad**:

- **Turn-Scoped Ephemerality (Why & How):** The scratchpad exists strictly within the ephemeral lifecycle of the active bounded turn. It lives in memory across discovery rounds and compiler healing retries, but is completely discarded when the turn ends. It is never written to session history (`.tauqe/history.jsonl`), never recorded in Git commits, and never displayed in the chat view.
- **Protocol Contract (`<scratchpad>`):** The model emits working hypotheses, verified facts, and exploration goals inside `<scratchpad>...</scratchpad>` tags (or `<scratchpad note="..." />`). Turn-marker normalization protects it against collisions with literal source code, while `clean_assistant_text` strips it completely from final user-facing responses.
- **Round-to-Round Injection (`<turn_scratchpad>`):** When the model transitions to the next discovery round or a verification healing step, `PromptAssembly` injects the accumulated scratchpad into the system prompt under `<turn_scratchpad>`. The model instantly regains its working context without re-reading past tool outputs.
- **Strict Budgeting & Anti-Bloat:** To prevent generative bloat, the scratchpad is capped at `MAX_SCRATCHPAD_CHARS` (4,000 characters) with UTF-8 safe boundary truncation. The prompt instructions guide the model to maintain dense, concise bullet points rather than runaway prose.

---

## 3.5 Unified Terminal Outcome Protocol (`<success>` / `<error>` / `<blocked>`)

To eliminate ambiguity at turn conclusion and prevent speculative execution cascades, every code-modifying turn and isolated task step adheres to a strict, three-way terminal outcome contract:

- **Protocol Rationale (Why & How):** Free-form conversational text is nondeterministic and fragile for automated task runners. When executing isolated plan steps or triaging review findings in batch queues, the system requires an unambiguous, machine-readable signal indicating whether the task was fully accomplished or blocked.
- **Terminal Outcome Tags:**
  - `<success summary="...">` (or `<success>...</success>`): Signifies that the target goal was fully implemented, staged, and verified by compiler checks. In batch execution (e.g. executing all steps of an implementation plan or resolving code review findings), `<success>` signals the runner to transition the item to `Done`/`Fixed` and advance to the next queued item.
  - `<error message="...">` (or `<error>...</error>`): Signifies that the model encountered an insurmountable architectural contradiction, missing prerequisite, or unresolvable toolchain failure. In batch pipelines, `<error>` immediately halts execution to prevent cascading corruption, marks the current item as failed, and reports the structured error message to the developer.
  - `<blocked reason="..." details="...">` (or `<blocked reason="...">...</blocked>`): Signifies that the model has determined the current step's preconditions or assumptions cannot be satisfied as stated. A block is a deliberate semantic assertion, not a transient failure, so the harness treats it as **semantic recovery** instead of a blind retry: the turn transaction is rolled back, no patch retry or verification-healing cycle runs, and the plan supervisor moves the step to `Blocked` so the remaining subtree can be re-planned locally (`plan/replan`) rather than repeating failing turns or restarting the entire plan.
- **Turn-Marker Protection:** Like all operational tags, `<success>`, `<error>`, and `<blocked>` are suffixed with the active turn marker (e.g., `<success_MARKER>`), preventing accidental invocation from literal source code, git diffs, or markdown examples.

---

## 3.6 Dual Protocol Circuits: Execution vs Discussion

TAUQE separates model interaction into two dedicated protocol circuits based on functional requirements:

1. **Execution Protocol (Streaming XML):**
   - Active during code modification and isolated plan step execution.
   - Employs streaming XML search/replace blocks (`<edit>`, `<create>`, `<delete>`).
   - Resistant to unescaped quotes, arbitrary source formatting, and indentation shifts.
   - Patches are validated in virtual staging before touching disk, followed by the toolchain compiler gate.

2. **Discussion Protocol (Strict Structured Output):**
   - Active during architectural discussions, plan refinement, and code review triage.
   - Strictly read-only: file edits, patch proposals, and code modification tags are prohibited.
   - Governed by JSON Schema (`DiscussionResponse`):
     - `message`: Markdown explanation streamed immediately to the TUI;
     - `plan_update`: Structured modifications to project plans (`save`, `update`, `delete`);
     - `context_requests`: Read-only file inspection requests.
   - Eliminates turn-marker leaking, regex parsing ambiguities, and history pollution.

---

## 3.7 Core Pipeline Architecture

The bounded turn is deliberately not implemented as one monolithic routine. The pipeline is decomposed into five orthogonal layers, each owning exactly one reason to change. This separation is what makes the harness testable without an LLM, keeps the Git transaction model decoupled from the healing loop, and lets new protocols be added without touching FSM control flow.

```text
        ┌────────────────── ExecutionMode ──────────────────┐
        │  SingleExecution         BatchExecution           │
        │  turn lifecycle          queue supervision         │
        │  role escalation         status sync               │
        │  cost accounting         aggregate cost            │
        └───────────────────────────┬───────────────────────┘
                                    ↓
        ┌───────────────────── Task / Loop ─────────────────┐
        │  AnswerEditLoop  EditLoop                         │
        │  StructuredTask  StructuredStreamingTask          │
        │  TurnCycle: Discovery → Staging → Verify → Healing│
        └───────────────────────────┬───────────────────────┘
                                    ↓
        ┌──────────────────── PromptProtocol ───────────────┐
        │  XmlPromptProtocol   JsonPromptProtocol           │
        │  prompt assembly, capability declaration,         │
        │  output decoding into normalized LlmCommand       │
        └───────────────────────────┬───────────────────────┘
                                    ↓
        ┌────────────────────── ApplyPolicy ────────────────┐
        │  DirectApplyPolicy    GitTransactionPolicy        │
        │  staging, temporary commit, finalize, rollback    │
        └───────────────────────────┬───────────────────────┘
                                    ↓
        ┌────────────────────── LlmProvider ────────────────┐
        │  OpenRouterProvider   MockLlmProvider             │
        │  network transport    deterministic scripts       │
        └───────────────────────────────────────────────────┘
```

### ExecutionMode: Task Lifecycle and Cost Accounting

`ExecutionMode` (`workflow/execution.rs`) owns the lifecycle of one or more turns and is the seam between "run one objective" and "run a queue of objectives".

- **`SingleExecution`** drives one complete FSM cycle. It is responsible for Coder Ladder escalation — when `auto_level_up` is enabled and healing stalls, the turn is advanced to the next ladder rung within the `Coder` role (see §3.3) — and for cost accounting. The session cost observed before the turn is recorded as *previous*, while the cost accumulated during the turn is recorded as *current*, so the interface can attribute spend to a single turn rather than a blurred running total.
- **`BatchExecution`** (`workflow/batch.rs`) is a supervisor over a queue of independent steps (implementation plan steps or code review findings). It synchronizes entity statuses (`InProgress`, `Done`, `Todo`), stops the queue on the first failure or cancellation to avoid cascading corruption, and accumulates token and cost usage across the whole chain via `BatchUsage`.
- The supervisor stops the queue in two distinct ways. A **hard cancellation** aborts the current member immediately and terminates the run. A **soft stop** is observed strictly between members: the running member finishes its bounded turn, compiler verification, and Git commit, and only then does the queue halt before launching the next member. A soft stop never interrupts a step mid-turn, so the work already committed by the finished member is preserved. When both signals are pending before the same member, hard cancellation takes priority, so an explicit reset is never downgraded to a graceful drain. The two modes surface as distinct terminal statuses on the batch completion event (`Stopped` versus `Cancelled`).

Both modes are parameterized by `WorkflowOptions`, which carries retry limits, discovery budgets, history budget, and model configuration. This keeps execution strategy independent of the concrete task being executed.

### Task and Loop: FSM Cycles

`Task` (`workflow/task.rs`) is the minimal unit of a single LLM exchange. A task composes a `TurnCycle` from ordered `TaskStep`s — `DiscoveryStep`, `StagingStep`, `VerificationStep`, and `HealingStep` — and executes it against a `TaskEnv` that bundles the provider, model, context manager, history manager, protocol, policy, and streaming/cancellation channels. Steps declare whether they repeat, which is how the cycle expresses the discovery loop and the healing retry loop declaratively rather than as hard-coded control flow.

Specialized loops (`workflow/loops.rs`) encode distinct contracts over the same machinery:

- **`AnswerEditLoop`** — the general discovery/editing/healing cycle. It may answer without producing edits and may generate or update plans.
- **`EditLoop`** — requires at least one valid edit and a terminal success tag; if either is missing, it triggers a corrective retry, making an edits-only task fail loudly instead of silently returning prose.
- **`StructuredTask` / `StructuredStreamingTask`** — strictly typed structured-data exchange (plan refinement, review) without the heavy edit and healing cycle.

### PromptProtocol: Assembly and Decoding

`PromptProtocol` (`prompt/protocol.rs`) separates prompt assembly and output decoding from FSM control flow. A protocol declares its `capabilities()`, assembles the system prompt, optionally provides a `response_format()` (JSON Schema for structured output), builds the matching streaming filter, and decodes raw model text into a normalized `Vec<LlmCommand>`.

Because decoding is a trait method, the identical FSM drives XML, JSON, or any future syntax without modification. Concrete implementations include `XmlPromptProtocol`, `MarkedXmlPromptProtocol` (turn-marker protected), and `JsonPromptProtocol`.

### ApplyPolicy: Transactional Application

`ApplyPolicy` (`workflow/policy.rs`) isolates *how* validated edits reach disk from FSM logic. The trait exposes four operations: `staging`, `apply_temporary`, `apply_final`, and `rollback`. This is precisely the abstraction that removes the implicit coupling between Git transactions and the healing loop.

Two policies cover the supported transactional models:

- **`DirectApplyPolicy`** — writes straight to disk with no Git involvement. It is used for naive mode and local tests where a persistent transaction is unnecessary.
- **`GitTransactionPolicy`** (`workflow/git_policy.rs`) — the production policy. It creates a pre-edit checkpoint, applies validated edits, issues a temporary step commit before toolchain verification, finalizes the result into a permanent commit, and performs a clean rollback to the checkpoint if the turn fails.

The `git` module remains a low-level facade over subprocess operations; `ApplyPolicy` encapsulates the step-level transactional model on top of it.

### LlmProvider: Transport Boundary

`LlmProvider` (`providers.rs`) is the model transport boundary. It exposes `stream_chat`, `stream_chat_with_effort`, and `stream_text`, emitting `StreamEvent`s into an async channel and honoring a cancellation watch channel so a bounded turn can be interrupted cleanly. Two implementations exist:

- **`OpenRouterProvider`** — real network transport.
- **`MockLlmProvider`** — a scripted provider that replays predetermined responses. It enables deterministic multi-round scenarios (context request, compiler failure, successful healing patch, cancellation) to be exercised in the standard test runner in milliseconds and without any external network call.

Cancellation is observed continuously, not only while the response body is streaming. The transport races the initial `send()` future against the cancellation watch channel with `tokio::select!`, so a turn aborts immediately even while the model is still reasoning and no body bytes have arrived. Without this race, the `Proposal` and `Awaiting` phases would stall until the provider finished generating, leaving cancellation effective only after the first token.

### Command Normalization: `LlmCapability` and `LlmCommand`

The syntax of a protocol (XML tags or JSON Schema) is kept strictly separate from core business logic.

- A protocol declares its behavior as a declarative set of **`LlmCapability`** values: `TextMessage`, `FileEdits`, `Discovery`, `DocRequest`, `ToolchainVerify`, `PlanCreation`, `PlanUpdate`, `Scratchpad`, `Outcome`, and `UserLanguage`. The core queries these capabilities through `supports()` instead of branching on protocol identity, so adding a capability never requires touching the FSM.
- Model output is decoded into a normalized **`Vec<LlmCommand>`**: `Text`, `Edit`, `ContextRequest`, `ContextDrop`, `DocRequest`, `Verify`, `Plan`, `Scratchpad`, `Outcome`, and `UserLanguage`.

The streaming parser splits the data stream in two. Lightweight streaming events (`EditFileStarted`, `EditHunk`, `TurnPhase`, usage, toolchain status) are emitted to the UI as they arrive for immediate feedback, while the normalized commands are accumulated and handed to the core only once the stream finishes. This is why streaming remains responsive even though the commands themselves are processed deterministically.

---

## 3.8 Observability and Diagnostics

A bounded turn runs detached from the developer's terminal: it streams, stages, and heals without a visible process. To make failures reconstructable after the fact and to keep the harness itself trustworthy, TAUQE records the turn into a structured, size-bounded server journal.

### Structured Server Journal

The `crates/server` daemon owns the observability sink and initializes `tracing-subscriber` at startup. Records are written as one-line JSON documents with millisecond RFC-3339 UTC timestamps into `<repo_root>/.tauqe/server.log`, so the journal stays readable both through plain console tools (`grep`, `tail -f`, `less`) and structured parsers (`jq`, `lnav`).

### Size-Bounded Rotation

The active journal is rotated by size: once it grows past a threshold (10 MiB by default), it is moved into a numbered archive (`server.log.1`) and a fresh active file is opened. Only a bounded number of archives is retained, so the `.tauqe/` directory can never grow without bound.

### Panic Observability

The daemon installs a custom panic hook at startup. Every uncaught panic is captured with its source location, textual payload, and — when `RUST_BACKTRACE` is set — its backtrace, then written as a single `error`-level record with `target = "panic"` before the process unwinds.

### Semantic Traces of the Turn

The turn lifecycle is traced at its significant boundaries rather than at its data plane:

- daemon startup (PID, repository path) and graceful shutdown (stdin EOF, drain phase, forced-abort timeout);
- RPC transport events (incoming call, sanitized parameter summary, parse failure, per-request duration);
- operation-slot acquisition and the soft-stop / hard-cancel signals;
- bounded-turn FSM phase transitions, batch queue steps, Coder Ladder escalation, and replanner activation;
- toolchain failures and provider/network failures (HTTP status, timeout, stream interruption, `finish_reason: error`).

### Anti-Bloat Guard

The journal never carries raw generation stream deltas, full prompt bodies, context file contents, `RepoMap` dumps, or raw diffs in normal operation. It records interaction metadata, compact identifiers, and aggregated token and time metrics only. This keeps the file analytically useful and prevents a single verbose turn from defeating rotation.
