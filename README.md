# TAUQE

> **T**he **A**nswer to the **U**ltimate **Q**uestion of **E**ngineering.  
> *(pronounced /tɔːk/ or /taʊk/)*

A Git-native, context-driven AI engineering environment inspired by the interactive traditions of **Magit** and **SLIME**.

---

## The Concept

In Douglas Adams' *The Hitchhiker's Guide to the Galaxy*, the supercomputer Deep Thought calculated the Answer to the Ultimate Question of Life, the Universe, and Everything to be **42**. But that answer proved useless because no one knew what the actual **Question** was.

Modern AI coding agents face the exact same dilemma: an LLM prompted with an unconstrained repository, no boundary controls, and an arbitrary shell will hallucinate and wander aimlessly. 

**TAUQE** is founded on a different principle:

> **The model provides the semantic transformation, but the environment deterministically defines the Question.**

By combining strict Git-backed checkpoints, an explicit three-tier context model (Pinned, User, Auto), deterministic syntax verification, and a presentation-stream protocol, TAUQE gives developers the speed of generative AI without losing control over their codebase.

Not *human-out-of-the-loop*, but **friction-out-of-the-loop**.

---

## Core Invariants

1. **Git Defines Reality**  
   Git is the sole source of truth. Uncommitted user work is protected via automatic checkpoints. Every AI change is isolated into clean, reproducible commits with deterministic, single-keystroke `undo`.
2. **Context Defines Knowledge**  
   The model does not speculate on codebase architecture. Explicit scopes (Pinned, User, and Auto context) combined with symbol-level Repo Maps (via rustdoc and Tree-sitter) ensure the model sees precisely what is required.
3. **Permissions Define Power**  
   **No arbitrary shell access.** The agent cannot run wild commands. It can only propose atomic edits to files with explicit `editable` permissions and run designated toolchain verification pipelines (`cargo check`, `npm test`).
4. **Never Guess What the Environment Answers Deterministically**  
   If an answer can be obtained from Git, `Cargo.toml`, AST parsers, or compiler diagnostics, it is resolved with code—never with generative reasoning.
5. **Minimal Coherent Change**  
   Modifications are applied using deterministic Search/Replace and atomic file creation, validated in-memory before touching disk.

---

## Architectural Pedigree: Magit & SLIME

TAUQE draws its UX and system design directly from the Lisp/Emacs engineering tradition:

- **Magit UX Heritage:**  
  The interface is not a linear chat window—it is a **living structured state document**. Keyboard-driven, dense, with org-mode turn structures, folding diffs, real-time validation spinners, and context actions on the item under the cursor.
- **SLIME / Swank Protocol:**  
  Clean decoupling between client and runtime server over a typed JSON-RPC protocol. No console scraping or ANSI regex parsing; the server emits semantic presentation streams (`edit/hunk`, `toolchain/result`, `model/reasoningDelta`).

---

## Workspace Layout

```text
tauqe/
├── crates/
│   ├── core/       # Core runtime: Git checkpoints, context, AST repo maps, edit loops
│   ├── protocol/   # JSON-RPC schemas, events, and semantic contracts
│   ├── server/     # Headless Swank-style session server (stdio / sockets)
│   └── tui/        # Magit-inspired keyboard-first terminal client (Ratatui)
├── docs/           # Architecture, protocols, conventions, and vision
├── plans/          # Verifiable roadmap and milestones
└── tauqe.toml      # Project configuration and model orchestration
```

---

## Quickstart

### 1. Configuration

Create or customize `tauqe.toml` in your project root:

```toml
[models]
default = "anthropic/claude-3.5-sonnet"

[edit]
workflow = "git"     # git | toolchain | naive
protocol = "xml"     # xml | structured

[context]
pinned = [
    "docs/Vision.md",
    "plans/bootstrap.org"
]
```

Provide credentials via `.tauqe/credentials.toml` or environment variable:

```bash
export OPENROUTER_API_KEY="sk-or-v1-..."
```

### 2. Launch

```bash
cargo run --bin tauqe-tui
```

### 3. Formulate the Question (Before Expecting the Answer)

Deep Thought took 7.5 million years to calculate **42** because nobody knew what the Question was. Do not repeat the galaxy's most famous mistake:

1. **Define the Scope:** Add relevant files to your context (`e` for Editable write-scope, `a` for Read-Only knowledge).
2. **State Your Intent:** Type your task in the multi-line editor (`Enter` to send, `Shift+Enter` for newlines).
3. **Verify the Answer:** Watch real-time reasoning (`Ctrl+R`), inspect in-memory validated diffs, and let TAUQE create an isolated, reproducible Git commit. Press `u` anytime to deterministically undo.

---

## License

TAUQE is free and open-source software under the [MIT License](LICENSE).
