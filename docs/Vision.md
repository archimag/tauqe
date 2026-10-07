# Vision

## 1. Purpose

The project builds an **AI-native development environment** where the developer retains full control over the project, and the model acts as a powerful semantic executor within strictly defined boundaries.

The system is neither an IDE, an IDE plugin, nor an autonomous coding agent. It is an **intelligent project control environment**, where:

- Git defines current state and change history;
- Project files define project capabilities;
- Explicit context defines what the model knows;
- Write scope defines what the model is permitted to modify;
- Task/plan scope defines what problem the model is solving;
- Project actions define what operations are allowed to run;
- The user controls this with low friction via voice or text;
- The model executes work strictly inside these boundaries.

The ultimate goal is not to remove the developer from the loop, but to remove **friction** between developer intent and the system.

> Not human-out-of-the-loop, but friction-out-of-the-loop.

## 2. Problem Statement

A typical modern agent-first model works as follows:

```text
user request
    ↓
agent
    ↓
filesystem + search + shell + git + browser
    ↓
agent gathers context on its own
    ↓
agent decides what to do
    ↓
agent executes autonomously
    ↓
developer review
```

This approach burdens the model with simultaneously serving as requirements interpreter, codebase explorer, shell operator, context manager, executor, planner, and test runner.

Tauqe is built on the opposite premise:

> **If any part of a task can be resolved deterministically, it must not be delegated to an LLM.**

Examples:

- Git knows what changed — the model should not guess it.
- `Cargo.toml` knows workspace structure — the model should not re-infer it.
- `package.json` knows scripts — the model should not invent commands.
- Parser/LSP knows definitions — the model should not visually scan for them.
- Project plan defines scope — the model should not expand the scope on its own.
- Permission model defines what can be changed — the model should not decide write targets autonomously.

The model should be applied specifically where true semantic reasoning is required: understanding intent, comprehending code, proposing localized diffs, explaining issues, resolving ambiguous human references, and performing bounded transformations.

## 3. Conceptual Inspirations: Magit and SLIME

The project draws inspiration from two foundational pillars of the Emacs ecosystem:

1. **Magit (User Interface Inspiration):**
   - The interface is a **living structured status document**, not a chat box or text editor.
   - Information density, keyboard-driven navigation, context actions on the item under cursor (`Object under cursor`), collapsible sections.

2. **SLIME / Swank (Protocol and Interaction Inspiration):**
   - **No text scraping:** communication relies on structured semantic objects (presentation streams), never parsing raw console stdout via regexes.
   - **Server as an independent runtime:** the server maintains project state and executes semantic work; clients are thin interactive projections.
   - **Parallel out-of-band channels:** the model streams text and edits asynchronously; verification occurs on the fly without blocking user interaction.

## 4. Core Principles

### 4.1 Git defines reality
Git is the formal source of truth for repository state: branch, HEAD, working tree, diff, commits, checkpoints, and undo.

### 4.2 Project files define capabilities
Project configuration files define the operational model: packages/workspaces, dependencies, scripts, build actions, tests, and linters.

### 4.3 Context defines knowledge
Context is first-class state. The user explicitly controls editable files, read-only files, evidence, plans, and the repo map.

### 4.4 Permissions define power
> **The agent has no arbitrary shell execution.**
The model can only invoke registered, authorized project actions.

### 4.5 Never use reasoning where a deterministic abstraction exists
If information can be retrieved deterministically, it must be computed by code.

### 4.6 Minimal coherent change
Changes must be minimal and targeted (Search/Replace blocks and explicit Create/Delete), avoiding unrelated refactoring.

## 5. Server-First Architecture

The primary product is the server/core. Clients are interchangeable:

```text
TUI ──────┐
Emacs ────┼── Protocol (SLIME-style) ── Server ── Core
Other ────┘
```

> **Server owns semantics. Client owns interaction.**

## 6. Clients

### Stage 1 — TUI
The initial client is a full-featured terminal application (reference implementation), combining Magit-style state display with org-mode turn blocks.

### Stage 2 — Emacs
The Emacs client follows the same principles: direct connection to the server, live state buffers, and seamless navigation to source code.

## 7. Formula

```text
Git
    → reality

Project files
    → capabilities

Project model
    → deterministic understanding

Plans
    → intended scope

Context
    → model knowledge

Permissions
    → model power

Voice & TUI
    → low-friction human control

LLM
    → semantic transformation
```

Summary:

> **An intelligent Git-native development environment that strengthens developer control and leverages LLMs exclusively where generative semantic reasoning is truly valuable.**
