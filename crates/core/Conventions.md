# Core Engine Conventions

This document specifies mandatory architectural and operational conventions for the Tauqe Core engine (`crates/core`).

---

## 1. Dual-Contour Architecture: Structured Output vs XML Edits

The interaction protocol between Tauqe and large language models is strictly divided into two orthogonal operational contours:

### 1.1. Code Modification Contour (Develop & Step Execution)
- **Protocol:** Strict XML marker protocol (`<tauqe_edits_...>`, `<context_request_...>`, `<verify_...>`, `<plan_...>`, etc.).
- **Purpose:** Virtual in-memory staging, atomic search/replace patch application, toolchain verification healing, and deterministic Git transactions.
- **Scope:** Used exclusively when the model is tasked with creating, editing, renaming, or deleting codebase files.

### 1.2. Structured Output Contour (All Non-Code Operations)
- **Protocol:** Enforced Structured Output (`response_format: json_schema` or typed JSON deserialization).
- **Mandatory Invariant:** Any operation, subsystem, or workflow that does **NOT** directly produce code modifications MUST use Structured Output. Free-form text with heuristic markdown parsing for machine-consumed data is strictly prohibited.
- **Covered Subsystems & Operations:**
  1. **Structured Discussion Mode:** Interactive planning and review discussions (`DiscussionResponse` with optional typed `plan_update`).
  2. **Code Review Synthesis:** Generating audit findings, severity classifications, and file locations.
  3. **Commit Message Generation:** Synthesizing squash or checkpoint commit summaries.
  4. **History & Plan Compaction:** Context summarization, state compaction, and progress checkpoints.
  5. **Ontology & Triage Queries:** Any categorization or diagnostic tasks parsed by code.
- **Architectural Rationale (Why):**
  - Eliminates markdown parsing fragility and regex discrepancies.
  - Prevents prompt injection and instruction drift.
  - Guarantees compile-time schema conformance and deterministic deserialization.
  - Decouples cognitive reasoning and conversational workflows from file-patching machinery.

---

## 2. PromptProtocol Interchangeability Invariant

1. **Fundamental Equivalence of Protocols:**
   - All implementations of `PromptProtocol` (`XmlPromptProtocol`, `JsonPromptProtocol`, and any future protocol variants) must be strictly interchangeable architectural peers.
   - The workflow engine, FSM turn runner, and server handlers must interact with LLM communication strictly through the generic `PromptProtocol` trait contract (`capabilities()`, `system_instructions()`, `response_format()`, `decode()`, `create_stream_filter()`).
   - Branching on the concrete protocol type or hardcoding protocol-specific control paths in workflow loops or turn execution routines is strictly prohibited.
2. **Unified Semantic Representation (`Vec<LlmCommand>`):**
   - Every protocol implementation is responsible for completely decoding raw model outputs into strongly typed semantic commands (`LlmCommand`), including text answers, file edits, plan operations, discovery requests, verification triggers, and terminal outcomes.
   - Consumers of protocol output must never inspect raw wire markup, XML tags, or unparsed JSON keys.
3. **Clean Streaming Isolation:**
   - Every protocol implementation must provide an `EditStreamFilter` that isolates the user-facing interface from protocol scaffolding.
   - In structured protocols, the target textual response field (such as `message` in discussion mode) must be streamed on the fly via `StreamEvent::TextDelta` with real-time unescaping, while structural metadata (`plan_update`, context requests, language tags) remains completely hidden from the REPL stream.

---

## 3. Storage and Persistence Invariants

1. **Multi-File Entity Isolation:**
   - Entities belonging to user workflows (such as plans in `.tauqe/plans/{id}.json` and review sessions in `.tauqe/reviews/{id}.json`) must be stored in independent per-entity files.
   - Monolithic single-file state dumps for dynamic entity collections are prohibited.
2. **Deterministic Auto-Migration:**
   - Any evolution of on-disk storage layouts must provide backward-compatible auto-migration from legacy single-file representations on the first read.
3. **Reins Invariant (User Control over State):**
   - Disk state of plans and review items cannot be altered without explicit user authorization or programmatic verification completion (e.g., status moves to `Fixed` strictly after compiler verification and atomic git commit).
4. **Computed Aggregate State (Single Source of Truth):**
   - The status of an entire plan or review session is strictly a computed property derived deterministically from the statuses of its constituent items (`compute_items_status`), never stored as an independent mutable field on disk. This prevents state desynchronization and ensures strict adherence to the underlying task hierarchy.

---

## 4. Determinism & Toolchain Guardrails

1. **Never Guess Deterministic Data:**
   - If an artifact can be resolved from Git, Tree-sitter, project manifests, or compiler output, resolve it through deterministic Rust routines, never LLM generation.
2. **Fail-Fast on Verification Failure:**
   - Autonomous execution pipelines must abort immediately upon encountering compiler errors or test failures, rolling back dirty working trees to the pre-turn Git checkpoint.

---

## 5. Instant Provider Cancellation

1. **Cancellation Before Response Headers:**
   - A provider network request (`http_client.send().await`) must race the turn cancellation signal (`cancel_rx`) through `tokio::select!` while awaiting the HTTP response headers, not only while consuming the streamed body.
2. **Rationale (Why):**
   - While a model is reasoning during the `Proposal` and `Awaiting` phases, no body bytes have arrived yet. A request that observes cancellation only once the body starts streaming forces the developer to wait for the provider to finish before the turn can abort. Racing the initial send against `cancel_rx` makes interruption immediate in every phase.
3. **Uniform Reporting:**
   - When cancellation wins the race before headers arrive, the provider emits the same `StreamEvent::Cancelled` used everywhere else and returns cleanly, so callers never branch on a provider-specific cancellation path.

---

## 6. Batch Queue Stop Semantics

1. **Two Stop Modes:**
   - A running batch supervisor (plan step queue or review finding queue) supports two stop modes besides normal completion and failure: a **soft stop** that lets the current member finish, and a **hard stop** that aborts it immediately. Outside a batch, cancellation is always immediate.
2. **Soft Stop Boundary:**
   - A soft stop is observed strictly between members. The member already running completes its bounded turn, compiler verification, and Git commit; the queue then halts before launching the next member. Its committed result is preserved, never discarded.
   - A soft stop never interrupts a step mid-turn: interrupting an in-flight step is the exclusive province of hard cancellation.
3. **Hard Cancellation Priority:**
   - When both a soft-stop and a hard-cancel signal are pending before the same member, hard cancellation wins. An explicit reset is never silently downgraded to a graceful stop.
4. **Distinct Terminal Status:**
   - The two stop modes surface as distinct terminal statuses (`Stopped` and `Cancelled`) on the batch completion event, so clients can distinguish a graceful drain from an immediate abort without inspecting internal signals.

---

## 7. Semantic Instrumentation (Observability)

1. **Structured Events via the `tracing` Facade:**
   - The core engine is a library: it emits semantic observability events exclusively through the lightweight `tracing` facade (`info!`, `warn!`, `error!`) and carries no dependency on file sinks, on-disk formats, or rotation. Subscriber initialization, the journal format, and the rotation policy belong entirely to `crates/server`.
2. **State Transitions Invariant:**
   - Key state transitions, operation lifecycles, and failure points in the domain engine must emit a semantic log record. This covers:
     - bounded-turn FSM phase transitions (`workflow/fsm.rs`);
     - batch queue steps and their terminal statuses (`workflow/batch.rs`);
     - Coder Ladder (`CoderTier`) escalation along the coder role ladder (`workflow/execution.rs`);
     - replanner activation in both manual and automatic modes (`plan`);
     - toolchain failure diagnostics derived from `ToolchainCheckResult`;
     - network and provider failures in `OpenRouterProvider` (HTTP status classes such as 401/429/502/504, connection timeouts, stream interruption, and `finish_reason: error`).
   - **Rationale (Why):** turns run detached from the developer's terminal, so a structured transition record is the only way to reconstruct a failure — which compile error aborted a step, which rung of the ladder was consumed, or which provider error terminated a stream.
3. **Anti-Bloat Logging Invariant:**
   - The engine must never log raw generation deltas (`TextDelta`, `ReasoningDelta`), full prompt bodies, context file contents, `RepoMap` dumps, or raw diffs during normal operation. Only interaction metadata, compact identifiers, and aggregated token and time metrics are recorded.
   - Toolchain failures are recorded as a compact, UTF-8-safe summary, never as a dumped compiler report.
