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

- **Discovery & Exploration Phase:** The model inspects the task and navigates the Tree-sitter repository map to locate missing symbols and type signatures, or requests internal system documentation via `<doc_request topic="...">`. If required files are missing from context, the system autonomously fetches them into the `auto` layer and proceeds to the next round. Throughout multi-round discovery, the model retains intermediate findings and working hypotheses in the ephemeral Turn Scratchpad. Code generation is deferred until the model has sufficient clarity to avoid blind speculation.

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

### Verification Healing and Role Escalation

When verification errors occur, the model enters a targeted healing cycle:
1. Compiler error messages and diagnostic line spans are parsed and formatted into a targeted healing prompt.
2. Missing error paths are automatically staged into the context working set.
3. The prompt is presented to the active role (typically `Coder`).
4. **Escalation to Architect:** If verification fails repeatedly or patch staging errors persist across retries, superficial syntactic adjustments are often insufficient because the failure stems from a deeper architectural contradiction or missing dependency. When `auto_level_up` is enabled in configuration, the harness automatically escalates the healing turn from `Coder` to `Architect`. The `Architect` model re-evaluates the problem with higher-order reasoning, adjusting types and signatures to resolve the blocker before the turn concludes.

---

## 3.4 Turn Scratchpad: Ephemeral Working Memory

During multi-round discovery and verification healing cycles, maintaining cognitive focus and retaining intermediate findings is essential. Without a dedicated mechanism, a model can lose track of what has already been checked or why specific files were requested.

However, recording intermediate reasoning or scratch notes into persistent session history would pollute context, cause token bloat, and introduce noise into subsequent turns. TAUQE resolves this with the **Turn Scratchpad**:

- **Turn-Scoped Ephemerality (Why & How):** The scratchpad exists strictly within the ephemeral lifecycle of the active bounded turn. It lives in memory across discovery rounds and compiler healing retries, but is completely discarded when the turn ends. It is never written to session history (`.tauqe/history.jsonl`), never recorded in Git commits, and never displayed in the chat view.
- **Protocol Contract (`<scratchpad>`):** The model emits working hypotheses, verified facts, and exploration goals inside `<scratchpad>...</scratchpad>` tags (or `<scratchpad note="..." />`). Turn-marker normalization protects it against collisions with literal source code, while `clean_assistant_text` strips it completely from final user-facing responses.
- **Round-to-Round Injection (`<turn_scratchpad>`):** When the model transitions to the next discovery round or a verification healing step, `PromptAssembly` injects the accumulated scratchpad into the system prompt under `<turn_scratchpad>`. The model instantly regains its working context without re-reading past tool outputs.
- **Strict Budgeting & Anti-Bloat:** To prevent generative bloat, the scratchpad is capped at `MAX_SCRATCHPAD_CHARS` (4,000 characters) with UTF-8 safe boundary truncation. The prompt instructions guide the model to maintain dense, concise bullet points rather than runaway prose.

---

## 3.5 Unified Terminal Outcome Protocol (`<success>` / `<error>`)

To eliminate ambiguity at turn conclusion and prevent speculative execution cascades, every code-modifying turn and isolated task step adheres to a binary terminal outcome contract:

- **Protocol Rationale (Why & How):** Free-form conversational text is nondeterministic and fragile for automated task runners. When executing isolated plan steps or triaging review findings in batch queues, the system requires an unambiguous, machine-readable signal indicating whether the task was fully accomplished or blocked.
- **Binary Outcome Tags:**
  - `<success summary="...">` (or `<success>...</success>`): Signifies that the target goal was fully implemented, staged, and verified by compiler checks. In batch execution (e.g. executing all steps of an implementation plan or resolving code review findings), `<success>` signals the runner to transition the item to `Done`/`Fixed` and advance to the next queued item.
  - `<error message="...">` (or `<error>...</error>`): Signifies that the model encountered an insurmountable architectural contradiction, missing prerequisite, or unresolvable toolchain failure. In batch pipelines, `<error>` immediately halts execution to prevent cascading corruption, marks the current item as blocked, and reports the structured error message to the developer.
- **Turn-Marker Protection:** Like all operational tags, `<success>` and `<error>` are suffixed with the active turn marker (e.g., `<success_MARKER>`), preventing accidental invocation from literal source code, git diffs, or markdown examples.

---

## 3.6 Dual Protocol Circuits: Execution vs Discussion

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
