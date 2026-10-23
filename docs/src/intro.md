# Introduction

Welcome to **The Tauqe Book** — the authoritative guide and conceptual reference for TAUQE (**T**he **A**nswer to the **U**ltimate **Q**uestion of **E**ngineering), an AI-native engineering control environment built on the **literal semantic harness** paradigm.

---

## What is TAUQE?

TAUQE is neither an autonomous bash agent nor a context micromanagement assistant. It is a strictly disciplined engineering environment designed to resolve the fundamental challenge of modern AI-assisted software development: **the duality of "What" versus "How"**.

### The "What" versus "How" Duality
Software engineering has always hinged upon two foundational questions:
1. **What** to build — architectural intent, system boundaries, security constraints, and domain requirements.
2. **How** to build it — synthesizing boilerplate, refactoring call sites, wiring data pipelines, and adjusting syntax.

Large language models are exceptionally skilled at answering *How* when guided within strict structural boundaries. However, modern tooling forces developers into one of two failing extremes:
- **Context Micromanagement (e.g. Aider):** Developers act as manual context logisticians (`/add`, `/drop`, `/read-only`), spending more mental energy counting tokens and guessing file dependencies than reasoning about design.
- **Uncontrolled Agentic Bash Loops (e.g. Devin):** Models are given an open-ended terminal shell where they drift aimlessly, re-install packages, corrupt system libraries, and fall into degenerative hallucination loops.

### The Semantic Harness
TAUQE liberates the developer's cognitive bandwidth so they can focus entirely on **What**, while holding the language model in a literal, physical harness:
- **Blinkers (Tree-sitter Repo Map):** Grants global architectural awareness across symbols without polluting the active context window.
- **Bit & Bridle (Strict Protocol):** Strips the model of arbitrary shell access. All changes occur through typed protocol tags, in-memory virtual staging, and deterministic toolchain verification.
- **Reins (Git Transactions):** Every turn is transactional. The developer steers direction, while Git guarantees instantaneous, zero-loss rollback (`u`) at any sign of drift.

Autonomy is granted only inside a single bounded turn. The moment the turn concludes, control returns to the developer. The harness exists to prevent **agentic drift** — the unconstrained tendency of a model to wander, repeat failed loops, or reach beyond its assigned mandate.

### Role Specialization: Dividing "What" from "How"
TAUQE splits the *What*/*How* duality across dedicated, non-overlapping model roles. Each role keeps a single mandate and never absorbs the responsibility of another:
- **Architect** owns the *What*: high-level decomposition, system invariants, and conceptual boundaries. It produces plans and concept nodes, not concrete source lines.
- **Coder** owns the *How*: minimal, coherent, compiling code changes that resolve one well-posed step.
- **Primary** owns the interactive engineering dialogue and is the mandatory base role: its first entry is the idle active model and the deterministic default for every specialized role that omits its own list.
- **Reviewer** performs analytical, read-only inspection and emits findings; it never modifies files.
- **Scribe** handles history compaction, turning long multi-turn transcripts into dense, lossless context.

Each role is configured as an ordered list of model entries. `primary` is mandatory, and any specialized role without its own list inherits the full `primary` list.

The Architect/Coder boundary is deliberately rigid. When code generation fails repeatedly, the harness **never switches the Coder into the Architect role**. Escalation stays intra-role: the *coder ladder* promotes execution to a stronger coder tier (a model paired with a reasoning effort) within the same *How* competence. Crossing back into the *What* role on a compile failure would blur the boundary the harness exists to enforce.

---

## Architectural Heritage: Magit and SLIME

TAUQE's interface and transport model inherit directly from two long-standing engineering tools:
- **Magit UX Heritage:** The screen is not a chat stream with bubbles, but a **structured, live document of state** — high density, fully keyboard-driven, with collapsible sections and rich interactive diffs.
- **SLIME / Swank Protocol:** Clean decoupling of client and server. The server encapsulates the project model, Git transactions, and model orchestration; clients (the TUI, and future frontends) are lightweight interactive frontends communicating via typed RPC.

---

## Guide Structure

The book is organized into five orthogonal parts. Each significant concept lives in its own sovereign chapter, so a `<doc_request>` loads focused invariants without the noise of neighboring subsystems.

### Part I — Foundations & Cognitive Architecture

- **[Philosophy: The Semantic Harness](ch01-foundations/semantic-harness.md)** — Core mission, the literal harness metaphor, and the architectural comparison against agentic loops and manual context logisticians.
- **[The Autonomous Bounded Turn FSM](ch01-foundations/bounded-turn-fsm.md)** — The deterministic finite-state turn: Discovery, in-memory staging, patch retry resilience, verification healing, and rollback.
- **[Model Roles and the Coder Ladder](ch01-foundations/model-roles.md)** — The mandatory primary role, specialized role lists, the coder and architect ladders, and decision immutability.
- **[Turn Scratchpad](ch01-foundations/turn-scratchpad.md)** — Turn-local working memory and its isolation from persisted history.

### Part II — Context Perception & Discovery

- **[Three-Tier Context Model](ch02-perception/three-tier-model.md)** — Architectural rationale of Pinned, User, and Auto context layers, token hygiene, and multi-round Discovery.
- **[Discussion Focus](ch02-perception/discussion-focus.md)** — Anchored architectural dialogue and its typed target.
- **[Repository Map and Blinkers](ch02-perception/repo-map.md)** — The deterministic Tree-sitter outline and its budgets.
- **[Discovery Engine](ch02-perception/discovery-engine.md)** — Handler dispatch, round guards, and the cumulative discovery journal.

### Part III — Project Integration & Execution

- **[The TauqeProjectProtocol](ch03-project/tauqe-project-protocol.md)** — The Unix CLI contract project operations obey, the single `[project]` declaration, and the process boundary that keeps the engine technology-agnostic.
- **[The Three-Phase Modification Lifecycle](ch03-project/three-phase-lifecycle.md)** — In-memory format, disk post-hook, and mandatory verification, in the only order that keeps the tree consistent.
- **[Structured Diagnostics and Verification Healing](ch03-project/verification-healing.md)** — Typed findings, deterministic reports, and the bounded repair loop that consumes them.
- **[Git Safety & Transactions](ch03-project/git-transactions.md)** — Checkpoints, temporary step commits, crash recovery, and deterministic rollback.

### Part IV — Workflows

- **[Local Plans](ch04-workflows/plans.md)** — Managing tactical multi-step tasks, surviving history compaction, and steering work via interactive checklists.
- **[Code Review](ch04-workflows/review.md)** — Why AI-assisted review is the primary interface at high generation speeds, finding triage, and selective context injection.

### Part V — System & Interface

- **[Configuration and Project Preparation](ch05-system/configuration.md)** — The configuration hierarchy and how a project is prepared.
- **[Keyboard-First Interface](ch05-system/tui-ux.md)** — Magit/Org-inspired live document state versus chat bubbles, view spatial hierarchy, and high-density interaction.
- **[Zero-Hardcode Interface Localization](ch05-system/i18n-subsystem.md)** — One canonical catalog, custom overlays, and the language directive of a turn.
