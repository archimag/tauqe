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
