# Vision: TAUQE

## 1. Mission and Paradigm: The Literal Harness

TAUQE (**T**he **A**nswer to the **U**ltimate **Q**uestion of **E**ngineering, pronounced */tɔːk/* or */taʊk/*) is an **AI-native engineering control environment** designed around the concept of a **semantic harness** for large language models.

The term *harness* is used here not in the metaphorical sense of a test framework (*eval harness*) or an autonomous agent framework, but in its **original, literal sense: bridle, bit, reins, and blinkers**.

A large language model is a fast, capable, but blind workhorse. If given unrestricted access to an operating system shell, it will drift aimlessly, break system environments, and get trapped in generative hallucinations. Conversely, if a human developer is forced to spoon-feed it every file manually, the tool becomes a cognitive liability.

TAUQE holds the model in a firm grip:
- **Blinkers (Repo Map):** The model perceives codebase contours and symbols through Tree-sitter, sufficient for architectural comprehension without context window bloat.
- **Bit & Bridle (Strict Protocol):** The model has no arbitrary shell or bash access. It interacts with the world solely through typed protocol operations (virtual in-memory staging, declarative file discovery requests, regex repository searches, deterministic toolchain verification).
- **Reins (Git Transactions):** The developer steers the objective, while Git ensures an instant, atomic rollback if the model deviates from course.

> **Maximum execution autonomy. Minimum procedural autonomy.**
>
> **TAUQE is not an agent. It is an environment where the model is autonomous within a strictly bounded turn, yet completely stripped of agentic drift.**

Unlike open-ended coding agents (such as Claude Code or OpenCode) that delegate the development lifecycle, toolchain dispatch, and operational decision-making to an unconstrained LLM loop, TAUQE hardcodes development workflows, state transitions, invariant checks, and Git transactions in deterministic Rust. Language models act as specialized cognitive components within a deterministic harness rather than autonomous process controllers.

### 1.1. The Fundamental Duality: "What" versus "How"

Software engineering has always hinged upon two foundational questions: **What** to build and **How** to build it.

Large language models are remarkably adept at answering *How*—synthesizing boilerplate, applying idioms, refactoring data pipelines, and implementing concrete routines. Yet they excel only when provided with a well-posed question and guided within disciplined structural boundaries. When released into open-ended, autonomous bash loops without guardrails, an LLM quickly drifts into generative hallucinations and speculative bloat.

In contemporary agentic practices, it is frequently asserted that imperative instructions must take precedence over declarative specifications. This contradicts a foundational principle of software engineering: specify *what* a system must achieve rather than micromanaging *how* it should execute. The emergence of large language models does not weaken this classical principle; it strongly reinforces it. Because an LLM is a probabilistic engine, long chains of imperative step-by-step instructions suffer from compounding error probabilities—one misstep in an imperative sequence quickly derails the entire operational trajectory. Conversely, language models demonstrate exceptional reasoning fidelity when bounded by explicit declarative constraints, architectural invariants, and typed schemas.

TAUQE's primary objective is to **liberate the developer's cognitive bandwidth so they can concentrate entirely on 'What'.** The engineer determines the objective, architectural intent, and requirements; the harness firmly steers and confines the model through a bounded turn to solve the *How*, deterministically and safely. TAUQE emphasizes declarative descriptions of the project, its boundaries, and its contracts, eliminating the need for fragile imperative micromanagement.

### 1.2. Model Role Specialization

TAUQE splits the *What*/*How* duality across dedicated, non-overlapping model roles. Each role keeps a single mandate and never absorbs the responsibility of another:

- **Architect** owns the *What*: high-level decomposition, system invariants, and conceptual boundaries. It produces plans and concept nodes, not concrete source lines.
- **Coder** owns the *How*: minimal, coherent, compiling code changes that resolve one well-posed step.
- **Primary** owns the interactive engineering dialogue and is the mandatory base role: its first entry is the idle active model and the deterministic default for every specialized role that omits its own list.
- **Reviewer** performs analytical, read-only inspection and emits findings; it never modifies files.
- **Scribe** handles history compaction, turning long multi-turn transcripts into dense, lossless context.

Each functional role is backed by an ordered list of model entries rather than a single model name. `primary` is mandatory and must list at least one entry; `coder`, `architect`, and `reviewer` are optional and inherit the entire `primary` list when omitted, so a specialized role never silently loses access to the configured models. Every entry may carry an optional reasoning depth (`effort`) and a human-readable `alias`, so the same model can run shallowly in one role and deeply in another without registering a duplicate.

The Architect/Coder boundary is deliberately rigid. The Architect defines the invariant; the Coder implements it. When code generation fails repeatedly, the harness **never switches the Coder into the Architect role**. Instead, escalation is intra-role: the *coder ladder* (`CoderTier`, each tier pairing a model with a reasoning effort) promotes execution to a stronger coder tier within the same *How* competence. Crossing back into the *What* role on a compile failure would blur the boundary the harness exists to enforce and would reintroduce the drift the paradigm is designed to prevent.

### 1.3. Decision Immutability and Guided Autonomy

A prevalent failure mode of autonomous agent loops is silent requirement drift: when an agent encounters friction during code generation, it frequently "changes its mind", weakens acceptance criteria, or alters unrelated subsystems to make tests pass.

TAUQE enforces strict **decision immutability**:
- Conceptual invariants (`concept`) and finished steps (`Done`) are immutable during plan execution.
- The Coder role cannot unilaterally relax requirements or re-architect boundaries.
- When an invariant cannot be satisfied or an unexpected dependency fails, the model emits a structured `<blocked>` protocol outcome, initiating a controlled replanning cycle rather than speculative drift.

### 1.4. Economic Efficiency via Role-Based Escalation

Separating high-level intent from low-level implementation delivers structural cost and token efficiencies:
- **Frontier reasoning models** are engaged where leverage is highest: initial problem formulation, invariant synthesis, and architectural refinement (Architect).
- **Cost-effective models** execute localized leaf implementation steps (Coder) within bounded turns guided by Repo Map blinkers.
- Compute escalates dynamically: the harness advances through the `CoderTier` ladder (deepening reasoning effort or promoting to stronger coder models) only when compiler verification catches errors.

---

## 2. Three Paradigms of AI-Assisted Development

Modern software engineering is caught between two ineffective extremes:

### 2.1. Context Micromanagement (The Aider Way)
The developer manually manages files: `/add`, `/drop`, `/read-only`. The human acts as a "context logistician", counting tokens and guessing which types the model will need.
- **The flaw:** Cognitive load on the developer increases rather than decreases.

### 2.2. Uncontrolled Agentic Loops (Claude Code / OpenCode / Shell Agents)
The model is granted an interactive shell or open tool dispatch and an open-ended goal. The agent runs in an autonomous loop: executing shell commands, guessing file paths across the disk, and attempting to steer the engineering process itself.
- **The flaw:** Loss of determinism, generative drift, shell hallucinations, zero safety guarantees, and broken trust when the model alters process invariants mid-flight.

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
        ┌─► [Discovery Phase] ──── (requests missing files via <context_request>,
        │           ↓               searches codebase via <repo_search>,
        │           │               or system docs via <doc_request>)
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
1. **Discovery:** The model inspects the Tree-sitter Repo Map, searches symbol usages or dependencies via `<repo_search>`, and requests required definitions or system documentation via protocol tags (`<context_request>`, `<doc_request>`). The server adds files to the `auto` context layer, returns search match summaries, or injects documentation blocks and iterates. No manual `/add` required.
2. **Staging:** No byte touches disk until the entire set of edits converges cleanly in virtual staging.
3. **Healing:** If the compiler detects an error, the model receives clean toolchain output and fixes only the offending lines without touching unrelated code.
4. **Git Transaction:** The turn is transactional. Issuing an undo command (`C-Z`) rolls back the working tree to its exact pre-turn state.

### 3.1. Isolated Working Memory: The Turn Scratchpad

Every bounded turn exposes a private, turn-local working-memory channel: the **scratchpad**. It is a hidden notebook where the model records working hypotheses, verified findings, and the purpose of context it still needs.

The scratchpad is strictly isolated from every other layer:
- It lives only inside the current turn and is discarded when the turn ends; it never enters persisted history.
- It is never rendered in the interface and is excluded from the streamed assistant text.
- It grants no capabilities and touches no bytes on disk. It is memory, not a tool.

Its purpose is to let the model hold state across multi-round discovery and verification-healing cycles without polluting the persistent conversation or the user-facing surface.

### 3.2. Deterministic Protocol Tags

The model communicates its intent exclusively through typed protocol tags. Each tag is a deterministic command rather than free-form prose:

- Discovery tags (`<context_request>`, `<repo_search>`, `<doc_request>`) request missing files, search codebase symbols across repository files, or request authoritative reference material.
- The scratchpad tag (`<scratchpad>`) records isolated, turn-local working memory.
- The verification tag (`<verify>`) requests deterministic toolchain execution.
- Terminal outcome tags (`<success>`, `<error>`, `<blocked>`) declare how the turn ends; the `<blocked>` outcome triggers semantic recovery instead of a blind retry.
- The language tag (`<user_language>`) pins the natural language for user-facing output.

Because these tags are parsed deterministically, the harness can enforce boundaries, react to declared outcomes, and guarantee that no unrecognized side effect ever reaches the operating system. There is no arbitrary shell: interaction with the world happens solely through typed, auditable protocol operations.

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
6. **Deterministic Process Over Agentic Delegation**  
   Workflow lifecycles, state transitions, invariant checks, and Git transactions are hardcoded in environment software (Rust), never delegated to probabilistic LLM decision-making. The model is an intellectual transformation engine within a disciplined harness, not a process dispatcher.
7. **Declarative Invariants Over Imperative Steering**  
   The environment prioritizes declarative specifications, typed contracts, and architectural invariants over imperative step-by-step micro-instructions. Specifying *what* properties and invariants must hold allows the harness and model to resolve *how* with maximum autonomy while eliminating the compounding errors inherent in procedural agentic steering.

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
Declarative Contracts → Invariant specifications over imperative steering
Model Roles           → Primary + Architect (What) + Coder (How) + Reviewer + Scribe
Developer             → Task specification and absolute control
LLM                   → Semantic transformation engine
```

> **Maximum execution autonomy. Minimum procedural autonomy.**
>
> Eliminating cognitive burden from the developer without stripping away control.
