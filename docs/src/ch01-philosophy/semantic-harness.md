# 1. Philosophy: The Semantic Harness

TAUQE is an **AI-native engineering control environment** structured around the concept of a **literal semantic harness** for large language models.

---

## 1.1 The Literal Harness

The term *harness* is used here in its original, physical meaning: **bridle, bit, reins, and blinkers**.

A modern large language model is a fast, capable, but blind workhorse:
- **Without boundaries (Uncontrolled Bash Agents):** Granted arbitrary shell access, a model spins in infinite loops, breaks OS packages, and wanders off course.
- **Without autonomy (Context Micromanagement):** Forcing developers to manually add and drop files via `/add` commands transforms engineers into "context logisticians", multiplying cognitive fatigue.

TAUQE resolves this dilemma by holding the model in a firm grip:
- **Blinkers (Repo Map):** Tree-sitter extracts symbol signatures across the codebase, providing immediate architectural orientation without context bloat.
- **Bit & Bridle (Strict Protocol):** The model cannot execute arbitrary shell commands. It interacts solely through typed protocol tags (`<edit>`, `<create>`, `<context_request>`, `<verify>`).
- **Reins (Git Transactions):** The developer steers high-level objectives, while Git guarantees instant, deterministic rollback at any sign of divergence.

> **TAUQE is not an agent. It is an environment where the model operates autonomously within a single bounded turn, completely stripped of persistent agentic drift.**

Autonomy exists only inside one bounded turn; the moment it concludes, control returns to the developer. This is what eliminates **agentic drift**: an unconstrained model, given an open-ended shell and an unbounded goal, tends to wander, repeat failed loops, and reach beyond its mandate.

---

## 1.2 Model Roles: The "What" and the "How"

Modern engineering workflows demand distinct cognitive strengths depending on the operational context. Rather than categorizing models into arbitrary difficulty tiers (*junior*, *middle*, *senior*), TAUQE structures model interactions around **semantic functional roles**:

- **Scribe:** Dedicated to history compaction, session summarization, and Git squash commit messages. Requires high speed, concise output, and minimal token cost without complex code reasoning overhead.
- **Coder:** Dedicated to patch generation, code editing, and toolchain verification healing. Specialized in syntactically sound diffs, surgical search/replace operations, and minimal blast radius. Owns the *How*.
- **Architect:** Dedicated to conceptual design, plan refinement (`plan/refine`), and architectural reasoning. It defines system invariants and decomposition — the *What* — and does not produce concrete source lines.
- **Advisor:** Dedicated to conversational engineering consultation, brainstorming, and interactive develop sessions with the human engineer.
- **Reviewer:** Dedicated to analytical, read-only code inspection. It emits findings and never modifies files.

### The Rigid Architect/Coder Boundary and Intra-Role Escalation

The Architect/Coder boundary is deliberately rigid: the Architect defines the invariant, the Coder implements it. When code generation fails repeatedly, the harness **never promotes the Coder into the Architect role**. Such a switch would blur the very boundary the harness exists to enforce and would reintroduce the drift the paradigm is designed to prevent.

Escalation is therefore **intra-role**. The *coder ladder* (`CoderTier`, each tier pairing a model with a reasoning effort) promotes execution to a stronger coder tier within the same *How* competence, governed by `auto_level_up`. The Architect remains an escalation target only for conceptual work — plan refinement and architectural reasoning — never for compile failures.

### Deterministic Fallback Resolution

TAUQE rejects speculative `default` configurations. A project configuration is valid as long as at least one semantic role is defined:
- Any unconfigured role is deterministically inferred along a fixed proximity chain across the specified roles (e.g. resolving `coder` -> `advisor` -> `architect` -> `scribe`).
- This invariant ensures zero configuration friction: teams can configure a purpose-tuned model per role for maximum efficiency, or specify a single model that predictably serves all roles.

### Role-Scoped Reasoning Depth

Reasoning depth is configured **per semantic role**, never bound to a model name. The optional `[models.effort]` subsection of `tauqe.toml` stays fully orthogonal to model identity: `[models]` answers *who thinks*, while `[models.effort]` answers *how deeply it thinks*.

- **Typed levels.** Effort is a strictly typed `ReasoningEffort` value (`low`, `medium`, `high`, `xhigh`), deserialized from lowercase strings rather than free-form text.
- **Conservative defaults.** Any role omitted from the subsection falls back to the provider default, and the subsection itself is optional. Existing configurations remain 100% valid without edits, so role-scoped effort never forces a migration.
- **Role reuse without duplication.** Because depth is role-scoped, the same concrete model can run shallowly in one role and deeply in another; there is no need to register a duplicate model entry per cognitive depth.

```toml
[models]
architect = "~google/gemini-pro-latest"

# Optional per-role reasoning depth; omitted roles use the provider default.
[models.effort]
architect = "xhigh"   # deep conceptual design and plan refinement
coder = "medium"      # balanced patch generation and verification healing
```

### Contextual Routing

Core subsystems automatically dispatch tasks to their designated semantic role:
- Compaction and Git squash commit generation invoke **Scribe**.
- Batch plan step execution and code review remediation invoke **Coder**.
- Plan refinement invokes **Architect**.
- Review session creation and analytical inspection invoke **Reviewer**.
- Interactive prompts in the Develop pane invoke **Advisor** (or the engineer's manual override).

---

## 1.3 Architectural Heritage: Magit and SLIME

TAUQE's interface and transport model are not invented in a vacuum; they inherit directly from two long-standing engineering tools:

- **Magit UX Heritage:** The interface is not a chat stream with bubbles, but a **structured, live document of state**. It is high density and fully keyboard-driven, with collapsible sections, contextual cursor operations, and rich interactive diffs.
- **SLIME / Swank Protocol:** Clean decoupling of client and server. The server encapsulates the project model, Git transactions, and model orchestration; clients (the TUI, and future frontends such as an Emacs package) are lightweight interactive frontends communicating via typed RPC.

This heritage reinforces the core harness principle: the environment exposes state and capabilities through a structured, inspectable surface, never through an open-ended command line.
