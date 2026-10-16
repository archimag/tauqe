# Development Conventions

This document specifies mandatory engineering and architectural conventions for the Tauqe repository.

---

## 0. Project Language Standard

1. **English as the Sole Project Language:**
   - English is the mandatory and official language for the entire codebase and repository artifacts.
   - All documentation (`README.md`, `Vision.md`, `Conventions.md`, architecture docs), source code comments, variable and function identifiers, Git commit messages, and user-facing interfaces (terminal UI views, modal dialogs, help menus, status/error messages, onboarding steps) must be written exclusively in English.
2. **Plain Technical English (Direct, Accessible, and Rigorous):**
   - Write in clean, concise, plain engineering English.
   - The primary audience consists of global software engineers—many of whom are non-native speakers—and large language models.
   - Avoid flowery prose, baroque vocabulary, archaic turns of phrase, metaphors, and convoluted sentence structures.
   - Exception: the project's defining paradigm metaphor—the model "harness" and its bridle, bit, and reins vocabulary, introduced in `Vision.md`—is a deliberate term of art, not decorative prose. It may name the paradigm, but it must never be extended into incidental ornamentation.
   - Prefer direct active voice, short unambiguous sentences, standard industry terminology, and straightforward syntax.

---

## 1. YAGNI, KISS, and DRY (Minimalism and Single Source of Truth)

1. **YAGNI (No Speculative Features):**
   - Never add unused enum variants, unneeded struct fields, dummy stub methods, or parameters "just in case" or "for future flexibility".
   - Code must be written strictly for the current step and immediate requirements.
2. **KISS (Minimum Sufficient Complexity):**
   - If a problem can be solved with a flat list, concrete types, or standalone helper functions without introducing abstract factories, speculative traits, or indirection layers, implement it directly.
   - Keep data models and execution control flows simple, explicit, and easy to trace.
3. **DRY and Single Source of Truth (SSOT):**
   - Every protocol schema, configuration parameter, domain state transition, and invariant must have a single authoritative definition.
   - Never duplicate wire-contract parsers, validation rules, or ad-hoc converters across crates. Derive, share via `protocol`/`core`, or compute from the single source of truth.
4. **Dead code removal:**
   - Code that is superseded or rendered obsolete (such as legacy protocols, abandoned memory concepts, or deprecated modes) must be deleted completely, never commented out or hidden behind dormant feature flags.

---

## 2. Workspace Architecture and Crate Responsibilities

The TAUQE workspace is decomposed into four specialized crates with strict, unidirectional dependency boundaries. Code must never drift across these boundaries:

1. **`crates/protocol` (Pure Wire Contract and Shared Domain DTOs):**
   - **Mandate:** Authoritative single source of truth for all external and inter-process communication contracts. Contains JSON-RPC request, response, and event data structures, protocol commands (`LlmCommand`), capability flags, error codes, and pure data transfer objects (DTOs).
   - **Allowed:** Serde serialization and deserialization, JSON schema generators, pure validation logic, and self-contained stateless string/path helpers (e.g., glob matching, XML escaping).
   - **Strict Prohibitions:** Strictly NO file system or disk I/O, NO OS command execution, NO network or HTTP clients, NO async runtime (`tokio`) dependencies, and NO workspace dependencies. Must remain a lightweight, fast-compiling leaf crate.
   - **Documented Exception — Plan Evaluation Surface:** A small, pure surface of plan and review computations lives in `protocol` even though it resembles domain logic. It comprises the read-only plan queries (`Plan::evaluate_scope`, `Plan::compute_items_status`, `Plan::count_stats`), the derived-state normalizers that recompute a value from existing structure (`Plan::normalize_leaf_files`, `Plan::refresh_parent_statuses`), and the leaf-scoped assignment helper used for optimistic UI updates (`Plan::update_item_status`). The `tui` crate depends only on `protocol` and must render plan readiness, progress, leaf file badges, and optimistic status changes without an RPC round-trip, so these deterministic, allocation-only functions over the shared plan DTOs are allowed here. They perform no I/O and never touch disk, Git, or the network. All durable plan transitions, persistence, and `Done` immutability enforcement remain in `core`.
   - **Status Cycling Is Presentation-Only:** The `PlanItemStatus::next` and `ReviewItemStatus::next` helpers define the UI cycle order for the status picker and intentionally allow transitions such as `Done -> Blocked`. They are not a permission model. The authoritative guard against mutating or regressing a finished step lives in `core` (`validate_done_immutability`), never in the protocol cycler.

2. **`crates/core` (Deterministic Harness and Domain Engine):**
   - **Mandate:** The headless engine implementing the literal harness paradigm. Owns the bounded turn FSM, virtual staging and fuzzy patching, Git transactions and checkpoints, LLM provider communication and streaming, Tree-sitter repo map generation, toolchain verification runners, prompt assembly, and plan/review storage engines.
   - **Allowed:** Interacting with Git, the file system, toolchain processes, and provider HTTP endpoints. Exposing domain traits (`PromptProtocol`, `LlmProvider`, `ApplyPolicy`) and headless domain workflows.
   - **Strict Prohibitions:** Strictly headless; NO presentation or terminal UI dependencies (`ratatui`, `crossterm`). NO RPC transport server or socket listening code.

3. **`crates/server` (RPC Daemon and Orchestration Gateway):**
   - **Mandate:** Persistent background daemon bridging client frontends with the core engine. Manages RPC transport (stdin/stdout or socket), client handshake, concurrency control (`OperationGuard`), request dispatching, and event streaming via `OutChannel`.
   - **Allowed:** Routing protocol requests to `core` workflows, translating operation lifecycles into protocol events, and managing active process state.
   - **Strict Prohibitions:** Must NEVER duplicate or re-implement domain logic (such as plan status evaluation, patch retry loops, prompt assembly, or Git diff computation). Domain transitions belong exclusively in `core`. Must NEVER contain terminal UI code.

4. **`crates/tui` (Interactive Terminal Presentation and Client Frontend):**
   - **Mandate:** High-density keyboard-driven terminal frontend inspired by Magit and SLIME. Owns terminal views (Develop, Context, Review, Plans, History, Onboarding), layout geometry, keyboard navigation, input event translation, and local text editor state.
   - **Allowed:** Ratatui and crossterm integration, local UI state management, and RPC transport communication with `tauqe-server`.
   - **Strict Prohibitions:** Strictly a client frontend. Must NEVER run Git commands directly, touch LLM provider APIs, execute toolchain verifications, or manipulate working tree files directly on disk. All domain operations must be dispatched through RPC requests to the server daemon.

5. **Anti-Drift Architectural Invariants:**
   - **Single Source of Wire Types:** All wire events, RPC parameter types, and model protocol objects must be defined in `crates/protocol`. Writing ad-hoc JSON structs or re-parsing raw wire payloads in `server` or `tui` is strictly forbidden.
   - **Unidirectional Dependency Flow:** Dependencies strictly flow downward (`tui` -> `protocol`, `server` -> `core` -> `protocol`). Cyclic dependencies or reverse couplings are forbidden.
   - **Strict Separation of Presentation and Domain:** Presentation logic belongs exclusively in `tui`; domain execution belongs exclusively in `core`.

---

## 3. Module Design and Architecture

1. **Modules as Black Boxes (Information Hiding & Minimal Visibility):**
   - **Encapsulation:** A module must expose strictly what external callers need to interact with it. Internal helper functions, intermediate data structures, and algorithmic details must stay hidden.
   - **Principle of least visibility:** Items are private by default. Promote only to `pub(super)` or `pub(crate)` when required across sibling files within a subsystem. Unrestricted `pub` is reserved strictly for public API entry points.
   - **Clean facades:** Module facades (e.g., `ui.rs`, `git.rs`) orchestrate subsystems. Never leak or re-export internal implementation helpers (such as string formatting, geometry math, or ad-hoc converters) through the facade.
2. **Single Responsibility Principle (SRP) and High Cohesion:**
   - Every module, struct, and routine must have a single, cohesive reason to change. Never mix transport/RPC handling, user input processing, domain mutations, and presentation logic in one place.
   - Avoid "God Objects" or dumping miscellaneous logic into `main.rs` or monolithic facades.
3. **Dependency Inversion & Trait Interchangeability:**
   - High-level orchestration (such as the workflow engine and FSM) depends on abstract protocol and engine contracts (`PromptProtocol`, `LlmProvider`, `ApplyPolicy`) rather than concrete implementations.
   - All implementations of a trait contract must be strictly interchangeable without caller-side type branching or ad-hoc special-casing.
4. **Rust 2018+ Module Layout (Prohibition of `mod.rs`):**
   - Using `mod.rs` files in new or refactored subsystems is **strictly forbidden**. Having numerous identical `mod.rs` files degrades editor navigation, stack traces, and debugging clarity.
   - For any module with submodules, place `foo.rs` alongside the `foo/` directory:
     ```text
     crates/core/src/
       history.rs              # Module root and facade
       history/
         entry.rs              # Types submodule
         storage.rs            # Storage submodule
         compaction.rs         # Summarization submodule
     ```
   - Inside `history.rs`, declare submodules as:
     ```rust
     pub mod entry;
     pub mod storage;
     pub mod compaction;
     ```
5. **Cohesive Sizing as a Health Metric (AI-Friendly Architecture):**
   - File size is an indicator of cohesion, not an arbitrary bureaucratic quota:
     - **Target size:** 150–400 lines of code. Modules of this size are easy to reason about, review, and maintain.
     - **Soft limit:** 600 lines. Crossing this threshold is a strong signal that the module contains mixed responsibilities and should be decomposed into focused submodules.
     - **Hard limit:** 800 lines. Considered an architectural smell requiring refactoring (unless an exceptional case such as generated parsers or deterministic lookup tables).
   - Compact, cohesive modules ensure reliable, unambiguous reasoning and diff generation by language models, eliminate context waste, and prevent merge conflicts.

---

## 4. Robustness and Panic-Free Invariants

1. **Strict Prohibition of Panics in Server and Runtime Logic:**
   - TAUQE operates as a persistent engineering control environment (server daemon and interactive TUI). A panic in any handler, background turn, or parsing routine crashes the process, drops in-flight work, and breaks transactional safety.
   - Production runtime code must never use `.unwrap()`, `.expect()`, `panic!()`, `unreachable!()`, or unbounded array indexing `arr[i]` on untrusted or external input. Always handle failure branches gracefully using `Result`, `Option`, `?`, or sensible fallback defaults. Unit tests (`#[test]`) are the sole exception where assertions and `unwrap()` are idiomatic.
2. **Safe UTF-8 String Slicing:**
   - Never slice strings with arbitrary byte offsets (e.g., `&s[..N]`). In UTF-8, multi-byte characters (Cyrillic, CJK, emoji) will panic at runtime with `byte index N is not a char boundary`.
   - Always slice on Unicode scalar boundaries: use `.chars().take(N).collect::<String>()`, `floor_char_boundary(N)`, or `char_indices()`. Prefer `strip_prefix()` and `strip_suffix()` over manual offset slicing.
3. **Graceful Degradation over Fatal Aborts:**
   - When parsing model output, protocol messages, or configuration, apply Postel's Law: be conservative in what you send, liberal in what you accept. Fall back to safe defaults with diagnostic logging rather than terminating execution.

---

## 5. Test Coverage and Code Quality

1. **Mandatory Test Coverage:**
   - All new functionality, bug fixes, and behavioral changes must be covered by automated unit or integration tests verifying their contract.
   - Regression tests must accompany every bug fix to prevent regressions.
2. **Zero Warnings Standard:**
   - All code across workspace crates must compile cleanly without compiler errors or Clippy warnings (`cargo clippy` runs strictly with `-D warnings`).

---

## 6. Commit Message Format (Conventional Commits)

All commits must follow the **Conventional Commits** specification:

```text
<type>(<scope>): <description>

[optional body]

[optional footer(s)]
```

- **Types:** `feat`, `fix`, `refactor`, `test`, `docs`, `perf`, `chore`.
- **Scopes (crates & subsystems):** `core`, `protocol`, `server`, `tui`, `workflow` (or omitted for cross-cutting changes).
- **Header Line:**
  - Written in English, imperative mood (*add*, *fix*, *update*), lowercase, no trailing period.
  - Maximum 72 characters in the header line.
- **Message Body & Formatting:**
  - Mandatory single blank line separating the header from the body.
  - **Strict 72-Character Line Limit:** Hard-wrap all body and footer lines at a maximum of **72 characters**. Never generate runaway, single-line paragraphs.
  - Focus on *what* changed and *why*, articulating conceptual motivation and trade-offs rather than low-level procedural trivia.
  - Use concise bullet lists or short paragraphs, cleanly wrapped to 72 columns.

---

## 7. Living Configuration Reference (`tauqe.toml` and User Config)

1. **Authoritative Example for Project Configuration:**
   - The root `tauqe.toml` file in this repository serves as the authoritative, living reference example of project-level configuration options.
   - Project configuration (`tauqe.toml`) governs toolchains, project models, context limits, and repomap budgets, and is safe for repository version control.
2. **Separation of Project and User Configuration:**
   - Personal developer preferences (terminal themes, keyboard layout mappings, primary command modifiers) must never be placed in `tauqe.toml`.
   - User-specific TUI configuration belongs in `$XDG_CONFIG_HOME/tauqe/tui.toml` (or `~/.config/tauqe/tui.toml`), keeping repository configs free of personal workstation settings.
3. **Mandatory Documentation of Parameters:**
   - When introducing any new configuration section or parameter in `AppConfig`, it must immediately be reflected in `tauqe.toml`.
   - New or optional parameters should be provided in commented-out form (`# key = value`) with their default values explicitly shown, along with a concise descriptive comment.
4. **No Hardcoded Budgets:**
   - All token budgets and operational limits (history tokens, tail turns count, repomap tokens, discovery rounds, retry counts) must be configurable through `AppConfig` and carry sensible, modern defaults.

---

## 8. Living Documentation and Conceptual Ontology (`docs/`)

1. **Dual-Use Documentation as First-Class Artifact:**
   - The user guide and architecture documentation located in `docs/` serve both as the authoritative reference for human developers and as the high-density semantic concept ontology for the AI model.
   - `docs/src/SUMMARY.md` is pinned as the primary conceptual index and must reflect all chapters.
2. **Rationale and Intent ("Why" Alongside "How"):**
   - Documentation must never be a dry catalog of procedural instructions, flags, or keystrokes describing merely *how* a feature operates.
   - Every chapter, architectural overview, and feature guide must explicitly articulate the underlying engineering motivation, trade-offs, and conceptual rationale (*why* it is designed this way).
   - Explaining the "why" allows both human engineers and the large language model to grasp the deep architectural intent, preventing misaligned refactorings, drift, and accidental violations of core invariants.
3. **Mandatory Documentation of Conceptual Changes:**
   - Whenever a task introduces or alters user-facing behavior, context mechanics (e.g., layers, permissions), the bounded turn lifecycle, interface workflows, or keybindings, the corresponding chapters in `docs/src/` must be updated within the same turn.
   - Obsolete explanations must be revised immediately to prevent documentation rot and model hallucination.
4. **Accessible Engineering Prose (No Florid or Baroque Language):**
   - While documentation must clearly convey deep architectural intent and rationale, it must be written in simple, direct, and unpretentious technical English.
   - High information density does not mean dense or complex grammar; clarity and accessibility take precedence over literary style.

---

## 9. System Prompts and Generalization (Anti-Overfitting)

1. **Generalized Rules over Discussion Artifacts:**
   - When authoring, modifying, or refining prompts generated by or built into the system (such as system instructions, code review prompts, patch-retry templates, or compaction prompts), all instructions, constraints, and requirements must be formulated strictly in a general, robust, and conceptual manner.
   - **Strict Prohibition of Anecdotal Overfitting:** Never inject verbatim examples, incidental bug snippets, task-specific names, or conversational fragments directly from the current discussion that motivated the prompt change.
   - Embedding discussion-specific examples leads to generative overfitting, context pollution, and brittle model behavior that fails to generalize across diverse programming languages, domains, and user styles.
2. **Operational Invariants over Illustrative Bloat:**
   - Define clear operational invariants, input/output contracts, and behavioral boundaries instead of listing exhaustive ad-hoc special cases.
   - When specifying behavioral rules (such as language consistency, error handling, or format requirements), state the invariant directly (e.g. *"Formulate responses in the primary natural language established by the user's instructions"*), avoiding incidental sample queries or task-specific illustrations.

---

## 10. Subsystem Conventions

Subsystems may define local conventions that supplement these repository-wide rules:
- **Terminal UI (`crates/tui`):** `crates/tui/Conventions.md` (layout semantics, Magit/Readline navigation, modal dialog architecture, and focus model).
- **Core Engine (`crates/core`):** `crates/core/Conventions.md` (dual-contour structured output and storage persistence invariants).

Subsystem conventions are resolved and included in the Pinned context layer automatically by the harness whenever files within that subsystem are loaded into context.

---

## 11. Observability and Structured Logging

1. **Library-versus-Daemon Logging Boundary (`core` vs `server`):**
   - `crates/core` is a library and a pure generator of semantic observability events. It emits them exclusively through the lightweight `tracing` facade (`info!`, `warn!`, `error!`) and never depends on file sinks, on-disk formats, or rotation policy.
   - `crates/server` owns the entire observability sink: subscriber initialization, the on-disk format, and the rotation policy. It installs `tracing-subscriber` (structured JSON records with millisecond RFC-3339 UTC timestamps) writing into `<repo_root>/.tauqe/server.log`, rotates that file by size (10 MiB by default), and retains only a bounded number of numbered archives (`server.log.1`).
   - Rotating file output is confined to the daemon. `crates/tui` must never open a competing file descriptor on the rotating journal: it observes turn progress through RPC events only and never writes the server journal directly.
2. **State Transitions Invariant:**
   - Every significant state transition, operation lifecycle boundary, and failure point in the server runtime and the domain engine must emit a semantic log record through the `tracing` facade.
   - This covers daemon startup and shutdown (stdin EOF, drain phase, forced-abort timeout), RPC transport events (incoming call, sanitized parameter summary, parse failure, per-request duration), operation-slot acquisition and cancellation signals, bounded-turn FSM phase changes, batch queue steps, Coder Ladder escalation, replanning activation, toolchain failures, and provider/network failures.
   - **Rationale (Why):** a persistent daemon runs detached from the developer's terminal. Without structured transition records there is no deterministic way to reconstruct why a turn failed, whether an operation slot leaked, or which provider error aborted a run after the fact.
3. **Anti-Bloat Logging Invariant:**
   - Raw generation stream deltas (`TextDelta`, `ReasoningDelta`), full prompt bodies, context file contents, `RepoMap` dumps, and raw diffs must never be written to the journal in normal operation.
   - The journal records exclusively interaction metadata, compact identifiers, and aggregated token and time metrics. This keeps the file analytically useful and bounds its size, so `grep`, `tail -f`, and `less` remain effective and rotation cannot be defeated by a single verbose turn.
4. **Panic Observability:**
   - The daemon installs a custom panic hook at startup that records the source location, the textual payload, and — when `RUST_BACKTRACE` enables it — the backtrace as a single `error`-level record with `target = "panic"` before the process unwinds.
5. **Subsystem Instrumentation:**
   - Domain-side instrumentation rules for the engine are specified in `crates/core/Conventions.md`.
