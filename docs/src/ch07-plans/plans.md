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

TAUQE provides two complementary workflows for working with tactical plans:

### 7.3.1 Context Injection (Manual Steering)
1. **Formulate the Plan:** In Develop (`Ctrl+1`), ask TAUQE to design an implementation plan. The model outputs a structured `<plan>` tag, which populates the Plans view.
2. **Select Focus Tasks:** In Plans (`Ctrl+4`), navigate with `n` / `p` or `↑` / `↓` and press `x` to check tasks (`[x] 2.2 Handler implementation`).
3. **Context Injection:** Checked items are injected into subsequent prompts as `<active_plan_context>`.
4. **Execute in Develop:** Return to Develop and instruct the harness to work on the selected items.

### 7.3.2 Autonomous Step Execution (Engine-Driven)
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
| `n` / `p` or `↑` / `↓` | Move selection through the plan hierarchy |
| `e` / `Enter` | Execute selected leaf step (opens confirmation dialog) |
| `x` | Toggle checkbox (mark item for injection into prompt context) |
| `t` / `s` / `d` | Open status selection dialog (`1` Todo, `2` InProgress, `3` Done, `4` Cancelled) |
| `Space` | Fold or unfold child items |
| `Tab` / `Shift+Tab` | Switch between active plans |
| `c` / `y` (or `Alt+C`) | Copy entire plan to clipboard in Markdown format |
| `Ctrl+H` / `?` | Open contextual Help dialog |
| `Ctrl+1..5` | Quick switch between views |
