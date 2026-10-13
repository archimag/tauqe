# TAUQE

> **T**he **A**nswer to the **U**ltimate **Q**uestion of **E**ngineering. *(pronounced /tɔːk/ or /taʊk/)*

**TAUQE** is an AI-native engineering control environment designed around a **literal semantic harness** for large language models, inspired by the stateful, keyboard-driven heritage of **Magit** and **SLIME**.

Instead of uncontrolled agentic loops with arbitrary shell access or tedious manual file juggling, TAUQE confines the model within an autonomous bounded turn—liberating you to focus entirely on **What** to build while the harness strictly controls **How**.

---

## Core Pillars

- **Deterministic Execution Control:**  
  No arbitrary shell commands or terminal hallucinations. The model navigates codebase contours via a Tree-sitter Repo Map, validates patches in virtual in-memory staging, and self-heals compiler and linter errors (`cargo check`, `npm test`) before writing changes.

- **Transactional Git Reality:**  
  Git is the single source of truth. Every bounded turn wraps into an atomic Git commit. Working trees are protected by pre-edit checkpoints and automatic crash recovery. Revert any turn instantly with a single keypress (`u`).

- **Hierarchical Engineering Plans:**  
  Turn complex objectives into structured architectures. Plans separate immutable system invariants (`concept`) from actionable steps (`items`). Every leaf step runs in an isolated turn with DAG prerequisite checks, pre-loaded context, and local replanning on blockers.

- **Integrated Code Review:**  
  Audit diffs and working trees directly inside the environment. Triage findings by severity (`TODO` / `FIXED` / `REJECTED`), debate trade-offs in structured discussions, and execute batch remediation with compiler verification.

---

## Quick Start

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
| `Ctrl+1` | **Develop** | Conversational harness, streaming diffs, toolchain healing, and instant undo |
| `Ctrl+2` | **Context** | Three-tier context manager (`Pinned`, `User`, `Auto`) with glob patterns |
| `Ctrl+3` | **Review** | Code audit sessions, finding triage, and autonomous batch fixes |
| `Ctrl+4` | **Plans** | Hierarchical task trees, deep refinement, and isolated step execution |
| `Ctrl+5` | **History** | Chronological session audit trail, token metrics, and commit inspect |

> **Ask TAUQE directly:** Launch the interface and ask anything about the system, shortcuts, or concepts—TAUQE queries its own authoritative manuals on the fly.

---

## License

Licensed under the [MIT License](LICENSE).
