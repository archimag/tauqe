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
