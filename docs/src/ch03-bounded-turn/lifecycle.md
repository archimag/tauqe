# 3. The Autonomous Bounded Turn

Instead of an unbounded background agent loop, TAUQE operates as a **finite-state turn machine**.

---

## 3.1 The Turn Lifecycle

```text
               User Prompt
                    ↓
        ┌─► [Discovery Phase] ──── (requests missing files via <context_request>)
        │           ↓
        │   [Proposal Generation] ─ (generates minimal coherent patches)
        │           ↓
        │   [In-Memory Staging] ── (validates patches in memory with fuzzy matching)
        │       ├── Match Error → [Patch Retry Loop] (up to N retries)
        │       └── Success → Apply to disk
        │           ↓
        │   [Toolchain Verification] ── (runs cargo check / npm test / tsc)
        │       ├── Compiler Errors → [Verification Healing Loop] ────┐
        │       │                                                     │
        │       └── Clean Build                                       │
        │           ↓                                                 │
        └────── Rollback on Failure / Commit to Git ◄─────────────────┘
```

---

## 3.2 In-Memory Staging & Patch Retry

Modifications never touch disk unverified:
- **Virtual Staging:** Search/replace hunks, new files, and deletions are staged in memory against the current file tree.
- **Fuzzy Indentation Resilience:** Indentation shifts and trailing whitespace discrepancies are resolved deterministically without human intervention.
- **Patch Retry Loop:** If an edit fails to match, successfully staged files are retained in memory while the model is prompted with targeted feedback to correct only the failing files (up to `max_retries`).

---

## 3.3 Verification & Self-Healing

- Once staging converges, edits are atomically applied to disk.
- TAUQE executes project-specific toolchain checks (`cargo check`, `npm test`).
- If compilation fails, compiler diagnostics are parsed and returned to the model for an immediate **Self-Healing Loop**, fixing syntax and type errors automatically before concluding the turn.
