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

---

## 1.2 The Semantic Quartet: Model Roles and Specialization

Modern engineering workflows demand distinct cognitive strengths depending on the operational context. Rather than categorizing models into arbitrary difficulty tiers (*junior*, *middle*, *senior*), TAUQE structures model interactions around **semantic functional roles**:

- **Scribe:** Dedicated to history compaction, session summarization, and Git squash commit messages. Requires high speed, concise output, and minimal token cost without complex code reasoning overhead.
- **Coder:** Dedicated to patch generation, code editing, and toolchain verification healing. Specialized in syntactically sound diffs, surgical search/replace operations, and minimal blast radius.
- **Architect:** Dedicated to conceptual design, plan refinement (`plan/refine`), deep code review, and architectural reasoning. Serves as the escalation target when implementation-level heuristics stall.
- **Advisor:** Dedicated to conversational engineering consultation, brainstorming, and interactive develop sessions with the human engineer.

### Deterministic Fallback Resolution

TAUQE rejects speculative `default` configurations. A project configuration is valid as long as at least one semantic role is defined:
- Any unconfigured role is deterministically inferred along a fixed proximity chain across the specified roles (e.g. resolving `coder` -> `advisor` -> `architect` -> `scribe`).
- This invariant ensures zero configuration friction: teams can configure four purpose-tuned models for maximum efficiency, or specify a single model that predictably serves all four roles.

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
- Plan refinement and review session creation invoke **Architect**.
- Interactive prompts in the Develop pane invoke **Advisor** (or the engineer's manual override).
