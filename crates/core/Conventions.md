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
- **One response follows exactly one branch.** A turn surface decomposes into the invariant ambient channels — the turn scratchpad and the single `message` field — and into mutually exclusive operational branches (`research`, `edit`, `plan`, `review`). The conversational message is ambient: it sits at the root of the response beside the scratchpad, is present at every stage of the turn whatever branch the `action` union selects, and is never duplicated inside a branch, so a composed wire carries exactly one `message` property. The name belongs to the answer alone: the error channel is the separate `error_message` property a branch declares beside it, so a failed turn reports through `error_message` while the user-facing answer keeps its one name. The `answer` branch is a bare marker that owns no field of its own: a round with nothing to act on names the branch and writes the ambient message. The answer language is not a channel of its own: the shared trunk states it once as an unconditional rule that user-facing output follows the language of the developer's request.
- **Grammatical Branch Enforcement:** The structured protocol expresses mutual exclusion grammatically via `action.anyOf`, where each branch is a strict object carrying a constant `kind` discriminator. A response selecting `research` contains no `changes` or `commit_message` property, and a response selecting `edit` contains no flat discovery properties (`context_requests`, `repo_searches`, `repo_dirs`, `repo_maps`, `dir_lists`, `tauqe_doc_requests`, `outer_doc_requests`) or `plan_update`. Terminal signals travel as flat fields on the branch (`commit_message`, `error_message`, `reason`), each one named by the branch that declares it.
- **Dialogue branches carry dialogue, not step signals.** A dialogue declares neither a `blocked` nor an `abort` branch: both escape hatches answer a condition only an unattended turn has to resolve, so they belong to the autonomous step contour alone. A dialogue branch also declares no step-termination signal, and only the branch that applies a change set carries `commit_message`, so a conversation is closed by the answer it was asked for instead of the vocabulary of a batch step.
- **Plan payloads never share one level.** A dialogue anchored on a plan advertises both plan capabilities, so its plan branch wraps them in one discriminated union (`plan_action`) whose `kind` values are `create` and `modify`. `plan_create` and `plan_update` are therefore never declared at the same level and never become simultaneously required; a free dialogue advertises plan authorship alone and carries the leaf directly.
- **Single Source of Truth:** `CapabilityExpr` projects an advertised surface onto its ambient part and its branches, and every consumer derives its behavior from that projection: the structured schema nests the branches under the discriminator, and the same tree decodes the response, so the branches the schema declares are the branches the decoder reads (§1.3).
- **The schema alone guarantees the exclusion.** Branch exclusion is enforced by the strict schema and by nothing else: a round that selects one branch carries no property of another because the schema forbids it, never because a prose rule restates the shape. The contract therefore speaks semantics — the role, the task, the engineering discipline — and never enumerates a branch or a field the schema already declares.
- **The engine never discards a change set a round produced.** A mixed round is a protocol defect rather than a command. Precedence belongs to the change set: a proposal about file content is checked by staging and verification, whereas a request for more material is checked by nothing, so discarding the proposal would let a turn that produced code reach the boundary looking like a turn that produced nothing.
### 1.3. Direct Capability Functions and the Capability Expression Tree

Normalized model commands and their schema definitions belong to
`core::llm::command`. RPC DTOs must not depend on `LlmCommand`; conversion from
validated edit operations is owned by core.

`core::llm::ModelResultProposal` and `DocTopic` are owned model-output types.
The outer `core::llm::ModelResult` has no serde contract. Server explicitly maps
its edit operations, nested proposal, and application outcome into independent
protocol DTOs when publishing RPC events. Never drop the serialized proposal or
add a transport dependency to core.

The operational `CapabilityTree`, `LlmCapability`, and `command_capability`
classification belong to `core::llm`, not the RPC contract.
Its composition, projection, and filtering rules are engine policy. Callers
consume the core type directly; protocol must not re-export it.
- **Direct mapping per atomic capability.** Each atomic `LlmCapability` maps directly in `capabilities/schema.rs`, `capabilities/decode.rs`, and `capabilities/rules.rs`: `capability_schema(cap, ctx)` projects the `schemars`-derived strict JSON schema fragment for that capability, `decode_capability_expr(expr, node, ctx)` decodes its commands, and `contract_rule` / `delta_rule` / `structured_instruction` render its prompt rules and field instructions.
- **One tree renders and decodes.** The strict `CapabilityExpr` tree is the single source of both the response schema and the parsing of the response. `capability_expr_schema(expr, ctx)` recursively walks the tree — calling `capability_schema` for each `Leaf`, merging properties for `Set`, and building an `anyOf` discriminated union with `kind` when `AnyOf` has two or more branches — and `decode_capability_expr(expr, node, ctx)` walks the same tree synchronously with the JSON response into a flat `Vec<LlmCommand>`, descending into `node[label]` by `kind` (returning `ProtocolError` on an unknown or missing `kind`) and deserializing each `Leaf` directly from the current `node`. Parsing reads exactly the surface the schema declares, so a command outside the active branch is never extracted from a response.
- **Branch labels are data.** The label of an operational branch is the label of its `CapabilityTree` node, read through `CapabilityTree::label()` and declared by the contour that builds the tree. Sniffing leaves for a label and special-casing branch names by string are prohibited (root `Conventions.md` §1.5).
- **No flat capability set.** The only flat form of a surface is the leaf projection (`CapabilityExpr::leaves`, `Vec<LlmCapability>`); a tree is never reconstructed from a flat list. Every control loop — the interactive dialogue, the bounded turn, and the architect synthesis — declares its surface as one tree, and the former `CapabilitySet` type is deleted rather than reintroduced.
- **The control loop owns the whole tree.** Each loop function is the single declaration of its contour's surface: `repl_capability_expr` in `control/repl.rs`, `turn_capability_expr` in `control/turn.rs`, and the synthesis tree in `control/refine.rs`. A protocol surface is narrowed by intersecting it with that very tree, and no contour declares a second tree.
- **The regime is a typed parameter, never a type.** The differences between contours travel as explicit typed parameters — whether the prompt carries the `<history>` block and whether plan mutation is authorized — so no enumeration of turn intents exists, and no rule is composed against a remainder of a surface.
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
   - The filter locates the single `message` field and the `changes` array at whatever depth the payload declares them — at the root of the response or, as a tolerance for the previous wire shape, inside `action`. It never models the branch envelope: uniqueness of `message` is a property of the capability tree (§1.2), so one filter serves a flat and a branched wire without knowing which it received. The error channel is metadata, not an answer: `error_message` is never streamed as the response text, so a failed turn cannot impersonate a written reply.
4. **Declarative Prompt Composition:**
   - The system prompt is composed directly from the enabled capabilities, never from a monolithic template. `compose_contract_rules` folds the shared trunk, the rules the active contour brief selects, and every enabled capability's rule through one `ContractContext`, and `compose_structured_protocol_instructions` folds every enabled capability's instruction through one `PromptRenderContext`, both in `crates/core/src/capabilities/rules.rs`.
   - `compose_structured_protocol_instructions` is the single source of the instruction field list: every instruction derives from the capability tree of the loop, so a narrowed tree removes a property from the instructions exactly as it removes it from the schema. A protocol whose mode differs renders its own heading and folds the same composition, so it never carries a hand-written list of fields its active tree does not declare.
   - The strict structured schema folds through the same `PromptRenderContext` as the protocol instructions, so a capability whose rule is conditional on the project declaration or patch strategy declares its property under the same condition.
   - **Single Source of Truth for Structure:** The strict JSON schema (derived via `schemars` and `capability_expr_schema`) is the authoritative single source of truth for response structure: field names, types, nesting, requiredness, and property descriptions. The prompt text is strictly reserved for engineering role, task semantics, and discipline invariants; it never re-enumerates schema fields or restates structural JSON schemas.
   - **Conditional Section Gating:** Specialized documentation and tooling sections are rendered only when authorized by the active capability tree: the external documentation catalog and its contract rule appear strictly when `OuterDocRequest` is present, and project custom tools strictly when `CustomTools` is enabled.
   - **Elimination of Host and Protocol Metadata:** The prompt omits ambient host file-system paths, Git transaction mechanics, and protocol implementation constants (`Project Metadata` block is eliminated); the model interacts with the codebase strictly through repository-relative paths and the provided `<context>` surface.
   - The text rule follows the capability that declares it: a surface declaring `TextMessage` renders the conversational-response rule, and the single ambient `message` field is the one channel the rule governs on every contour — a dialogue and an isolated step alike. No rule widens the field, which the schema declares once at the root of the response (§1.2).
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
   - Any evolution of on-disk storage layouts must provide backward-compatible
     auto-migration from legacy single-file representations. Review migration runs
     during repository startup; entity reads remain free of write side effects.
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

## 9. Prompt Architecture: The Chronological Transcript and Provider Prefix Caching

1. **One Assembly Function, One Order:**
   - Turning a rendered `PromptAssembly` into the message sequence of a turn is a single
     free function, `assemble_chronological_messages`
     (`crates/core/src/prompt/layout.rs`), reached through
     `PromptAssembly::assemble_chat_messages` and
     `PromptAssembly::assemble_chat_messages_with_context_cache`.
   - No layout trait, no policy enumeration, and no selection step exists: the transcript
     is the only sequence a turn can be composed into, so a contour never chooses an
     assembly shape and no rule is composed against one.
2. **Block Order Belongs to That Function:**
   - The order in which the already-rendered blocks become messages is a property of
     that one function, not of the assembly and not of a contour.

   | Material | Placement |
   | --- | --- |
   | Core system contract, protocol instructions | system head |
   | Repository outline (symbol map or directory tree, never both) | system head |
   | Context files | `<context>` groups interleaved with the history records |
   | Session history | one message per history record |
   | Active plan | trailing user message |
   | Context manifest (paths and access, no content) | trailing user message |
   | Turn scratchpad | trailing user message |

   - The three-part shape stays the same in every turn: a byte-stable head, the material
     that legitimately grows, and an isolated volatile tail.
3. **Byte-Stable Head:**
   - The head carries only blocks that are identical from one round of a turn to the
     next: the introduction, the Core System Contract, the protocol instructions, and the
     repository outline.
   - The repository outline stays in the head. The dominant regime of a turn is
     exploration and planning, where the outline hardly changes, while the outline is
     also the largest block: placing it in the tail would pay a full reprocessing pass on
     every round instead of only on the turns that regenerate it.
   - The accepted price is explicit. A regenerated outline changes the head itself and
     therefore invalidates the whole tail, because a later turn renders an outline that
     excludes the files which entered the context. That divergence is a documented
     property of the outline, not an accident of the assembly.
   - Within a turn the outline is frozen (`TurnInputCache`), so no later round of the
     same turn rewrites it even after discovery widened the context.
4. **Contract Vocabulary Follows the Transcript:**
   - The Core System Contract describes the context surface in the dictionary of the
     transcript: the interleaved `<context>` groups and the trailing manifest.
   - The prompt-side description of the `<file>` element and of the `access` attribute is
     shared, because it describes payload the model reads rather than a command channel
     it emits.
5. **Deterministic Rendering:**
   - The prompt carries no runtime-varying content: no timestamps, no dates, no random or
     per-request identifiers, no context revision counters, no counters or latencies, and
     no toolchain output.
   - Every collection rendered into the transcript keeps a deterministic order (context
     files by insertion watermark, symbols and directories in their generated order), so
     identical state produces identical bytes.
   - Context files travel through one layout-agnostic `ContextBlockCache`, so an element
     rendered once is reproduced byte for byte.
6. **Append-Only Growth:**
   - Research widens the context by appending newly discovered files after the records
     already in the transcript. Previously rendered elements keep their exact bytes.
   - Promoting a file from read-only to editable rewrites only that file's own `access`
     attribute in place; it never relocates the element or reflows its neighbors.
   - The transcript is ordered by the insertion watermark every context entry carries
     (`ContextItem::insertion_seq`). A file is placed after the prompt record of the turn
     during which its watermark was allocated: the assembly compares the file's
     `insertion_seq` against the `context_watermark` each `HistoryEntry::Turn` persisted
     and selects the last turn that started before the file arrived. That slot is stable
     on every later turn, so a turn adopting a mid-turn arrival appends its own records
     behind it instead of moving it. A file no watermark-bearing turn bounds keeps the
     recorded starting context of the first turn that listed it, and a file that carries
     no watermark at all closes the transcript.
   - **A content change is a position event.** A file keeps the watermark it
     received while nothing changed it, but a change set that converged or was
     refused in virtual staging changes what the model must read next:
     `ContextManager::move_files_to_tail` re-issues the watermarks of the named
     paths and moves their entries to the tail of their context layers, so the
     chronological layout renders the hot group — the converged members first,
     the refused members after them — behind the last recorded turn instead of
     at the arrival slot. The relocation is the documented exception to the
     stable slot above, and the `merge_turn_context` max-watermark rule carries
     the re-issued position back into the primary state.
7. **Isolated Volatile Tail:**
   - All genuinely round-varying content is confined to the end of the message sequence:
     the active plan, the context manifest, and the turn scratchpad; round-specific
     diagnostics (patch retry reports, verification failures) travel as append-only
     extensions of the user message.
   - The context manifest carries paths and effective access only, never content. It keeps
     a complete view of what is in context while no content byte moves into the tail.
   - Nothing volatile is ever injected into an earlier block, because an early insertion
     invalidates every cached token that follows it.
8. **Documented Structural Invalidation Points:**
   - Appending grows the sequence at its tail and therefore preserves the cached prefix.
   - The structural mutations that legitimately move an earlier byte are named rather
     than discovered, and each of them bounds the invariant:
     1. Repository outline regeneration between turns, which rewrites the head.
     2. Access promotion, which rewrites one attribute of one element in place.
     3. Prune and rollback, which remove an element and shift every message after it;
        the removal is the purpose of the operation.
     4. History compaction, which replaces the opening of the transcript and rotates the
        dialogue session identifier together with the prefix it rewrites (rule 9),
        because the provider must not be asked to reuse a node cache whose prefix the
         dialogue no longer sends.
      5. Context churn relocation, which re-issues the arrival watermark of
         every path a change set converged on or was refused, so the reseeded
         prefix renders the rewritten files — with the anchors of their
         current bytes — behind the last recorded turn. The relocation is
         bounded by the named paths: every element the churn did not name keeps
         its exact bytes and its exact position, so the cache loss is confined
         to the stretch between the earliest relocated element and the tail.
9. **Dialogue Session Boundary:**
   - An interactive turn binds its provider requests to a dialogue session identifier,
     which is what a provider uses to route consecutive requests to the same cache-warm
     node.
   - While the context block stays byte-for-byte identical and the target model remains
     unchanged, the identifier stays too: a series of REPL answers that only reads context
     keeps one session, so every round reuses the prefix cache warmed by its predecessor.
   - Session invalidation and rotation occur at exactly two deterministic boundaries:
     1. When shrinking or clearing the context block: the post-turn release of the
        transient context (`Application::clear_context(ContextClearScope::Transient)`, backed by
        `ContextManager::clear_transient`), an explicit clear of the Auto or the
        OuterDocs layer, or a full reset through `Application::clear_context(ContextClearScope::All)`. A fresh
        session opens a prefix no earlier request warmed; keeping the old identifier
        would address a node whose cached prefix the dialogue no longer sends. The
        rotation follows the boolean the underlying clear reports, so a no-op clear
        preserves the warm session.
     2. When the effective model changes (`maybe_rotate_session_on_model_change`).
        Provider KV caches and routing affinities are physically model-specific. Reusing
        a session identifier across different models produces guaranteed cache misses or
        affinity routing conflicts. Changing only reasoning effort or reselecting the
        identical model preserves session affinity.
   - Focus transitions never rotate the identifier. Rotation reads exactly two
     boundaries — the context block and the target model — and a focus moves neither, so
     setting a focus, replacing it, switching between a plan and a review target,
     starting a dialogue from the Plans or Review view, or clearing the focus keeps the
     provider session warm. The head states the capability surface the dialogue loop
     declares, so a focus that authorizes a capability the previous surface did not carry
     rewrites the head once, exactly as any other change of surface does.
10. **Rationale (Why):**
    - A bounded turn issues several requests per turn: discovery rounds, the proposal,
      staging retries, and verification healing. Reusing the provider's cached prefix
      across those requests removes the dominant repeated cost and keeps multi-round
      turns economically viable.
    - The invariant is only effective when it holds byte-for-byte, so any dynamic
      content, reordering, or unexpected relocation of a block silently defeats it. One
      function makes the order explicit and reviewable instead of implicit in the
      assembly.
    - The transcript grows at its tail, which is why the lost semantic grouping is
      compensated by the compact context manifest instead of by reloading file bodies.
    - A session identifier that outlives its prefix or target model is worse than a cold
      one: it asks the provider to reuse a node cache the next request can never match,
      which is why rotation is bound strictly to context shrinking and model changes
      rather than to a timer or a round counter.
    - A focus transition is a non-event for the session for the same reason. The context
      block and the target model are what the identifier is bound to, and a focus moves
      neither, so the dialogue keeps the node cache its next request can still match; a
      head a newly declared intent rewrites is reprocessed once by the provider, which
      costs less than a cold session for every later round.
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

1. **One Turn Loop plus Step Functions:**
   - A turn has exactly one loop: `run_turn_loop` in `crates/core/src/control/turn.rs`. The phases of the turn are plain step functions it calls in sequence — `execute_research_step`, `execute_staging_step`, and `execute_edit_step` — and each of them executes exactly one iteration and requests no model round of its own.
   - A step function never terminates the turn. A refused patch, a failed verification, and a dead-end discovery round all return a typed result to `run_turn_loop`, which alone decides whether to request another round, to conclude, or to surface the outcome.
   - The `run_<domain>_loop` naming standard stays reserved for the functions that own a whole lifecycle: `run_repl_loop`, `run_plan_sequence_loop`, `run_plan_step_loop`, `run_turn_loop`, and `run_structured_refine`. A phase of a turn declares no loop of its own, so `run_research_loop`, `run_edit_loop`, and `run_staging_loop` no longer exist as entry points.
   - Single-method execution traits (e.g. `ExecutionLoop`) and dynamic trait dispatch (`Box<dyn ...>`) are strictly forbidden. Control flow is composed functionally through direct asynchronous function calls.

2. **Execution Context (`LoopContext`):**
   - Ambient operational infrastructure is encapsulated in a lightweight `LoopContext` passed by mutable reborrow:
     - `cancel: &CancellationToken` — cooperative cancellation token raced across network and toolchain futures via `tokio::select!`.
     - `events: &EventSink` — typed domain event emitter isolating the domain engine from TUI, sockets, and transport channels.
     - `repo_root: &Path` — root of the working tree every loop addresses.
     - `context_manager: &mut ContextManager` — live context a loop mutates (e.g. during a research round) and snapshots through.
     - `journal: &mut TurnResearchJournal` — turn-scoped cumulative epistemic journal collecting findings and scratchpad notes.
     - `policy: &mut dyn ApplyPolicy` — transactional apply policy governing checkpointing, staging, step commits, and rollback.
     - `capabilities: CapabilityExpr` — active model capability surface for the current execution branch.
     - `prompt_builder: PromptBuilder` — model round assembly and execution builder.
     - `transcript: Arc<Mutex<TurnTransscript>>` — round transcript of the turn, shared with every reborrowed child context (§14).
     - `rounds_spent: Arc<AtomicUsize>` — monotonic count of physical provider calls, shared with every reborrowed child context (§14).
   - **A value the whole turn observes travels behind `Arc`.** `reborrow_with_capabilities` copies every field by value, so a counter held directly in the context would be incremented inside the child and lost to the parent. The transcript and the round counter are therefore cloned through `Arc` by the reborrow, and the counter is incremented atomically.
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
     - Control loops consume only the capabilities in their domain via `envelope.take(LlmCapability)` or `envelope.take_any(&[...])`. As the envelope travels upward through the loop hierarchy, it shrinks.
     - `execute_staging_step` drains the file-edit capabilities (`EditReplace`, `EditCreate`, `EditDelete`, `EditMove`, `EditMoveDir`, `EditOverwrite`); `execute_research_step` drains the discovery capabilities; `run_turn_loop` drains the terminal outcome capabilities (`CommitMessage`, `TurnError`, `TurnSkip`); `run_repl_loop` drains `LlmCapability::TextMessage`, `PlanCreation`, and `PlanUpdate`.
      - **Prohibition of Ad-hoc Convenience Extractors:** `ResponseEnvelope` provides strictly generic capability-based draining (`take` and `take_any`). Adding typed helper methods for specific capabilities (such as `take_edits`, `take_text`, `take_outcome`, `take_blocked`, etc.) to `ResponseEnvelope` is strictly forbidden. Each loop or subsystem knows exactly which `LlmCapability` variants belong to its domain, calls `take(...)`, and pattern-matches the resulting `LlmCommand` items locally.
   - **Top-Level Anti-Leak Invariant:**
     - At the top-level turn completion boundary, `envelope.is_empty()` MUST be true. Any remaining commands (`envelope.remaining_capabilities()`) indicate commands emitted by the model that no level of the control hierarchy handled, triggering structured diagnostic logging.

6. **Protocol Sanitization Boundary:**
   - Model output violating structural contracts (unrecognized tags outside `capabilities`, invalid JSON/XML syntax, mixed operational branches) is a protocol defect, not a semantic divergence.
   - Loops intercept protocol violations locally via dedicated sanitizers (`heal_protocol_violation`), requesting format repair without leaking raw defects into domain logic.

7. **Strictly Localized Transaction and Error Boundaries:**
   - Virtual patch application and anchor matching are confined to `execute_staging_step`, and the retry rounds a refusal opens are requested by `run_turn_loop`.
   - Compiler verification, the declared post-hook, and the healing round a failure opens are confined to `execute_edit_step` and the turn loop that requests it.
   - Git checkpoints, coder ladder attempt accounting, gatekeeper acceptance, and commit squashing are confined to `run_plan_step_loop`.
   - Task ordering, dependency evaluation, and architect replanning triggers are confined to `run_plan_sequence_loop`.

---

## 14. Two-Tier History and the Turn Transcript

1. **Two tiers, two lifetimes:**
   - The session keeps two histories, and neither stands in for the other. The
     **turn transcript** (`TurnTranscript`, `crates/core/src/control/context.rs`) is the
     local message sequence of one bounded turn; the **session journal**
     (`HistoryManager`, `.tauqe/history.jsonl`) is the durable record a later turn reads.
   - `LoopContext` owns the transcript behind `Arc<Mutex<TurnTranscript>>`, so the rounds
     a `reborrow_with_capabilities` or `reborrow_extending_capabilities` child appends
     stay visible to the context it came from. The transcript is dropped with the turn.
   - The journal is written through `HistoryEntry`: a turn card, an answer record, and the
     rounds nested inside the card. `HistoryManager` never writes the transcript, and the
     transcript never writes the journal.
2. **Append-only growth inside a phase:**
   - `TurnTranscript` stores five values and nothing else: the rendered head
     of the assembly up to the task (`prefix`), the frozen task message
     (`task`), the assistant/feedback pairs already exchanged (`rounds`), the
     serialized capability tree the head was seeded with (`seed_signature`),
     and the churn epochs that mark the rendered prefix stale (`seed_churn`,
     `context_churn`).
   - `seed` freezes the first round, `push_assistant` and `push_feedback` append exactly
     one pair per round, `take_pending_feedback` hands the next round its user message,
     and `messages` returns a sequence that only grows.
   - Two boundaries reseed the prefix: the capability tree and a marked
     context churn. `advertises` compares the seed signature with the tree the
     loop now declares, and `prefix_is_stale` compares the epoch the prefix
     was rendered at with the epoch the context reached, so `reseed_prefix`
     re-renders the head once while the task and every exchanged round keep
     their exact bytes.
   - Rationale (Why): a provider caches the longest unchanged token prefix. A round that
     reassembled the whole request paid a reprocessing pass per round; a transcript pays
      one reseed per phase boundary, and a change set that churned the context
      pays one more, because a healing round that reads stale bytes pays for
      it in retry rounds.
3. **One feedback slot:**
   - `set_pending_feedback` is the only channel through which a producer reaches the next
     round, and the round that appends a pair drains it exactly once, so feedback never
     reaches the provider twice and blank feedback never occupies the slot.
   - Every producer of an extra round writes that one slot: the research journal
     (`LoopContext::publish_round_feedback`), verification healing
     (`LoopContext::request_healing`), patch retry (`LoopContext::request_repair`), and the
     corrective reminder of an empty round.
   - A finding is therefore delivered once, by the round it opened, and stays readable
     afterwards inside the assistant answer and the user feedback the transcript replays.
     No contour concatenates the findings of a turn onto the task body on every round.
4. **The scratchpad travels through the transcript, never through the head:**
   - The turn scratchpad is not re-injected into the system message between rounds. The
     model reads its own notes back from the assistant answers the transcript replays, so
     the head stays byte-stable inside a phase.
   - `StructuredEditProtocol::parse_scratchpad` and `drain_scratchpad` remain: the first
     extracts the channel from a payload, the second owns the buffer of a contour that
     assembles its own request (§13).
   - Persisting a turn strips the scratchpad from the recorded payload, because the
     channel is turn-local memory rather than a durable result (§3.5).
5. **Intermediate rounds are nested, never siblings:**
   - A turn that discovered material, retried a rejected patch, or healed a compiler error
     answered several times. Those attempts live inside the turn record itself
     (`HistoryEntry::Turn { rounds }`) and are constructed through `round_record`, so a
     rollback or a compaction drops them together with the attempt they belong to.
   - Only the intermediate rounds `1..N-1` are persisted. The final answer stays the
     ordinary `HistoryEntry::Response` of the same turn, so one answer is never recorded
     twice.
   - A recorded round carries the cleaned assistant text and the compact summary of the
     feedback that opened the next round: never raw protocol JSON, never a file body.
     `round_record` performs that cleaning, so no caller can record a raw payload.
   - `TurnRoundRecord` and the `rounds` field are additive: `#[serde(default)]` decodes a
     session written before either existed as a turn without rounds, so the layout evolves
     without a migration (§3.2).
6. **Two consumers, two projections:**
   - A later turn replays the rounds into its own transcript as one developer-side message
     per round, `[Round N] <message>` (`crate::history::compaction::round_replay`, reached
     through `assemble_chronological_messages`). The recorded feedback and the raw payload
     stay out of the prompt.
   - Compaction replays the same rounds into the source the Scribe reads
     (`build_compaction_request`), so a summary states what each turn verified and what it
     corrected instead of flattening the attempts.
   - `HistoryManager::estimated_tokens` and `needs_compaction` price a turn with the bytes
     the journal reported for it, which already include its rounds.
7. **An interrupted turn keeps what it finished:**
   - A turn interrupted after at least one completed round records those rounds and
     publishes the record under `HistoryEntryStatus::Interrupted` (`"interrupted"` on the
     wire): the finished rounds stay durable attempts while no final answer exists.
   - A partially streamed answer is never captured, because a fragment is not a round; a
     turn that produced nothing leaves the journal through `rollback_entry`, as §3 states.
   - The journal and the working tree are independent: the interruption restores the tree
     through the Git transaction and records the attempts through the journal, and neither
     operation is ordered against the other.
8. **The Architect synthesis stays outside the transcript:**
   - `run_structured_refine` keeps its own reassembly through `execute_rebuilt_round` and
     its own scratchpad injection. Its rounds answer with a synthesized horizon, an
     exceeded context budget, or an abort rather than a patch, so they do not cross the
     switched `execute_round` path.
   - Routing the synthesis through the transcript is a separate decision; until that
     decision is taken, no rule of this section governs that contour.
9. **The transcript outlives an attempt; the budget is authoritative at the call boundary:**
   - One `LoopContext` drives every rung of the coder ladder, so the transcript survives an attempt: a step re-asked on the same rung appends its retry round to the sequence its predecessor produced instead of reseeding it.
   - The round budget is consumed at the single point where the provider is actually called, `execute_round_inner`. Every physical call increments the shared counter, including each pass of the internal empty-answer loop, so one unit of the budget equals exactly one network call.
   - The boundary rejects a call that exceeds the pool with a typed stop (`SystemError::RoundBudgetExhausted`) rather than with a domain outcome. Only `run_turn_loop` maps that stop to the outcome its contour declares: an `Exhausted` report for a plan step, and the last answer the model wrote beside the notice of the limit for a dialogue.
   - The last permitted round receives the limit directive through the single feedback slot, so the model can close the turn with an answer instead of being cut off mid-investigation.

## Application Boundary Migration

`core::api::Application` is the public application facade. It owns private session
state and exposes typed scenario methods without RPC request ids, JSON values, or
transport DTOs. Clones address the same session; separately constructed applications
have independent sessions. Internal application types live under the private
`application` module and are exported through `api` only as needed.

Discussion focus is the first migrated scenario. The core owns change detection
and atomic target comparison and release. The server converts focus DTOs and
publishes the resulting notification. Existing `AppState` focus methods are
transport adapters; they must not regain state or focus transition rules.


The application runtime owns exclusive operation registration, process-wide
operation identities, cooperative hard-cancel and soft-stop signals, task abort
handles, and guard release. The registry and its lock are private. A cancelled
operation remains registered until its guard drops or an orphan is explicitly
released by identity. A stale guard, abort request, or release must never affect
a newer operation. Core reports registration conflicts as `OperationInProgress`;
server maps that error to the RPC contract.

Core owns watchdog scheduling, cooperative unwind grace, bounded operation-exit
confirmation, orphan registration release, and abort deferral while a Git
transaction is open. The server owns transport shutdown and resolves its
caller-scoped timeout overrides before delegating to `Application`; those
values must be passed explicitly across spawned task boundaries. Transaction
activity belongs to the application session; the turn host obtains a Git policy
through `Application::git_transaction_policy` instead of supplying a shared
atomic flag. Shutdown reads that same private session state. Only the existing
`test-support` feature exposes an activity seam for simulated stalled-transaction
tests; production callers cannot mutate the flag.
The server never inspects an active-operation record or locks its mutex.


Dialogue identity is private application session state. Core creates and resets
its UUIDv7 and decides whether an effective model change invalidates the provider
cache. Reselecting the same effective model preserves the identity. The server
may request a reset after an existing history or context mutation, but it neither
stores the identifier nor implements model-change comparison. Configuration and active selection belong to the same private application
mutex. Server readers receive detached `ConfigurationSnapshot` values, never
locks. Model selection and configuration patches validate all candidates before
committing; an invalid field must leave selection, configuration, and dialogue
identity unchanged. Core rotates dialogue identity after a successful effective
model change. Reload explicitly chooses whether to preserve or reset selection.
Configuration loading and module ownership inspection run in core infrastructure
through the blocking-work helpers. Application scenarios preserve explicit CLI
paths on reload and reject module-owned patch fields before value validation.
Runtime model selection alone must not trigger module I/O. The server maps typed
configuration errors to RPC codes. Configuration creation, credential saving,
and stub installation are application scenarios backed by core infrastructure. Template/module conflicts
are checked before writing. Credential save validates and trims the key, reloads
configuration, and preserves active selection and dialogue identity. A stub
changes only the file. Configuration reload resets selection and rotates the
dialogue only on an effective model change. System status is an application
result with filesystem locations and readiness (`Git && API key`) computed in
core. Git and location probes run through the
blocking-work helpers; a failed probe yields the existing conservative fallback.
After a file-writing scenario, status reports that scenario’s written path even
if rediscovery fails. Server only converts paths and configuration summaries to
the RPC shape and publishes events. Configuration mutation is publicly
available only through `ConfigurationOperation`, obtained with `Application::begin_configuration_change`.
The object owns the exclusive operation guard through I/O, commit, notification,
and response construction, including error paths. Direct mutation helpers on
`Application` are crate-private. The server acquires typed access before decoding
parameters to preserve the existing busy-error precedence; it only maps the
`OperationInProgress` error into the handler-specific RPC response. Repository
initialization holds this same access across its configuration rebind. Orphan
registration release is internal to core and cannot invalidate this access from
an external caller.


The application session owns the shared history manager and its mutex. Server
code reads pages or issues named record, rollback, and clear commands; it never
receives the shared guard or an arbitrary mutation callback. Disk access runs on
the blocking pool with the owned history guard held across the complete operation.
History mutation notifications use the typed core channel installed at repository
bootstrap and retained across rebinding. Pagination defaults and token estimates
are core policy. Clearing requires an active operation from the same session.

Compaction is an application scenario requiring an operation from the same
session. Core checks the token budget before resolving a Scribe provider,
compacts a private manager copy without holding the shared lock over provider
I/O, and adopts the copy only after a successful journal rewrite. Provider
resolution failures, failed streams, and cancellation leave the journal intact.
The copy/adoption helpers are crate-private; server never receives a history
manager in production. Arbitrary fixture mutations are exposed only by
`test-support`; production server code must use the named commands.


Dialogue-history cleanup is available through `HistoryOperation`, acquired with
`Application::begin_history_change`. It holds exclusive admission through journal
cleanup, focus detachment, identity rotation, and adapter publication. The raw
clear command is crate-private. Cleanup reports a typed persistence error while
still applying the existing in-memory reset policy; server logs that error and
preserves the established RPC result. A repeated clear rotates the dialogue
again but emits no redundant focus-change notification.


The application session owns the shared context manager and its read/write lock.
Server readers receive detached state or turn-local manager copies, never shared
guards. Context mutation keeps the owned write guard on the blocking pool across
the whole read-modify-write sequence, including caller cancellation. Turn merges
and repository rebinding use named application commands. Context clearing and
external-document removal rotate dialogue identity in core only when resident
material was removed; a User-only clear preserves identity.
Manual edits require `ContextOperation`, obtained before parameter decoding to
preserve busy-error precedence. It owns admission through mutation, notification,
and response construction. Core rejects manual additions to Pinned and OuterDocs
and clears of Pinned; server maps typed errors into existing RPC codes. Pattern
admission resolves the configured token budget and repository listing in core,
before taking the context lock. Declared preloads and external-document admission
use named application commands and return decisions paired with their snapshots.
The generic mutation callback is crate-private; arbitrary fixture mutations are
available only through `test-support`. Server production code cannot mutate the
shared manager through callbacks.


External-document cache inspection and removal are application scenarios. Removal
is available through `ContextOperation`, which holds exclusive admission through
disk deletion, resident-context reconciliation, dialogue rotation, and adapter
publication. Core selects affected resident documents under the context lock and
returns an optional context update only when material was released. Removing an
unmounted snapshot preserves context revision and dialogue identity. Cache
inventory and removal counters are core-owned values; server converts them into
wire DTOs and maps typed failures to the existing RPC errors. The synchronous
cache helpers are crate-private and expose no transport contracts.


Declared external-document preloading is an application scenario. Core trims and
deduplicates requests, materializes snapshots, enforces the context budget, and
returns diagnostics with an optional context update. A trust refusal stops before
any candidates from this preload are admitted; delivery and admission failures
remain diagnostics so the model can work with available material. The registry,
budget, and cancellation receiver come from the running turn's frozen inputs.
The server publishes the returned update and forwards the outcome. Trust-refusal
messages and unavailable-document prompt sections belong to core; their text and
ordering must remain stable during this migration.


Implementation step outcome classification belongs to core, through
`workflow::implementation::classify_step_turn`. Commit failures are infrastructure
failures and take precedence over semantic outcomes. A semantic block precedes
abort; both precede verification-based success. Passed or unconfigured verification
accepts the change set; failed or cancelled verification remains a domain failure.
Without a verification verdict, only an explicit commit or skip outcome declares
success. Conversational text alone never does. Exhaustion and unclassified
divergence remain domain failures. Server forwards this classification and maps
its own transport errors and cancellation without reinterpreting model commands.


Implementation frame resolution and active-step document selection belong to
`Application`. Frame reads run on the blocking pool against the authoritative
plan or review storage. The active queue item selects the leaf; a stale queue,
an empty declaration, or a failed frame read keeps the existing empty-preload
fallback, while explicit frame resolution still returns the typed failure.
Preloading re-resolves the frame for each call and uses the running host's frozen
registry and token budget, without reading live application configuration. Server
publishes the resulting context update and forwards any trust refusal. Refinement
uses the same application frame reader instead of scheduling storage work itself.


Implementation refinement preparation belongs to `Application`. Core resolves
the exact Architect model/effort pair and its configured rung parameters, reads
the stored frame, creates the scope-specific provider session, and assembles the
horizon prompt from failure diagnostics and developer instructions. Frozen run
configuration supplies all budgets. A failed frame read returns its diagnostic
with the selected tier before the server publishes a start event. Core also maps
structured refinement outcomes to workflow results, preserving the final tier
and rejecting a synthesized horizon with no steps. Server owns contour execution,
telemetry publication, and transport-error adaptation.


Implementation step preparation is one application scenario requiring an active
operation from the same session. Core compacts history, releases transient context,
admits declared target/reference files, preloads external documents, and appends
developer directives followed by unavailable-document diagnostics to the prompt.
Existing best-effort handling of compaction, cleanup, and file admission is retained;
a trust refusal aborts after the earlier preparation effects. A synchronous
notification callback receives detached context snapshots at each original boundary,
including boundaries preceding refusal, so server can publish them immediately and
in order. It never receives a manager or mutation guard. Server retains wire-context
projection, review notifications, provider execution, and transport adaptation.


Single-step and sequential step execution belong to `control::step`. Core owns
step-body/report types, per-run journal and input cache, prompt-builder and loop
context assembly, frozen execution budgets, dispatch to the turn or sequence loop,
and control-error classification. The resolved provider, protocol, apply policy,
turn-local context, cancellation signal, and event sink are explicit inputs.
Server retains provider test injection and wire-event relay during
this migration; it must not reconstruct a step's control context or choose the
engine loop itself.


`Application::run_step_task` owns spawning, abort-handle registration, joining,
interrupted-transaction recovery, and shared-context adoption for step execution.
It accepts the execution future only with an active operation from the same
session. After an abnormal exit, the operation's authoritative cancellation signal
distinguishes cancellation from task failure; recovery is best effort and never
replaces that result. Failed tasks fall back to the current shared context instead
of adopting unfinished local changes. Normal completion adopts context before
returning to the server's edit/usage relay. The abort handle is cleared after task
exit while the operation registration stays owned by its caller. Interactive turns
also delegate interrupted-transaction recovery to this same core scenario.


Turn model resolution, session-affinity defaults, provider construction, and apply
policy selection belong to core. `ResolvedTurnModel` carries the configuration
snapshot, selected model, semantic role, effective effort, and catalog alias
resolved together. Interactive specific-model selections inherit REPL effort;
step selections inherit Coder effort. Explicit effort overrides either default.
Provider construction uses the resolved snapshot even after live configuration
changes. Server retains only the scripted-provider fixture seam and conversion of
provider errors into the established RPC error. An explicit session always wins;
otherwise interactive turns bind the private dialogue identity and isolated steps
stay stateless. The application selects direct/Git apply policy and wires the
session's transaction activity and operation cancellation into the Git policy.


Interactive execution uses `control::repl::run_repl_execution`. Core creates the
turn-local research journal and input cache, assembles the prompt builder from
frozen configuration/history and explicit session/effort, derives capabilities
from typed focus, constructs the control context, and runs the REPL loop. Provider,
protocol, apply policy, cancellation, event sink, and the detached context are
explicit dependencies. Server retains streaming and usage adaptation during this
migration; it no longer reconstructs
the interactive prompt builder or control context.


`Application::prepare_interactive_history` owns journal compaction, capture of the
starting context paths and insertion watermark, and snapshot-before-record turn
creation. It requires an active operation from the same session. Context file
reads run on the blocking pool from one detached manager copy; no context lock
is held while reading files or taking the history mutex. The watermark includes
all recorded context arrivals even when a file has disappeared and is omitted
from readable paths. Compaction and persistence retain their existing best-effort
policy, and a failed start still lets the server use an empty history fallback.
The raw turn-start command is crate-private; server supplies the prompt and
consumes the resulting turn id and frozen history without deriving metadata.


Completed interactive-loop classification belongs to core. It preserves artifact,
streamed-text, and fallback precedence and returns model results with plan/review
mutation metadata. The server projects edit/Git events from that result before
calling `persist_interactive_completion`, preserving publication order. Core records
the full response and edit metadata; edit completion releases transient context and
rotates dialogue identity even if journal persistence fails. Answers preserve
context and identity. Completion persistence requires a same-session operation.



Interactive cancellation and stream/task-failure journal transitions use
`persist_interactive_interruption` with an explicit `InteractiveInterruption`.
Core rolls back the addressed cancelled turn, leaves history untouched when its
id is absent, and preserves the existing stream/task diagnostic response formats.
These transitions require a same-session operation and never clear context or
rotate dialogue identity. Server determines the typed interruption from transport
and cancellation signals, publishes events, and retains best-effort error handling.
The raw application rollback command is crate-private.

Migration debt: the legacy interactive stream-error branch records accumulated
streamed text as a response. That behavior is preserved here; aligning it with
section 14's completed-round interruption policy requires a separate behavior
change rather than a boundary-only refactor.


Interactive task lifetime uses `Application::start_interactive_task` and
`InteractiveTask::join`. Core owns the private session execution mutex, captures
the detached context after acquiring it, spawns execution, and registers its abort
handle. The server consumes events before joining. Joining clears the abort handle
and performs best-effort transaction recovery after a panic or forced abort;
control-loop errors remain normal task outcomes. The same-session operation stays
registered through subsequent server publication. Cancellation classification
still combines transport and operation signals in server, preserving RPC behavior.
The task returns its local context without adopting it: completion and interruption
paths retain their existing, different context policies. No execution lock or raw
interactive join handle is exposed to server.


Usage accounting belongs to the application session. Its ledger and window identity
are private, and `UsageGuard` owns observation, elapsed-time measurement, heartbeat,
and final folding into session/project totals. A synchronous publication callback
receives operation identity and snapshots; server only converts them to RPC events.
The heartbeat invokes this port while holding the accounting lock, preserving its
ordering against window replacement and flush. Callbacks must not wait or re-enter
the accounting API. Dropping or flushing a guard stops its ticker; stale windows
retain the existing discard policy. Only `test-support` exposes a compressed tick.

Core infrastructure reads and atomically writes `.tauqe/usage.json` without changing
the document format or best-effort failure handling. Bootstrap restores only project
lifetime; reset clears only lifetime; repository rebind adopts the new project's
lifetime while preserving process-session totals. Persistence precedes final
publication. Accounting totals and snapshots are core-owned values; server explicitly projects
them into protocol DTOs for RPC responses and events. Accumulation and legacy
provider-report conversion belong to core. Provider streams, response envelopes,
and batch workflows use core-owned `providers::ModelUsageInfo`; the protocol type
is reserved for RPC DTOs. Server projects reports through one adapter that preserves
unknown optional metrics and zero costs. Stored totals retain their existing Serde
format; RPC serialization and transport are confined to server.


Repository startup belongs to `Application::for_repository`. Before constructing
usage, context, or history state, core runs best-effort interrupted-workflow
recovery and then legacy review migration, in that order. Recovery outcomes retain
their distinct tracing levels; a skipped recovery is never reported as completed.
Synchronous startup uses the runtime-aware blocking boundary. A new repository
session runs startup once; application clones and read commands do not rerun it.
Server only supplies configuration, repository root, history notifications, and its
transport. `Application::new` remains configuration-only and performs no repository
startup. Rebinding retains its existing policy and does not invoke startup recovery.


Review post-analysis grouping belongs to `review::analysis::PostAnalysisRegrouping`.
Core heals absent finding identities, validates the verdict and references, builds
stable architectural groups and fallback assignments, and routes only findings in
`Discussion` or `Todo`. Findings already executing or terminal retain their status.
The prepared replacement binds private groups and assignments to its review id;
its persistence method delegates one classification-layer mutation to storage.
Core also prepares routing counts and the existing history response text.
Server still owns stream orchestration, cancellation checks, RPC error mapping,
and publication during this migration. Core checks cancellation after validation
and before persistence.
The current reference lookup behavior and stored/RPC formats remain unchanged.


`Application::prepare_review_post_analysis` requires a same-session operation.
Core rejects empty findings before mutation, heals identities on a copy, and saves
new identities before selecting the Architect or constructing the request. A later
selection or provider failure does not roll back those identities. Identity writes
retain the existing blocking-pool join-error handling without retrying mutations.
The prepared round contains the healed session, frozen configuration, resolved
Architect model and effort, messages, response schema, and provider session id.
An explicit model must belong to the Architect entries; an absent selection uses
the base rung. Server retains provider test injection and RPC publication.


`Application::start_review_post_analysis` owns provider invocation, the bounded
100-event channel, task spawning, returned-error forwarding, and abort registration.
It requires a same-session operation and executes the prepared request unchanged.
`ReviewAnalysisTask` exposes event reads, abort-registration release, and bounded
abort confirmation. Server consumes and paces events before releasing the stream's
abort handle, then flushes usage before cancellation/failure returns. Successful
completion retains the existing detach behavior; no extra join delays publication.
Core's shared task-exit helpers accept explicit budgets and preserve the original
orphan diagnostic. Server retains its default and task-scoped deadline resolution.
Provider construction and test injection, stream relay, and RPC publication
remain in server.


`ReviewAnalysisResponse` owns summary-field decoding, partial-output reset on retry,
provider/cancellation state, tolerant final parsing, and regrouping validation.
It consumes raw events after server pacing and emits typed reasoning, content,
usage, reset, and terminal updates. Usage reports remain visible across retries;
only partial response JSON is discarded. Server flushes usage before requesting
completion classification. Cancellation takes priority over provider errors, then
empty/invalid JSON, then schema errors. Only cancellation/provider failures require
provider task termination; successful or malformed-output paths retain their
existing detach policy. Server maps typed failures to the existing RPC codes. Core rechecks cancellation
immediately before persistence.


`Application::persist_review_post_analysis` requires a same-session operation and
rejects a regrouping prepared for a different review. Core checks cancellation
before scheduling the storage mutation; once started, persistence retains its
existing cancel-blind completion policy. Storage/join failures and a disappeared
review are typed errors. No history exchange is recorded on these paths.
After a successful regrouping, core emits its existing semantic log and records
the existing prompt, audited paths, response, and summary as one serialized history
exchange. History failures remain best effort and never roll back the review.
Review writes retain the existing blocking-pool join-error handling. Server only
maps persistence failures and publishes review notifications after this scenario.


`Application::execute_review_post_analysis` owns the execution sequence after the
adapter publishes started notices: usage window, provider task, paced stream
consumption, abort-registration release, usage flush and publication, response
classification, failure termination, and review/history persistence. Classification
happens after accounting publication, preserving cancellation/error priority and
charging failed rounds. Transport access uses `ReviewAnalysisObserver`: async raw
event pacing and synchronous typed updates/usage publication. Its callbacks must
not re-enter accounting. The adapter supplies the resolved task-exit deadline and
publishes terminal review notifications only after the scenario returns.


Review post-analysis target parsing belongs to `parse_review_analysis_target`.
The adapter runs it before admission to preserve invalid-input precedence;
`Application::load_review_for_post_analysis` validates it again through the same
parser and requires a same-session operation before reading storage. Empty or
missing targets never fall back to the active/latest review. Loading remains
read-only and retains typed missing/storage failures and blocking-pool join-error
handling. The standalone handler claims the operation slot before loading.

`PreparedReviewPostAnalysis::provider` constructs the selected provider from its
private frozen configuration. Later configuration changes do not affect that
round. Server retains only its scripted-provider injection seam and maps setup
failures to the existing RPC error; no key or provider policy is re-derived there.


`review::synthesis::ReviewSynthesisResponse` owns single-audit summary filtering,
retry reset, tolerant final decoding, title/description fallback, finding identity
assignment, and session materialization. Launch metadata is explicit: review id,
creation time, model, user prompt, and audited files are captured before streaming.
The model title wins; otherwise core uses up to 60 characters of the trimmed first
prompt line, then the existing timestamp placeholder. Raw provider text is retained.
Only empty output is refused; unstructured nonempty output retains the existing
informational-finding fallback. Existing finding UUIDs are preserved. The session
keeps its implicit single-audit shape, without explicit groups or launch metadata.
Core owns single-audit launch/stream orchestration and completion persistence.
Server supplies provider test injection, event pacing, and RPC publication.


`Application::persist_review_synthesis` requires a same-session operation and
checks cancellation before scheduling the single-audit save. A started write
retains the existing cancel-blind completion policy. Storage and join failures
return typed errors before any saved-review publication or history record.
After saving, core invokes the adapter's async publication callback without
holding a history lock, then records the review history entry best effort with
the existing warning on failure. The callback publishes the updated review and
list; the adapter publishes the terminal finished event after the scenario returns.
This preserves save -> updated/list notifications -> history -> finished ordering.
The callback carries no domain mutation policy and must only publish the saved
review. History failures do not roll back or withdraw a durable review.


`review::window` owns audit path expansion and domain counters shared by single
launches, preset groups, and preview. Core normalizes patterns, lists and sorts
matches, excludes context duplicates, verifies repository containment, reads source,
and applies the combined context/file token budget including prompt wrappers.
Expansion stops at the first unaffordable readable candidate; unreadable paths
encountered before that point have their own counter. Existing repository-listing
fallback and blocking-pool join-error behavior are preserved. Blank patterns do
not list the repository or add files. Group aggregation saturates counters and
sums independent budget allowances. Server explicitly projects `ReviewWindow`
into `ReviewPreviewWindow`; core does not consume the preview DTO. The legacy
wire `omitted_files` alias is derived from the single domain omission counter.


`Application::resolve_review_synthesis` freezes reviewer selection, effort,
provider configuration, and the file token budget. Explicit tier effort is kept
even when multiple reviewer entries name the same model. The resolved provider
is constructed before reading context, preserving setup-error precedence; server
retains only its scripted-provider override. `prepare_review_synthesis_window`
then reads a detached context snapshot and unions readable context with normalized
path matches through `review::window`, without mutating context or claiming an
operation. No-match patterns are refused even with existing context; a remaining
empty window is a separate typed refusal. Read/offload failures retain their
existing best-effort diagnostics and context fallback. Budget omissions remain
visible in launch metadata. The adapter claims admission only after preparation,
preserving the existing refusal order and frozen configuration semantics.


`prepare_review_synthesis_request` requires a same-session operation and freezes
single-audit identity, timestamp, prompts, Structured Output schema, effort, audited
paths, and provider cache session. The resulting metadata is reused when decoding
the completed response. It is prepared before started publication; provider launch
still follows the started notice and usage-window creation.

Single-audit synthesis and post-analysis share core's `ProviderTask` implementation:
100-event channel, provider invocation, returned-error forwarding, abort registration,
and bounded termination. Each caller supplies its explicit task label and resolved
exit deadline, preserving diagnostics and test overrides. Successful completion
retains the existing detached-task policy. Server owns the outer RPC task and raw
event pacing/publication; it no longer constructs a provider request or raw provider
join handle for a single audit. Both preparation and provider start require an
operation from the application session.


`ReviewSynthesisStream` classifies single-audit completion after usage publication.
Explicit stream cancellation and the operation signal win over provider errors;
provider errors win over empty-output validation. Only cancellation and provider
failures require abort confirmation; empty output retains the existing detach
policy. Nonempty unstructured output still becomes an informational finding.
Server maps cancellation to its existing notice and other typed failures to its
existing review error event, after flushing accounting.

Single-audit and post-analysis streams share `review::stream` transitions: summary
field decoding, retry reset, reasoning/content/usage updates, terminal state, and
cancellation priority. Their response schemas and materialization/validation stay
in their own completion classifiers. Usage reports are forwarded independently of
partial JSON, so retry discards output without losing earlier usage. Raw event
pacing still precedes observation and RPC publication remains in server.


`Application::execute_review_synthesis` owns the single-audit execution sequence:
usage window, provider task, paced stream consumption, abort-registration release,
usage flush/publication, completion classification, failure termination, and
review/history persistence. `ReviewObserver` is the shared pacing/publication port
for synthesis and post-analysis; synthesis adds async saved-review publication.
Core preserves usage -> save -> saved notifications -> history ordering and returns
the saved session with the same frozen usage snapshot for terminal publication.
The outer RPC task and operation guard remain alive through the finished notice.
The caller supplies its existing RPC operation id and resolved exit deadline;
core does not replace that id with the operation registry identity. Failed and
cancelled rounds remain charged and never publish a saved review or history entry.


### Headless workflow ownership

Preset reviews, group audits, review execution queues, locale generation, squash
message generation and application, plan batch/pipeline execution, horizon
synthesis, and independent critique now enter through `Application`. Core owns
selection, scheduling, cancellation classification, usage-window lifetime,
validation, durable transitions, context adoption, and history policy. The
registered operation remains alive through the server's terminal publication.
The adapters retain the established RPC error codes and event order.

`ReviewBatchObserver`, `PlanBatchObserver`, `PlanQueueObserver`, and
`ArchitectObserver` expose event/provider/turn bindings. Queue admission resolves
every requested member before execution. A plan failure ends its pipeline;
independent review members retain their existing local-skip policy. A review
finding in an architectural group cannot bypass its group horizon. Open questions
must be resolved before that group runs. These rules belong to core tests.

The Architect task skeleton owns abort registration, summary accumulation, retry
reset, usage accounting across rounds and tiers, and context merging. Server
only projects provider events into RPC events and supplies transport backpressure.
Caller-scoped task-exit deadlines are passed explicitly to one-shot provider
workflows, including independent critique, locale, and squash generation.

`ConfigurationOperation::rebind_repository` owns configuration -> context ->
usage lifetime -> history rebinding under exclusive admission. Each subsystem
releases its guard before entering the next one. The callback only publishes usage.

`preview_review` is read-only even while another operation owns admission. It
reads context through the blocking pool and aggregates independent preset-group
budgets without loading interactive context into a preset. Its result contains
domain window counters; server constructs the preview RPC DTO and its legacy
omission alias.

Locale requests, catalog entries, and results are core-owned execution models.
Server constructs them explicitly from protocol DTOs and projects the result
back. Core no longer re-exports the protocol crate through its public facade.

### Enforced dependencies and completed migration audit

The workspace dependency gate lives in `server/tests/architecture_tests.rs` and
runs with the regular server test suite. Protocol may depend only on serde,
serde_json, and schemars; tools cannot depend on core or protocol; TUI cannot
depend on core; server cannot depend directly on tools. Core depends only on
workspace tools and external engine/infrastructure libraries.
The transitional core-to-protocol allowance has been removed from Cargo.toml
and the dependency gate. A source gate also rejects protocol references anywhere
in core, including tests.

The migration audit is complete across production server handlers and startup.
Domain I/O, stored-document selection/mutation, Git query/preview policy,
execution assembly, and terminal lifecycle policy enter through application
ports. The global server source gate rejects direct storage/Git/blocking-I/O
calls and domain normalization. Transport, event lifetimes, RPC adaptation,
logging, and signal handling remain in server. Pure client graph computations
reside in TUI's projection library, outside protocol. New work must preserve
these boundaries rather than restart transitional DTO sharing.


### History model ownership

History items, lifecycle statuses, file-change records, and intermediate rounds
belong to `core::history`. The history manager, application page result, and
mutation channel expose domain models, never `protocol::UiHistoryItem`. Only
stored round/file records carry serde derives; in-memory history projections
have no wire encoding. Server owns the exhaustive projection into history RPC
results and appended/compacted/rolled-back notifications.

The retired display-prefix file format is decoded only by the core history
reader. Protocol must not implement stored-session migration. Typed file records
retain their JSON field names, optional-field defaults, and lenient legacy
operation decoding; file-operation identity is never inferred for a new record.
History slice pagination, token accounting, status publication, and compaction
ordering are unchanged. Boundary tests prevent history from importing protocol.


### Context model ownership

`core::context` owns access levels, layer identities, file entries, and snapshots.
Context managers, application operations, model commands, prompt builders,
implementation hosts, and provider `StreamEvent::ContextChanged` carry those
models. A snapshot has no serde contract: it cannot be accidentally published
as an RPC result. `ContextAccess` retains its serde/schema definition because it
is also a typed field of the core-owned model command schema.

Server's context projection explicitly converts request access/layers and all
outgoing snapshot fields. Its single publication function constructs the
protocol `ContextChangedEvent`; every workflow uses that function. Core owns
revision, insertion ordering, token estimates, layer protection, and merge rules.
The adapter cannot recompute or normalize those facts. Protocol retains legacy
request aliases and defaults, including `refs` -> `outer_docs`, and the RPC
snapshot remains unchanged. Dependency tests forbid protocol imports in context
management and application mutation modules.


### Model configuration ownership

`core::models` owns provider/model identity, semantic roles, reasoning effort,
explicit model selection, configured role entries, Architect tiers, provider
catalog entries, and resolved role summaries. Configuration parsers, providers,
control loops, application scenarios, and CLI overrides consume these domain
types. `AppConfig::effective_roles_summary` resolves efforts, role lists, and
catalog presence; server projects the resolved result without choosing defaults.

Generation-attempt defaults and their interaction with `level_up` belong only
to core model entries and Architect tiers. Protocol retains optional fields and
wire validation but cannot resolve an absent attempt budget. Its `level_up`
accessor is still used by TUI presentation and returns the declared marker.

Model and document projections now belong to `server::contracts`. Its local
`Project` trait adapts foreign core/protocol types without adding transport
conversion implementations to core. The previous infrastructure model bridge
has been removed. Projections preserve explicit selection variants, optional
efforts, attempt budgets, escalation markers, catalog aliases, and role-list
order; conversion never serializes and deserializes values or infers effort
from model names.

Compatibility tests compare domain and protocol parsers, serialized payloads,
and round trips through typed projections. Configuration keeps its existing
canonical string forms, accepted legacy spellings, precise errors, and defaults.
Dependency gates keep the model module and model/provider configuration parsers
free of protocol imports.


### Plan and review document ownership

`core::plan` owns plan headers, concept/step trees, implementation horizons,
validation reports, mutation proposals/rejections, and synthesis outcomes.
`core::review` owns findings, statuses, groups, post-analysis, launch snapshots,
execution targets, and review sessions. Storage, parsers, configuration presets,
application scenarios, and model commands consume those types. Stored-document
serde names, aliases, defaults, required nested horizons, model-name leniency,
and finding identity/provenance remain compatible.

The server decodes RPC DTOs, explicitly projects their payloads into core types,
and projects results/events back. `server::contracts::Project` is an exhaustive
field/variant mapping shared by RPC adapters and compatibility tests. It adds no
normalization, scheduling, lifecycle decisions, or serde transcoding. It also
owns the model projections previously located in core infrastructure.

TUI owns its pure graph projections, including readiness, progress, leaf file
normalization, and optimistic leaf status updates. Protocol only carries their
input DTOs. Core computations are authoritative; parity tests link the actual
TUI library and cover every pair of leaf statuses and scoped
readiness together with normalization and counters. Durable lifecycle provenance
normalization belongs only to core. Its test moved with the implementation.
Post-analysis model-output payloads are core-only and no longer exported by
protocol. They must not return to the client contract.

Lexical plan path comparison uses `context::lexical_path`, without filesystem
I/O. It preserves the existing escaping and invalid-path fallback semantics;
physical containment remains enforced by tools/context. Edit decoding and the
legacy discussion stream now use owned core payloads and the same lexical helper.

Dependency gates prohibit protocol imports in owned plan/review document models,
storage, post-analysis, review preset configuration, and migrated scenario entry
points. RPC layout and stored-document compatibility are verified independently
of domain computations.


### Execution identity and queue ownership

`core::workflow` owns `WorkflowKind` and `EditProtocolKind`; configuration,
workflow factories, prompt protocols, and control loops consume these types.
Their stored configuration names, case/whitespace handling, historical edit
protocol aliases, and parser errors remain compatible. Protocol retains separate
client DTOs; server explicitly maps configuration input, initialization, and
configuration output through `contracts::Project`.

`core::workflow::turn` owns work items, ordered queues, work context, plan
coordinates, review lifecycle phases, and turn phases. These in-memory execution
values have no serde contract. Implementation requests, review batch scheduling,
and provider phase events use domain types. Server projects them only when
constructing RPC notifications; it does not reorder queues, repair indices, or
infer work from titles.

`ExecutionScope::plan_context` and `ExecutionScope::work_context` declare the
coordinates of an implementation step in core. Only local plans carry legacy
plan coordinates. Review scopes keep the existing identifier fallback for the
session title because the request declares only the horizon title. An out-of-range
queue index keeps an empty active step while retaining the complete ordered scope.
Every execution scope has a work context; only plan coordinates are optional.
The server implementation host consumes these declarations without recreating
scope policy. Behavior tests moved to core with this logic, and server contract
tests pin all work variants, phases, queue bounds, and configuration compatibility.


### Edit, research, and model-output ownership

`core::edits` owns concrete edit operations, change proposals, typed conversion
rejections, file-event vocabulary, application outcomes, and verification verdicts.
`core::workflow::PatchStrategy` owns patch addressing configuration. Staging and
Git policies consume domain edits; `edits::substrate` maps them into tools inputs
and maps successful technical operations back without passing through protocol.
All proposal conversion and model-facing edit schemas belong only to core.
Protocol retains independent client-visible edit/proposal DTOs but cannot select
an execution operation or resolve a malformed model proposal.

`core::llm` owns result proposals, terminal model signals, documentation topics,
research request payloads, dialogue output, discussion targets, tolerant decoding,
and Structured Output schema composition. `llm::discussion::schema` owns cached
plan/review schemas and their bounded tree unrolling. Those schemas and edit
proposal converters have been removed from protocol. Legacy parsing behavior
remains pinned by the moved tests and core's capability/stream suites.

Repository path decoding uses one core lexical grammar for plans, edit operations,
and research/discussion normalization. Operation serde keeps the inverse encoding
for literal backslashes. File path requests still reject escapes; tolerant research
lists still discard unusable entries while preserving order, duplicates, access
levels, repository-root requests, and precise malformed-entry diagnostics.

Server projects every edit operation, raw change field, nested signal, research
request, documentation topic, outcome, and verification verdict explicitly through
`contracts::Project`. Its edit-cycle guard retains exactly-once RPC closure and
projects the completed core outcome at publication. Configuration and RPC payload
compatibility are tested independently of core execution rules.


### Discussion preparation and protocol export audit

`core::discussion` is the single owner of discussion target identity and focus
state. Session operations and turn capability/prompt selection use that same
type. The former duplicate application/model target definitions are removed;
server's focus and model-ask adapters reuse the same target projection.

`Application::prepare_model_ask` resolves free text, explicit targets, and active
focus. Core loads plan/review documents through the blocking pool, parses focused
finding identifiers, resolves effective review groups, and renders prompts.
A group dialogue includes every member, including implicit default membership,
regardless of the requested finding selection. Core returns typed invalid-input,
missing-target, and storage failures; server maps them to existing RPC errors.

Blank text is refused before focus handling. An explicit discussion replaces focus
only after target loading/rendering succeeds. A failed explicit request preserves
the previous focus. An implicit focus that cannot load its target is cleared and
the original text is used verbatim, including on storage failure. Repeated focus
selection retains its unchanged marker. Core returns a `FocusUpdate` for the
adapter to publish before launching the turn; it does not publish RPC events or
reset dialogue history. Existing event order, diagnostics, and fallback behavior
remain pinned by core scenario and server adapter tests.

Protocol no longer exports `DiscussionResponse`, plan discussion updates/actions,
directory-list model requests, review decision/update model payloads, or patch
strategy configuration. They are not fields of an RPC request, result, or event.
Their model parsing/normalization and patch configuration tests live in core.
Client-visible `ModelResultProposal` and its nested edit/context/search/document
payloads remain part of RPC and retain their exact wire encoding. Removing an
unused server converter is not permission to remove a nested serialized field.


### Turn completion facts and document publication

`classify_step_edit_completion` owns the edit outcome of a single or sequential
step execution. A commit error takes priority over a hash, retains changed paths
and verification, and yields a refusal. A single turn claims applied edits only
when it carries a commit. A completed sequence preserves ordered paths and their
duplicates, the last available commit, and its existing aggregate passed verdict.
Empty completed sequences retain their legacy applied status. Interrupted or
otherwise incomplete sequences retain the existing empty unapplied outcome.
Cancellation, refusal, and infrastructure failure remain distinct dispositions;
a completed disposition by itself does not imply an applied change.

Server maps the disposition onto its edit-cycle termination vocabulary and
publishes the core outcome. Exactly-once closure and fallback RPC descriptions
remain in the event guard. Server must not inspect step artifacts to decide
whether a change was applied, aggregate their paths, or select a verification
verdict.

`Application::interactive_plan_updates` and `interactive_review_updates` own the
read-only snapshots used after interactive completion. Core reads plans only
when a plan mutation was applied, resolves the active plan without repairing a
stale active marker, and returns the full list with that same marker. Failed
active reads retain the prior absent-marker fallback; failed list reads suppress
both plan notices. A missing/non-directory plan path retains storage's empty-list
behavior. Review decisions refresh the session addressed by a review or group
focus and retain the original decision count, including duplicates. Missing or
unreadable reviews remain best effort with the existing diagnostics.

The adapter calls these scenarios after history persistence and context
publication, then publishes active plan -> plan list -> one review notice per
decision -> terminal model events. It does not select an active entity or load
stored domain documents. Read-only publication requires no new admission or
cancellation checkpoint; the existing operation remains alive throughout.


### Bounded execution assembly

`Application::start_interactive_execution` owns protocol composition from the
exact capability grant, cancellation wiring, repository-root capture, best-effort
history start, apply-policy selection, and the task closure that drives the REPL.
It returns the supervised task, the event receiver, and the history turn identity.
Interactive context is still captured only after the private execution lock is
acquired. The frozen configuration, focus, history, effort, and provider session
travel unchanged to the control loop; history failures retain their default
fallback. Server supplies the resolved provider (including scripted injection)
and consumes events without constructing an engine closure.

`prepare_step_execution` freezes the protocol, cancellation, apply policy, root,
and context before usage/relay setup. Its private prepared value is bound to the
specific registered operation. `execute_prepared_step` rejects a different
operation or session before provider invocation or context adoption, then runs
through the existing step supervision, abort registration, recovery, and merge
policy. A retained server event sink keeps terminal edit closure ahead of the
relay's finished notice. Successful task completion clears its abort handle but
keeps admission alive through terminal publication.

The closure-taking task helpers and apply-policy constructor are crate-private.
Server cannot bypass the typed execution scenarios to assemble bounded loops.
`turn_prompt_protocol` is the core factory shared by those scenarios and protocol
behavior tests; schema/instruction/decoder composition remains one operation.
Core reads the repository snapshot after commit publication through
`repository_after_turn_commit`, preserving its best-effort warning and the
existing commit -> repository state -> edit completion event order. Server no
longer performs that blocking Git read directly.


### Interactive finalization priority and ordering

`InteractiveStreamState` owns the consumed attempt's text, reset, cancellation,
and stream-error facts. It observes raw events after server pacing. Server still
publishes immediate error/cancel/reset notices and stops consumption at the same
terminal event; it does not independently classify the completed task.

`Application::finalize_interactive_turn` owns settlement after the supervised
join. Cancellation wins over task and stream failures. Without cancellation,
a task failure wins over a stream error; a returned control error remains a
normal completion classification, distinct from a failed task. Interrupted runs
do not adopt returned context. Task/stream failures retain their existing journal
response rules; cancellation rolls back only the recorded turn identity.

Ordering is explicit: an implicit cancellation after a successful join is
published before rollback and usage flush. An implicit cancellation after a task
failure is published after rollback and flush. A cancellation already published
by the stream is never repeated. Usage is consumed once and its totals are
published before the legacy usage callback. Interrupted rounds retain their
observed usage and never run successful-completion cleanup.

For a normal completion, core flushes usage before classification, merges context,
publishes edit/Git completion through the observer, persists history and performs
existing cleanup, then publishes context -> plan snapshots -> review snapshots.
The adapter publishes terminal model results only after this scenario returns.
No extra cancellation checkpoint is introduced during publication or persistence.
History and document-refresh failures keep their established best-effort policy.

`InteractiveFinalizationObserver` adapts typed facts to RPC events without owning
journal, cleanup, selection, or accounting policy. Its callbacks may read snapshots
but must not mutate history, context, or usage. The existing operation remains
alive through every callback and the adapter's final notices. Foreign operations
are refused before flushing usage or invoking the observer.


### Repository and document application ports

`Application::repository_state`, `repository_files`, `repository_diff`, and
`squash_preview` own blocking Git work. Revision validation precedes Git queries;
commit diffs ignore the working-tree path filter. Squash preview requires an
owned operation and resolves requested base -> session base -> configured/detected
upstream -> `HEAD~1`, retaining error propagation and Git read order. Repository
initialization is exposed by the exclusively admitted `ConfigurationOperation`.
Startup repository discovery/configuration loading also use core API entry points,
while server installs logging and signal handlers in their existing order.

`Application::documents_at` binds stored-document scenarios to an explicit root.
Core owns ID/tree validation, leaf-file normalization, read-only active/first
fallbacks, explicit-session review updates, note trimming/clearing, target
selection, replan leaf/Done guards, and focus release after deletion. Missing or
stale active pointers are never repaired by reads. Publication refreshes retain
best-effort semantics and server sends the existing update/list event sequence.
Every document mutation claims its own operation registration inside core, so
headless callers cannot race an admitted turn. Legacy RPC busy checks remain
before parameter parsing; a raced admission failure maps to operation-in-progress.
No new persistence formats or RPC fields are introduced.

Protocol's retired `EditProposal` was an engine execution envelope, not a nested
RPC field, and has been removed with its unused server projections. Client-visible
`StructuredChangeProposal`/`ModelResultProposal` and their serialized nested fields
remain intact. Headless document/repository tests and existing RPC suites pin
selection, validation, admission, focus release, revisions, and wire behavior.


### Client computation boundary

Protocol no longer evaluates plan/review graphs, derives status or counters,
normalizes optimistic tree state, renders Markdown, or cycles picker statuses.
Those operations belong to `tui::projection`. Scope evaluation and client-side
validation reports are local TUI types, not exported wire contracts. Core's
headless authoritative algorithms and stored models are unchanged. Server's
composition-root integration tests link the TUI projection library through a
dev-only dependency; runtime dependency gates continue to reject TUI -> core,
core -> protocol, and server -> TUI.


Shared path-pattern matching belongs to `tools::path_patterns`, alongside other
utilities. Core's context and review modules import it directly rather than
re-exporting it as an engine abstraction. The former standalone matcher crate
has been removed; matcher behavior is unchanged.
