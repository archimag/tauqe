# 8. Configuration and Project Preparation

A harness is only as reliable as the declarations it receives. TAUQE therefore treats configuration as a first-class engineering artifact, not as an afterthought scattered across environment variables and magic constants. This chapter describes every configuration file the environment reads, how those files are resolved, and how a project should be prepared so that the model reasons about *What* instead of guessing at *How*.

Two rules govern the whole chapter:

- **One authoritative project file.** Project behavior is declared in `tauqe.toml` at the repository root. The shipped file is the living reference: every section is documented inline with its default value, so the file itself answers "what can I configure here?".
- **Configuration is not guessed.** When a developer asks about a parameter, a budget, a keybinding, or a project convention, the model requests this chapter through `<doc_request topic="config">` instead of inferring an answer from the source code it happens to hold. A guessed default is indistinguishable from a fabricated one.

---

## 8.1 Why Configuration Is Split into Three Files

Three distinct kinds of configuration exist, and mixing them causes well-known failures:

| File | Scope | Under version control? |
| --- | --- | --- |
| `tauqe.toml` | Project: models, toolchain, budgets, pinned files | **Yes** — describes the project, not the developer |
| `$XDG_CONFIG_HOME/tauqe/tui.toml` | Workstation: keybindings, theme, notifications | No |
| `credentials.toml` (or environment) | Secrets: provider API keys | **Never** |

The separation is deliberate. A project file that carried a personal theme would force every contributor to inherit one developer's colors, and a project file that carried an API key would publish the key on the first push. Conversely, a workstation file that carried the toolchain command would make the project build differently for every engineer. Each concern lives exactly where it is safe for it to live.

---

## 8.2 Resolution Hierarchy

Every configuration file is resolved by searching an ordered candidate list and taking the **first existing** candidate. The order is deterministic, and an explicitly passed path short-circuits the search entirely.

### 8.2.1 Project configuration (`tauqe.toml`)

1. The explicit `--config <path>` command-line argument, when provided (the only candidate considered).
2. `<repo_root>/tauqe.toml`
3. `<repo_root>/.tauqe.toml`
4. `$XDG_CONFIG_HOME/tauqe/tauqe.toml`
5. `~/.config/tauqe/tauqe.toml`
6. `~/.tauqe.toml`

When no candidate exists, the harness runs on documented defaults that are deep-merged with the preferred path, so a missing file is a valid state rather than an error.

**Why the repository comes first.** The repository file is the one every contributor shares and every code review can inspect. A machine-wide file exists only so that a developer can work without committing a `tauqe.toml`; it must never silently override what the project declares.

### 8.2.2 Credentials

1. The explicit `--credentials <path>` argument.
2. `<repo_root>/.tauqe/credentials.toml`
3. `$XDG_CONFIG_HOME/tauqe/credentials.toml`
4. `~/.config/tauqe/credentials.toml`
5. `~/.tauqe/credentials.toml`

An API key is written with owner-only permissions (`0600`) on Unix, created with those permissions *before* any byte is written, so the secret is never briefly world-readable. Pre-existing files with looser modes are tightened on rewrite.

**Why `.tauqe/`.** The directory is ignored by Git, so project-local credentials are structurally incapable of being committed. Section 8.4 covers how a key reaches the process at all.

### 8.2.3 User interface configuration (`tui.toml`)

Resolved independently of the project file, in `$XDG_CONFIG_HOME/tauqe/tui.toml` (or `~/.config/tauqe/tui.toml`). It is never read from the repository, because terminal preferences are personal (see [5.5](../ch05-interface/tui-ux.md#55-user-configuration-tuitoml)).

---

## 8.3 Project Configuration Reference (`tauqe.toml`)

The following sections are the complete project surface. `tauqe.toml` at the repository root documents each key inline, including commented-out optional keys and their defaults; treat that file as the authoritative value list and this chapter as the rationale behind it.

### 8.3.1 `[providers.openrouter]`

- `models` — the catalog of native model identifiers the model picker may offer.
- `api_key` is deliberately **not** deserializable from this file. It is marked `#[serde(skip)]`, so a key pasted into `tauqe.toml` is silently ignored and can never be committed by accident.

### 8.3.2 `[models]` — Semantic Roles

Roles replace a flat "one model" setting with semantic responsibilities:

- `primary` is **mandatory** and lists at least one entry. Its first entry is the idle active model and the deterministic default for every specialized role that omits its own list.
- `scribe` is a single utility model for history compaction, squash commit messages, and diff summarization. It defaults to the first `primary` model.
- `coder` is the escalation ladder: an ordered list whose rungs pair a model with a reasoning effort. Verification healing and exhausted retries advance one rung at a time and saturate on the last rung.
- `architect` produces plans, concept nodes, and horizon syntheses.
- `reviewer` performs read-only analysis and emits findings.
- `auto_level_up` enables coder-ladder escalation on verification stalls.

Every entry accepts either a bare string (`"~provider/model"`) or a table:

```toml
coder = [
  { model = "~deepseek/deepseek-flash-latest", effort = "high" },
  { model = "~deepseek/deepseek-flash-latest", effort = "max" },
]
```

- `effort` selects the reasoning depth (`low`, `medium`, `high`, `xhigh`, `max`) for that entry, so the same model can run shallowly in one role and deeply in another without duplicating a catalog entry.
- `alias` replaces the raw model name in the picker.

**Why roles instead of tiers.** An omitted specialized role inherits the whole `primary` list, so a project never loses access to a configured model merely because it did not repeat the list. Escalation also stays strictly inside the Coder role: a compile failure is a *How* problem, so it is answered by a stronger Coder, never by handing the task to the Architect.

### 8.3.3 `[develop]`

- `max_retries` — the retry budget for failed edit staging and for corrective re-runs of a step turn.
- `max_replans` — the bound on automated replanning cycles triggered by a blocked step.
- `workflow` — the edit workflow (`git`, `naive`).
- `protocol` — the active edit protocol (`xml`, `structured`).

### 8.3.4 `[toolchain]` — Verification Phases and Formatters

Verification is a pipeline of typed phases: `check`, then `lint`, then `test`. Each phase resolves its command in a fixed order:

1. its own section (`[toolchain.check]`, `[toolchain.lint]`, `[toolchain.test]`), when it names a `command`;
2. the legacy `check_command` field;
3. deterministic project auto-detection (`Cargo.toml`, `package.json`, `go.mod`).

A phase that names no command keeps auto-detection, so declaring a section never *disables* a phase by accident. Each phase accepts `command`, `args`, `timeout_secs` (a phase-local override of the global timeout), and `enabled` (set `false` to drop the phase without deleting it).

Global keys:

- `check_command` — a custom verification command that overrides auto-detection.
- `max_retries` — the bound on the compiler-driven self-healing loop.
- `auto_heal` — whether healing starts automatically on a failed check.
- `timeout_secs` — the wall-clock budget of one verification command.
- `auto_verify` — whether verification runs at all on a turn that applied edits.
- `target` — the default verify target (`all`, `check`, `clippy`, `test`) when a request does not name one.

Formatters are a named registry under `[toolchain.fmt.<name>]`. `extensions` is mandatory: an entry without it selects no file, so a monorepo registers one formatter per language and never reformats the others. Formatting is best-effort — it runs after a verified build and immediately before the commit, and a formatter failure restores the verified content instead of interrupting the turn. Arguments are passed to the process literally rather than through a shell, so a path containing spaces or metacharacters stays safe.

### 8.3.5 `[git]`

- `upstream` — the base branch used when computing squashes and diffs; auto-detected when omitted.
- `bypass_hooks` — passes `--no-verify` so harness commits are not reshaped by project hooks.

### 8.3.6 `[history]`

- `budget_tokens` — the session-history budget that triggers compaction.
- `tail_turns` — how many recent turns stay uncompressed in their original form.

### 8.3.7 `[context]`

- `max_discovery_rounds` — the bound on autonomous discovery round-trips per turn.
- `files_token_budget` — the aggregate estimated-token budget of all context files.
- `repomap_token_budget` — the budget of the repository outline, both the eager map and every targeted map.
- `include_repo_map` — `true` renders the eager Tree-sitter symbol map; `false` renders the compact `<repo_dirs>` folder tree instead. The two outlines are mutually exclusive by design (see [2.6.1](../ch02-context/three-tier-model.md#261-on-demand-repository-map)).
- `max_pattern_batch` — the safety cap on files admitted by one glob pattern request.
- `pinned` — the pinned patterns resolved hierarchically (section 8.5).

**Why every limit is configurable.** Token budgets, retry counts, and discovery rounds are project economics, not universal truths: a small library tolerates a wide context, a monorepo does not. Hardcoding a limit forces one of two failures — an overly generous default that burns budget, or a restrictive one that blocks legitimate work.

---

## 8.4 Credentials

Three sources supply a provider key, in priority order:

1. `credentials.toml` resolved through the hierarchy of section 8.2.2;
2. the provider API key environment variable;
3. an interactive first-run onboarding step, which writes `credentials.toml` for the developer.

`credentials.toml` holds:

```toml
[openrouter]
api_key = "sk-or-v1-..."
```

The harness also offers a stub-creation path that writes a commented template instead of a real key, so a developer can locate the file, understand its shape, and fill it in with an editor of their choice. Credential operations are exposed as typed requests, so a frontend never edits the file directly and never has to parse it.

**Why credentials are never a project setting.** A key in `tauqe.toml` is a key in the first commit. Keeping secrets in a separate, permission-restricted, Git-ignored file means the project file stays reviewable and shareable, and the secret stays on the machine where it was issued.

---

## 8.5 Preparing a Project for TAUQE

The environment reads project knowledge from files it can find deterministically. Preparing a repository is therefore an act of *writing down what is already known* so the model does not have to infer it.

### 8.5.1 `Vision.md` — the *What*

`Vision.md` states the mission, the paradigm, the architectural invariants, and the role boundaries. It answers the question the model must never answer on its own: *what is this system, and what is it forbidden to become?*

A task phrased against an explicit vision produces coherent changes; the same task phrased against nothing produces plausible drift. The file exists so that intent survives history compaction, session restarts, and a change of model.

### 8.5.2 `Conventions.md` — the *How*

`Conventions.md` at the repository root carries the engineering rules: the project language, minimalism and single-source-of-truth rules, crate responsibilities, panic-free requirements, test expectations, and the commit message format. It is the contract every proposed patch is judged against.

Subsystems may add their own `Conventions.md` next to their code (for example `crates/core/Conventions.md` for engine invariants or `crates/tui/Conventions.md` for layout and navigation semantics). A subsystem convention supplements the root rules and references them instead of restating them, so one rule keeps one owner.

Keep each conventions file cohesive and small. A conventions file that grows without bound stops being read and starts being skimmed, by humans and by models alike.

### 8.5.3 `docs/src/SUMMARY.md` — the conceptual index

The book summary is the map of the conceptual ontology the model can request on demand. Listing a chapter is what makes that chapter reachable through `<doc_request>`; an unlisted chapter is invisible knowledge.

### 8.5.4 Pinned Context and Hierarchical Resolution

`[context].pinned` names files that travel with *every* prompt. A typical declaration:

```toml
[context]
pinned = [
  "Conventions.md",
  "Vision.md",
  "docs/src/SUMMARY.md",
]
```

Resolution is hierarchical, not a flat root lookup. For every file in the User or Auto context layer, the harness walks its parent directories up to the repository root and, at each level, resolves every pinned pattern as a relative path. Existing matches join the Pinned layer read-only, deduplicated, and verified never to escape the repository root.

The consequence is practical: opening `crates/core/src/workflow/fsm.rs` automatically brings `crates/core/Conventions.md` into the prompt, because that file exists at an ancestor level. Subsystem rules arrive exactly when the subsystem code does, without a discovery round and without a manual `/add`.

Pinned files are read-only by default. Editing one is still possible — an explicit write upgrades its effective access — but the default states the intent: conventions are authoritative input, not a scratchpad.

### 8.5.5 Keeping the Static Prefix Lean

Pinned files and the assembled repository outline form the static prefix of every request. That prefix is designed to be cached byte-for-byte across the rounds of a turn, so its size is paid once per turn rather than once per round — but it is still paid on every turn.

Practical guidance follows directly:

- pin conventions and indexes, not source files;
- keep each conventions file cohesive rather than exhaustive;
- prefer the compact `<repo_dirs>` outline (`include_repo_map = false`) in large repositories and request symbol outlines on demand;
- let discovery fetch what a specific task needs instead of front-loading everything.

### 8.5.6 Runtime State under `.tauqe/`

Not everything in a prepared repository is written by hand. The harness keeps its own runtime state in `.tauqe/`, a directory Git ignores, so no generated artifact can ever reach a commit:

- `history.jsonl` — the interaction journal of the current repository;
- `plans/{id}.json` and `reviews/{id}.json` — one file per plan and per review session;
- `prompts.history` — the prompt editor history;
- `usage.json` — the project lifetime telemetry;
- `server.log` — the rotating daemon journal.

`usage.json` deserves its own note, because it is the one artifact that crosses a process boundary. Session token, money, and time totals belong to the running daemon and restart with it; the *project* lifetime scope accumulates every completed operation and is persisted after each one, so the accumulated cost of a repository survives restarts. The document is written atomically, and a missing, unreadable, or incompatible document degrades to zero totals instead of blocking startup. Clearing it is an explicit interface action, never a side effect of quitting.

---

## 8.6 Self-Knowledge and This Chapter

The model is expected to know how the environment is configured, and it is expected to know it authoritatively. The knowledge path is deterministic:

1. The system prompt states that environment behavior is declared in `tauqe.toml` at the repository root and that this chapter is the authoritative reference.
2. When a question concerns a parameter, a budget, a file hierarchy, or a project layout, the model emits `<doc_request topic="config">`.
3. The harness returns this chapter and the model answers from it.

The same path applies to every other chapter of this book: the model requests the chapter instead of reconstructing an answer from a partial memory of the source tree. Prompted self-knowledge is a contract about *where the answer lives*, not a claim that the answer is already known.

---

## 8.7 Related Chapters

- [2. Three-Tier Context Model](../ch02-context/three-tier-model.md) — layers, permissions, discovery, and the on-demand repository map.
- [3. The Autonomous Bounded Turn](../ch03-bounded-turn/lifecycle.md) — where toolchain phases and healing budgets are consumed.
- [5. Keyboard-First Interface](../ch05-interface/tui-ux.md) — `tui.toml` in full: input modifiers, theme presets, and notifications.
- [7. Local Plans](../ch07-plans/plans.md) — `develop.max_retries`, `develop.max_replans`, and the coder ladder in action.
