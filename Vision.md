# Vision: TAUQE

## 1. Mission and Paradigm: The Literal Harness

TAUQE (**T**he **A**nswer to the **U**ltimate **Q**uestion of **E**ngineering, pronounced */tɔːk/* or */taʊk/*) is an **AI-native engineering control environment** designed around the concept of a **semantic harness** for large language models.

The term *harness* is used here not in the metaphorical sense of a test framework (*eval harness*) or an autonomous agent framework, but in its **original, literal sense: bridle, bit, reins, and blinkers**.

A large language model is a fast, capable, but blind workhorse. If given unrestricted access to an operating system shell, it will drift aimlessly, break system environments, and get trapped in generative hallucinations. Conversely, if a human developer is forced to spoon-feed it every file manually, the tool becomes a cognitive liability.

TAUQE holds the model in a firm grip:
- **Blinkers (Repo Map):** The model perceives codebase contours and symbols through Tree-sitter, sufficient for architectural comprehension without context window bloat.
- **Bit & Bridle (Strict Protocol):** The model has no arbitrary shell or bash access. It interacts with the world solely through typed protocol operations (virtual in-memory staging, declarative file discovery requests, deterministic toolchain verification).
- **Reins (Git Transactions):** The developer steers the objective, while Git ensures an instant, atomic rollback if the model deviates from course.

> **TAUQE is not an agent. It is an environment where the model is autonomous within a strictly bounded turn, yet completely stripped of agentic drift.**

### 1.1. The Fundamental Duality: "What" versus "How"

Software engineering has always hinged upon two foundational questions: **What** to build and **How** to build it.

Large language models are remarkably adept at answering *How*—synthesizing boilerplate, applying idioms, refactoring data pipelines, and implementing concrete routines. Yet they excel only when provided with a well-posed question and guided within disciplined structural boundaries. When released into open-ended, autonomous bash loops without guardrails, an LLM quickly drifts into generative hallucinations and speculative bloat.

TAUQE's primary objective is to **liberate the developer's cognitive bandwidth so they can concentrate entirely on 'What'.** The engineer determines the objective, architectural intent, and requirements; the harness firmly steers and confines the model through a bounded turn to solve the *How*, deterministically and safely.

---

## 2. Three Paradigms of AI-Assisted Development

Modern software engineering is caught between two ineffective extremes:

### 2.1. Context Micromanagement (The Aider Way)
The developer manually manages files: `/add`, `/drop`, `/read-only`. The human acts as a "context logistician", counting tokens and guessing which types the model will need.
- **The flaw:** Cognitive load on the developer increases rather than decreases.

### 2.2. Uncontrolled Agentic Loops (Devin / Bash Agents)
The model is granted a shell terminal, file system access, and an open-ended goal. The agent runs in an infinite loop: running grep across the disk, installing packages, breaking system libraries, and spinning in circles.
- **The flaw:** Loss of determinism, shell hallucinations, zero safety guarantees, and broken trust.

### 2.3. The Semantic Harness (The TAUQE Paradigm)
The developer states the engineering task in natural language without spending time manually assembling files. The system executes an **autonomous bounded turn**:
- Autonomously discovers and requests missing files via multi-round `Discovery`;
- Applies and validates patches in memory (`Staging`);
- Automatically repairs pattern-matching discrepancies (`Patch Retry`);
- Runs deterministic project compilers and linters (`cargo check`, `tsc`, `pytest`);
- Undergoes a targeted self-healing cycle on compiler errors (`Verification Healing`);
- Records the result in an isolated Git commit or executes a clean rollback.

**The moment the turn concludes, control returns immediately to the developer.** The model has no persistent background daemon and no shell access.

---

## 3. Anatomy of an Autonomous Bounded Turn

Instead of an unbounded `while true` loop, TAUQE implements a deterministic finite-state turn machine:

```text
               User Prompt
                    ↓
        ┌─► [Discovery Phase] ──── (requests missing files via <context_request>
        │           ↓               or system docs via <doc_request>)
        │   [Proposal Generation] ─ (generates minimal coherent patches)
        │           ↓
        │   [In-Memory Staging] ── (validates patches in memory with fuzzy matching)
        │       ├── Match Error → [Patch Retry Loop] (up to N retries)
        │       └── Success → Apply to disk
        │           ↓
        │   [Toolchain Verification] ── (executes cargo check / npm test)
        │       ├── Compiler Errors → [Verification Healing Loop] ────┐
        │       │                                                     │
        │       └── Clean Build                                       │
        │           ↓                                                 │
        └────── Rollback on Failure / Squash into Final Commit ◄──────┘
```

Every phase in the turn is deterministic:
1. **Discovery:** The model inspects the Tree-sitter Repo Map, identifies required definitions or system documentation, and requests them via protocol tags (`<context_request>`, `<doc_request>`). The server adds files to the `auto` context layer or injects documentation blocks and iterates. No manual `/add` required.
2. **Staging:** No byte touches disk until the entire set of edits converges cleanly in virtual staging.
3. **Healing:** If the compiler detects an error, the model receives clean toolchain output and fixes only the offending lines without touching unrelated code.
4. **Git Transaction:** The turn is transactional. Pressing `u` rolls back the working tree to its exact pre-turn state.

---

## 4. Fundamental Architectural Invariants

1. **Git Defines Reality**  
   Git is the single source of truth for repository state. Uncommitted user work is protected by pre-edit checkpoints. The system recovers cleanly on abnormal terminations (Crash Recovery on startup).
2. **Never Guess What Is Deterministically Known**  
   If an answer can be derived from Git, AST parsers (Tree-sitter), build manifests, or compiler output, it is computed by deterministic code, not generative reasoning.
3. **No Arbitrary Shell**  
   The model cannot run arbitrary OS commands. Compiler and test invocations are governed strictly by the project action registry.
4. **Minimal Coherent Change**  
   Modifications are made via localized search/replace blocks and atomic file operations. Gratuitous reformatting and unrelated refactoring are strictly avoided.
5. **Dual-Use Documentation as Concept Ontology**  
   Documentation serves a dual purpose: authoritative technical reference for developers and high-density semantic concept ontology for the AI model. When asked about system architecture, shortcuts, or workflows, the model requests authoritative documentation via `<doc_request>` rather than hallucinating.

---

## 5. Architectural Heritage: Magit and SLIME

- **Magit UX Heritage:**  
  The interface is not a chat stream with bubbles, but a **structured, live document of state**. High density, fully keyboard-driven, with collapsible sections, contextual cursor operations, and rich interactive diffs.
- **SLIME / Swank Protocol:**  
  Clean decoupling of client and server. The server encapsulates the project model, Git transactions, and model orchestration; clients (TUI, future Emacs package) are lightweight interactive frontends communicating via typed RPC.

---

## 6. Synthesis

```text
Git                   → Reality and transactional safety
Project Files         → Manifests and toolchain capabilities
Tree-sitter Repo Map  → Deterministic symbol awareness
Bounded Turn          → Autonomous turn lifecycle (Discovery + Staging + Healing)
Strict Harness        → Enforcing boundaries without shell access
Developer             → Task specification and absolute control
LLM                   → Semantic transformation engine
```

> **Eliminating cognitive burden from the developer without stripping away control.**
