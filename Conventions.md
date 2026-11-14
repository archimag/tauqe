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
5. **Typed Intent over Textual Inference:**
   - It is strictly forbidden to determine an execution mode, an execution restriction, or an operation type by parsing unstructured text: no prefix checks (`starts_with`), no regular expressions, no keyword searches. A prefix, a heading, or a keyword is a guess, not a contract. It silently changes behavior when the wording changes and is never checked by the compiler.
   - When a subsystem needs to know its calling context, the caller must pass it as a strictly typed DTO or enum. The receiving side reads typed fields and matches variants; it never inspects the text it was given.
   - The canonical example is the bounded turn: its mode is declared by the typed parameters of the loop that runs it, and is never derived from the body of a prompt.
   - This is the operational form of the harness invariant *"Never guess what is deterministically known"*.
   - Subsystem conventions do not duplicate this rule; they reference it instead.
6. **Anti-NIH: Standard Primitives Come from Ecosystem Crates:**
   - Never write a private implementation of an established specification, format, or primitive. Widely standardized artifacts — RFC identifiers (UUID, ULID, URI), cryptographic hashes and algorithms, character encodings and codecs, compression and serialization formats, calendar and time arithmetic, glob and regular-expression matching — are consumed from the de-facto standard Rust crate and never re-derived inside this repository.
   - The trigger is a published specification together with a mature, maintained crate that implements it. When both exist, hand-writing the algorithm is a defect, not a stylistic option: a conforming implementation must reproduce every edge the specification defines — bit layout, endianness, timestamp epochs, version and variant fields, validation and canonical-text rules — and an in-repo re-derivation almost always misses some of them.
   - Adopting the ecosystem crate also protects pretraining alignment. A language model writes code against the API it saw most often during training, so a mainstream crate yields correct calls on the first attempt, while a local invention forces every later modification to reverse-engineer an unfamiliar API. The rationale is the same one that governs dependency selection in §12.1.
   - A local implementation is permitted only when no maintained crate covers the requirement, when the crate cannot enter the build graph for a documented reason (licensing, unsupported target, or the dependency purity of a leaf crate), or when the local code is the deliberate subject of the work. The exemption must be stated next to the code it excuses, never assumed.
   - Rationale (Why): a homegrown primitive is a permanent maintenance liability with an invisible blast radius. It drifts from the standard without any compiler or linter noticing, fails on inputs nobody thought to test, and every downstream consumer inherits the divergence as if it were correct behavior.

---

## 2. Workspace Architecture and Crate Responsibilities

The TAUQE workspace is decomposed into five specialized crates with strict, unidirectional dependency boundaries. Dependencies flow one way only (`tui` -> `protocol`, `server` -> `core` -> `tools` -> `protocol`), and code must never drift across these boundaries:

1. **`crates/protocol` (Pure Wire Contract and Shared Domain DTOs):**
   - **Mandate:** Authoritative single source of truth strictly for server–client (TUI) inter-process communication contracts. Contains JSON-RPC request, response, and event data structures, error codes, and pure data transfer objects (DTOs) exchanged across the server–client boundary.
   - **Allowed:** Serde serialization and deserialization, JSON schema generators, pure validation logic, and self-contained stateless string/path helpers (e.g., glob matching, XML escaping).
   - **Strict Prohibitions:** Strictly NO internal LLM-facing or engine-only protocol contracts (such as model command types, capability trees, or LLM response schemas not exchanged between server and client), NO file system or disk I/O, NO OS command execution, NO network or HTTP clients, NO async runtime (`tokio`) dependencies, and NO workspace dependencies. Must remain a lightweight, fast-compiling leaf crate.
   - **Documented Exception — Plan Evaluation Surface:** A small, pure surface of plan and review computations lives in `protocol` even though it resembles domain logic. It comprises the read-only plan queries (`Plan::evaluate_scope`, `Plan::compute_items_status`, `Plan::count_stats`), the derived-state normalizers that recompute a value from existing structure (`Plan::normalize_leaf_files`, `Plan::refresh_parent_statuses`), and the leaf-scoped assignment helper used for optimistic UI updates (`Plan::update_item_status`). The `tui` crate depends only on `protocol` and must render plan readiness, progress, leaf file badges, and optimistic status changes without an RPC round-trip, so these deterministic, allocation-only functions over the shared plan DTOs are allowed here. They perform no I/O and never touch disk, Git, or the network. All durable plan transitions, persistence, and `Done` immutability enforcement remain in `core`.
   - **Status Cycling Is Presentation-Only:** The `PlanItemStatus::next` and `ReviewItemStatus::next` helpers define the UI cycle order for the status picker and intentionally allow transitions such as `Done -> Blocked`. They are not a permission model. The authoritative guard against mutating or regressing a finished step lives in `core` (`validate_done_immutability`), never in the protocol cycler.

2. **`crates/tools` (Deterministic Substrate):**
   - **Mandate:** Every operation the harness performs without model judgement. Owns virtual staging and its patch algorithms, atomic file writes, Git transactions and checkpoints, the toolchain command runners and task-runner integration, and the Tree-sitter repository map.
   - **Allowed:** Interacting with Git, the file system, and toolchain processes, and consuming the shared wire contract from `protocol`. It holds no cognitive state and drives no turn.
   - **Strict Prohibitions:** NO dependency on `core`, NO LLM provider client, NO bounded turn FSM, NO prompt assembly, NO history or context management.

3. **`crates/core` (Deterministic Harness and Domain Engine):**
   - **Mandate:** The headless engine implementing the literal harness paradigm. Owns the bounded turn FSM, model-facing protocol decoding, prompt assembly, context and history management, LLM provider communication and streaming, plan and review storage engines, and the adapters that hand the substrate its declared inputs.
   - **Allowed:** Driving the substrate against Git, the file system, and toolchain processes, and calling provider HTTP endpoints. Exposing domain traits (`PromptProtocol`, `LlmProvider`, `ApplyPolicy`) and headless domain workflows.
   - **Strict Prohibitions:** Strictly headless; NO presentation or terminal UI dependencies (`ratatui`, `crossterm`). NO RPC transport server or socket listening code.

4. **Protocol Crate Purity:**
   - `crates/protocol` remains a lightweight leaf crate. It declares only
     `serde`, `serde_json`, and `schemars`, carries no workspace dependency, and
     pulls in no file-system, process, network, or async runtime crate.
   - `schemars` derives the JSON Schema of a command from the command type
     itself, so the strict schema a model is constrained by and the type the
     decoder reads are one declaration. Deriving a schema is pure data
     processing: it reads no file, spawns no process, and opens no socket.
   - Introducing any dependency that performs I/O into `crates/protocol` is
     forbidden; such work belongs to `tools`, `core`, or `server`.
   - Rationale (Why): the wire contract must stay the cheapest crate to compile
     and the fastest to load, because every other crate depends on it and the
     client frontend links it directly.
