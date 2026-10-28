# TAUQE

> **T**he **A**nswer to the **U**ltimate **Q**uestion of **E**ngineering. *(pronounced /tɔːk/ or /taʊk/)*
>
> **Maximum execution autonomy. Minimum procedural autonomy.**

**TAUQE** is an AI-native engineering control environment designed around a **literal semantic harness** for large language models, inspired by the stateful, keyboard-driven heritage of **Magit** and **SLIME**.

Instead of uncontrolled agentic loops with arbitrary shell access or tedious manual file juggling, TAUQE confines the model within an autonomous bounded turn—liberating you to focus entirely on **What** to build while the harness strictly controls **How**.

Unlike open-ended coding agents (such as Claude Code or OpenCode) that delegate the entire engineering lifecycle, tool invocation, and process control to an unconstrained LLM loop, TAUQE hardcodes workflows, state machines, and Git transactions in deterministic Rust. Language models serve as specialized cognitive components inside a disciplined harness, not autonomous process dispatchers.

TAUQE is actively self-hosted: starting from the second day of its development, the system has been developed and maintained exclusively using TAUQE itself. As such, the repository serves as a living reference implementation of AI-native engineering under strict harness constraints, aiming to set an architectural and operational standard for reproducible, deterministic workflows and decision immutability.

---

## Philosophy & Core Principles

### Primacy of Engineering Principles Over Technology

Technology must adapt to the engineering process, not the engineering process to the limitations of a specific technology.

TAUQE is not an attempt to adapt software development to artificial intelligence. It is an effort to build an engineering environment where the intelligence of the executor does not dictate the structure of the process.

### Humans and AI as Equal Engineering Participants

The engineering process must not depend on who executes the work — a human or an AI.

They are interchangeable within their roles, but differ in capabilities, efficiency, and cost. Tasks should be assigned to whichever participant can deliver the required quality at a reasonable cost.

**What is good for a human must be good for an AI — and vice versa.** Tools, documentation, and engineering practices must be shared and retain their intrinsic value regardless of AI involvement.

TAUQE does not invent a new engineering discipline for AI. It applies proven engineering principles, enabling human and artificial intelligence to reinforce each other's strengths.

---

## Core Pillars

- **Declarative Invariants Over Imperative Drift:**  
  Contemporary agents often rely on fragile chains of imperative shell commands where error probabilities compound at every step. TAUQE upholds the classical engineering principle of describing *what* the system must satisfy rather than micromanaging *how* to do it. Declarative constraints, immutable concepts, and typed contracts guide the model without procedural babysitting.

- **Deterministic Execution Control:**  
  No arbitrary shell commands or terminal hallucinations. The model navigates codebase contours via a Tree-sitter Repo Map, validates patches in virtual in-memory staging, and self-heals compiler and linter errors (`cargo check`, `npm test`) before writing changes.

- **Transactional Git Reality:**  
  Git is the single source of truth. Every bounded turn wraps into an atomic Git commit. Working trees are protected by pre-edit checkpoints and automatic crash recovery. Revert any turn instantly with a single keypress (`u`).

- **Hierarchical Plans & Decision Immutability:**  
  Turn complex objectives into structured architectures. Plans separate immutable system invariants (`concept`) from actionable steps (`items`). Completed steps and invariants remain frozen during execution—preventing the model from silently altering requirements when an implementation hits friction.

- **Role Specialization & Economic Efficiency:**  
  High-leverage planning and architectural design are assigned to frontier reasoning models (Architect), while concrete code edits run on faster, cost-effective models (Coder) with automated tier escalation on compiler failures.

- **Integrated Code Review:**  
  Audit diffs and working trees directly inside the environment. Triage findings by severity (`TODO` / `FIXED` / `REJECTED`), debate trade-offs in structured discussions, and execute batch remediation with compiler verification.

---

## Quick Start

### Prerequisites

1. **Rust Toolchain:**
   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```
   Or follow the installation guide at [rust-lang.org/tools/install](https://www.rust-lang.org/tools/install/).

2. **Just command runner:**
   ```bash
   cargo install just
   ```

### Installation & Launch

```bash
# Build and install to ~/.cargo/bin:
just install

# Or run directly in development mode from source:
just dev
```

On first launch, an interactive onboarding wizard automatically checks your Git repository, configures toolchains in `tauqe.toml`, and saves credentials securely.

### Workspace Navigation

| Shortcut | View | Description |
| :--- | :--- | :--- |
| `Ctrl+1` | **REPL** | Conversational harness, streaming diffs, toolchain healing, and instant undo |
| `Ctrl+2` | **Context** | Four-layer context manager (`Pinned`, `Refs`, `User`, `Auto`) with glob patterns |
| `Ctrl+3` | **Review** | Code audit sessions, finding triage, and autonomous batch fixes |
| `Ctrl+4` | **Plans** | Hierarchical task trees, deep refinement, and isolated step execution |
| `Ctrl+5` | **History** | Chronological session audit trail, token metrics, and commit inspect |

---

## Documentation

TAUQE includes comprehensive architectural and user documentation built with mdBook (located in `docs/`):

```bash
# Build the documentation:
just docs-build

# Serve documentation with live reload:
just docs-serve
```

The simplest way, however, is to **ask TAUQE directly** in the interactive environment: ask anything about concepts, workflows, architecture, or keyboard shortcuts, and TAUQE will consult its authoritative manuals and explain them in your language.

---

## Current Limitations

- **Provider Support:** Currently, only [OpenRouter](https://openrouter.ai/) is supported as the LLM provider.
- **Tested Models:** Models that have been actively tested and verified in real workflows are documented directly in the project configuration file (`tauqe.toml`).
- **Terminal Compatibility:** The interactive client has been tested and verified specifically with [kitty](https://sw.kovidgoyal.net/kitty/).

---

## Contributing

Pull requests containing raw code are not accepted. In the modern LLM paradigm, synthesizing raw code carries negligible marginal cost—code is no longer the scarce resource. What truly matters is conceptual clarity, invariant design, and cohesive system architecture.

Not a single line of code in this repository was written by a human, and never will be. TAUQE is built exclusively through its own semantic harness: every change is synthesized and verified by the model inside a deterministic bounded turn. The repository is a living proof of its own paradigm.

If you would like to contribute:
- Open an issue to discuss conceptual ideas, architectural trade-offs, or UX workflows.
- Better yet, contribute well-defined, ready-to-implement engineering specifications and formal invariants.

Once a specification is sound, implementing it within TAUQE's semantic harness is a matter of deterministic bounded execution.

---

## License

Licensed under the [MIT License](LICENSE).
