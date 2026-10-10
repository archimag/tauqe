# 7. Local Plans

While high-level roadmaps and project backlogs are best committed to repository documentation, engineering work frequently involves complex, multi-step tactical tasks (such as protocol additions, database migrations, or subsystem refactorings).

TAUQE provides a dedicated **Local Plans** subsystem designed around structured task execution and the "What" vs "How" duality.

---

## 7.1 Architecture & Storage

- **Isolated & Persistent:** Tactical plans are stored locally in `.tauqe/plans/<id>.json`. Because `.tauqe/` is ignored by Git, plans never pollute repository commits or branch history.
- **Immune to History Compaction:** Unlike plain conversational chat where instructions fade after compaction rounds, local plans survive session restarts and compaction cycles intact.
- **Typo-Tolerant Identifier Matching:** Plan identifiers (slugs like `jwt-auth` or `tui-refactor`) are resolved using Levenshtein distance and prefix matching. Minor typos made by the developer or model resolve seamlessly to the intended plan.

---

## 7.2 Model Interaction Protocol

TAUQE cleanly divides plan interaction into two specialized protocol modes:

### 7.2.1 Structured Output in Planning & Discussion
When creating, refining, or discussing plans in Discussion mode (`# Plan Discussion:`), interaction is governed by strict Structured Output (`DiscussionResponse` JSON schema). The model populates:
- `message`: Explanatory rationale and answers, streamed directly to the TUI;
- `plan_update`: Structured plan payload with `action: "save" | "update" | "delete"`, `id`, `title`, `description`, and hierarchical `items`.

This guarantees 100% deterministic deserialization directly into `PlanStorage` without relying on XML tag extraction, turn-marker heuristics, or string parsing.

### 7.2.2 Step Execution Protocol
When running an assigned leaf task in an isolated turn:
- The turn is focused solely on the single target step with chat history suppressed.
- Code edits are produced via the streaming XML protocol.
- When requirements are satisfied and verified, the step is marked complete via:
  ```xml
  <plan_step_done id="2.1" />
  ```
- The engine marks the step `DONE` on disk and creates an atomic Git commit only after all project toolchain checks (`check`, `clippy`, `test`) pass cleanly.

---

## 7.3 Workflow: From Planning to Execution

TAUQE implements a disciplined, multi-stage planning pipeline:

### 7.3.0 The Duality: "What" vs "How" in Planning
Software architecture thrives when the distinction between intent and implementation is strictly maintained:
1. **The "What" Phase (Conversational Planning):** High-level architectural trade-offs, scope boundaries, and core design invariants are discussed and agreed with the developer in Develop (`Ctrl+1`). In this phase, TAUQE focuses strictly on understanding *what* needs to be achieved, avoiding low-level boilerplate drift.
2. **The "How" Phase (Deep Plan Refinement):** Once the high-level plan structure is established, the developer triggers automated Deep Refine (`R`). An AI Architect analyzes the actual codebase AST contours and Tree-sitter symbols to produce an actionable, atomic engineering decomposition.

### 7.3.1 Deep Plan Refinement (Architectural Decomposition)
Deep Plan Refinement (`R` in Plans view or RPC `plan/refine`) executes an isolated architectural pass with strict invariants:
- **Anti-Drift Invariant (No Conceptual Drift):** The architect model is explicitly prohibited from redesigning agreed requirements, inventing unprompted features, or altering the high-level architectural intent. Its sole mandate is to break down existing high-level items into verified leaf steps.
- **Context Isolation:** To prevent generative hallucinations and prompt pollution, Deep Refine executes outside normal conversational history. The architect receives the Tree-sitter Repo Map, active context files, and the target plan structure.
- **Atomic Decomposition & Roles:** Steps are decomposed into leaf tasks with concrete verification requirements. The model can assign optimal model tiers (Senior for intricate algorithmic changes, Junior/Middle for mechanical steps).
- **Explicit Blocker Protocol (Refusal to Guess):** If requirements are contradictory, specifications incomplete, or codebase conventions violated, the architect must **refuse to guess**. It returns a structured `Blocked` status explaining the exact conflict. TAUQE presents this in a dedicated blocker modal with an option to immediately discuss the resolution with AI in Develop (`d`).

### 7.3.2 Context Injection (Manual Steering)
1. **Formulate the Plan:** In Develop (`Ctrl+1`), ask TAUQE to design an implementation plan. The model outputs a structured `<plan>` tag, which populates the Plans view.
2. **Select Focus Tasks:** In Plans (`Ctrl+4`), navigate with `n` / `p` or `↑` / `↓` and press `x` to check tasks (`[x] 2.2 Handler implementation`).
3. **Context Injection:** Checked items are injected into subsequent prompts as `<active_plan_context>`.
4. **Execute in Develop:** Return to Develop and instruct the harness to work on the selected items.

### 7.3.3 Autonomous Step Execution (Engine-Driven)
For disciplined step-by-step implementation, execute plan leaves directly through the engine:
1. **Select a Leaf Step:** In Plans (`Ctrl+4`), place the cursor on a leaf task and press `e` or `Enter`. Container nodes with child tasks cannot be executed directly; each leaf must be run individually to maintain minimal change scope.
2. **Confirm Execution:** A modal confirmation dialog displays the task title and details. Press `Enter` to proceed or `Esc` to cancel.
3. **History-Isolated Context:** The engine marks the step `in_progress` on disk, suppresses conversational chat history to eliminate model drift, and provides the model with the plan overview, completed milestones, and the single target task.
4. **Autonomous Turn & Live Monitoring:** The interface automatically switches to Develop (`Ctrl+1`). The model generates edits, and the harness verifies them against project compilers and linters in real time.
5. **Verification Safety Gate:** The step is marked `done` on disk only if the model emits `<plan_step_done id="..."/>` AND all verification commands pass cleanly. If verification fails or the model does not emit the tag, the step remains `in_progress`.
6. **Parent Status Recalculation:** When all child tasks under a parent node transition to `done`, the parent node is automatically marked `done`.
7. **Safe Cancellation:** Pressing `Ctrl+C` cancels execution, safely rolls back working tree modifications to the pre-turn checkpoint, and retains the step in `in_progress`.

---

## 7.4 Plans View Keybindings

| Keybinding | Action |
|---|---|
| `n` / `p` (or `↑` / `↓`) | Move selection through the plan hierarchy |
| `Enter` / `x` | Execute leaf step, group, or entire plan (with confirmation) |
| `d` | Discuss selected item or plan architecture in Develop |
| `R` | Deep refine plan architecture & steps with AI Architect |
| `t` / `s` | Open status selection dialog (`DISCUSSION`, `TODO`, `IN_PROGRESS`, `DONE`, `CANCELLED`) |
| `Tab` / `Space` | Fold or unfold child items |
| `a` | Toggle fold / unfold all plans and items |
| `c` / `y` | Copy entire plan to clipboard in Markdown format |
| `r` | Refresh plans from server storage |
| `Delete` | Delete plan (with confirmation) |
| `Ctrl+H` / `?` | Open contextual Help dialog |
| `Ctrl+1..5` | Quick switch between views |
