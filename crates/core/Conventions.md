# Core Engine Conventions

This document specifies mandatory architectural and operational conventions for the Tauqe Core engine (`crates/core`).

---

## 1. Dual-Contour Architecture: Structured Output vs XML Edits

The interaction protocol between Tauqe and large language models is strictly divided into two orthogonal operational contours:

### 1.1. Code Modification Contour (Develop & Step Execution)
- **Protocol:** Strict XML marker protocol (`<edits_...>`, `<context_request_...>`, `<repo_search_...>`, `<verify_...>`, `<plan_...>`, etc.).
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
- **Unified Registration SSOT:** Every structural XML control tag emitted by the model (`<edits>`, `<edit>`, `<search>`, `<replace>`, `<create>`, `<overwrite>`, `<delete>`, `<move>`, `<context_request>`, `<repo_search>`, `<doc_request>`, `<plan>`, `<verify>`, `<scratchpad>`, `<success>`, `<error>`, `<blocked>`, `<user_language>`) MUST participate in the unified turn-marker protection mechanism. A tag is declared exactly once as an `XmlTagSpec` in `XML_TAG_SPECS` (`crates/core/src/edits/protocol/xml/engine.rs`), and the capability modules in `crates/core/src/capabilities` resolve those specs by base name for their own prompt text, decoding, and sanitization. The protected base names, the marker tolerance, the accepted unmarked children, and the stream-suppression flags are all derived from that single registry. Introducing a control tag anywhere else — or reviving a parallel `XML_TAG_BASES` list — is strictly prohibited.
- **Parametric Tag Machinery:** Boundary scanning (`find_next_tag`), attribute reading (`XmlTagMatch::attr`), stripping (`strip_tag_by_spec`), and streamed tag classification (`TextScan`) are implemented once and parametrized by the spec, and the incremental scanner resumes from the offsets it already cleared instead of rescanning the accumulated buffer. A tag therefore cannot drift between its scanner, its stripper, and its stream suppressor, and hand-written per-tag scanners in `tags/*.rs` must never be reintroduced.
- **Capability Modules Own Their Tags:** Each capability module (`TextMessageModule`, `FileEditsModule`, `DiscoveryModule`, `DocRequestModule`, `ToolchainVerifyModule`, `PlanCreationModule`, `PlanUpdateModule`, `ScratchpadModule`, `OutcomeModule`, `BlockedModule`, `UserLanguageModule`, `CustomToolsModule`) declares the canonical tag it owns, the aliases it still accepts, the tag specifications it resolves, its delivery mode, the prompt fragments it contributes, and the decoder that produces its commands. Registering a new capability means adding one module to `CapabilityRegistry`, never editing a shared scanner or a monolithic prompt template.
- **Canonical Tag with Declared Aliases:** A capability publishes exactly one canonical name and, where history requires it, the spellings an earlier revision emitted. Suppression, cleaning, and decoding resolve through the canonical specification, so an alias reproduces canonical behavior instead of surviving as a second code path. The edit container publishes the canonical name `edits`, while `tauqe_edits` stays a declared alias, so transcripts, sessions, and model output written before the rename keep decoding identically. A spelling that is neither the canonical name nor a declared alias is prohibited.
- **Declared Delivery Mode:** Each capability states how its output reaches the harness — `LiveStreaming` for the contours consumed while tokens arrive (conversational text and file edits), `RoundDeferred` for the payloads decoded once a round finishes (discovery, documentation, verification, plans, scratchpad, outcome, blockers, language, project tools). Streaming scanners and round-level decoders read that declared property rather than a capability name or tag literal, so delivery stays orthogonal to capability phase.
- **Cryptographic Turn Binding:** The turn marker defends the tagged command channel from injection, so the engine generates its unpredictable alphanumeric value (`generate_turn_marker`) only for a turn whose active protocol uses the XML contour (`Xml` and `Mixed`). System prompt instructions append that suffix to every control tag (`<{base}_{marker}>`). A structured turn emits no control tag at all, so it never mints a marker: the value stays `None` on every server and FSM level that would otherwise carry it, and the system prompt appends no suffix.
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
   - **Contour Dictionary Dualism:** Every contract rule and every protocol instruction is rendered in the vocabulary of the active contour. The XML contour names its marker tags; the structured contour names the fields of its JSON Schema (`plan_update`, `repo_searches`, `outcome`, `blocked`, `abort`, `custom_tools`). A rule never mentions the vocabulary of the other contour.
   - **Single Sanctioned Exception:** The prompt side — the `<context>` block, the `access` attribute, and the `<system_documentation>` block — is described identically in both contours, because it is payload the model reads rather than a command channel it emits.
   - **Typed Branch Input:** The contour travels as typed knowledge, not as inferred text. Its input is `ContractContext.protocol` (`EditProtocolKind`) and its predicate is `ctx.protocol == EditProtocolKind::Xml`. `Mixed` never reaches the contract, because assembly hands over the kind of the concrete protocol; a `Mixed` project therefore renders XML rules for code editing and structured rules for every non-code contour.
5. **Capability-Set-Scoped Decoding:**
   - `CapabilityRegistry::decode_xml_commands` runs only the modules enabled by the active `CapabilitySet` and then retains only commands whose capability is enabled. `XmlPromptProtocol::with_capabilities` and `with_turn_marker` produce such scoped protocols.
   - Role and regime profiles (`interactive_repl`, `isolated_step`, `discussion`, `architect_discovery`) are the only sanctioned way to narrow a protocol. A profile names a turn regime, never a wire format: both transports decode the same capability surface, so format-specific profile factories must not be reintroduced. Decoding must never be narrowed by ad-hoc tag filtering outside the registry.
6. **Polymorphic Discovery Dispatch:**
   - Discovery phase execution must dispatch through registered handlers implementing `DiscoveryCapabilityHandler` (`crates/core/src/workflow/discovery.rs`) instead of branching on command variants. Each handler owns the validation, disk work, and feedback text of its own capability.
   - The dispatcher resolves repository facts once per round, folds every handler's findings into a single `DiscoveryRoundOutcome`, and halts the round as soon as the files token budget would be breached, so no later capability runs against a context that was never widened.
   - Findings that are not repository files — search reports, directory listings, targeted outlines, and documentation chapters — never enter the `<context>` block. They accumulate in the turn-scoped `TurnDiscoveryJournal` (`crates/core/src/workflow/discovery/journal.rs`), an append-only, de-duplicated tail of the user message that survives the whole turn, so Staging retries and Verification Healing rounds keep the knowledge Discovery already paid for. `TurnDiscoveryJournal::compose_user_prompt` expresses the message order exactly once — task, journal, phase diagnostics — and the phase executors must obtain their round prompts through it instead of concatenating fragments themselves.
7. **Delivery Declared by Capability, Not by Tag Literal:**
   - `CapabilityDelivery` is a property of the capability itself, declared beside it in `crates/protocol`: `LiveStreaming` for the contours consumed while tokens arrive, `RoundDeferred` for the payloads decoded once the round finishes.
   - A protocol advertises a capability set and the harness reads each capability's declared delivery, so protocols stay interchangeable and a future wire format inherits the same streaming and round-decoding behavior without touching a scanner.

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
3. **Fail-Fast on Ignored Paths:**
   - A patch that would create, overwrite, or move onto a path excluded by `.gitignore` or `.git/info/exclude` is rejected during in-memory staging with `EditError::IgnoredByGit`, before any byte reaches disk and before `git add` runs. Writing such a path would otherwise surface only after the whole change set is applied, where a refused `git add` aborts the Git transaction instead of the edit.
   - The check is batched through `git check-ignore`, so one subprocess answers for the whole change set and a repository Git cannot inspect degrades to "nothing is ignored" rather than blocking an otherwise valid turn.
   - Post-hook artifacts are exempt by construction: they are discovered through a Git status differential that honors the same ignore rules, so a generated build file is absent from the change set instead of breaking the transaction. The separation of responsibility is deliberate — the model may not write ignored paths, while a declared external generator may produce them freely.

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

## 9. Prompt Architecture: Layout Policies and Provider Prefix Caching

1. **Layout Policies Are Interchangeable Implementations of One Trait:**
   - Turning a rendered `PromptAssembly` into the message sequence of a turn is a
     policy behind one trait (`PromptLayout`, `crates/core/src/prompt/layout.rs`),
     never a hardcoded order inside the assembly.
   - Two implementations exist and are strictly interchangeable: `SemanticLayout`
     groups the material into stable sections, and `ChronologicalLayout` turns the
     same material into an append-only transcript.
   - A caller chooses a policy exclusively through the typed `PromptLayoutKind`
     (`Semantic` or `Chronological`) and `layout_for`. Branching on a policy name,
     on a rendered heading, or on any other textual property is prohibited (the
     root `Conventions.md` §1, *Typed Intent over Textual Inference*).
   - Both policies render context files through the same layout-agnostic
     `ContextBlockCache`, so a `<file>` element produced by one policy is
     reproduced byte for byte by the other.
2. **Block Order Is a Property of the Policy:**
   - The order in which the already-rendered blocks become messages belongs to the
     policy; it is no longer a global constant of the assembly.

   | Material | `SemanticLayout` | `ChronologicalLayout` |
   | --- | --- | --- |
   | Core system contract, protocol instructions, project metadata | system head | system head |
   | Repository outline (symbol map or directory tree, never both) | system head | system head |
   | Language directive | system head | system head |
   | Context files | one `<context>` block in the system message | `<context>` groups interleaved with the history records |
   | Session history | one `<history>` block in the system message | one message per history record |
    | Active plan | system message tail | trailing user message |
    | Regime delta (intent-specific contract rules) | leading block of the final user message | first block of the trailing user message, ahead of the active plan |
   | Context manifest (paths and access, no content) | absent | trailing user message |
   | Turn scratchpad | system message tail | trailing user message |
   | Current task | final user message | final user message |

   - Both policies keep the same three-part shape: a byte-stable head, the
     material that legitimately grows, and an isolated volatile tail.
3. **Byte-Stable Head:**
   - The head carries only blocks that are identical from one round of a turn to
     the next: the introduction, the Core System Contract, the protocol
     instructions, the project metadata, the repository outline, and the language
     directive.
   - The repository outline stays in the head under both policies. The dominant
     regime of a turn is exploration and planning, where the outline hardly
     changes, while the outline is also the largest block: placing it in the tail
     would pay a full reprocessing pass on every round instead of only on the
     turns that regenerate it.
   - The accepted price is explicit. A regenerated outline changes the head itself
     and therefore invalidates the whole tail, because a later turn renders an
     outline that excludes the files which entered the context. That divergence is
     a documented property of the outline, not an accident of the layout.
   - Within a turn the outline is frozen (`TurnInputCache`), so no later round of
     the same turn rewrites it even after discovery widened the context.
4. **Contract Vocabulary Follows the Active Layout:**
   - The Core System Contract describes the context surface in the dictionary of
     the active policy, exactly as it selects the dictionary of the active contour
     (§2.4). `SemanticLayout` names the `<context>` block; `ChronologicalLayout`
     names the interleaved transcript and the tail manifest.
   - A rule never mixes the two dictionaries. The prompt-side description of the
     `<file>` element and of the `access` attribute is shared, because it describes
     payload the model reads rather than a command channel it emits.
5. **Deterministic Rendering:**
   - The prompt carries no runtime-varying content: no timestamps, no dates, no
     random or per-request identifiers, no context revision counters, no counters
     or latencies, and no toolchain output.
   - Every collection rendered into a policy keeps a deterministic order (context
     files by insertion order or by insertion watermark, symbols and directories
     in their generated order), so identical state produces identical bytes.
6. **Append-Only Growth:**
   - Discovery widens the context by appending newly discovered files. Under
     `SemanticLayout` the new element extends the existing `<context>` block at its
     tail; under `ChronologicalLayout` it is appended after the records already in
     the transcript. Previously rendered elements keep their exact bytes.
   - Promoting a file from read-only to editable rewrites only that file's own
     `access` attribute in place; it never relocates the element or reflows its
     neighbors.
   - The chronological transcript is ordered by the insertion watermark every
     context entry carries (`ContextItem::insertion_seq`). A file is placed after
     the first history record that already observed it, so a file the dialogue
     discovered between two turns appears between them, and a file no record
     observed yet closes the transcript.
7. **Isolated Volatile Tail:**
   - All genuinely round-varying content is confined to the end of the message
     sequence: the active plan, the context manifest of the chronological policy,
     and the turn scratchpad; round-specific diagnostics (patch retry reports,
     verification failures) travel as append-only extensions of the user message.
   - The context manifest carries paths and effective access only, never content.
     It compensates for the lost semantic grouping: the model keeps a complete view
     of what is in context while no content byte moves into the tail.
   - Nothing volatile is ever injected into an earlier block, because an early
     insertion invalidates every cached token that follows it.
8. **Documented Structural Invalidation Points:**
   - Appending grows the sequence at its tail and therefore preserves the cached
     prefix.
   - The structural mutations that legitimately move an earlier byte are named
     rather than discovered, and each of them bounds the invariant:
     1. Repository outline regeneration between turns, which rewrites the head.
     2. Access promotion, which rewrites one attribute of one element in place.
     3. Prune and rollback, which remove an element and shift every message after
        it; the removal is the purpose of the operation.
     4. History compaction, which replaces the opening of the transcript and
        rotates the dialogue session identifier together with the prefix it
        rewrites (rule 9), because the provider must not be asked to reuse a node
        cache whose prefix the dialogue no longer sends.
9. **Dialogue Session Boundary:**
   - An interactive turn binds its provider requests to a dialogue session
     identifier, which is what a provider uses to route consecutive requests to
     the same cache-warm node.
   - While the context block stays byte-for-byte identical and the target model
     remains unchanged, the identifier stays too: a series of REPL answers that
     only reads context keeps one session, so every round reuses the prefix
     cache warmed by its predecessor.
   - Session invalidation and rotation occur at exactly two deterministic
     boundaries:
     1. When shrinking or clearing the context block (clearing the Auto layer
        via `AppState::clear_auto_context` or resetting context via
        `AppState::clear_all_context`). A fresh session opens a prefix no earlier
        request warmed; keeping the old identifier would address a node whose
        cached prefix the dialogue no longer sends. The rotation follows the
        boolean reported by `ContextManager::clear_auto` or `clear`, so a
        no-op clear preserves the warm session.
     2. When the effective model changes (`maybe_rotate_session_on_model_change`).
        Provider KV caches and routing affinities are physically model-specific.
        Reusing a session identifier across different models produces guaranteed
        cache misses or affinity routing conflicts. Changing only reasoning
        effort or reselecting the identical model preserves session affinity.
   - Focus transitions never rotate the identifier. The prompt head renders the
     main capability list and the shared trunk, so it is byte-identical for the
     free REPL, the plan discussion, and the review discussion. Setting a focus,
     replacing it, switching between a plan and a review target, starting a
     dialogue from the Plans or Review view, or clearing the focus therefore
     moves no head byte and keeps the provider session warm.
10. **Rationale (Why):**
    - A bounded turn issues several requests per turn: discovery rounds, the
      proposal, staging retries, and verification healing. Reusing the provider's
      cached prefix across those requests removes the dominant repeated cost and
      keeps multi-round turns economically viable.
    - The invariant is only effective when it holds byte-for-byte, so any dynamic
      content, reordering, or unexpected relocation of a block silently defeats
      it. A policy makes the order explicit and reviewable instead of implicit in
      the assembly.
    - The chronological policy exchanges the semantic grouping for a transcript
      that grows at its tail, which is why the lost grouping is compensated by the
      compact context manifest instead of by reloading file bodies.
    - A session identifier that outlives its prefix or target model is worse than
      a cold one: it asks the provider to reuse a node cache the next request can
      never match, which is why rotation is bound strictly to context shrinking
      and model changes rather than to a timer or a round counter.
    - A focus transition is a non-event for the session for the same reason. One
      head is shared by every dialogue contour, so switching the discussed entity
      changes only the tail delta; rotating there would discard a prefix the next
      request still sends.
---

## 10. Model Catalog as the Single Source of Truth for Model Names

1. **Catalog Owns Identity and Alias:**
   - Every model a project exposes is declared once in the provider catalog
     (`[providers.openrouter].models`), as either a bare model specification or a
     `{ model, alias }` table. The optional `alias` is metadata of the model
     itself, so every consumer — the picker, the headers, the telegraphy of role
     summaries — resolves a display name through that one declaration.
2. **Role Entries Carry Runtime Parameters Only:**
   - A role entry (`ModelEntry`) names a model by provider identifier or by
     catalog alias and adds only the runtime parameters of that slot, such as the
     reasoning `effort`. `alias` is not a role field: a role entry that carries
     one is rejected instead of reinstating a second source of truth for the same
     label.
3. **Deterministic Bidirectional Resolution:**
   - Resolution runs only after the whole configuration file has been decoded, so
     a role may reference an alias declared in the same file. Both reference forms
     normalize to a concrete `ModelRef` before any consumer observes them, and an
     unresolvable reference is reported as a configuration diagnostic that names
     the offending role.
4. **Rationale (Why):**
   - A display name is a property of the model, not of the role that model happens
     to fill. Declaring it once keeps one model from appearing under different
     labels across roles and lets a project rename a model everywhere by editing a
     single line.

---

## 11. Single In-Memory Formatting Contour

1. **One Formatting Surface:**
   - Formatting is declared once — by `[project.format]` in `tauqe.toml`, or by the
     `format` recipe the declared task runner exports when that entry is omitted —
     and it runs exclusively while the change set lives in virtual staging.
   - The filter receives the staged text of one file on standard input and returns
     the rewritten text on standard output, so the file is formatted before any byte
     reaches disk and before Git observes the change set.
2. **No Disk Formatting Stage:**
   - Nothing reformats a verified working tree: there is no formatting stage after
     the change set is written, and no compensation layer that would restore
     formatter modifications afterwards. A formatter that ran later would introduce
     a diff the turn never proposed.
3. **Fail-Safe Outcome:**
   - A non-zero exit status, a timeout, a cancellation, an unresolvable declaration,
     or a filter that emits nothing while the staged text was not empty keeps the
     authored content and reports a compact reason. The turn continues, and the
     failure never re-prompts the model.
4. **Rationale (Why):**
   - Formatting improves the shape of a change; it never proves its correctness. It
     therefore cannot sit on the verification gate, and confining it to staging
     keeps one set of bytes — the formatted ones — travelling into verification and
     into the atomic commit.
