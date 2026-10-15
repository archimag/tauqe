# 7. Local Plans

While high-level roadmaps and project backlogs are best committed to repository documentation, engineering work frequently involves complex, multi-step tactical tasks (such as protocol additions, database migrations, or subsystem refactorings).

TAUQE provides a dedicated **Local Plans** subsystem designed around structured task execution and the "What" vs "How" duality.

---

## 7.1 Architecture & Storage

- **Isolated & Persistent:** Tactical plans are stored locally in `.tauqe/plans/<id>.json`. Because `.tauqe/` is ignored by Git, plans never pollute repository commits or branch history.
- **Two-Section Document Model:** Each plan cleanly separates intent from execution into two distinct trees:
  - **Concept Tree (`concept: Vec<ConceptItem>`):** The structured architectural blueprint specifying *What* must be built—core invariants, interfaces, scope boundaries, and non-functional requirements.
  - **Implementation Blueprint (`items: Vec<PlanItem>`):** The ordered checklist of engineering tasks specifying *How* to build it—concrete coding steps, each linked to related concept items (`concept_refs: Vec<String>`). Target files (`files: Vec<String>`) are carried exclusively by leaf steps.
- **Explicit DAG Dependencies (`depends_on: Vec<String>`):** Tasks form a Directed Acyclic Graph (DAG) rather than a rigid linear list. Steps declare dependencies on preceding items via their IDs. Topological scope validation (`Plan::evaluate_scope`) ensures tasks can only start once all direct prerequisites reach `PlanItemStatus::Done`. Parent task groups have container dependencies normalized so execution remains strictly leaf-scoped.
- **Leaf-Only File Binding Invariant:** Parent items are conceptual milestones, phases, and task groups. They are never executed autonomously and must not carry file lists. Only leaf steps (`PlanItem::is_leaf() == true`) define the target context files and form the atomic execution units of a bounded turn. Any item with children has its `files` and its read-only reference files (`readonly_files`) cleared automatically during XML parsing, storage, and deserialization, so containers and leaves can never disagree. The TUI renders file badges only on leaf task rows and keeps section headers clean: a badge counts both roles (for example `[1 file + 2 ref]`), and an expanded leaf lists the target `files:`, the reference `ref:` paths, and the addressed `concept:` nodes on separate lines.
- **"Done is Immutable" Persistence Invariant:** Completed tasks (`PlanItemStatus::Done`) represent verified, committed engineering progress. `PlanStorage::validate_done_immutability` semantically safeguards them: once a step is marked `Done`, subsequent plan saves, conversational updates, or replanning passes can neither delete it, alter its title, details, or target files, nor regress its status back to `Todo` or `InProgress`. Established architectural milestones cannot be rewritten or erased by generative drift.
- **Wave Horizon Planning Model:** Tactical execution separates an immutable foundation from an adaptive, rolling task horizon. Completed steps (`Done`) are permanent, verified engineering milestones that the model is never asked to re-generate. Uncompleted work forms an open horizon that can be synthesized or revised in full without perturbing completed achievements. `PlanStorage::replace_uncompleted_horizon` atomically splices the verified Done foundation with the new horizon (`[...done_foundation, ...new_horizon_items]`), preventing status regressions, state inconsistencies, and ID collisions.
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
- **Completion Marker:** When requirements are satisfied and verified, the step is marked complete by the terminal success tag (`<success summary="..." />`). The active protocol uses a dynamic per-turn marker suffix (for example `<success_<marker>>`); if the model omits the suffix, an unmarked `<success>` tag outside Markdown code fences is accepted as a fallback (§7.3.3).
- **Semantic Blocker Protocol (`<blocked>`):** If an essential precondition fails, requirements contradict code invariants, or an unexpected architectural impediment arises, the model must **refuse to guess** or engage in blind trial-and-error. It emits `<blocked reason="..." details="..." />`. The harness instantly intercepts this command, aborts the turn, rolls back working tree modifications to the clean pre-turn checkpoint (`policy.rollback()`), and marks the step `BLOCKED` on disk without wasting cognitive tokens on futile patch-retry or healing loops. In batch execution, this triggers the automated closed-loop replanning workflow (§7.3.4).
- The engine marks the step `DONE` on disk and creates an atomic Git commit only after all project toolchain checks (`check`, `clippy`, `test`) pass cleanly.

---

## 7.3 Workflow: From Planning to Execution

TAUQE implements a disciplined, multi-stage planning pipeline:

### 7.3.0 The Duality: "What" vs "How" in Planning
Software architecture thrives when the distinction between intent and implementation is strictly maintained:
1. **The "What" Phase (Conversational Planning):** High-level architectural trade-offs, scope boundaries, and core design invariants are discussed with the developer in Develop (`Ctrl+1`) or targeted node discussions (`d`). The developer and AI collaborate to build the structured Concept Tree.
2. **The "How" Phase (Deep Plan Refinement & Horizon Synthesis):** Once the concept is established, the developer triggers automated Deep Refine (`R`). An AI Architect analyzes the actual codebase AST contours and Tree-sitter symbols to produce the Implementation Blueprint.

### 7.3.1 Deep Plan Refinement & Horizon Synthesis (Architectural Decomposition)
Deep Plan Refinement (`R` in Plans view or RPC `plan/refine`) executes an isolated architectural pass with strict invariants:
- **Concept Immutability Invariant:** The AI Architect receives the Concept Tree as an immutable architectural requirement. The architect is strictly prohibited from altering, rephrasing, or redesigning agreed concept items. Its sole mandate is to generate the implementation steps under `items`.
- **Done Foundation Preservation:** When refining an existing plan that already has completed tasks, those tasks form an immutable foundation. The Architect synthesizes only the remaining horizon, and `PlanStorage::replace_uncompleted_horizon` atomically splices the new tasks into the plan.
- **Target File Identification:** For every leaf step, the architect identifies the concrete repository files that must be touched (`files: [...]`) and links the step to corresponding concept nodes (`concept_refs: [...]`). Parent items stay file-free conceptual containers.
- **Context Isolation:** To prevent generative hallucinations and prompt pollution, Deep Refine executes outside normal conversational history. The architect receives the Tree-sitter Repo Map, active context files, and the target plan structure.
- **Deep Reasoning Depth:** The pass runs under the Architect role and therefore inherits the reasoning depth configured for that role in `[models.effort]` (for example `xhigh`). Deep planning thus receives maximum cognitive effort without binding that effort to a specific model name.
- **Explicit Blocker Protocol (Refusal to Guess):** If requirements are contradictory, specifications incomplete, or codebase conventions violated, the architect must **refuse to guess**. It returns a structured `Blocked` status explaining the exact conflict. TAUQE presents this in a dedicated blocker modal with an option to immediately trigger replanning or discuss the resolution with AI in Develop (`d`).

### 7.3.2 Autonomous Scope Execution (Server-Orchestrated)
A **scope** is one leaf step, a group of steps, or the whole plan. The client sends a single `plan/execute` request with `plan_id` and an optional `step_id`. The server owns the queue, the status transitions, and the advance from step to step.

**Why the server owns the batch.** The step lifecycle (mark `in_progress`, run a turn, mark `done`) must be atomic and must not depend on a client that can disconnect or restart. If the TUI kept the queue, a reconnect would lose the batch and two clients could disagree about the plan state. With one owner, the TUI is a thin view: it only starts the scope and renders the events `plan/updated`, `plan/listChanged`, `turn/phase`, and `model/*`.

1. **Select a Scope:** In Plans (`Ctrl+4`), place the cursor on a leaf, a group, or the plan header and press `x` (or `Enter` on a leaf). A modal dialog lists the steps. Press `y` or activate `Execute` to confirm; the default focus is `Cancel`.
2. **Server Scope Evaluation & DAG Verification:** The server evaluates the requested scope against active task states and dependencies:
   - Rejects the request if any step in scope is in `DISCUSSION` status (`STEP_IN_DISCUSSION`). A step explicitly targeted while `CANCELLED` or `BLOCKED` is refused with `STEP_CANCELLED` or `STEP_BLOCKED`; a broad scope instead skips such terminal states.
   - Validates prerequisites (`Plan::evaluate_scope`): if any scheduled step references prerequisites (`depends_on`) that are not yet marked `DONE`, execution is refused upfront with `STEP_BLOCKED_BY_DEPENDENCIES`.
   - If all tasks in scope are already `DONE`, returns `ALL_COMPLETED`.
   - Otherwise, constructs the topologically ordered sequence of executable leaf steps (`TODO` or `IN_PROGRESS` only). Only one batch operation can execute concurrently.
3. **Mandatory Pre-Flight Verification:** Before any step starts, the server runs the exact same toolchain pipeline (`VerifyTarget::All`) that the core runs after every autonomous step. If the repository does not build or its tests fail, execution is blocked synchronously, before any LLM request, before any step moves to `in_progress`, and at zero token cost. The server returns `PREFLIGHT_VERIFICATION_FAILED` with the full compiler or test output and leaves the scope statuses untouched. Repositories without a detected toolchain skip this gate transparently.
4. **Per-Step Isolated Turn:** For each step, sequentially, the server marks it `in_progress`, pre-loads `step.files` into editable context and `step.readonly_files` into read-only reference context, injects the concept tree, suppresses conversational history, and runs a bounded turn. The interface switches to Develop (`Ctrl+1`) and shows the live turn together with a compact **Plan Stepper** (§5.8).
5. **Atomic Completion & Semantic Blocking:** A step becomes `done` only when the turn is successful: the edits were applied and passed all toolchain checks, or the model emitted the terminal success tag. If the model emits `<blocked>`, the step transitions to `BLOCKED`, its working changes are rolled back, and the batch supervisor initiates automated closed-loop replanning (§7.3.4). A text-only answer without code and without a terminal tag does not close the step.
6. **Corrective Reprompt, then Fail-Fast:** If a step turn ends without verified code changes and without any terminal protocol tag (`<success>` or `<blocked>`), the server re-runs the step with a corrective prompt while the retry budget lasts (§7.3.3). Only once the budget is exhausted, the turn is cancelled, or a terminal error is reported does the server reset the step back to `todo`, report the reason, and stop the batch. Steps completed earlier stay `done`. Each failed turn's own file changes are undone by the Git workflow checkpoint.
7. **Parent Status Recalculation:** When all child tasks under a parent node become `done`, the parent is automatically marked `done`.
8. **Safe Cancellation:** `Esc` / `Ctrl+C` cancels the running turn. The cancelled step returns to `todo` and the rest of the batch does not start.

**What Develop shows while a scope runs.** The server owns the queue, so Develop (`Ctrl+1`) stays a pure view. Above the model output it renders a compact **Plan Stepper**: the plan title followed by title-only rows for the steps in the active scope, each with a short status (`Done`, `InProgress`, `Todo`). The stepper never expands diffs, details, or concept nodes, and it is absent for plain ad-hoc turns. The Proposed Edits section and the streaming file list belong strictly to the *current active step*: once the batch advances, the previous step's files are already committed to Git and are dropped from the working area, so the visible diffs always describe one step rather than an accreted history.

### 7.3.3 Step Reprompt Loop & Deterministic Reset

A bounded turn can fail the step contract in a purely conversational way: the model answers in prose, forgets to emit an edit block, or omits the terminal success tag. Treating every such drift as a hard batch failure is brittle and wastes the entire scope. The Step Reprompt Loop turns this drift into a recoverable, bounded correction.

**Why reprompt instead of abort.** A conversational slip is not evidence that the step is impossible. The loop separates *insurmountable* failures (terminal error tag, cancellation, exhausted budget) from *recoverable* protocol drift (a prose-only answer, a missing terminal tag). For the latter, the server re-runs the same turn with explicit corrective feedback instead of aborting the batch.

1. **Corrective Prompt.** When a turn ends without verified code changes and without the terminal success tag, the server issues a corrective prompt built by the core. It echoes the model's previous answer, includes the diagnostic reason when available, and restates the active edit protocol: either produce a valid edit block that conforms to the protocol, or confirm completion explicitly with the terminal success tag. Conversational text is never accepted in place of a protocol operation.
2. **Deterministic Reset Between Attempts (Clean Basis Invariant).** Before every corrective attempt the server guarantees a predictable baseline: the failed turn's own file changes are rolled back by the Git workflow checkpoint, the transient auto-context layer is cleared, and the stable step files (`step.files`) are re-loaded. Each attempt therefore starts from the exact same context rather than an accreted, contaminated one.
3. **Bounded Retry Budget.** The number of corrective attempts is governed by `develop.max_retries`. The budget is finite by design: the loop can never spin forever, and once it is exhausted the failed step returns to `todo` and the batch stops (Server-Side Fail-Fast, §7.3.2).
4. **Coder Ladder Escalation (`auto_level_up`).** When `models.auto_level_up = true`, a Coder turn that fails the step contract advances the corrective attempt to the next rung of the configured Coder Ladder (§3.3). Escalation stays inside the Coder role: the first attempt uses the base rung (fast and cheap), and each retry moves one rung up, pairing a model with an optional reasoning depth so recovery can first deepen reasoning on the same model and then, if needed, switch to a stronger one. The ladder saturates on its final rung, so the retry loop always terminates. A terminal `<blocked>` report bypasses the ladder: the step is marked `BLOCKED` without consuming any rung.
5. **Terminal Tag Marker Tolerance.** The active edit protocol uses a dynamic per-turn marker (for example `<success_<marker>>`). Deep-reasoning models occasionally drop the suffix and emit a bare `<success>`. The protocol accepts an unmarked terminal tag as a fallback when no marked tag is present, provided the tag sits outside Markdown code fences and inline code, so that literal examples in the answer are not mistaken for real signals.

Both settings that shape this loop are surfaced in project configuration and documented inline in `tauqe.toml`: `develop.max_retries` (the attempt budget) and `models.auto_level_up` (escalation on retry).

### 7.3.4 Semantic Blocking, Wave Horizon Planning & Automated Replanning (`plan/replan`)

Real-world software development is iterative and non-linear. When an engineering task encounters an insurmountable roadblock—such as a contradictory requirement, an invalidated architectural assumption, or a broken third-party dependency—discarding the entire plan or manually editing JSON structures wastes effort and breaks architectural continuity.

Earlier designs attempted localized sub-tree grafting (`graft_subtree`), decomposing only the blocked task while preserving subsequent pending items. In practice, software engineering tasks are deeply interdependent: when an architectural assumption fails at step $N$, subsequent steps ($N+1 \dots M$) were designed around that flawed assumption. Retaining those downstream steps introduced subtle architectural drift, orphaned dependencies, and broken interface contracts.

TAUQE deprecates single-step grafting in favor of **Wave Horizon Planning**:

```text
       Step N Execution (Coder)
                  │
          Precondition Fails
                  │
           <blocked> tag
                  │
        Git Rollback to Pre-Turn Checkpoint
        Step N status → BLOCKED
                  │
   Automated Replanning Loop (bounded by max_replans)
                  │
       Architect Invocation (Wave Horizon Synthesis)
       ├── Immutable Concept Tree
       ├── Immutable Done Foundation (Steps 1..N-1)
       ├── Blocker Diagnostics (reason + details)
       └── Uncompleted Horizon Context
                  │
       PlanStorage::replace_uncompleted_horizon()
       ├── Preserves Done Foundation (Steps 1..N-1)
       └── Atomically Splices Newly Synthesized Horizon
                  │
       Transactional History Recording
                  │
       Batch Execution Resumes on First New Leaf Step
```

1. **Deterministic Rollback on Block:** When a Coder turn emits `<blocked reason="..." details="..." />`, the turn loop immediately bypasses patch-retry and verification healing stages. The Git policy rolls back the working tree to the clean pre-turn checkpoint (`policy.rollback()`), transient auto-context files are flushed, and the step status is recorded as `BLOCKED`.
2. **Unified Wave Horizon Synthesis:** Replanning and initial plan refinement are unified into a single pure synthesis function:
   $$\text{Horizon} = \text{Architect}(\text{Concept}, \text{DoneFoundation}, \text{Context}, \text{Diagnostics})$$
   The Architect is never asked to re-emit completed tasks (`Done`), completely eliminating the risk of status regression or violating `validate_done_immutability`. The Architect receives:
   - The immutable Concept Tree (architectural intent and invariants);
   - The immutable Done Foundation (verified milestones already in Git);
   - The explicit diagnostic report of why step $N$ failed (reason, details, and compiler output);
   - The repository context (Tree-sitter Repo Map and relevant files).
   Based on this foundation, the Architect re-engineers the entire remaining task horizon to reach the goal under the newly discovered constraints.
3. **Atomic Horizon Splicing (`PlanStorage::replace_uncompleted_horizon`):** Rather than performing fragile surgery on a single node, the storage engine atomically replaces all uncompleted items with the new horizon:
   - **Done Foundation Preserved:** Completed steps are retained verbatim without risk of mutation or deletion.
   - **ID Collision Avoidance:** Any items in the new horizon that collide with completed IDs are safely disambiguated or discarded.
   - **Clean DAG State:** Uncompleted downstream dependencies are re-computed cleanly by the Architect rather than leaving orphaned prerequisite IDs.
4. **Automated Closed-Loop Recovery & `max_replans`:** During autonomous batch execution (`plan/execute`), encountering a `<blocked>` outcome does not force the developer to manually intervene immediately. The batch supervisor automatically triggers replanning with the diagnostic packet, applies the new horizon, and resumes batch execution from the first executable leaf step. To prevent infinite cycles where models repeatedly block and replan, the loop is bounded by `develop.max_replans` (default: 2, configurable in `tauqe.toml`). If consecutive automated replans exceed this threshold, batch execution safely halts, keeping the step `BLOCKED` and requesting developer attention.
5. **Session History Transactionality:** The Architect's replanning dialogue is recorded in session history **strictly after** the updated plan is successfully validated and persisted to disk. If validation fails or storage encounters an I/O error, the session history remains unpolluted by phantom replanning attempts.
6. **Interactive TUI Integration:** If the developer wishes to inspect or guide replanning manually, pressing `R` on a blocked step in Plans view (`Ctrl+4`) opens the Refine/Replan dialog with the blocker reason and details automatically pre-seeded into the instructions editor. The developer can refine the instructions and trigger replanning directly without falling back into an unconstrained conversational REPL session. The Plan Stepper in Develop view and the Plans tree instantly update to reflect the new horizon.

---

## 7.4 Plans View Keybindings

| Keybinding | Action |
|---|---|
| `n` / `p` (or `↑` / `↓`) | Move selection through plans, concept items, and implementation items |
| `Enter` / `x` | Execute leaf implementation step, group, or entire plan (with confirmation) |
| `d` | Discuss selected concept item, implementation step, or plan architecture in Develop |
| `R` | Deep refine implementation blueprint or replan uncompleted horizon with AI Architect |
| `t` / `s` | Open status selection dialog (`DISCUSSION`, `TODO`, `IN_PROGRESS`, `DONE`, `BLOCKED`, `CANCELLED`) |
| `Tab` / `Space` | Fold or unfold section (Concept / Implementation) or hierarchical items |
| `a` | Toggle fold / unfold all plans and sections |
| `c` / `y` | Copy entire plan to clipboard in Markdown format |
| `r` | Refresh plans from server storage |
| `Delete` | Delete plan (with confirmation) |
| `Ctrl+H` / `?` | Open contextual Help dialog |
| `Ctrl+1..5` | Quick switch between views |
