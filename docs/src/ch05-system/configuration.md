# Configuration and Project Preparation

TAUQE configuration is a **single project surface with two audiences**, and the
section boundary between them is the concept.

- The **harness-policy sections** of `tauqe.toml` declare how the environment
  behaves: model roles, context budgets, retry and replan limits, verification
  policy.
- The **`[project]` declaration** of the same file declares how the repository is
  built and verified: the mandatory verification command, the optional external task
  runner, the optional in-memory formatter, the optional disk post-hook, the named
  commands the model may address, and the trust boundaries no staged operation may cross.

Two further files complete the picture without belonging to either audience: the
personal workstation file (`tui.toml`) and the credentials file. Neither is ever read
from the repository, so a theme or an API key can never reach a commit.

Two rules govern the whole chapter. Project behavior is declared in `tauqe.toml`, and
the shipped file is the living reference: every section is documented inline with its
default, so the file itself answers "what can I configure here?". And configuration is
never guessed: when a question concerns a parameter, a budget, or a project
convention, the answer comes from this chapter rather than from a recollection of
source code.

---

## 1. Motivation & Invariants ("Why")

### 1.1 Four kinds of configuration

A working environment carries four genuinely different kinds of configuration:

| Kind | Question it answers | Author | Lives in |
| --- | --- | --- | --- |
| Harness policy | how does the environment behave? | the team | `tauqe.toml` |
| Project commands | how is this repository built? | the build owner | `tauqe.toml`, `[project]` |
| Workstation preference | how does this terminal look and respond? | the individual | `tui.toml` |
| Secret | which credentials open the provider? | the individual, privately | `credentials.toml` |

Collapsing them produces failures that are well known and still common:

- A theme committed with the project forces every contributor to inherit one
  developer's colors.
- An API key committed with the project is published on the first push, and no later
  review can retract it.
- A compiler invocation committed with the workstation file makes the project build
  differently on every machine.
- A compiler invocation baked into the harness couples the harness to one language,
  and a toolchain upgrade becomes a harness release.

### 1.2 Why the `[project]` section is separate inside one file

Harness policy answers questions about cost, patience, and safety: how many discovery
rounds a turn may spend, how many verification retries are tolerated, which model
performs which role. The team that owns the engineering process decides those.

Project commands answer questions about compilers, formatters, and code generators.
The person who owns the build decides those, and they change on a different cadence: a
formatter upgrade must not require a review of harness policy, and tightening a retry
budget must not rewrite the build.

Keeping both in one file with two sections buys the useful half of each option: one
diff to review and one place to look, without one concern's churn landing inside the
other's declaration. What the harness must never do is parse a build system itself —
it runs a declared command line and reads its exit status.

### 1.3 The invariants

1. **One project surface.** `tauqe.toml` declares both policy and project commands;
   no second engineering manifest exists.
2. **The harness never parses the build system.** It runs a declared command, a
   declared stream filter, or a declared post-hook, and nothing else.
3. **Verification is mandatory.** A configuration that declares no verification
   command is rejected as a whole, because a turn that cannot verify its own result
   must not run.
4. **Resolution is first-existing-wins over a fixed candidate list**, and an explicit
   path short-circuits the search entirely.
5. **A missing file is a valid state.** Documented defaults apply, so a repository
   without configuration is usable rather than broken.
6. **A secret is never a project setting.** The provider key is not deserializable
   from `tauqe.toml`, and the credentials file is created with owner-only permissions
   before any byte is written.
7. **No operational limit is hardcoded.** Rounds, retries, replans, bytes, tokens, and
   command timeouts are configuration with modern defaults.
8. **Every new parameter is documented in the shipped template**, in commented form
   with its default, in the same change that introduces it.
9. **Configuration is read, never inferred.** A guessed default is indistinguishable
   from a fabricated one.
10. **A project file the harness could not load is never committed.** Any rewrite of
    `tauqe.toml` is validated in memory before it reaches the working tree.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Tier** — one audience of configuration: harness policy or project commands.
- **Candidate list** — the ordered paths a file is looked up in.
- **Template** — the `tauqe.toml` shipped with the binary and written by the
  onboarding create action; it is also the living reference.
- **Role** — a semantic responsibility (`primary`, `scribe`, `coder`, `architect`,
  `reviewer`) resolved to a concrete model by the server.
- **Entry** — one model reference with an optional reasoning `effort` and an optional `level_up` marker; a role slot
  never carries a display alias.
- **Catalog entry** — a model registered in `[providers.openrouter].models`, with an
  optional `alias` that is the single source of truth for its display name.
- **Ladder rung** — one entry of an ordered role list, consumed for intra-role
  escalation.
- **Phase** — one verification stage: `check`, `lint`, or `test`.
- **Patch strategy** — how a change set names the lines it replaces: by the text it
  echoes (`search_replace`) or by the opaque anchors rendered beside them
  (`line_anchor`).
- **Project command** — a declared command line with literal arguments and an optional
  timeout.
- **Reference** — an executable written as `@` plus the name of another declared
  command.
- **Formatter** — a streaming filter that rewrites staged content before it reaches
  disk.
- **Post-hook** — a disk-side side effect that runs after the write.
- **Named command** — an allowlisted entry the model may address as a typed tool.
- **Pinned pattern** — a repository-relative path resolved hierarchically into the
  read-only Pinned context layer.
- **Runtime state** — the generated artifacts the harness keeps under `.tauqe/`, which
  Git ignores.

### 2.2 Harness-policy sections

| Section | Declares |
| --- | --- |
| `[providers.openrouter]` | the native model catalog the picker may offer |
| `[models]` | semantic roles, their entries, and `auto_level_up` |
| `[develop]` | edit retries, verification healing budget, replan budget, workflow mode, edit protocol, patch strategy, prompt layout |
| `[git]` | upstream base and hook bypass |
| `[history]` | token budget and uncompressed tail length |
| `[context]` | discovery rounds, file, outline, and external-reference budgets, report caps, pinned patterns |

The `[develop]` section also selects the wire format of the model exchange through
its `protocol` key. The value is `structured` (the default), `xml`, or `mixed`; the
legacy alias `auto` still parses and resolves to `mixed`. The format never changes
the capabilities a turn exposes — its intent and the active discussion focus decide
those — so the three modes are interchangeable by design and differ only in
transport, token cost, and prefix caching.

| `protocol` | Transport |
| --- | --- |
| `structured` | strict JSON-Schema structured output for every turn |
| `xml` | marker-protected XML tags for every turn |
| `mixed` | two contours: XML for code edits, structured output for discussions, commit messages, and history compaction |

The same section declares how a change set addresses the lines it replaces, through
its `patch_strategy` key. The strategy is orthogonal to `protocol`: the protocol
picks the encoding of an edit, the strategy picks the fields that address its
range.

| `patch_strategy` | A range is addressed by | Prompt rendering |
| --- | --- | --- |
| `search_replace` | the replaced lines, echoed verbatim and matched exactly | the raw text of every context line |
| `line_anchor` | two opaque anchors naming the first and last replaced line | the opaque anchor of every context line |

One declaration feeds every consumer: the prompt renders the anchors, the wire
schema declares the fields that carry them, staging resolves the named ranges
against one snapshot from the bottom of the file upwards, and a failed patch is
explained in the same vocabulary. The profile module that owns that vocabulary is
described in *The Three-Phase Modification Lifecycle*; this chapter declares only
the key.

The same section declares how the rendered prompt blocks become a message sequence,
through its `prompt_layout` key. The policy is orthogonal to both the transport and
the patch strategy: the transport picks the encoding of a model answer, the strategy
picks how a change set addresses a line range, and the layout picks the order and the
growth shape of the message sequence.

| `prompt_layout` | Message sequence | Resolved default |
| --- | --- | --- |
| `semantic` | blocks grouped by kind: one system message carrying the contract, the outline, the whole context block, the active plan, the language directive, the history block, and the scratchpad | read-only discussion, isolated step |
| `chronological` | a byte-stable head, an append-only transcript of history records interleaved with the context files they observed, and a volatile tail of plan, context manifest, and scratchpad | interactive REPL |

An absent key keeps the resolution by turn intent: an interactive REPL turn takes the
chronological transcript, whose append-only growth keeps the provider prefix cache
warm from one turn to the next, while a read-only discussion and an isolated step
carry no growing history to interleave and keep the semantic order. A declared value
overrides that resolution for every intent, and the blocks themselves stay identical
under both policies — only their composition differs.

`providers.openrouter.api_key` is deliberately skipped on deserialization: a key
pasted into `tauqe.toml` is ignored rather than honored, so the mistake cannot become
a committed secret (section 3.8, *Why credentials are never a project setting*). The
key is supplied by the credentials file, by the environment variable, or by
onboarding.

### 2.3 The `[project]` declaration

| Section | Declares | Required | Moment |
| --- | --- | --- | --- |
| `[project.runner]` | the external task runner: `just` or `none` | optional | when the command surface is queried |
| `[project.verify]` | the verification command line | mandatory | after every other phase |
| `[project.format]` | the in-memory stream filter | optional | before the write |
| `[project.post_hook]` | the disk-side step | optional | after the write |
| `[project.commands]` | named allowlisted commands | optional | on an explicit invocation |
| `[project.boundaries]` | the repository paths no turn may modify | optional | on every staged operation |

Every entry accepts `command`, `args`, and `timeout_secs`. An entry whose `command`
is `@` plus a declared name reuses that declaration; such an entry carries no
arguments of its own, and a reference cycle or an undeclared target invalidates the
declaration rather than being ignored. The command contract itself is specified in
*The TauqeProjectProtocol*; this chapter states only where it lives and who owns it.

The boundary subsection is the one part of the declaration that runs no process. `protected`
lists repository glob patterns, resolved by the shared matcher, and every staged operation
that would create, rewrite, move, or delete a matching path is refused before the toolchain
starts. The shipped `tauqe.toml` is the living example: it protects `Justfile`, the
specifications under `docs/specs/`, and `.tauqe/refs/refs.md`, because each of them judges a
change set rather than being judged by one. The rejection itself belongs to *Protected Paths
and Trust Boundaries*.

### 2.4 Resolution hierarchy

Project configuration:

1. the explicit `--config PATH` argument (the only candidate considered);
2. `REPO/tauqe.toml`;
3. `REPO/.tauqe.toml`;
4. `$XDG_CONFIG_HOME/tauqe/tauqe.toml`;
5. `~/.config/tauqe/tauqe.toml`;
6. `~/.tauqe.toml`.

The repository comes first because it is the file every contributor shares and every
review can inspect. A machine-wide file exists only so a developer can work without
committing one; it never silently overrides what the project declares.

Credentials:

1. the explicit `--credentials PATH` argument;
2. `REPO/.tauqe/credentials.toml`;
3. `$XDG_CONFIG_HOME/tauqe/credentials.toml`;
4. `~/.config/tauqe/credentials.toml`;
5. `~/.tauqe/credentials.toml`.

The workstation file is resolved separately, in `$XDG_CONFIG_HOME/tauqe/tui.toml`
(falling back to `~/.config/tauqe/tui.toml`), and is never read from the repository.

### 2.5 Budgets are declared

| Key | Documented default | Bounds |
| --- | --- | --- |
| `context.max_discovery_rounds` | 5 | widening rounds per turn |
| `context.files_token_budget` | 300,000 | aggregate cost of context files |
| `context.repomap_token_budget` | 4,000 | the repository outline |
| `context.refs_token_budget` | 50,000 | the lazily loaded external reference documents |
| `history.budget_tokens` | 20,000 | session history before compaction |
| `history.tail_turns` | 10 | turns kept uncompressed |
| `develop.max_retries` | 5 | staging retries, verification healing rounds, and corrective step re-runs |
| `develop.max_replans` | 2 | automated replans after a blocked step |
| project entry `timeout_secs` | 600 | one verification, format, post-hook, or named-command invocation |

A project entry's own `timeout_secs` overrides the harness budget for that entry
alone. A repository may override any documented default, and this repository does
(section 3.6). The point is not the numbers but their addressability: a small library
tolerates a wide context and a monorepo does not, so a hardcoded limit forces either
a generous default that burns budget or a restrictive one that blocks legitimate
work. Volume is measured in tokens only: no parameter bounds how many files a
request may admit, and each file is priced as its content plus the wrapper its
`<file>` element renders, so the budget compares what the prompt actually carries.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Loading, defaults, onboarding, and reload

The daemon loads configuration at startup, resolves credentials, and validates the
model roles before publishing a typed configuration snapshot. A missing file is not an
error: documented defaults are applied, so the environment starts on a repository that
was never configured.

A discovered or explicit file that exists but cannot be read, parsed, or validated is
recorded as a diagnostic instead of aborting, so the interface can warn that the
project declaration was ignored rather than silently switching models and budgets.

First-run onboarding writes the living template and advances through a fixed order —
Git, configuration, credentials, workstation, command modifier, ready — derived solely
from completion flags. An optional phase the developer deliberately skips is recorded,
so the flow never loops back and no step is silently dropped. While onboarding is
open, view switching and header navigation are locked, so setup cannot be bypassed by
accident.

Reload is explicit and non-disruptive. The project configuration is re-read through
the `config/reload` request, and the workstation file is re-read by pressing `Ctrl+O`
in the terminal interface. Neither path restarts the process or interrupts an active
turn: a changed theme, modifier, or retry budget applies to the next frame or the next
turn.

### 3.2 Semantic roles and the two ladders

`[models]` maps responsibilities to models. `primary` is mandatory and lists at least
one entry; its first entry is the idle active model and the deterministic default
everywhere a role is implied. `scribe` is a single utility model and defaults to the
first `primary` entry. `coder`, `architect`, and `reviewer` are ordered lists, and an
omitted list inherits the entire `primary` list, so a project never loses access to a
configured model merely because it did not repeat it.

The provider catalog declares each model once. A catalog entry is either a bare
model specification or a `{ model, alias }` table, so a display name is written
beside the model itself instead of on every role slot that selects it. A role entry
then names a model either by its provider identifier or by that catalog alias and
adds only the runtime parameters of the slot — an optional `effort` (`low`,
`medium`, `high`, `xhigh`, `max`) and an optional `level_up` marker (`false`
excludes the entry from ladder escalation while leaving it selectable by hand). Both reference forms normalize to the same
concrete model before a turn starts, and an entry that repeats an `alias` is
rejected, because two sources of truth for one label drift apart.

A single string yields a one-rung ladder; an ordered array yields an explicit ladder
whose rungs differ by model or by reasoning depth, so the same model can run
shallowly in one role and deeply in another without a duplicate catalog entry.

Two ladders exist and they never cross. `coder` answers a failed *how* and is consumed
by staging retries, verification healing, and corrective step re-runs; `architect`
answers a failed *what* and is consumed by replanning a broken horizon. `auto_level_up`
enables the coder ladder; without it, a failing turn rolls back and reports instead of
escalating. The escalation rules themselves belong to *Model Roles and the Coder
Ladder*; this chapter declares only where the rungs live.

### 3.3 Verification: one declared lifecycle

`[project.verify]` is the verification lifecycle, and it is mandatory: a configuration
that declares neither a verification command nor a task runner exporting one is
rejected as a whole (section 3.5), because a change set the harness cannot verify is
never accepted.

The declaration *is* the pipeline. The harness carries no knowledge of a compiler or a
build system, so it runs the declared command line once per change set and nothing
else: there is no switch that disables verification, no automatic-healing toggle, no
verification target that selects a different set of commands, and no manifest
auto-detection. A repository that declares no command — or whose reference chain is
broken — resolves no step, and the change set is reported as unverified instead of
running an unintended program.

The wall-clock budget comes from the entry's `timeout_secs`, and the fallback is 600
seconds, which a real test suite routinely needs. Healing is bounded by the edit retry
budget (`develop.max_retries`), so a verification failure receives the same number of
repair rounds as a failed patch.

Every invocation is one process in its own process group with that budget, so a timeout
or a cancellation tears down the compiler and every binary it spawned. The exit status
is the verdict; a failing command's output is parsed into typed diagnostics — file,
line, column, severity, code, and message — instead of a transcript. *The Three-Phase
Modification Lifecycle* owns the order in which the phases run, and *Structured
Diagnostics and Verification Healing* owns what a failing phase produces.

### 3.4 The task runner and project commands

`[project.runner]` declares whether an external task runner supplies the command
surface. It is either `just` or `none`, and the default `none` means the project relies
solely on its own declarations. There is no autodetection: a `justfile` on disk is
inert until the declaration opts in.

With `runner = "just"`, the harness queries the runner once through its own declarative
dump and merges the result. A recipe is reachable only when it carries
`[group('tauqe')]`; its `[doc(...)]` becomes the tool description and its
`metadata('timeout=...')` becomes the invocation budget. A recipe named `verify` fills
the verification lifecycle, and every other admitted recipe becomes a typed custom
tool. The merge follows one rule — configuration wins: an explicit `[project.verify]`
or `[project.commands]` entry always overrides a recipe of the same role. A runner that
is missing, fails, or answers with an unreadable dump is reported as a typed failure
and never treated as one that exports nothing. *The TauqeProjectProtocol* owns the
command contract itself.

`[project.commands]` declares a closed allowlist of operations the model may address
by name. Each entry names an executable with literal arguments and an optional
timeout, so the model never composes a command line: it selects a declared operation,
and the harness runs exactly that declaration.

Three properties make the allowlist safe rather than decorative:

- **No free-form arguments.** The declaration already carries the arguments, so an
  invocation adds none. A command that would need a path or a flag from the model is
  the wrong command to declare.
- **Resolution before execution.** A broken reference chain, a cycle, or an undeclared
  name is refused before a process is spawned, and an unlisted name is answered with
  the declared allowlist instead of a guess.
- **Real output in the answer.** An invocation is reported with its exit status and
  the head of its output through the turn journal, so the next round reasons about
  what the command printed rather than the assumption that it ran. Invocations share
  the per-turn discovery round budget, so a turn cannot spend the whole budget on
  project commands without proposing a change.

The rationale is the harness invariant *No Arbitrary Shell*: the model may run an
operation the build owner published, and it may not invent one.

### 3.5 The self-preservation invariant

A configuration file the harness cannot load on its next start is a configuration that
breaks the environment. Any rewrite of `tauqe.toml` is therefore validated twice while
it still lives in virtual staging:

1. **Syntactic validation.** The staged bytes are parsed as TOML. A malformed file is
   rejected immediately.
2. **Semantic validation.** The parsed declaration is validated as a whole: the model
   roles must resolve, and the `[project]` declaration must name a verification
   command whose reference chain is sound.

A rejected stage is not a crash and not a commit: the failure surfaces as an ordinary
staged-file error, which activates the patch-retry loop, so the model receives the
precise reason — invalid TOML, a missing verification command, an undeclared
reference — and corrects the declaration before anything reaches the working tree. The
rejection is also logged as its own event, because a self-preservation failure is
worth distinguishing from a stale search block.

### 3.6 Context budgets and hierarchical pinned resolution

`[context]` sets the discovery round cap, the aggregate file budget, the outline
budget, and whether the eager symbol map is rendered (`include_repo_map`). When the
eager map is disabled, the prompt carries the compact directory tree instead; the two
outlines are mutually exclusive by design. This repository widens the documented
defaults (8 rounds, 500,000 file tokens, 40,000 outline tokens), which is exactly why
they are configuration rather than constants.

The file budget is the single measure of volume. No key bounds how many files one
request may admit, and every file is priced as its content plus the wrapper its own
`<file>` element renders, so the budget is spent on what the prompt actually carries.

`refs_token_budget` (50,000 by default) bounds one more working set: the external
documents the registry materialized during the task. It is the only ceiling that can drop
a document from that layer — it evicts the least recently requested one until the layer
fits, and it refuses a document that alone exceeds it instead of evicting every other
document for nothing. The aggregate `files_token_budget` counts a loaded snapshot like
every other context entry, yet it never removes one. *The External Reference Registry*
owns the layer, the manifest, and those eviction rules.

`pinned` names files that travel with every prompt, and resolution is hierarchical
rather than a flat root lookup. Each pattern is resolved at the repository root, and
then for every file in the User or Auto context layer each parent directory up to the
root resolves every pattern as a relative path. Loading
`crates/core/src/workflow/fsm.rs` therefore brings `crates/core/Conventions.md` into
the prompt, because subsystem conventions live at an ancestor level. Subsystem rules
arrive exactly when the subsystem code does, without a discovery round and without a
manual addition.

Pinned entries are read-only and configuration-owned: the interface cannot add,
remove, or re-permission them, and a path that escapes the repository root is
rejected. The layers themselves belong to *The Context Model*.

### 3.7 Preparing a repository

The environment reads project knowledge from files it can find deterministically, and
runs only commands the project declares, so preparation is the act of writing down
what is already known.

- **`Vision.md` states the *what*.** Mission, paradigm, architectural invariants, and
  role boundaries. It answers the question the model must never answer on its own:
  what is this system, and what is it forbidden to become?
- **`Conventions.md` states the *how*.** Language, minimalism and single-source rules,
  crate responsibilities, panic-free requirements, test expectations, and the commit
  format. Every proposed patch is judged against it.
- **Subsystem conventions supplement it.** A `Conventions.md` next to the code carries
  local invariants and is resolved into the Pinned layer hierarchically (section 3.6).
- **`[project.verify]` states how the tree is proven.** It is mandatory, so a
  repository is prepared only when its verification command is declared. The
  `[project.format]`, `[project.post_hook]`, and `[project.commands]` entries are
  optional and describe the operations this repository actually has.
- **`docs/src/SUMMARY.md` is the conceptual index.** Listing a chapter is what makes
  it reachable as an ontology; an unlisted chapter is invisible knowledge.

Keep the static prefix lean, because it is paid on every turn even when the provider
caches it: pin conventions and indexes rather than source files, keep each conventions
file cohesive rather than exhaustive, and let discovery fetch what a specific task
needs instead of front-loading everything.

The harness keeps its own runtime state under `.tauqe/`, which Git ignores, so no
generated artifact can reach a commit: `history.jsonl` (the interaction journal),
`plans/{id}.json` and `reviews/{id}.json` (one file per entity), `usage.json` (project
lifetime telemetry), `server.log` (the rotating daemon journal),
`active-transaction.json` (the open Git transaction, if any), and the project-local
`credentials.toml`.

The prompt recall list is the one piece of runtime state that deliberately lives
elsewhere. It once sat beside these files as `.tauqe/prompts.history`, and that
placement was wrong: a submitted prompt can embed a key, an internal hostname, or a
fragment of production data, and no file permission prevents a working tree from
staging what it holds. The editor now resolves the list in the developer's private
state directory — `$XDG_STATE_HOME/tauqe/prompts.history`, falling back to
`$HOME/.local/state/tauqe/prompts.history` — so a checkout never receives it, and the
session keeps prompts in memory only when the environment declares no usable state
directory.

### 3.8 Why credentials are never a project setting

A provider key answers *which identity opens the provider*, not *how this repository
behaves*. The first question belongs to one developer and must never be published; the
second belongs to the team and must be reviewable by every contributor. That split is
the reason the key has no home in `tauqe.toml`.

Three sources supply the key, in priority order: the resolved `credentials.toml`, the
`OPENROUTER_API_KEY` environment variable, and the interactive onboarding step, which
writes the file for the developer.

```toml
[openrouter]
api_key = "sk-or-v1-..."
```

On Unix the file is created with mode `0600` before any byte is written, so the secret
is never briefly world-readable, and a pre-existing file with a looser mode is
tightened on rewrite. A stub-creation path writes a commented template instead of a
real key, so a developer can locate the file and fill it in with an editor of their
choice. Credential operations are exposed as typed requests, so a frontend never edits
the file directly and never has to parse it.

An explicit `--credentials` path that cannot be read or parsed is reported as a
diagnostic instead of silently falling back to another file: a mistyped path is a
developer error, not a reason to spend a turn under the wrong identity.

### 3.9 The workstation file

`tui.toml` holds nothing the project should inherit:

- `[input]` — a `layout` preset, `primary_modifier` (`ctrl` or `alt`),
  `meta_modifier` (`alt` or `ctrl`), and an optional `langmap` for non-Latin keyboard
  layouts.
- `[theme]` — a built-in preset `name` (`tauqe-dark`, `tauqe-light`, `catppuccin`,
  `gruvbox`, `solarized-dark`), the legacy `mode` fallback, and a `color_depth`
  override (`auto`, `truecolor`, `ansi256`, `ansi16`).
- `[notifications]` — `sound`, `desktop`, `desktop_protocol`, `only_unfocused`,
  `min_duration_seconds`, and an optional `command` hook.

The semantics live where they are used: *Keyboard-First Interface* owns chords, the
semantic color roles, and notification granularity, and *Zero-Hardcode Interface
Localization* owns the interface language.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not two project files.** A separate engineering manifest would reintroduce
  two owners, two review cadences, and a second place to look for the command a turn
  just ran.
- **It is not an environment-variable soup.** One secret has an environment override
  for convenience; budgets, roles, and project commands are declared.
- **It is not a secret store.** A provider key in the project file is ignored, and the
  credentials file is permission-restricted and Git-ignored.
- **It is not guessed.** Parameters and defaults are answered from this chapter, never
  inferred from a partial memory of the source tree.
- **It is not parsed by the harness as a build system.** The harness runs a declared
  command line; it never reads `Cargo.toml` or `package.json`, and it never detects a
  toolchain from them.
- **It is not a description of the build.** `tauqe.toml` holds harness policy and the
  declared command lines; it does not describe the pipeline those commands run.
- **It is not a shell entry point.** A declared command has literal arguments and no
  free-form input, so the allowlist can never widen into arbitrary execution.
- **It is not a transport choice.** `patch_strategy` says how a change set addresses
  the lines it replaces; `protocol` says how the change set is encoded. Neither
  implies the other.
- **It is not static.** Both tiers reload without a restart and without cancelling
  work.
- **It is not a substitute for documentation.** A parameter that exists but is
  undocumented is indistinguishable from one that does not exist.

---

## 5. Related Chapters

- [The TauqeProjectProtocol](../ch03-project/tauqe-project-protocol.md) — the command
  contract, its declaration, and its process boundary.
- [Protected Paths and Trust Boundaries](../ch03-project/protected-paths.md) — the
  `[project.boundaries]` subsection and the staging rejection it drives.
- [The Three-Phase Modification Lifecycle](../ch03-project/three-phase-lifecycle.md) —
  where formatting, post-hooks, and verification run, and where the patch-strategy
  profile lives.
- [Structured Diagnostics and Verification Healing](../ch03-project/verification-healing.md) —
  what a failing phase produces.
- [The Context Model](../ch02-perception/context-model.md) — the layers these
  budgets bound.
- [The External Reference Registry](../ch02-perception/reference-registry.md) — the
  external-reference context layer and the `refs_token_budget` that bounds it.
- [Model Roles and the Coder Ladder](../ch01-foundations/model-roles.md) — the role
  boundaries both ladders obey.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — where the
  budgets are consumed.
- [Keyboard-First Interface](tui-ux.md) — `[input]`, `[theme]`, and `[notifications]` in
  use.
- [Zero-Hardcode Interface Localization](i18n-subsystem.md) — the interface language and
  its catalog.
- [Local Plans](../ch04-workflows/plans.md) — `develop.max_replans` and `auto_level_up`
  in action.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — `[git]
  bypass_hooks` and upstream resolution.
