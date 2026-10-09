# 3. The Autonomous Bounded Turn

Instead of an unbounded background agent loop, TAUQE operates as a **finite-state turn machine**.

---

## 3.1 The Turn Lifecycle

```text
               User Prompt
                    ↓
        ┌─► [Discovery Phase] ──── (requests missing files via <context_request>
        │           ↓               or system docs via <doc_request>)
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

- **Discovery & Exploration Phase:** The model inspects the task and navigates the Tree-sitter repository map to locate missing symbols and type signatures, or requests internal system documentation via `<doc_request topic="...">`. If required files are missing from context, the system autonomously fetches them into the `auto` layer and proceeds to the next round. Code generation is deferred until the model has sufficient clarity to avoid blind speculation.

---

## 3.2 In-Memory Staging & Patch Retry

Modifications never touch disk unverified:
- **Virtual Staging:** Search/replace hunks, new files, renames, and deletions are staged in memory against the current file tree.
- **Fuzzy Indentation Resilience:** Indentation shifts, tab vs. space variances, and trailing whitespace discrepancies are resolved deterministically without human intervention.
- **Patch Retry Loop:** From time to time, models generate diffs that cannot be reliably anchored to existing source files due to stale context or ambiguous surrounding lines. When a match error occurs, TAUQE does not abandon the turn or corrupt the file system:
  1. Already validated and staged files are preserved in memory.
  2. The failing file edits and precise matching errors are formatted into an error report.
  3. The model is called again in a dedicated retry round to re-emit only the failing hunks, repeating until all files stage cleanly or the retry limit is exhausted.

---

## 3.3 Verification & Self-Healing

- Once staging converges, edits are atomically applied to disk.
- TAUQE executes project-specific toolchain checks (`cargo check`, `npm test`).
- If compilation fails, compiler diagnostics are parsed and returned to the model for an immediate **Self-Healing Loop**, fixing syntax and type errors automatically before concluding the turn.

---

## 3.4 Dual Protocol Circuits: Execution vs Discussion

TAUQE separates model interaction into two dedicated protocol circuits based on functional requirements:

1. **Execution Protocol (Streaming XML):**
   - Active during code modification and isolated plan step execution.
   - Employs streaming XML search/replace blocks (`<edit>`, `<create>`, `<delete>`).
   - Resistant to unescaped quotes, arbitrary source formatting, and indentation shifts.
   - Patches are validated in virtual staging before touching disk, followed by the toolchain compiler gate.

2. **Discussion Protocol (Strict Structured Output):**
   - Active during architectural discussions, plan refinement, and code review triage.
   - Strictly read-only: file edits, patch proposals, and code modification tags are prohibited.
   - Governed by JSON Schema (`DiscussionResponse`):
     - `message`: Markdown explanation streamed immediately to the TUI;
     - `plan_update`: Structured modifications to project plans (`save`, `update`, `delete`);
     - `context_requests`: Read-only file inspection requests.
   - Eliminates turn-marker leaking, regex parsing ambiguities, and history pollution.
