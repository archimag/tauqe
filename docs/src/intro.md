# Introduction

Welcome to **The Tauqe Book** — the authoritative guide and conceptual reference for TAUQE (**T**he **A**nswer to the **U**ltimate **Q**uestion of **E**ngineering), an AI-native engineering control environment built on the **literal semantic harness** paradigm.

---

## What is TAUQE?

TAUQE is neither an autonomous bash agent nor a context micromanagement assistant. It is a strictly disciplined engineering environment designed to resolve the fundamental challenge of modern AI-assisted software development: **the duality of "What" versus "How"**.

### The "What" versus "How" Duality
Software engineering has always hinged upon two foundational questions:
1. **What** to build — architectural intent, system boundaries, security constraints, and domain requirements.
2. **How** to build it — synthesizing boilerplate, refactoring call sites, wiring data pipelines, and adjusting syntax.

Large language models are exceptionally skilled at answering *How* when guided within strict structural boundaries. However, modern tooling forces developers into one of two failing extremes:
- **Context Micromanagement (e.g. Aider):** Developers act as manual context logisticians (`/add`, `/drop`, `/read-only`), spending more mental energy counting tokens and guessing file dependencies than reasoning about design.
- **Uncontrolled Agentic Bash Loops (e.g. Devin):** Models are given an open-ended terminal shell where they drift aimlessly, re-install packages, corrupt system libraries, and fall into degenerative hallucination loops.

### The Semantic Harness
TAUQE liberates the developer's cognitive bandwidth so they can focus entirely on **What**, while holding the language model in a literal, physical harness:
- **Blinkers (Tree-sitter Repo Map):** Grants global architectural awareness across symbols without polluting the active context window.
- **Bit & Bridle (Strict Protocol):** Strips the model of arbitrary shell access. All changes occur through typed protocol tags, in-memory virtual staging, and deterministic toolchain verification.
- **Reins (Git Transactions):** Every turn is transactional. The developer steers direction, while Git guarantees instantaneous, zero-loss rollback (`u`) at any sign of drift.

---

## Guide Structure

- **[Chapter 1: Philosophy: The Semantic Harness](ch01-philosophy/semantic-harness.md)** — Core mission, the literal harness metaphor, and architectural comparison against agentic loops and manual logisticians.
- **[Chapter 2: Three-Tier Context Model](ch02-context/three-tier-model.md)** — Architectural rationale of Pinned, User, and Auto context layers, token hygiene, and multi-round Discovery.
- **[Chapter 3: The Autonomous Bounded Turn](ch03-bounded-turn/lifecycle.md)** — Anatomy of a bounded execution cycle: Discovery, In-Memory Virtual Staging, Patch Retry resilience, and Compiler-driven Self-Healing.
- **[Chapter 4: Git Safety & Transactions](ch04-git-safety/checkpoints-and-undo.md)** — Why real Git commits beat stashes, author isolation, dirty-tree preservation, and deterministic rollback.
- **[Chapter 5: Keyboard-First Interface](ch05-interface/tui-ux.md)** — Magit/Org-inspired live document state versus chat bubbles, view spatial hierarchy, and high-density interaction.
- **[Chapter 6: Structured Code Review](ch06-review/review.md)** — Why AI-assisted review is the primary interface at high generation speeds, finding triage, and selective context injection.
- **[Chapter 7: Local Plans](ch07-plans/plans.md)** — Managing tactical multi-step tasks, surviving history compaction, and steering work via interactive checklists.
  </overwrite

  <overwrite_18DC9645CA7E6311 path="docs/src/ch04-git-safety/checkpoints-and-undo.md">
# 4. Git Safety & Transactions

In TAUQE, Git is not treated as a mere version-control transport or an afterthought invoked via shell commands. **Git defines repository reality.**

Every engineering turn is executed as an isolated, atomic transaction with mathematical recovery guarantees.

---

## 4.1 Architectural Rationale: Why Real Commits Over `git stash` or Shadow Files

A common temptation in AI coding assistants is to manage undo state using temporary mechanisms: stash stacks (`git stash`), shadow temporary directories, or internal patch caches. TAUQE explicitly rejects these approaches in favor of **native Git commits**:

1. **The Fragility of `git stash`:**
   - Stash entries are organized as an untyped, volatile stack. A secondary branch switch or external Git command can drop, pop, or corrupt stash items.
   - Untracked and ignored files require special flags (`-u`, `-a`) that easily lead to silent merge conflicts and data loss during restoration.
2. **The Peril of Shadow Directories & Custom Formats:**
   - External snapshot files or proprietary diff caches living outside Git break catastrophically if the process is terminated abruptly (`SIGKILL`, power failure, or system reboot).
   - Recovery depends on custom code that might itself fail, whereas Git's native object database is mathematically resilient and verifiable.
3. **Transparency and Universal Auditability:**
   - By creating real, atomic Git commits, every state transition is recorded in Git's native Directed Acyclic Graph (DAG) and visible in `git reflog`.
   - If TAUQE were completely removed from the machine mid-turn, the developer could still inspect, recover, or unwind any change using standard, universal Git commands.

---

## 4.2 Checkpoints: Protecting Developer Work

Before allowing the language model to modify a single byte of source code, TAUQE inspects the Git working tree:

- **Dirty Working Tree Preservation:**
  If the developer has uncommitted changes when invoking a turn, TAUQE automatically commits them into an isolated pre-edit checkpoint commit:
  ```text
  tauqe-checkpoint: uncommitted user changes
  ```
- **Author Isolation & Provenance:**
  Model-generated modifications are committed separately under strict, non-human authorship:
  ```text
  Author: Tauqe AI <ai@tauqe.dev>
  ```
  Developer changes and AI-synthesized code are never merged into an ambiguous diff. This guarantees pristine legal and engineering provenance: `git blame` cleanly identifies exactly which lines were authored by a human and which were generated by the model.

---

## 4.3 Deterministic Undo (`u`)

Reverting an AI modification is instant, deterministic, and safe:

1. Pressing `u` in the terminal UI triggers an immediate verification check.
2. TAUQE inspects the HEAD commit and ensures it was authored by `Tauqe AI`.
3. The AI commit is reset completely.
4. If an uncommitted user checkpoint preceded the AI turn, TAUQE softly unwinds it (`git reset HEAD~1`):
   - The developer's uncommitted working tree is restored to the exact state it was in prior to the turn.
   - Zero work is lost, zero stash conflicts are possible.

---

## 4.4 Crash Recovery on Startup

If TAUQE is interrupted mid-turn (due to unexpected process termination, crash, or power loss), the next startup routine inspects the repository state before opening the interface:

- Scans for dangling `tauqe-step:` or `tauqe-checkpoint:` markers at HEAD.
- Unwinds interrupted step transactions back to the clean baseline.
- Restores the developer's working directory to a consistent, safe state automatically.
  </overwrite_18DC9645CA7E6311

  <overwrite path="docs/src/ch05-interface/tui-ux.md">
# 5. Keyboard-First Interface

TAUQE's terminal user interface is built on the design heritage of **Magit** and **Org-mode**.

---

## 5.1 Architectural Rationale: Live Document State vs. Chat Bubbles

Most AI coding tools mimic consumer messaging applications (chat bubbles, avatars, endless vertical conversational streams). TAUQE rejects this design paradigm as fundamentally misaligned with software engineering:

### Why Chat Bubbles Fail for Software Engineering
1. **Low Information Density:** Chat bubbles consume substantial screen real estate with blank margins, author badges, and visual fluff. An engineer can see only a fraction of a function or diff on a single screen.
2. **Destruction of Hierarchy:** Source code, abstract syntax trees, unified diffs, review finding lists, and tactical plan trees are inherently hierarchical structures. Chat bubbles flatten this rich structure into an unstructured, linear stream of text.
3. **Loss of Spatial Stability:** In a scrolling chat stream, critical information constantly scrolls out of view. Comparing a proposed patch against surrounding code or reviewing previous compiler diagnostics requires constant, disorienting scrolling.

### The Live Document Paradigm
In TAUQE, the screen is a **structured, navigable document of state**:
- Everything is an interactive, collapsible tree node (using Org-mode style folding with `Tab` / `Space`).
- Unified diffs can be navigated and folded hunk by hunk.
- Review findings and plan tasks are interactive items that carry discrete status and can be checked for contextual injection.
- Zero visual noise: technical tags (`<plan>`, `<context_request>`, `<doc_request>`) are parsed and hidden during streaming.

---

## 5.2 Cognitive Spatial Separation: The 5 Dedicated Views

TAUQE separates the developer's mental stages into dedicated workspaces accessible via `Ctrl+1..5`:

### 1. `Ctrl+1`: Develop View (Execution Cockpit)
The central engineering canvas where coding turns occur.
- **Turn Progression:** Displays streaming markdown explanations, collapsible reasoning traces (`Ctrl+R`), and interactive unified diffs.
- **The Autonomous Multi-Round Turn:**
  1. *Discovery Rounds:* If symbols, type definitions, or docs are missing, the model requests them autonomously (`<context_request>`, `<doc_request>`).
  2. *In-Memory Virtual Staging:* Edits are tested against memory representations first.
  3. *Patch Retry Loop:* Mismatched hunks or indentation variances trigger targeted retry rounds without corrupting disk.
  4. *Toolchain Verification & Self-Healing:* Runs `cargo check` / linters; compiler diagnostics trigger an immediate healing cycle.
  5. *Git Transaction:* Atomically commits the result or permits instant rollback (`u`).
- **Response Clipboard:** Press `Alt+C` (or `c` / `y` with an empty prompt) to copy the model's markdown response directly to the system clipboard.

### 2. `Ctrl+2`: Context View (Working Memory & Hygiene)
Displays and manages the three context layers (Pinned, User, Auto).
- Monitor token costs per file and total active working memory.
- Quick actions: `e` (add editable), `r` (add read-only), `t` (toggle permission), `p` (promote auto to user), `c` (clear auto), `d` (remove file).

### 3. `Ctrl+3`: Review View (Independent Static Audit)
A dedicated workspace for asynchronous, unbiased code inspection.
- Independent audit model operates without conversation history or repo map.
- Interactive checklist: triage findings (`TODO`, `DONE`, `REJECTED`), fold/unfold (`Tab` / `Space`), and check (`x`) items for targeted resolution in Develop.

### 4. `Ctrl+4`: Plans View (Tactical Roadmaps)
Structured task execution trees stored locally in `.tauqe/plans/`.
- Immune to history compaction; persists across sessions.
- Interactive task tree: select focus tasks (`x`) to inject `<active_plan_context>` into Develop, cycle status (`t` / `d`), fold branches (`Tab` / `Space`), and copy plan markdown (`c` / `y`).

### 5. `Ctrl+5`: History View (Audit Log)
Paginated session history and audit trail of previous turns.
- Inspect prior model explanations, AI commit hashes, and file modifications.
- Collapsible entries: fold/unfold with `Tab` or `Space`.
- Navigation: `[` / `]` or `p` / `n` (`Alt+↑` / `Alt+↓`). Active entry header is kept pinned in view.

---

## 5.3 Universal Navigation & Shortcuts

| Shortcut | Scope | Action |
|---|---|---|
| `Ctrl+1..5` | Global | Switch between Develop, Context, Review, Plans, History |
| `Tab` / `Space` | Global | Fold or unfold the active section, finding, diff hunk, or plan item |
| `u` | Develop (empty prompt) | Undo last AI commit and restore pre-turn state |
| `Alt+C` / `c` / `y` | Develop / Review / Plans | Copy formatted content to system clipboard |
| `?` | Global | Open contextual help dialog |
| `Esc` | Global | Cancel active streaming turn or dismiss modal dialog |
