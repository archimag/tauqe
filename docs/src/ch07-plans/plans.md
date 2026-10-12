# 7. Local Plans

While high-level roadmaps and project backlogs are best committed to repository documentation, engineering work frequently involves complex, multi-step tactical tasks (such as protocol additions, database migrations, or subsystem refactorings).

TAUQE provides a dedicated **Local Plans** subsystem designed around structured task execution and the "What" vs "How" duality.

---

## 7.1 Architecture & Storage

- **Isolated & Persistent:** Tactical plans are stored locally in `.tauqe/plans/<id>.json`. Because `.tauqe/` is ignored by Git, plans never pollute repository commits or branch history.
- **Two-Section Document Model:** Each plan cleanly separates intent from execution into two distinct trees:
  - **Concept Tree (`concept: Vec<ConceptItem>`):** The structured architectural blueprint specifying *What* must be built—core invariants, interfaces, scope boundaries, and non-functional requirements.
  - **Implementation Blueprint (`items: Vec<PlanItem>`):** The ordered checklist of engineering tasks specifying *How* to build it—concrete coding steps, each linked to target files (`files: Vec<String>`) and related concept items (`concept_refs: Vec<String>`).
- **Immune to History Compaction:** Unlike plain conversational chat where instructions fade after compaction rounds, local plans survive session restarts and compaction cycles intact.
- **Typo-Tolerant Identifier Matching:** Plan identifiers (slugs like `jwt-auth` or `tui-refactor`) are resolved using Levenshtein distance and prefix matching. Minor typos made by the developer or model resolve seamlessly to the intended plan.

---

## 7.2 Model Interaction Protocol

TAUQE cleanly divides plan interaction into specialized protocol modes:

### 7.2.1 Structured Output in Planning & Discussion
When creating, refining, or discussing plans in Discussion mode (`# Plan Discussion:`), interaction is governed by strict Structured Output (`DiscussionResponse` JSON schema). The model populates:
- `message`: Explanatory rationale and answers, streamed directly to the TUI;
- `plan_update`: Structured plan payload with `action: "save" | "update" | "delete"`, `id`, `title`, `description`, hierarchical `concept` items, and `items`.

Engineers can trigger targeted discussions on specific concept nodes or implementation steps (`d` in Plans view). The model updates the target node without rewriting unrelated branches of the plan.

### 7.2.2 Step Execution Protocol
When running an assigned leaf task in an isolated turn:
- **Automatic File Pre-loading:** The harness inspects `step.files` and automatically loads the target files into the turn's editable context before the model generates any edits. This completely eliminates redundant Discovery rounds for known files.
- **Concept Focus Retention:** The turn is supplied with the entire concept tree (`<plan_concept>`), completed milestones, and the single target task. The model retains full architectural awareness while chat history is suppressed to prevent hallucination.
- **Streaming Edits:** Code edits are produced via the streaming XML protocol.
- **Completion Marker:** When requirements are satisfied and verified, the step is marked complete via:
  ```xml
  <plan_step_done id="2.1" />
  ```
- The engine marks the step `DONE` on disk and creates an atomic Git commit only after all project toolchain checks (`check`, `clippy`, `test`) pass cleanly.

---

## 7.3 Workflow: From Planning to Execution

TAUQE implements a disciplined, multi-stage planning pipeline:

### 7.3.0 The Duality: "What" vs "How" in Planning
Software architecture thrives when the distinction between intent and implementation is strictly maintained:
1. **The "What" Phase (Conversational Planning):** High-level architectural trade-offs, scope boundaries, and core design invariants are discussed with the developer in Develop (`Ctrl+1`) or targeted node discussions (`d`). The developer and AI collaborate to build the structured Concept Tree.
2. **The "How" Phase (Deep Plan Refinement):** Once the concept is established, the developer triggers automated Deep Refine (`R`). An AI Architect analyzes the actual codebase AST contours and Tree-sitter symbols to produce the Implementation Blueprint.

### 7.3.1 Deep Plan Refinement (Architectural Decomposition)
Deep Plan Refinement (`R` in Plans view or RPC `plan/refine`) executes an isolated architectural pass with strict invariants:
- **Concept Immutability Invariant:** The AI Architect receives the Concept Tree as an immutable architectural requirement. The architect is strictly prohibited from altering, rephrasing, or redesigning agreed concept items. Its sole mandate is to generate the implementation steps under `items`.
- **Target File Identification:** For every step, the architect identifies the concrete repository files that must be touched (`files: [...]`) and links the step to corresponding concept nodes (`concept_refs: [...]`).
- **Context Isolation:** To prevent generative hallucinations and prompt pollution, Deep Refine executes outside normal conversational history. The architect receives the Tree-sitter Repo Map, active context files, and the target plan structure.
- **Explicit Blocker Protocol (Refusal to Guess):** If requirements are contradictory, specifications incomplete, or codebase conventions violated, the architect must **refuse to guess**. It returns a structured `Blocked` status explaining the exact conflict. TAUQE presents this in a dedicated blocker modal with an option to immediately discuss the resolution with AI in Develop (`d`).

### 7.3.2 Autonomous Scope Execution (Server-Orchestrated)
A **scope** is one leaf step, a group of steps, or the whole plan. The client sends a single `plan/execute` request with `plan_id` and an optional `step_id`. The server owns the queue, the status transitions, and the advance from step to step.

**Why the server owns the batch.** The step lifecycle (mark `in_progress`, run a turn, mark `done`) must be atomic and must not depend on a client that can disconnect or restart. If the TUI kept the queue, a reconnect would lose the batch and two clients could disagree about the plan state. With one owner, the TUI is a thin view: it only starts the scope and renders the events `plan/updated`, `plan/listChanged`, `turn/phase`, and `model/*`.

1. **Select a Scope:** In Plans (`Ctrl+4`), place the cursor on a leaf, a group, or the plan header and press `x` (or `Enter` on a leaf). A modal dialog lists the steps. Press `y` or activate `Execute` to confirm; the default focus is `Cancel`.
2. **Server Scope Evaluation:** The server rejects the request if any step in the scope is in `DISCUSSION`, if the target is `CANCELLED`, or if everything is already `done`. Otherwise it builds the ordered list of leaf steps in `TODO` or `IN_PROGRESS`. Only one operation can run at a time.
3. **Mandatory Pre-Flight Verification:** Before any step starts, the server runs the exact same toolchain pipeline (`VerifyTarget::All`) that the core runs after every autonomous step. If the repository does not build or its tests fail, execution is blocked synchronously, before any LLM request, before any step moves to `in_progress`, and at zero token cost. The server returns `PREFLIGHT_VERIFICATION_FAILED` with the full compiler or test output and leaves the scope statuses untouched. Repositories without a detected toolchain skip this gate transparently.
4. **Per-Step Isolated Turn:** For each step, sequentially, the server marks it `in_progress`, pre-loads `step.files` into editable context, injects the concept tree, suppresses conversational history, and runs a bounded turn. The interface switches to Develop (`Ctrl+1`) and shows the live turn.
5. **Atomic Completion:** A step becomes `done` only when the turn is successful: the edits were applied and passed all toolchain checks, or the model emitted the terminal success tag. A text-only answer without code and without the tag does not close the step. Marking the step `done` and moving to the next step is one server decision.
6. **Server-Side Fail-Fast:** On any step failure (patch cannot be applied, failed compile or tests, terminal error tag, no decision from the model, cancellation) the server sets the step back to `todo`, reports the reason, and stops the batch. Steps completed earlier stay `done`. The failed turn's own file changes are undone by the Git workflow checkpoint.
7. **Parent Status Recalculation:** When all child tasks under a parent node become `done`, the parent is automatically marked `done`.
8. **Safe Cancellation:** `Esc` / `Ctrl+C` cancels the running turn. The cancelled step returns to `todo` and the rest of the batch does not start.

---

## 7.4 Plans View Keybindings

| Keybinding | Action |
|---|---|
| `n` / `p` (or `↑` / `↓`) | Move selection through plans, concept items, and implementation items |
| `Enter` / `x` | Execute leaf implementation step, group, or entire plan (with confirmation) |
| `d` | Discuss selected concept item, implementation step, or plan architecture in Develop |
| `R` | Deep refine implementation blueprint with AI Architect (preserves concept) |
| `t` / `s` | Open status selection dialog (`DISCUSSION`, `TODO`, `IN_PROGRESS`, `DONE`, `CANCELLED`) |
| `Tab` / `Space` | Fold or unfold section (Concept / Implementation) or hierarchical items |
| `a` | Toggle fold / unfold all plans and sections |
| `c` / `y` | Copy entire plan to clipboard in Markdown format |
| `r` | Refresh plans from server storage |
| `Delete` | Delete plan (with confirmation) |
| `Ctrl+H` / `?` | Open contextual Help dialog |
| `Ctrl+1..5` | Quick switch between views |
