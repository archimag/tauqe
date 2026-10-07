# TAUQE

> **T**he **A**nswer to the **U**ltimate **Q**uestion of **E**ngineering.  
> *(pronounced /tɔːk/ or /taʊk/)*

**TAUQE** is an AI-native engineering control environment built on transactional Git principles, deterministic symbolic analysis, and a **literal harness** for large language models.

Inspired by the interactive heritage of **Magit** and **SLIME**.

---

## Philosophy: The Literal Harness

Modern AI-assisted engineering is trapped between two flawed extremes:

1. **Context Micromanagement (Aider):** The developer acts as a "context janitor", manually juggling files via `/add` and `/drop` to keep the model from getting lost. Cognitive burden is not reduced—it simply shifts into manual context maintenance.
2. **Uncontrolled Agentic Loop (Agent Loop with Arbitrary Bash):** The model is granted terminal access and arbitrary OS commands. The agent writes throwaway bash scripts, breaks system packages, hallucinates shell outputs, and loops aimlessly, burning tokens while the developer waits anxiously.

**TAUQE introduces a third paradigm — The Semantic Harness:**

> We do not grant the model autonomous agent privileges or shell access. We keep it strictly harnessed with typed protocols and Git transactions, while providing **complete autonomy within a single bounded turn**.

- **Blinkers (Repo Map):** The model perceives the structural contours of the codebase (Tree-sitter Repo Map) without cluttering the context window.
- **Bit & Bridle (Strict Protocol):** The model has no terminal shell access. Every action is expressed through a typed protocol: virtual edits, declarative file requests, or toolchain verification commands.
- **Reins (Git Transactions):** The developer defines the objective, while Git ensures an instant atomic rollback with a single keypress (`u`) if the outcome is unsatisfactory.

---

## Anatomy of an Autonomous Bounded Turn

You no longer need to manually collect context before asking a question. You describe the engineering task, and TAUQE executes a bounded turn:

```text
                 Developer Prompt
                         ↓
  1. Discovery Phase     → The model inspects the Repo Map and autonomously
                           requests missing files via protocol tags.
                         ↓
  2. In-Memory Staging   → Edits are applied and verified in a virtual buffer
                           without touching disk.
                         ↓
  3. Patch Retry         → If a replacement block drifts in indentation or lines,
                           the system automatically corrects the patch.
                         ↓
  4. Toolchain Healing   → TAUQE triggers project toolchain checks (`cargo check`,
                           `npm test`). On compilation errors, the model
                           targets and heals defect lines until a clean build.
                         ↓
  5. Atomic Git Commit   → Changes are wrapped into a single atomic Git commit.
```

The moment the turn completes, control returns immediately to the developer. No background loops, no infinite drift.

---

## Core Invariants

1. **Git Defines Reality**  
   Git is the single source of truth. Uncommitted user work is protected by automatic pre-edit checkpoints. In the event of an abnormal server termination, Crash Recovery restores the repository to a pristine state.
2. **Never Guess What Is Deterministically Known**  
   File trees, symbol definitions (Tree-sitter), build manifests (`Cargo.toml`, `package.json`), and linter diagnostics are resolved deterministically by code, not generated through probabilistic guesswork.
3. **No Arbitrary Shell**  
   Secure by design: the model cannot delete files indiscriminately, install untrusted packages, or corrupt system environments.
4. **Server-First Architecture**  
   Core state and orchestration reside in a headless server (SLIME/Swank style). The TUI and future clients (Emacs) communicate over strict JSON-lines RPC.
5. **Magit UX Heritage**  
   No conversational chat bubbles: high-density keyboard-driven terminal interface, collapsible diffs, real-time status indicators, and contextual cursor actions.

---

## Repository Structure

```text
tauqe/
├── crates/
│   ├── core/       # Engine: Git transactions, context manager, Tree-sitter repo map, turn pipeline
│   ├── protocol/   # RPC schemas, typed events, and error codes
│   ├── server/     # Headless server runtime (Swank-style runtime, stdio/sockets)
│   └── tui/        # Magit-inspired terminal client built with Ratatui
├── docs/           # Architecture, specifications, and conceptual vision
└── tauqe.toml      # Project configuration and context/history parameters
```

---

## Quick Start

### 1. Configuration

Initialize or edit `tauqe.toml` in your project root:

```toml
[models]
default = "anthropic/claude-3.5-sonnet"

[edit]
workflow = "git"     # git | naive
protocol = "xml"     # xml | structured

[context]
max_discovery_rounds = 3
repomap_token_budget = 1024
```

Configure your OpenRouter API key via `.tauqe/credentials.toml` or environment variable:

```bash
export OPENROUTER_API_KEY="sk-or-v1-..."
```

### 2. Launch TUI

```bash
cargo run --bin tauqe-tui
```

### 3. Workflow

1. **Submit a task:** Enter your prompt in the editor (`Enter` to submit, `Shift+Enter` for newline).
2. **Observe the turn:** TAUQE discovers missing files, stages patches in memory, validates against `cargo check` / `test`, and renders a verified diff.
3. **Control the result:** Press `u` to instantly roll back the entire turn via Git undo, or press `F6` to squash session commits into a feature commit.

---

## License

TAUQE is licensed under the [MIT License](LICENSE).
