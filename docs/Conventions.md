# Development Conventions

This document specifies mandatory engineering and architectural conventions for the Tauqe repository.

---

## 0. Project Language Standard

1. **English as the Sole Project Language:**
   - English is the mandatory and official language for the entire codebase and repository artifacts.
   - All documentation (`README.md`, `Vision.md`, `Conventions.md`, architecture docs), source code comments, variable and function identifiers, Git commit messages, and user-facing interfaces (terminal UI views, modal dialogs, help menus, status/error messages, onboarding steps) must be written exclusively in English.

---

## 1. YAGNI and Minimalism (No Speculative Code)

1. **No speculative features:**
   - Never add unused enum variants, unneeded struct fields, dummy stub methods, or parameters "just in case" or "for future flexibility".
   - Code must be written strictly for the current step and immediate requirements.
2. **Minimum sufficient complexity:**
   - If a problem can be solved with a flat list or standalone helper functions without introducing abstract factories, complex traits, or indirection layers, implement it directly.
3. **Dead code removal:**
   - Code that is superseded or rendered obsolete (such as legacy protocols, abandoned memory concepts, or deprecated modes) must be deleted completely, never commented out or hidden behind dormant feature flags.

---

## 2. Module Design and Architecture

1. **Modules as Black Boxes (Information Hiding & Minimal Visibility):**
   - **Encapsulation:** A module must expose strictly what external callers need to interact with it. Internal helper functions, intermediate data structures, and algorithmic details must stay hidden.
   - **Principle of least visibility:** Items are private by default. Promote only to `pub(super)` or `pub(crate)` when required across sibling files within a subsystem. Unrestricted `pub` is reserved strictly for public API entry points.
   - **Clean facades:** Module facades (e.g., `ui.rs`, `git.rs`) orchestrate subsystems. Never leak or re-export internal implementation helpers (such as string formatting, geometry math, or ad-hoc converters) through the facade.
2. **Single Responsibility and High Cohesion:**
   - Every module must have a single, cohesive reason to change. Never mix transport/RPC, input handling, state mutations, and presentation logic in one place.
   - Avoid "God Objects" or dumping miscellaneous logic into `main.rs` or monolithic facades.
3. **Rust 2018+ Module Layout (Prohibition of `mod.rs`):**
   - Using `mod.rs` files in new or refactored subsystems is **strictly forbidden**. Having numerous identical `mod.rs` files degrades editor navigation, stack traces, and debugging clarity.
   - For any module with submodules, place `foo.rs` alongside the `foo/` directory:
     ```text
     crates/core/src/
       history.rs              # Module root and facade
       history/
         entry.rs              # Types submodule
         storage.rs            # Storage submodule
         compaction.rs         # Summarization submodule
     ```
   - Inside `history.rs`, declare submodules as:
     ```rust
     pub mod entry;
     pub mod storage;
     pub mod compaction;
     ```
4. **Cohesive Sizing as a Health Metric (AI-Friendly Architecture):**
   - File size is an indicator of cohesion, not an arbitrary bureaucratic quota:
     - **Target size:** 150–400 lines of code. Modules of this size are easy to reason about, review, and maintain.
     - **Soft limit:** 600 lines. Crossing this threshold is a strong signal that the module contains mixed responsibilities and should be decomposed into focused submodules.
     - **Hard limit:** 800 lines. Considered an architectural smell requiring refactoring (unless an exceptional case such as generated parsers or deterministic lookup tables).
   - Compact, cohesive modules ensure reliable, unambiguous reasoning and diff generation by language models, eliminate context waste, and prevent merge conflicts.

---

## 3. Mandatory Code Verification

1. **Always verify after changes:**
   - After any modifications to code, tests, or build configuration, execute full project verification (`check`, `clippy`, `test`).
   - The model must always request verification (`target="all"`) when proposing code edits.
2. **Zero linter warnings and clean test suites:**
   - No compiler errors or `cargo clippy` warnings are permitted (clippy runs strictly with `-D warnings`).
   - All unit and integration tests across all workspace crates must pass cleanly (`test result: ok`).

---

## 4. Commit Message Format (Conventional Commits)

All commits must follow the **Conventional Commits** specification:

```text
<type>(<scope>): <description>
```

- **Types:** `feat`, `fix`, `refactor`, `test`, `docs`, `perf`, `chore`.
- **Scopes (crates & subsystems):** `core`, `server`, `tui`, `workflow` (or omitted for cross-cutting changes).
- **Style:** English language, imperative mood (*add*, *fix*, *update*), lowercase, no trailing period, maximum 72 characters in the header.

---

## 5. Living Configuration Reference (`tauqe.toml`)

1. **Authoritative Example:**
   - The root `tauqe.toml` file in this repository serves as the authoritative, living reference example of all configuration options.
2. **Mandatory Documentation of Parameters:**
   - When introducing any new configuration section or parameter in `AppConfig`, it must immediately be reflected in `tauqe.toml`.
   - New or optional parameters should be provided in commented-out form (`# key = value`) with their default values explicitly shown, along with a concise descriptive comment.
3. **No Hardcoded Budgets:**
   - All token budgets and operational limits (history tokens, tail turns count, repomap tokens, discovery rounds, retry counts) must be configurable through `AppConfig` and carry sensible, modern defaults.

---

## 6. Living User Guide (`book/`)

1. **Dual-Use Documentation as First-Class Artifact:**
   - The user guide located in `book/` serves as both the authoritative end-user manual and the semantic concept ontology for the AI model.
   - `book/src/SUMMARY.md` is pinned as the primary conceptual index.
2. **Mandatory Documentation of Conceptual Changes:**
   - Whenever a task introduces or alters user-facing behavior, context mechanics (e.g., layers, permissions), the bounded turn lifecycle, interface workflows, or keybindings, the corresponding chapters in `book/src/` must be updated within the same turn.
   - Obsolete explanations must be revised immediately to prevent documentation rot and model hallucination.

---

## 7. System Prompts and Generalization (Anti-Overfitting)

1. **Generalized Rules over Discussion Artifacts:**
   - When authoring, modifying, or refining prompts generated by or built into the system (such as system instructions, code review prompts, patch-retry templates, or compaction prompts), all instructions, constraints, and requirements must be formulated strictly in a general, robust, and conceptual manner.
   - **Strict Prohibition of Anecdotal Overfitting:** Never inject verbatim examples, incidental bug snippets, task-specific names, or conversational fragments directly from the current discussion that motivated the prompt change.
   - Embedding discussion-specific examples leads to generative overfitting, context pollution, and brittle model behavior that fails to generalize across diverse programming languages, domains, and user styles.
2. **Operational Invariants over Illustrative Bloat:**
   - Define clear operational invariants, input/output contracts, and behavioral boundaries instead of listing exhaustive ad-hoc special cases.
   - When specifying behavioral rules (such as language consistency, error handling, or format requirements), state the invariant directly (e.g. *"Formulate responses in the primary natural language established by the user's instructions"*), avoiding incidental sample queries or task-specific illustrations.
