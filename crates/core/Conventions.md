# Core Engine Conventions

This document specifies mandatory architectural and operational conventions for the Tauqe Core engine (`crates/core`).

---

## 1. Protocol Architecture: Enforced Structured Output

All interaction between Tauqe and large language models is governed exclusively through enforced Structured Output (`response_format: json_schema`). Free-form text protocols, ad-hoc regex scrapers, and XML tag parsing for machine-consumed commands are strictly prohibited.

### 1.1. Mandatory Invariants
- **Deterministic Schema Conformance:** Every model turn interacts through typed JSON schema validation, guaranteeing compile-time schema conformance and deterministic deserialization.
- **Prompt Injection Defense:** Constrained decoding at the token level prevents prompt injection, instruction drift, and syntax discrepancies without requiring cryptographic turn markers or ad-hoc tag scanning.
- **Decoupled Reasoning and Payloads:** Ambient cognitive reasoning (`message`, `scratchpad`) is isolated from structural operational commands (`action`).
- **Covered Subsystems & Operations:**
  1. **Interactive REPL & Discussions:** Natural language dialogue with optional typed `plan_update` or `review_update`.
  2. **Code Modification:** Virtual in-memory staging, atomic search/replace patch application, and verification healing.
  3. **Code Review Synthesis:** Generating audit findings, severity classifications, and file locations.
  4. **Commit Message Generation:** Synthesizing squash or checkpoint commit summaries.
  5. **History & Plan Compaction:** Context summarization, state compaction, and progress checkpoints.

### 1.2. Mutually Exclusive Operational Branches
- **One response follows exactly one branch.** A turn surface decomposes into the invariant ambient channel — the turn scratchpad — and into mutually exclusive operational branches (`research`, `edit`, `plan`, `review`). The conversational channel belongs to the branches: every operational branch declares the single `message` field of the response, so a composed wire carries exactly one `message` property because exactly one branch is selected. The name belongs to the answer alone: the error channel is the separate `error_message` property a branch declares beside it, so a failed turn reports through `error_message` while the user-facing answer keeps its one name. A discovery or planning branch reports through a short description of the action it performs, while a branch that answers or changes the working tree carries the full explanation. The answer language is not a channel of its own: the shared trunk states it once as an unconditional rule that user-facing output follows the language of the developer's request.
- **Grammatical Branch Enforcement:** The structured protocol expresses mutual exclusion grammatically via `action.anyOf`, where each branch is a strict object carrying a constant `kind` discriminator. A response selecting `research` contains no `changes` or `commit_message` property, and a response selecting `edit` contains no flat discovery properties (`context_requests`, `repo_searches`, `repo_dirs`, `repo_maps`, `dir_lists`, `tauqe_doc_requests`, `outer_doc_requests`) or `plan_update`. Terminal signals travel as flat fields on the branch (`commit_message`, `error_message`, `reason`), each one named by the branch that declares it.
- **Dialogue branches carry dialogue, not step signals.** A dialogue declares neither a `blocked` nor an `abort` branch: both escape hatches answer a condition only an unattended turn has to resolve, so they belong to the autonomous step contour alone. A dialogue branch also declares no step-termination signal, and only the branch that applies a change set carries `commit_message`, so a conversation is closed by the answer it was asked for instead of the vocabulary of a batch step.
- **Plan payloads never share one level.** A dialogue anchored on a plan advertises both plan capabilities, so its plan branch wraps them in one discriminated union (`plan_action`) whose `kind` values are `create` and `modify`. `plan_create` and `plan_update` are therefore never declared at the same level and never become simultaneously required; a free dialogue advertises plan authorship alone and carries the leaf directly.
- **Single Source of Truth:** `CapabilityExpr` projects an advertised surface onto its ambient part and its branches, and every consumer derives its behavior from that projection: the structured schema nests the branches under the discriminator, and the same tree decodes the response, so the branches the schema declares are the branches the decoder reads (§1.3).
- **The schema alone guarantees the exclusion.** Branch exclusion is enforced by the strict schema and by nothing else: a round that selects one branch carries no property of another because the schema forbids it, never because a prose rule restates the shape. The contract therefore speaks semantics — the role, the task, the engineering discipline — and never enumerates a branch or a field the schema already declares.
- **The engine never discards a change set a round produced.** A mixed round is a protocol defect rather than a command. Precedence belongs to the change set: a proposal about file content is checked by staging and verification, whereas a request for more material is checked by nothing, so discarding the proposal would let a turn that produced code reach the boundary looking like a turn that produced nothing.
### 1.3. Direct Capability Functions and the Capability Expression Tree
- **Direct mapping per atomic capability.** Each atomic `LlmCapability` maps directly in `capabilities/schema.rs`, `capabilities/decode.rs`, and `capabilities/rules.rs`: `capability_schema(cap, ctx)` projects the `schemars`-derived strict JSON schema fragment for that capability, `decode_capability_expr(expr, node, ctx)` decodes its commands, and `contract_rule` / `delta_rule` / `structured_instruction` render its prompt rules and field instructions.
- **One tree renders and decodes.** The strict `CapabilityExpr` tree is the single source of both the response schema and the parsing of the response. `capability_expr_schema(expr, ctx)` recursively walks the tree — calling `capability_schema` for each `Leaf`, merging properties for `Set`, and building an `anyOf` discriminated union with `kind` when `AnyOf` has two or more branches — and `decode_capability_expr(expr, node, ctx)` walks the same tree synchronously with the JSON response into a flat `Vec<LlmCommand>`, descending into `node[label]` by `kind` (returning `ProtocolError` on an unknown or missing `kind`) and deserializing each `Leaf` directly from the current `node`. Parsing reads exactly the surface the schema declares, so a command outside the active branch is never extracted from a response.
- **Branch labels are data.** The label of an operational branch is `CapabilityBranch::kind`, set by the contour that builds the tree. Sniffing leaves for a label and special-casing branch names by string are prohibited (root `Conventions.md` §1.5).
- **No flat capability set.** The only flat form of a surface is the leaf projection (`CapabilityExpr::leaves`, `Vec<LlmCapability>`); a tree is never reconstructed from a flat list. Every control loop — the interactive dialogue, the bounded turn, and the architect synthesis — declares its surface as one tree, and the former `CapabilitySet` type is deleted rather than reintroduced.
- **The control loop owns the whole tree.** Each loop function is the single declaration of its contour's surface: `repl_capability_expr` in `control/repl.rs`, `turn_capability_expr` in `control/turn.rs`, and the synthesis tree in `control/refine.rs`. A protocol surface is narrowed by intersecting it with that very tree, and no contour declares a second tree.
- **The regime is a typed parameter, never a type.** The differences between contours travel as explicit typed parameters — whether the prompt carries the `<history>` block, whether plan mutation is authorized, and which prompt layout the turn runs under — so no enumeration of turn intents exists, and no rule is composed against a remainder of a surface.
- **Descriptions are mandatory.** Every command and every property of every schema fragment carries a non-empty `description`, which is the control prompt of constrained decoding and the contract documentation at once. A property without a description is a defect, and a unit test walks the schema of every `LlmCapability` and composed surface to enforce it.
- **The contour brief belongs to its governing loop.** The introductory brief of a contour — its kind, its role, the paradigm it serves, and the task it carries — is a typed `TurnBrief` that the loop function declares (`run_repl_loop`, `run_plan_step_loop`, `run_structured_refine`, and the server path of a single step), never text selected by a boolean flag. `PromptAssembly` renders the brief it receives and gates the mode-specific rules on the typed kind that brief carries; `is_isolated_step_surface` heuristics and `is_isolated_step` flags are eliminated across all loops, builders, and assemblies.

---

## 2. PromptProtocol Interchangeability Invariant

1. **Unified PromptProtocol Contract:**
   - The workflow engine, control loops, and server handlers interact with LLM communication strictly through the generic `PromptProtocol` trait contract (`capabilities()`, `system_instructions()`, `response_format()`, `decode()`, `create_stream_filter()`).
   - Hardcoding protocol-specific control paths in workflow loops or turn execution routines is strictly prohibited. `JsonPromptProtocol` is the canonical implementation of the trait, leaving `PromptProtocol` as the single extension point.
2. **Unified Semantic Representation (`Vec<LlmCommand>`):**
   - Every protocol implementation is responsible for completely decoding raw model outputs into strongly typed semantic commands (`LlmCommand`), including text answers, file edits, plan operations, discovery requests, and terminal outcomes.
   - Consumers of protocol output must never inspect raw wire markup or unparsed JSON keys.
3. **Clean Streaming Isolation:**
   - Every protocol implementation must provide an `EditStreamFilter` that isolates the user-facing interface from protocol scaffolding.
   - The target textual response field (`message`) must be streamed on the fly via `StreamEvent::TextDelta` with real-time unescaping, while structural metadata (`action`, `changes`, `plan_update`, `error_message`, `commit_message`, `reason`) remains completely hidden from the REPL stream.
   - The filter locates the single `message` field and the `changes` array at whatever depth the payload declares them — at the root of the response or inside `action` — because a branched wire nests them in the branch the round selected. It never models the branch envelope: uniqueness of `message` is a property of the capability tree (§1.2), so one filter serves a flat and a branched wire without knowing which it received. The error channel is metadata, not an answer: `error_message` is never streamed as the response text, so a failed turn cannot impersonate a written reply.
4. **Declarative Prompt Composition:**
   - The system prompt is composed directly from the enabled capabilities, never from a monolithic template. `compose_contract_rules` folds the shared trunk, the rules the active contour brief selects, and every enabled capability's rule through one `ContractContext`, and `compose_structured_protocol_instructions` folds every enabled capability's instruction through one `PromptRenderContext`, both in `crates/core/src/capabilities/rules.rs`.
   - `compose_structured_protocol_instructions` is the single source of the instruction field list: every instruction derives from the capability tree of the loop, so a narrowed tree removes a property from the instructions exactly as it removes it from the schema. A protocol whose mode differs renders its own heading and folds the same composition, so it never carries a hand-written list of fields its active tree does not declare.
   - The strict structured schema folds through the same `PromptRenderContext` as the protocol instructions, so a capability whose rule is conditional on the project declaration or patch strategy declares its property under the same condition.
   - **Single Source of Truth for Structure:** The strict JSON schema (derived via `schemars` and `capability_expr_schema`) is the authoritative single source of truth for response structure: field names, types, nesting, requiredness, and property descriptions. The prompt text is strictly reserved for engineering role, task semantics, and discipline invariants; it never re-enumerates schema fields or restates structural JSON schemas.
   - **Conditional Section Gating:** Specialized documentation and tooling sections are rendered only when authorized by the active capability tree: the external documentation catalog and its contract rule appear strictly when `OuterDocRequest` is present, and project custom tools strictly when `CustomTools` is enabled.
   - **Elimination of Host and Protocol Metadata:** The prompt omits ambient host file-system paths, Git transaction mechanics, and protocol implementation constants (`Project Metadata` block is eliminated); the model interacts with the codebase strictly through repository-relative paths and the provided `<context>` surface.
   - The text rule follows the capability that declares it: a surface declaring `TextMessage` renders the conversational-response rule, and a surface declaring `ShortDescription` renders the concise-explanation rule. A dialogue that carries both in different branches states both in its contract, and the branch a round selects decides which of them its `message` obeys; neither rule widens the field, which is declared once per branch (§1.2).
   - A disabled capability contributes no rule, no instruction, and no schema property. Prompt text and decoding derive from the same `CapabilityExpr`.
   - **A rule travels with the functionality it describes.** A capability, a project declaration, or a contour that changes shape updates its rule, its instruction, and this document in the same step, so no rule outlives the behaviour it explains and no behaviour ships without the rule that states it.
   - **Contract Rule Composition:** The core system contract is assembled as an ordered sequence of typed `ContractRule { title, body }` values and rendered by one loop with monotonic numbering `1..N`. Rule numbers are never written into string literals.
5. **Polymorphic Research Dispatch:**
   - Research phase execution dispatches through registered handlers implementing `ResearchCapabilityHandler` (`crates/core/src/workflow/research.rs`). Each handler owns the validation, disk work, and feedback text of its own capability.
   - Findings that are not repository files accumulate in the turn-scoped `TurnResearchJournal` (`crates/core/src/workflow/research/journal.rs`), an append-only tail of the user message that survives the whole turn.
6. **Delivery Declared by Capability:**
   - `CapabilityDelivery` is a property of the capability itself: `LiveStreaming` for the contours consumed while tokens arrive, `RoundDeferred` for the payloads decoded once the round finishes.
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

5. **Plan Steps Journal Two-Sided, Budget-Bounded Cards:**
   - An isolated plan step records a card on both sides of its turn: the structured execution
     prompt that opened it, and a compact result that closed it — the status of the step, the
     commit it produced, the files it changed, and the first reported line of the reason or
     summary. The full reply of the model is never recorded for a step.
   - Because a batch executes many steps, a step turn passes through the same budgeted
     compaction as a dialogue (`compact_history_if_needed_with_budget`), so the journal is
     bounded by the configured token budget instead of the length of the batch.
   - The typed history parameter of the loop governs rendering only: it keeps the `<history>`
     block out of the prompt a step assembles, and it never suppresses the record the step
     writes to the journal.

---

## 4. Determinism & Toolchain Guardrails

1. **Never Guess Deterministic Data:**
   - If an artifact can be resolved from Git, Tree-sitter, project manifests, or compiler output, resolve it through deterministic Rust routines, never LLM generation.
2. **Fail-Fast on Verification Failure:**
   - Autonomous execution pipelines must abort immediately upon encountering compiler errors or test failures, rolling back dirty working trees to the pre-turn Git checkpoint.
k4Nb | 3. **Rejected Paths Are Reported, Never Retried Blindly:**
kgUZ |    - A change set the substrate refuses because a path is excluded by the repository ignore rules never reaches disk, so the engine reports the typed rejection to the model and lets it choose a tracked path within the same turn. The staging rules themselves are declared once in `crates/tools/Conventions.md`.
     | 4. **Post-Hook Failures Trigger Verification Healing:**
     |    - The declared post-hook (`[project.post_hook]`) runs immediately after proposed edits reach disk to generate code or refresh manifests. A failing hook (non-zero exit code or compilation error) is captured as a verification failure (`VerificationStatus::Failed`) rather than an unrecoverable disk error, routing diagnostics into the Verification Healing loop so the model can fix the generator within its retry budget.
---

## 5. Instant Provider Cancellation

1. **Cancellation Before Response Headers:**
   - A provider network request (`http_client.send().await`) must race the turn cancellation signal (`cancel_rx`) through `tokio::select!` while awaiting the HTTP response headers, not only while consuming the streamed body.
2. **The Completion Path Inherits the Same Discipline:**
   - A phase that runs after the streamed body has been delivered is still part of the turn. The best-effort generation-stats fetch that resolves the reported cost races the same `cancel_rx` and carries its own short per-request `.timeout()`, sized well below the shared client's read budget.
   - The fetch is optional by contract: a cancelled, slow, hung, or unreachable endpoint yields no enrichment and the stream completes normally. It must never be the reason a turn waits.
3. **Rationale (Why):**
   - While a model is reasoning during the `Proposal` and `Awaiting` phases, no body bytes have arrived yet. A request that observes cancellation only once the body starts streaming forces the developer to wait for the provider to finish before the turn can abort. Racing the initial send against `cancel_rx` makes interruption immediate in every phase.
   - The same argument holds after the last delta. The answer is already delivered, so a post-stream fetch that ignores the cancellation signal and inherits a multi-minute read budget turns a finished result into an unresponsive turn, and a hard cancel into a request nobody can stop.
4. **Uniform Reporting:**
   - When cancellation wins the race before headers arrive, or while the completion path runs, the provider emits the same `StreamEvent::Cancelled` used everywhere else and returns cleanly, so callers never branch on a provider-specific cancellation path.

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
     code, or `crate::blocking::run_blocking_async` from async code when the
     closure is `Send + 'static`; it reports a failed offload as a typed error
     that proves whether the closure ran, never as the join error of its task.
     Both helpers are
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
   | Core system contract, protocol instructions | system head | system head |
   | Repository outline (symbol map or directory tree, never both) | system head | system head |
   | Context files | one `<context>` block in the system message | `<context>` groups interleaved with the history records |
   | Session history | one `<history>` block in the system message | one message per history record |
    | Active plan | system message tail | trailing user message |
    | Context manifest (paths and access, no content) | absent | trailing user message |
   | Turn scratchpad | system message tail | trailing user message |
   | Current task | final user message | final user message |

   - Both policies keep the same three-part shape: a byte-stable head, the
     material that legitimately grows, and an isolated volatile tail.
3. **Byte-Stable Head:**
   - The head carries only blocks that are identical from one round of a turn to
   - The head carries only blocks that are identical from one round of a turn to
     the next: the introduction, the Core System Contract, the protocol
     instructions, and the repository outline.
     instructions, the project metadata, and the repository outline.
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
   - Research widens the context by appending newly discovered files. Under
     `SemanticLayout` it is appended at the tail; under `ChronologicalLayout` it
     is appended after the records already in
     the transcript. Previously rendered elements keep their exact bytes.
   - Promoting a file from read-only to editable rewrites only that file's own
     `access` attribute in place; it never relocates the element or reflows its
     neighbors.
   - The chronological transcript is ordered by the insertion watermark every
     context entry carries (`ContextItem::insertion_seq`). A file is placed after
     the prompt record of the turn during which its watermark was allocated: the
     policy compares the file's `insertion_seq` against the `context_watermark`
     each `HistoryEntry::Turn` persisted and selects the last turn that started
     before the file arrived. That slot is stable on every later turn, so a turn
     adopting a mid-turn arrival appends its own records behind it instead of
     moving it. A file no watermark-bearing turn bounds keeps the recorded
     starting context of the first turn that listed it, and a file that carries no
     watermark at all closes the transcript.
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
     1. When shrinking or clearing the context block: the post-turn release of
        the transient context (`AppState::clear_transient_context`, backed by
        `ContextManager::clear_transient`), an explicit clear of the Auto or the
        OuterDocs layer, or a full reset through `AppState::clear_all_context`.
        A fresh session opens a prefix no earlier request warmed; keeping the
        old identifier would address a node whose cached prefix the dialogue no
        longer sends. The rotation follows the boolean the underlying clear
        reports, so a no-op clear preserves the warm session.
     2. When the effective model changes (`maybe_rotate_session_on_model_change`).
        Provider KV caches and routing affinities are physically model-specific.
        Reusing a session identifier across different models produces guaranteed
        cache misses or affinity routing conflicts. Changing only reasoning
        effort or reselecting the identical model preserves session affinity.
   - Focus transitions never rotate the identifier. Rotation reads exactly two boundaries — the context block and the target model — and a focus moves neither, so setting a focus, replacing it, switching between a plan and a review target, starting a dialogue from the Plans or Review view, or clearing the focus keeps the provider session warm. The head states the capability surface the dialogue loop declares, so a focus that authorizes a capability the previous surface did not carry rewrites the head once, exactly as any other change of surface does.
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
    - A focus transition is a non-event for the session for the same reason. The context block and the target model are what the identifier is bound to, and a focus moves neither, so the dialogue keeps the node cache its next request can still match; a head a newly declared intent rewrites is reprocessed once by the provider, which costs less than a cold session for every later round.
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

## 11. Substrate Boundaries

1. **Deterministic Substrate Lives in `crates/tools`:**
   - Virtual staging and its patch algorithms, atomic file writes, Git
     transactions and checkpoints, the toolchain command runners and the
     task-runner integration, and the Tree-sitter repository map are executed by
     `tauqe-tools`.
   - The engine adapts configuration into that substrate and reports its typed
     results; it never restates or re-implements an operation the substrate owns.
2. **Single Declaration of Substrate Invariants:**
   - Staging transactionality, atomic-write durability, process isolation, the
     repository ignore rules, and the single in-memory formatting contour are
     declared once in `crates/tools/Conventions.md`.
   - A core rule describes only the engine-side consequence of a substrate
     decision, so the two documents can never disagree about the same invariant.

---

## 12. Transient Context Lifecycle

1. **One transient context, two layers:**
   - The Auto layer and the OuterDocs layer form one transient context. The files
     a research round discovered and the documents the external documentation
     registry materialized are provided for the same reason — so the model can
     decide — and they are released for the same reason once that decision is
     durable.
   - No rule of this document treats one of them as persistent while the other
     is transient. A claim that a snapshot survives the task, or that only a
     full context reset drops it, contradicts this section.
2. **One release primitive:**
   - `ContextManager::clear_transient` drops both layers, recomputes the
     hierarchical pinned entries, advances the context revision, and reports
     whether anything was removed. It is the only primitive an automatic
     boundary calls.
   - `clear_auto` and `clear_outer_docs` remain available to an explicit
     per-layer developer action and to tests, and neither of them releases the
     transient context on a turn boundary: releasing one layer while keeping the
     other would leave resident material the turn already paid for.
3. **Released on commit, kept until then:**
   - `TransactionRollbackGuard::finalize` releases the transient context on the
     commit path, after the change set is applied and verified, so the material
     that served one decision never inflates the next one.
   - A rolled-back, blocked, or cancelled turn only prunes context paths that no
     longer exist on disk, so the research a retry still needs survives the
     failed attempt.
   - Inside the REPL the transient context grows across the rounds and replies of
     one task until the model edits code: the boundary is the committed change
     set, never the round.
4. **Isolated boundaries:**
   - A plan step, a review finding run, and an Architect wave start from a clean
     transient context through
     `crate::handlers::common::cleanup_transient_context` and release it when the
     step ends, so no discovered file or external document of a previous step
     leaks into the next one.
   - The declared target files and the declared `outer_docs` entries of a step
     reach the context after that sweep, so the isolation holds without
     weakening the preload contract.
5. **Session affinity follows the release:**
   - A release that actually removed material shrinks the stable prompt prefix,
     so the dialogue session identifier rotates with it: the `AppState` clearing
     facade reports the boolean that rotation reads. A release that removed
     nothing leaves both the prefix and the session warm.
6. **The cache outlives the entry:**
   - Releasing the context never deletes a materialized snapshot. The cache under
     `.tauqe/cache/outer_docs/` stays immutable, and a later request re-admits the
     same bytes without a transfer, so the durable cache and the transient entry
     never merge into one lifetime.
7. **Only an explicit request deletes a cached snapshot:**
   - No automatic boundary deletes a materialized snapshot (rule 6). The
     durable cache is removed only by an `outerdocs/cacheRemove` request that
     names a logical path or a logical directory of the cache.
   - The path is validated before a byte moves: an empty request names no
     document, and `is_clean_relative_path` refuses a parent directory, an
     absolute path, a backslash, and a foreign scheme, so a request can never
     reach material the declaration did not publish. The cache directory
     itself is never removed.
   - Every file is unlinked atomically and every directory the removal
     emptied is pruned afterwards, so the inventory never reports a directory
     that no longer holds a document.
   - The resident OuterDocs layer is reconciled in the same request: a
     document the cache no longer holds leaves the layer, and the release
     reports whether it really held one. A release that took a document away
     rotates the dialogue session identifier with the prefix it shrank
     (rule 5); a release that took nothing keeps the warm session. The
     removal claims the operation slot, so no turn merges the removed
     documents back afterwards.
   - `outerdocs/cacheList` answers the same logical paths, so the interface
     and the model address one document by one name and the disk prefix
     reaches neither.
8. **Rationale (Why):**
   - Discovery material is bought for one decision. Leaving it resident after that
     decision is durable pays for it on every later request of a prompt prefix
     that no longer needs it, and it lets a finished step's documents silently
     shape the next step's reasoning.
   - Releasing only the repository half of that material would make the two layers
     disagree about one lifetime, which is why the release is defined once, over
     the transient context as a whole.

The concept chapter `docs/src/ch02-perception/reference-registry.md` owns the declaration,
the cache, and the trust boundary; this section states only the engine-side consequence of
the lifecycle those mechanisms share.

---

## 13. Functional Control Loops Architecture (`crates/core/src/control`)

1. **Functional Decomposition over Single-Method Traits:**
   - Execution flows are implemented strictly as nested, composable plain `async` functions adhering to the canonical naming standard: `run_<domain>_loop` (`run_sequence_loop` -> `run_step_loop` -> `run_edit_loop` -> `run_staging_loop`).
   - Single-method execution traits (e.g. `ExecutionLoop`) and dynamic trait dispatch (`Box<dyn ...>`) are strictly forbidden. Control flow is composed functionally through direct asynchronous function calls.

2. **Canonical Execution Context (`LoopContext`):**
   - Ambient operational infrastructure is encapsulated in a lightweight `LoopContext` passed by mutable reborrow:
     - `cancel: &CancellationToken` — cooperative cancellation token raced across network and toolchain futures via `tokio::select!`.
     - `events: &EventSink` — typed domain event emitter isolating the domain engine from TUI, sockets, and transport channels.
      - `capabilities: CapabilityExpr` — active model capability surface for the current execution branch.
     - `repo_root: &Path` — root of the working tree every loop addresses.
     - `context_manager: &mut ContextManager` — live context a loop mutates (e.g. during research context expansion) and snapshots through.
      - `journal: &mut TurnResearchJournal` — turn-scoped cumulative epistemic journal collecting findings and scratchpad notes.
      - `policy: &mut dyn ApplyPolicy` — transactional apply policy governing checkpointing, staging, step commits, and rollback.
      - `capabilities: CapabilityExpr` — active model capability surface for the current execution branch.
      - `prompt_builder: PromptBuilder` — model round assembly and execution builder.
   - **Prohibition of `Clone` on `LoopContext`:** `LoopContext` must NEVER derive or implement `Clone`. It holds exclusive mutable references to execution state (`context_manager`, `journal`). Subordinate loops obtain narrowed or extended contexts exclusively via explicit reborrowing (`ctx.reborrow_with_capabilities(...)`, `ctx.reborrow_extending_capabilities(...)`).

3. **Principle of Least-Privilege Configuration Cascading:**
   - Passing monolithic `AppConfig` or broad global configurations into control loops is strictly prohibited.
   - Domain configurations cascade downward, narrowed exclusively to the immediate parameters the specific loop governs (`SequenceLoopConfig` -> `StepLoopConfig` -> `EditLoopConfig`).
   - Loops that require no local tuning parameters omit the configuration parameter entirely (`Option<&Config>` or omitted).

4. **Input Continuity Bridging (`initial_response`):**
   - To eliminate code duplication between conversational REPL turns and headless batch step execution, `run_edit_loop` accepts `initial_response: Option<LlmProposal>`.
   - When an interactive turn has already generated a candidate code proposal, `run_edit_loop` skips prompt assembly and LLM generation, proceeding directly to virtual in-memory staging and toolchain verification. When `None`, it initiates execution from the research/prompting phase.

5. **Three-Tier Outcome Classification & Drainable Response Envelope (`LoopResult<T>`):**
   - Control loops return `Result<LoopResult<T>, SystemError>` to cleanly decouple infrastructure failures from domain outcomes:
     - `Err(SystemError)` — Infrastructure failure (exhausted network retries, Git repository corruption, disk I/O failure, cancellation). System errors **never consume model attempts** and immediately propagate upward for transaction rollback or pause.
      - `Ok(LoopResult::Success(LoopSuccess<T>))` — Target artifact `T` successfully obtained and validated (e.g. `StagedEdits`). Wraps the drainable `ResponseEnvelope` carrying remaining companion capabilities.
      - `Ok(LoopResult::Diverged(ResponseEnvelope))` — Legitimate alternative output permitted by active capabilities (e.g. `<blocked>`, explanation text) that the local loop does not govern. The envelope is forwarded upward untouched for higher-level replanning or user display.
     - `Ok(LoopResult::Exhausted(FailureReport))` — The model attempted the target task, but local iterations (e.g. patch-retries or verification healing) exceeded their budget. Signals the caller to decrement the coder tier attempt and escalate.
    - **Drainable Envelope Pattern (Consumable `ResponseEnvelope`):**
      - Rather than a monolithic "God-struct" with static fields for every conceivable command, `ResponseEnvelope` is an envelope of semantic commands indexed by `LlmCapability` (`cmd.capability()`).
     - Control loops consume only the capabilities in their domain via `envelope.take(LlmCapability)` or `envelope.take_any(&[...])`. As the envelope travels upward through the loop hierarchy, it shrinks ("худеет").
      - `run_staging_loop` drains the file-edit capabilities (`EditReplace`, `EditCreate`, `EditDelete`, `EditMove`, `EditMoveDir`, `EditOverwrite`); `run_edit_loop` drains `LlmCapability::ToolchainVerify`; `run_step_loop` drains the terminal outcome capabilities (`CommitMessage`, `TurnError`, `TurnSkip`); `run_repl_loop` drains `LlmCapability::TextMessage`, `PlanCreation`, and `PlanUpdate`.
      - **Prohibition of Ad-hoc Convenience Extractors:** `ResponseEnvelope` provides strictly generic capability-based draining (`take` and `take_any`). Adding typed helper methods for specific capabilities (such as `take_edits`, `take_text`, `take_outcome`, `take_blocked`, etc.) to `ResponseEnvelope` is strictly forbidden. Each loop or subsystem knows exactly which `LlmCapability` variants belong to its domain, calls `take(...)`, and pattern-matches the resulting `LlmCommand` items locally.
   - **Top-Level Anti-Leak Invariant:**
     - At the top-level turn completion boundary, `envelope.is_empty()` MUST be true. Any remaining commands (`envelope.remaining_capabilities()`) indicate commands emitted by the model that no level of the control hierarchy handled, triggering structured diagnostic logging.

6. **Protocol Sanitization Boundary:**
   - Model output violating structural contracts (unrecognized tags outside `capabilities`, invalid JSON/XML syntax, mixed operational branches) is a protocol defect, not a semantic divergence.
   - Loops intercept protocol violations locally via dedicated sanitizers (`heal_protocol_violation`), requesting format repair without leaking raw defects into domain logic.

7. **Strictly Localized Transaction and Error Boundaries:**
   - Virtual patch-application and anchor-matching retries are confined to `run_staging_loop`.
   - Compiler verification and self-healing iterations are confined to `run_edit_loop`.
   - Git checkpoints, coder ladder attempt accounting, gatekeeper acceptance, and commit squashing are confined to `run_step_loop`.
   - Task ordering, dependency evaluation, and architect replanning triggers are confined to `run_sequence_loop`.