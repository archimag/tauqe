# 3. The Autonomous Bounded Turn

Instead of an unbounded background agent loop, TAUQE operates as a **finite-state turn machine**.

---

## 3.1 The Turn Lifecycle

```text
               User Prompt
                    ↓
        ┌─► [Discovery Phase] ──── (requests missing files via <context_request>,
        │           ↓               searches codebase via <repo_search>,
        │           │               or system docs via <doc_request>)
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

- **Discovery & Exploration Phase:** The model inspects the task, navigates the Tree-sitter repository map to locate missing symbols and type signatures, queries the codebase via typed `<repo_search path="..." regex="..." />` commands, or requests internal system documentation via `<doc_request topic="...">`. Search queries run in pure Rust without shell access and report matching file paths with hit counts in `<search_feedback>` blocks. If required files are missing from context, the system autonomously fetches them into the `auto` layer via `<context_request>` and proceeds to the next round. Throughout multi-round discovery, the model retains intermediate findings and working hypotheses in the ephemeral Turn Scratchpad. Code generation is deferred until the model has sufficient clarity to avoid blind speculation.

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

## 3.6 Two Wire Formats, One Capability Profile

TAUQE ships two interchangeable wire formats for model interaction. They are
architectural peers, not separate pipelines: the same FSM, the same normalized
`Vec<LlmCommand>`, and the same `ApplyPolicy` drive both, so switching the format
never changes what a turn is allowed to do.

1. **XML (marker-protected streaming tags):**
   - The default format, used whenever a turn must modify code.
   - Employs streaming XML search/replace blocks (`<edit>`, `<create>`, `<delete>`).
   - Resistant to unescaped quotes, arbitrary source formatting, and indentation shifts.
   - Patches are validated in virtual staging before touching disk, followed by the toolchain compiler gate.

2. **Structured Output (JSON Schema):**
   - Carries the identical operational surface as schema fields: `message`,
     `changes`, `context_requests`, `repo_searches`, `doc_requests`,
     `scratchpad`, `verify`, `outcome`, `blocked`, and `user_language`.
   - The `message` field streams to the interface while the remaining fields stay
     hidden, so the REPL experience matches XML exactly.
   - Removes turn-marker leaking and regex parsing ambiguities from the wire.

Because both formats decode into the same command vocabulary, the capability
profile — not the wire format — decides what a turn may do. A plan discussion
narrows the profile to a read-only planning surface (plan `save`/`update`,
context requests, language) and renders the `DiscussionResponse` schema. A review
focus dialogue keeps file editing and toolchain verification, so the developer can
reason about a finding and request the fix in the same anchored turn (see 2.8.3
and 6.5).

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

Because decoding is a trait method, the identical FSM drives XML, JSON, or any future syntax without modification. Concrete implementations include `XmlPromptProtocol`, the turn-marker protected XML variant backed by `MarkedXmlEditProtocol`, and `JsonPromptProtocol`.

Prompt assembly is declarative rather than templated. `XmlPromptProtocol` delegates to the capability registry, which folds the contract rules and protocol instructions of exactly the modules enabled by the active `CapabilitySet`. A `PromptRenderContext` supplies the permitted editable paths and the active turn marker, so every tag name the prompt advertises already carries its marker suffix. The same registry decides which commands the decoder accepts: a scoped protocol built with `with_capabilities` or `with_turn_marker` decodes only what its profile enables. Adding a capability therefore means adding one module, not editing a prompt template and a parallel decoder in lockstep.

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

- A protocol declares its behavior as a declarative set of **`LlmCapability`** values: `TextMessage`, `FileEdits`, `Discovery`, `DocRequest`, `ToolchainVerify`, `PlanCreation`, `PlanUpdate`, `Scratchpad`, `Outcome`, `Blocked`, and `UserLanguage`. The core queries these capabilities through `supports()` instead of branching on protocol identity, so adding a capability never requires touching the FSM.
- Every capability belongs to a **`CapabilityPhase`** (`Discovery`, `Execution`, `Verification`, `Outcome`). A consumer can therefore reason about the stage a command advances without knowing its wire syntax.
- A **`CapabilitySet`** is the profile mask naming the capabilities enabled for one role or mode: `develop_xml`, `isolated_step_xml`, `develop_structured`, `isolated_step_structured`, `discussion`, and `architect_discovery`. Profiles, not scattered boolean checks, decide both what the prompt advertises and what the decoder accepts.
- Model output is decoded into a normalized **`Vec<LlmCommand>`**: `Text`, `Edit`, `ContextRequest`, `RepoSearch`, `DocRequest`, `Verify`, `Plan`, `Scratchpad`, `Outcome`, `Blocked`, `UserLanguage`, and `ProtocolError`.

#### Capability as a Unit: the Registry

The registry in `crates/core/src/capabilities` turns each capability into an autonomous module. A `CapabilityModule` is the single source of truth for one capability and owns four concerns at once:

- the **tag schema** it recognizes, resolved from the parametric `XmlTagSpec` registry instead of being re-declared;
- the **prompt fragments** it contributes to the core contract section, the XML instruction section, and the structured-output instruction section;
- the **decoder** that converts raw model output into normalized commands;
- the **sanitizer** that removes its tags from conversational text.

`CapabilityRegistry::global()` composes exactly the modules a `CapabilitySet` enables. Because the same module supplies the tag, the instruction, and the decoder, the prompt can never advertise a command the decoder rejects, and a disabled capability leaves no trace in either the prompt or the command stream.

#### Parametric XML Engine under Turn-Marker Protection

Every structural tag is declared once as an `XmlTagSpec` in `XML_TAG_SPECS` (`edits/protocol/xml/engine.rs`): its base name, the capability it realizes, its close mode, its marker policy, the unmarked children it accepts, and whether an open tag must be withheld from the stream. Boundary scanning, attribute extraction, stripping, and safe streaming truncation are implemented once and parametrized by that declaration, so a tag cannot drift between its scanner, its stripper, and its stream suppressor.

The turn marker rides on top of the same registry. Each turn generates an unpredictable marker, prompt rendering suffixes every tag with it through `PromptRenderContext::tag_name`, and normalization honors only marker-bound tags. Unsuffixed tags found in source code, diffs, or documentation stay literal. This is why a plan body may keep its readable unmarked `<item>` children while a quoted `</search>` inside a marked edit block can never close anything.

#### Polymorphic Discovery Execution

The Discovery phase does not branch on command variants. Each discovery capability registers a `DiscoveryCapabilityHandler` that declares which commands it handles and executes them, owning its own validation, disk scan, and feedback text. The dispatcher resolves repository facts once per round, folds every handler's findings into a single `DiscoveryRoundOutcome`, and halts the round as soon as the files token budget would be breached, so no capability runs against a context that was never widened.

`DiscoveryRoundOutcome` also answers whether a round made progress. That single question is what lets the FSM and the Architect discovery loop distinguish a productive round from a dead end without inspecting individual request types.

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

---

## 3.9 Turn Intent and the Dynamic System Contract

Every bounded turn runs in exactly one execution regime, and the harness must know
that regime before the first token is generated. This section explains where that
knowledge comes from, what it decides, and why it is a typed value rather than a
property of the prompt text.

### 3.9.1 TurnIntent as the Single Typed Source of Truth

`TurnIntent` (`crates/protocol/src/intent.rs`) is the authoritative vocabulary of
turn modes:

- `InteractiveRepl` — free interactive dialogue started from the REPL, including
  a dialogue anchored on a review finding.
- `ReadonlyDiscussion` — read-only architectural discussion of a plan.
- `IsolatedStep` — isolated execution of one plan step or one review finding.

Deterministic predicates — `is_interactive_repl()`, `is_readonly_discussion()`,
and `is_isolated_step()` — are mutually exclusive: exactly one holds for every
variant. Callers branch on these predicates instead of on strings, so a new
regime is a compile-time addition rather than a runtime guess.

The type deliberately implements neither `Default` nor any implicit conversion.
Every turn must name its mode, because a default value would turn a forgotten
argument into a silent mode substitution.

### 3.9.2 Where the Intent Comes From

The server produces the intent from typed RPC parameters, before the prompt is
assembled:

- `ModelAskParams::Discussion` carries a typed `DiscussionTarget`
  (`Plan { id }` or `Review { id }`). A plan target establishes
  `ReadonlyDiscussion`; a review target stays `InteractiveRepl`, because the
  developer may ask for the fix in the same anchored turn (see 2.8.3).
- A plain prompt that continues an active `DiscussionFocus` inherits the intent of
  the focused entity. A stale focus — the plan or review was deleted elsewhere —
  drops the anchor and answers the raw prompt as `InteractiveRepl` instead of
  failing the turn.
- The plan-step and review-finding supervisors start their turns directly as
  `IsolatedStep`.

The wire contract does not change: the mode travels inside parameters that
already carry the target, so no client — including the TUI — has to send or
interpret a mode flag.

### 3.9.3 What the Intent Decides

One value drives four decisions that would otherwise drift apart:

- **Wire protocol.** A read-only plan discussion always resolves to the structured
  discussion schema, regardless of `develop.protocol`, because the discussion
  prompt instructs the model to answer with that schema. An isolated step narrows
  the configured protocol to the isolated-step capability set.
- **Capability surface.** `TurnIntent::narrow_capabilities` is the single mapping
  from intent onto a `CapabilitySet`, so the prompt instructions, the structured
  response schema, the stream filter, and the command decoder observe one surface.
  A capability can neither be advertised while undecodable nor accepted while
  hidden from the instructions.
- **Plan mutation mode.** The storage layer receives the matching
  `PlanMutationMode`, so a develop turn cannot silently rewrite an executing plan.
- **History recording.** Isolated steps do not append a turn entry to the session
  history, which keeps batch execution out of the interactive transcript.

### 3.9.4 Why a Prompt Prefix Must Never Decide the Mode

A textual heuristic is a silent failure class, not a contract:

- **Wording fragility.** If the mode were inferred from a prompt prefix or a
  heading, editing one sentence of prompt assembly would change the turn's
  permissions without touching a single line of execution code.
- **Unverifiable.** The compiler cannot check that a string starts with the right
  words; a regex or keyword scan is guarded only by the tests someone remembered
  to write.
- **Hidden coupling.** Frontend prose would become a domain control channel, so a
  user message that merely resembles an internal heading could switch the turn
  into the wrong regime, and identifiers read out of prose could be wrong.

`Conventions.md` §1 (*Typed Intent over Textual Inference*) therefore forbids
deriving an execution mode, an execution restriction, or an operation type from
unstructured text: when a subsystem needs its calling context, the caller passes a
typed DTO or enum. The bounded turn is the canonical example of that rule, and
subsystem conventions reference it instead of restating it.

### 3.9.5 The Dynamic System Contract

The `## Core System Contract` section of the system prompt is assembled, not
templated. Each rule is a typed `ContractRule { title, body }` value, and a single
rendering loop numbers the ordered sequence monotonically from `1` to `N`.

Rule numbers never appear in a string literal. Consequently:

- enabling or disabling a capability (`doc_request`, `scratchpad`, `file_edits`)
  leaves neither a gap nor a lettered surrogate such as `1a.` or `10a.` in the
  numbering;
- a rule is placed by typed inputs (`TurnIntent` and `CapabilitySet`) rather than
  by editing text in the middle of a numbered block;
- adding a capability module contributes its own fragments — contract rule, XML
  instruction, structured-output instruction, decoder, and sanitizer — without
  renumbering anything by hand.

This is the same single-source discipline described for the capability registry in
3.7: the prompt advertises exactly the modules the active capability set enables,
and the decoder accepts exactly the same set.

### 3.9.6 Balanced Authorization Rules

Two classes of rule are deliberately scoped differently.

- **Action Authorization** applies only to `InteractiveRepl` turns with
  `FileEdits` enabled. When the required change is clear, specific, and
  unambiguous (direct requests, straightforward bug fixes, obvious error
  corrections), the model proceeds directly with minimal coherent edits. It
  proposes, analyzes, and seeks confirmation first only when there is genuine
  architectural ambiguity, competing trade-offs, multiple viable approaches, or
  open exploratory questions.
- **Minimal Coherent Change** and **Always Explain Changes** apply everywhere
  `FileEdits` is enabled, including plan steps and review findings. Quality rules
  describe how a change must be made, so they hold in every editing regime,
  whereas the authorization rule describes whether a change is wanted at all — a
  question only an interactive turn can leave open.

An isolated step turn is itself the explicit instruction to change code, so the
authorization rule is omitted there instead of being contradicted; a read-only
discussion has no editing capability and never reaches it.

---

## 3.10 Prompt Layout for Provider Prefix Caching

Every round of a bounded turn re-sends the whole assembled prompt. Providers
cache the longest unchanged token prefix of a request (KV cache reuse, prompt
caching), so the price of a round depends far less on the total prompt size than
on how much of it moved since the previous round. A layout that reshuffles an
early block wastes that cache and forces the provider to reprocess every token
after the change.

TAUQE therefore renders the prompt as an append-only document: a frozen head that
must never move inside a turn, and a volatile tail that is expected to change on
every round. Four invariants define that layout, and the tests in
`crates/core/tests/fsm_tests.rs` hold the implementation to them (§3.10.5).

### 3.10.1 Prefix Invariance

Every round of one bounded turn extends the previous request instead of
rewriting it. The head of the system message is rendered in a fixed order:

```text
Core System Contract   invariant rules of the turn
Repository Map         frozen for the whole turn
Session History        rewritten only by compaction
Context Files          insertion order
Active Plan            read-only discussion and develop turns
Language Directive     once the user language is known
---
Turn Scratchpad        volatile tail, rewritten on every round
```

The turn prompt is the user message, and round feedback appends to it.

Nothing before the context block may carry dynamic data: no context revision
counter, no per-round list of files discovered in this round, no round banner.
Such a value invalidates the cache from its own offset onward, so the turn pays
for a change that carries no information the model needs.

### 3.10.2 Turn-Frozen Repository Map

The repository map is generated once, on the first proposal round of a turn, and
frozen until the turn ends.

A regenerated map would drop the files that discovery pulled into the context,
because the map excludes whatever is already rendered in full below it. Dropping
a node rewrites a block that may hold thousands of tokens and invalidates
everything after it. The frozen map is also semantically correct: it describes
the repository contours the turn started from, and every file the turn requested
is rendered completely in the context block right beneath it. Listing a file
twice is cheap; a cache invalidated by the turn's own discovery is not.

### 3.10.3 Insertion-Order Context

Context files are rendered in the order they were added, not in alphabetical
order. A file requested through `<context_request>` is appended to the end of the
context block, so the tokens of every earlier file keep their position.
Promoting a read-only file to editable rewrites only that file's own `access`
attribute in place, so a promotion neither moves the file nor reshuffles its
neighbors.

Staged content follows the same rule: the body of a file already in context is
replaced where it stands, and files created during the turn are appended in path
order. A round therefore never relocates a file that an earlier round already
rendered.

Alphabetical ordering is a presentation decision and lives exclusively in the
interface, which sorts rows for the developer.

### 3.10.4 Cumulative Feedback and the Volatile Tail

Round-specific data is appended behind the stable head:

- **Discovery feedback** — search reports, documentation blocks, and missing-path
  notes — is appended to the user message of the next round.
- **Patch-retry reports** accumulate in one append-only repair log, so retry `n`
  carries the full history of attempts `1..n` instead of replacing it.
- **Verification diagnostics** and the healing instruction are appended to the
  user message of the healing round.
- **The turn scratchpad** is rendered as the last block of the system message,
  after the context and the active plan.

Everything in this tail changes on every round by design. Keeping it last means
a change starts after the frozen head, so the cached prefix survives.

### 3.10.5 What the Tests Enforce

The invariants are covered by end-to-end tests in
`crates/core/tests/fsm_tests.rs`. They drive real discovery, patch-retry, and
scratchpad rounds against a scripted provider and inspect the recorded requests:

- the head of the system message is byte-identical across a discovery round, and
  the context block extends instead of being reshaped;
- the repository map block is byte-identical across rounds and still lists the
  file a discovery round added to the context;
- the scratchpad appears only after the context block, in the last position of
  the system message;
- a patch retry leaves the system message untouched and appends its report to
  the user message.

These tests are the regression guard. A change that reintroduces a moving value
into the stable head fails them immediately, instead of silently paying for a
cache miss on every round.
