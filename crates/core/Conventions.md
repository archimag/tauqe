# Core Engine Conventions

This document specifies mandatory architectural and operational conventions for the Tauqe Core engine (`crates/core`).

---

## 1. Dual-Contour Architecture: Structured Output vs XML Edits

The interaction protocol between Tauqe and large language models is strictly divided into two orthogonal operational contours:

### 1.1. Code Modification Contour (Develop & Step Execution)
- **Protocol:** Strict XML marker protocol (`<tauqe_edits_...>`, `<context_request_...>`, `<repo_search_...>`, `<verify_...>`, `<plan_...>`, etc.).
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

### 1.3. Turn-Marker Tag Protection Invariant (Unified XML Defense)
- **Unified Registration SSOT:** Every structural XML control tag emitted by the model (`<tauqe_edits>`, `<edit>`, `<search>`, `<replace>`, `<create>`, `<overwrite>`, `<delete>`, `<move>`, `<context_request>`, `<repo_search>`, `<doc_request>`, `<plan>`, `<verify>`, `<scratchpad>`, `<success>`, `<error>`, `<blocked>`, `<user_language>`) MUST participate in the unified turn-marker protection mechanism. A tag is declared exactly once as an `XmlTagSpec` in `XML_TAG_SPECS` (`crates/core/src/edits/protocol/xml/engine.rs`), and the capability modules in `crates/core/src/capabilities` resolve those specs by base name for their own prompt text, decoding, and sanitization. The protected base names, the marker tolerance, the accepted unmarked children, and the stream-suppression flags are all derived from that single registry. Introducing a control tag anywhere else — or reviving a parallel `XML_TAG_BASES` list — is strictly prohibited.
- **Parametric Tag Machinery:** Boundary scanning (`find_next_tag`), attribute reading (`XmlTagMatch::attr`), stripping (`strip_tag_by_spec`), and safe streaming truncation (`compute_safe_stream_emit_len`) are implemented once and parametrized by the spec. A tag therefore cannot drift between its scanner, its stripper, and its stream suppressor, and hand-written per-tag scanners in `tags/*.rs` must never be reintroduced.
- **Capability Modules Own Their Tags:** Each capability module (`TextMessageModule`, `FileEditsModule`, `DiscoveryModule`, `DocRequestModule`, `ToolchainVerifyModule`, `PlanCreationModule`, `PlanUpdateModule`, `ScratchpadModule`, `OutcomeModule`, `BlockedModule`, `UserLanguageModule`) declares the tags it recognizes, renders its prompt fragments, and decodes its own commands. Registering a new capability means adding one module to `CapabilityRegistry`, never editing a shared scanner or a monolithic prompt template.
- **Cryptographic Turn Binding:** For each bounded turn, the engine generates an unpredictable alphanumeric turn marker (`generate_turn_marker`). System prompt instructions append this marker suffix to every control tag (`<{base}_{marker}>`).
- **Strict Normalization and Injection Defense:** Active execution parsers (`MarkedXmlEditProtocol`) strictly decode only marker-suffixed tags via `normalize_marked_xml`. Unsuffixed tags occurring in source code, diffs, conversational text, or documentation are protected as literals (`restore_xml_literals`) and must NEVER trigger commands, context mutations, repository searches, plan changes, or patch staging.
- **Streaming Suppression:** Stream filters (`XmlStreamFilter`, `find_next_control_tag`) detect marker-suffixed tags during generation and suppress them from leaking into the user-facing `TextDelta` stream.

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
4. **Declarative Prompt Composition:**
   - The system prompt is composed from the enabled capability modules, never from a monolithic template. `CapabilityRegistry::compose_contract_rules`, `compose_xml_protocol_instructions`, and `compose_structured_protocol_instructions` fold each module's fragments through a `PromptRenderContext`, which applies the turn-marker suffix to every tag name it renders.
   - A disabled capability therefore contributes no rule, no instruction, and no tag, so the prompt can never advertise a command the decoder would reject. Prompt text and decoding derive from the same `CapabilitySet`.
   - **Contract Rule Composition:** The core system contract is assembled as an ordered sequence of typed `ContractRule { title, body }` values and rendered by one loop with monotonic numbering `1..N`. Rule numbers are never written into string literals, so enabling or disabling a capability (`doc_request`, `scratchpad`, `file_edits`) leaves neither gaps nor lettered surrogates such as `1a.` or `10a.`.
   - Rule placement is decided by typed inputs (`TurnIntent` and `CapabilitySet`), never by text. The prohibition on heuristic intent analysis itself is stated once in the root `Conventions.md` (§1, *Typed Intent over Textual Inference*) and is not repeated here.
5. **Capability-Set-Scoped Decoding:**
   - `CapabilityRegistry::decode_xml_commands` runs only the modules enabled by the active `CapabilitySet` and then retains only commands whose capability is enabled. `XmlPromptProtocol::with_capabilities` and `with_turn_marker` produce such scoped protocols.
   - Role and mode profiles (`develop_xml`, `isolated_step_xml`, `develop_structured`, `isolated_step_structured`, `discussion`, `architect_discovery`) are the only sanctioned way to narrow a protocol. Decoding must never be narrowed by ad-hoc tag filtering outside the registry.
6. **Polymorphic Discovery Dispatch:**
   - Discovery phase execution must dispatch through registered handlers implementing `DiscoveryCapabilityHandler` (`crates/core/src/workflow/discovery.rs`) instead of branching on command variants. Each handler owns the validation, disk work, and feedback text of its own capability.
   - The dispatcher resolves repository facts once per round, folds every handler's findings into a single `DiscoveryRoundOutcome`, and halts the round as soon as the files token budget would be breached, so no later capability runs against a context that was never widened.

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
   - Progress counting obeys the same single-source rule. A `resolved / total` metric counts only actionable items: terminal, non-actionable outcomes are excluded from the denominator so an entity whose remaining work is finished reports `resolved == total` rather than a fraction that can never reach one. `Plan::count_stats` drops `Cancelled` leaves; `ReviewSession::stats` drops `Skipped` and `Rejected` findings.

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

---

## 8. Async Executor and Blocking I/O

1. **No Blocking I/O on Async Task Threads:**
   - The engine drives synchronous filesystem, Git, and toolchain work from
     inside async tasks. Blocking calls — an `fsync`-bounded history write
     (`SessionStorage::append_entry`, `rewrite_entries`), a context metadata
     check (`prune_missing_files`, `add_auto_file`), a toolchain configuration
     read (`tauqe.toml`, `package.json`), or a `git` subprocess — must never run
     directly on a Tokio worker thread.
   - Route such work through `crate::blocking::run_blocking` from synchronous
     code, or `crate::blocking::run_blocking_async` (returning `Result<T, JoinError>`)
     from async code when the closure is `Send + 'static`. Both helpers are
     reentrant and degrade to a direct call outside a multi-threaded runtime, so
     synchronous callers and tests stay panic-free.
2. **Rationale (Why):**
   - An `fsync` barrier or a cold metadata read can block for tens of
     milliseconds. Running it inline on the executor thread freezes the bounded
     turn, provider streaming, and RPC handling. Offloading the call keeps the
     runtime responsive without introducing background state.

---

## 9. Prompt Architecture and Provider Prefix Caching Invariant

1. **Single Static Prefix:**
   - Every bounded turn assembles the system prompt as one monolithic static
     prefix that is byte-for-byte identical from one round of the turn to the
     next.
   - Modern LLM providers cache the shared token prefix of consecutive
     requests. Any byte that changes inside the prefix invalidates the whole
     cached prefix and forces a full reprocessing pass, multiplying latency
     and cost on every discovery and healing round.
2. **Frozen Block Order:**
   - The blocks of the stable prefix are rendered in a fixed order: the Core
     System Contract, the repository outline (symbol map or directory tree,
     never both), the session history, the context files in insertion order,
     the active plan, and the language directive.
   - A block never moves relative to its neighbors. When a caller must render
     an additional block, it appends that block after the existing stable
     blocks, never between them.
3. **Deterministic Rendering:**
   - The prefix carries no runtime-varying content: no timestamps, no dates,
     no random or per-request identifiers, no context revision counters, no
     counters or latencies, and no toolchain output.
   - Every collection rendered into the prefix keeps a deterministic order
     (context files in insertion order, symbols and directories in their
     generated order), so identical state produces identical bytes.
4. **Append-Only Context:**
   - Discovery widens the context by appending newly discovered files at the
     end of the existing `<context>` block. Previously rendered files keep
     their exact bytes.
   - Promoting a file from read-only to editable rewrites only that file's own
     `access` attribute in place; it never relocates the file or reflows its
     neighbors.
5. **Isolated Volatile Tail:**
   - All genuinely round-varying content is confined to the end of the message
     sequence: the turn scratchpad is appended after the whole stable prefix,
     and round-specific diagnostics (patch retry reports, verification
     failures) travel as append-only extensions of the user message.
   - Nothing volatile is ever injected into an earlier block, because an early
     insertion invalidates every cached token that follows it.
6. **Rationale (Why):**
   - A bounded turn issues several requests per turn: discovery rounds, the
     proposal, staging retries, and verification healing. Reusing the
     provider's cached prefix across those requests removes the dominant
     repeated cost and keeps multi-round turns economically viable.
   - The invariant is only effective when it holds byte-for-byte, so any
     dynamic content or reordering silently defeats it.
