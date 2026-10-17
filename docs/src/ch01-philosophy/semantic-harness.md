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
- **Bit & Bridle (Strict Protocol):** The model cannot execute arbitrary shell commands. It interacts solely through typed protocol tags (`<edit>`, `<create>`, `<context_request>`, `<repo_search>`, `<verify>`). Instead of delegating arbitrary `grep` or `find` to an open-ended bash loop, TAUQE provides typed repository search (`<repo_search>`) implemented natively and safely in Rust.
- **Reins (Git Transactions):** The developer steers high-level objectives, while Git guarantees instant, deterministic rollback at any sign of divergence.

> **TAUQE is not an agent. It is an environment where the model operates autonomously within a single bounded turn, completely stripped of persistent agentic drift.**

Autonomy exists only inside one bounded turn; the moment it concludes, control returns to the developer. This is what eliminates **agentic drift**: an unconstrained model, given an open-ended shell and an unbounded goal, tends to wander, repeat failed loops, and reach beyond its mandate.

---

## 1.2 Model Roles: The "What" and the "How"

Modern engineering workflows demand distinct cognitive strengths depending on the operational context. Rather than categorizing models into arbitrary difficulty tiers (*junior*, *middle*, *senior*), TAUQE structures model interactions around **semantic functional roles**:

- **Scribe:** Dedicated to history compaction, session summarization, and Git squash commit messages. Requires high speed, concise output, and minimal token cost without complex code reasoning overhead.
- **Coder:** Dedicated to patch generation, code editing, and toolchain verification healing. Specialized in syntactically sound diffs, surgical search/replace operations, and minimal blast radius. Owns the *How*.
- **Architect:** Dedicated to conceptual design, plan refinement (`plan/refine`), and architectural reasoning. It defines system invariants and decomposition — the *What* — and does not produce concrete source lines.
- **Primary:** The mandatory base role and the default active model. It drives interactive engineering dialogue and development sessions with the human engineer, and its first entry is the deterministic default for every specialized role that omits its own list.
- **Reviewer:** Dedicated to analytical, read-only code inspection. It emits findings and never modifies files.

### The Rigid Architect/Coder Boundary and Intra-Role Escalation

The Architect/Coder boundary is deliberately rigid: the Architect defines the invariant, the Coder implements it. When code generation fails repeatedly, the harness **never promotes the Coder into the Architect role**. Such a switch would blur the very boundary the harness exists to enforce and would reintroduce the drift the paradigm is designed to prevent.

Escalation is therefore **intra-role**. The *coder ladder* (`CoderTier`, each tier pairing a model with a reasoning effort) promotes execution to a stronger coder tier within the same *How* competence, governed by `auto_level_up`. The Architect remains an escalation target only for conceptual work — plan refinement and architectural reasoning — never for compile failures.

### Mandatory Primary and List Inheritance

TAUQE rejects speculative `default` configurations and requires an explicit anchor. A project configuration is valid exactly when `models.primary` is present and lists at least one entry:
- `primary` is the mandatory base role, and its first entry is the idle active model.
- `coder`, `architect`, and `reviewer` are optional ordered lists. When a specialized role omits its own list, it inherits the entire `primary` list, so it never silently loses access to the configured models.
- `scribe` is a single utility model that defaults to the first `primary` entry.

This invariant ensures zero configuration friction: teams can attach a purpose-tuned list of models to each role for maximum efficiency, or specify a single `primary` model that predictably serves every role.

### Reasoning Depth and Aliases per Entry

Reasoning depth and human-readable aliases are attached **per model entry**, never bound to a model name. Each list entry is a `ModelEntry` that pairs a concrete model with an optional reasoning depth and an optional alias:

- **Typed levels.** Effort is a strictly typed `ReasoningEffort` value (`low`, `medium`, `high`, `xhigh`), deserialized from lowercase strings rather than free-form text.
- **Conservative defaults.** Effort is optional; an entry that omits it uses the provider default. Existing configurations remain 100% valid without edits.
- **Role reuse without duplication.** Because depth is attached to the entry, the same concrete model can run shallowly in one role and deeply in another; there is no need to register a duplicate model entry per cognitive depth.
- **Readable aliases.** An entry may carry an `alias` that replaces the raw model name wherever the model is presented, keeping cryptic provider identifiers out of the picker.

```toml
[models]
primary = ["~google/gemini-flash-latest"]

architect = [
  { model = "~google/gemini-pro-latest", effort = "xhigh", alias = "Deep Architect" },
]

coder = [
  "~deepseek/deepseek-flash-latest",
  { model = "~anthropic/claude-sonnet-latest", effort = "medium" },
]
```

### Contextual Routing

Core subsystems automatically dispatch tasks to their designated semantic role:
- Compaction and Git squash commit generation invoke **Scribe**.
- Batch plan step execution and code review remediation invoke **Coder**.
- Plan refinement invokes **Architect**.
- Review session creation and analytical inspection invoke **Reviewer**.
- Interactive prompts in the Develop pane invoke **Primary** (or the engineer's manual override).

---

## 1.3 Architectural Heritage: Magit and SLIME

TAUQE's interface and transport model are not invented in a vacuum; they inherit directly from two long-standing engineering tools:

- **Magit UX Heritage:** The interface is not a chat stream with bubbles, but a **structured, live document of state**. It is high density and fully keyboard-driven, with collapsible sections, contextual cursor operations, and rich interactive diffs.
- **SLIME / Swank Protocol:** Clean decoupling of client and server. The server encapsulates the project model, Git transactions, and model orchestration; clients (the TUI, and future frontends such as an Emacs package) are lightweight interactive frontends communicating via typed RPC.

This heritage reinforces the core harness principle: the environment exposes state and capabilities through a structured, inspectable surface, never through an open-ended command line.
