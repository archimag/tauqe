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

- **[Chapter 1: Philosophy: The Semantic Harness](ch01-philosophy/semantic-harness.md)** — Core mission, the literal harness metaphor, model role specialization, and architectural comparison against agentic loops and manual logisticians.
- **[Chapter 2: Three-Tier Context Model](ch02-context/three-tier-model.md)** — Architectural rationale of Pinned, User, and Auto context layers, token hygiene, and multi-round Discovery.
- **[Chapter 3: The Autonomous Bounded Turn](ch03-bounded-turn/lifecycle.md)** — Anatomy of a bounded execution cycle: Discovery, In-Memory Virtual Staging, Patch Retry resilience, and Compiler-driven Self-Healing.
- **[Chapter 4: Git Safety & Transactions](ch04-git-safety/checkpoints-and-undo.md)** — Why real Git commits beat stashes, author isolation, dirty-tree preservation, and deterministic rollback.
- **[Chapter 5: Keyboard-First Interface](ch05-interface/tui-ux.md)** — Magit/Org-inspired live document state versus chat bubbles, view spatial hierarchy, and high-density interaction.
- **[Chapter 6: Structured Code Review](ch06-review/review.md)** — Why AI-assisted review is the primary interface at high generation speeds, finding triage, and selective context injection.
- **[Chapter 7: Local Plans](ch07-plans/plans.md)** — Managing tactical multi-step tasks, surviving history compaction, and steering work via interactive checklists.
