# Development Conventions

This document specifies mandatory engineering and architectural conventions for the Tauqe repository.

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

## 2. Rust Module Organization (Prohibition of `mod.rs`)

1. **Rust 2018+ Style:**
   - Using `mod.rs` files in new subsystems is **strictly forbidden**. Having numerous identical `mod.rs` files degrades editor navigation, stack traces, and debugging clarity.
2. **Naming convention:**
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
