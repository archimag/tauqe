# The Three-Phase Modification Lifecycle

Sovereign concept chapter: the three phases a modification passes through —
in-memory format, disk post-hook, and mandatory verification — and why their order
is not negotiable.

A change set is not written to disk and then repaired. It is formatted while it still
lives in virtual staging, side effects run only after it reaches disk, and
verification runs only after those side effects finish. Each phase has one job, one
moment, and one failure contract.

---

## 1. Motivation & Invariants ("Why")

### 1.1 A modification is not one write

Three different concerns surround a change set:

- its **shape**, which a formatter can improve without knowing the task;
- its **consequences on disk**, which a code generator, a lock-file refresh, or a
  migration must produce;
- its **correctness**, which only a compiler, a linter, or a test suite can judge.

Collapsing them into one step forces a bad compromise. Formatting after the write
creates diffs nobody proposed. Running a generator before the write reads stale
input. Verifying before the generator runs checks a tree that is not final.

### 1.2 Why the order is fixed

1. **Format before disk.** The staged content is the real payload. Rewriting it in
   memory keeps the proposed change coherent and prevents a formatter from changing
   bytes staging never validated.
2. **Post-hook after disk, before verification.** Generated sources, refreshed lock
   files, and migrations must exist when verification runs. Running a hook before the
   write would generate from the previous content.
3. **Verification after both.** The verification command must see the final tree:
   formatted input, applied patch, completed side effects.

### 1.3 The invariants

1. **In-memory format touches no disk.** The filter receives content on standard
   input and returns content on standard output.
2. **An unclaimed or failed format keeps the authored content.** Formatting is
   best-effort and never blocks a turn.
3. **A disk formatter failure restores the verified content.** The registry that runs
   after verification can never turn a compiling change into a non-compiling one.
4. **Post-hooks run strictly after the write and before verification.**
5. **Post-hook artifacts join the same change set.** They are captured through Git
   with `.gitignore` respected, so verification and the final atomic commit see them.
6. **A failed post-hook aborts the change set.** A tree whose generator failed cannot
   be committed.
7. **Verification is declared and mandatory.** A change set the harness cannot verify
   is never accepted, and a failing command yields typed diagnostics rather than a
   transcript.
8. **Routing is declarative.** The declaration decides which command owns a phase;
   the engine never hardcodes a language, and a phase that does not apply is skipped,
   not faked.

---

## 2. Mental Model & Ontology ("What")

### 2.1 The phases

| Phase | Declared by | Channel | Moment | Failure contract |
| --- | --- | --- | --- | --- |
| **In-Memory Format** | `[project.format]` | standard input and output | before any byte reaches disk | the authored content is kept |
| **Disk Post-Hook** | `[project.post_hook]` | a plain process | after the write, before verification | the change set is aborted |
| **Verify** | `[project.verify]` | a plain process | after every phase above | typed diagnostics feed the healing loop |

### 2.2 Terms

- **Staged content** — the validated in-memory text a change set will write.
- **In-memory format** — rewriting staged text through the declared stream filter.
- **Disk post-hook** — a side effect that needs the file to exist on disk.
- **Verification command** — the mandatory command that decides whether the change
  set is accepted.
- **Change set** — the repository-relative paths the turn touched: the patch, the
  post-hook artifacts, and any path a failed repair left dirty.
- **Best-effort disk formatters** — the harness-level `[toolchain.fmt.<name>]`
  registry, which runs over changed files after a verified build and immediately
  before the commit.

### 2.3 Two formatting surfaces, deliberately distinct

The in-memory formatter and the best-effort disk formatter registry are not the same
mechanism:

| Surface | Input | Moment | Failure behavior |
| --- | --- | --- | --- |
| In-memory formatter | staged content on stdin | before the write | authored content is kept |
| Disk formatter registry | literal changed paths | after verification, before commit | verified content is restored |

The first shapes the patch. The second is a convenience pass over a tree that already
compiles. Neither is allowed to turn a verified change into an unverified one.

### 2.4 The change set is the single truth of a turn

Verification, the final commit, and the reported result all read one list. It is
built from three sources: the paths the patch wrote, the artifacts the post-hook
produced, and the paths a failed repair left dirty in the working copy. Merging them
into one ordered, duplicate-free list is what prevents a file that is visibly
modified on disk from falling outside the commit that claims to describe the turn.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 In-memory format

The harness selects the declared `[project.format]` entry and streams one file's
staged content through it. The declaration owns the arguments, the harness owns the
stream, and the child runs in its own process group, so a filter that spawns a helper
cannot outlive the phase.

The outcome distinguishes an absent declaration, a rewritten file, and a failure:
a non-zero exit status, a timeout, or a cancellation keeps the authored content and
reports why. Because the filter reads a stream and writes a stream, it cannot create,
delete, or rename anything in the working tree.

### 3.2 Disk post-hook

Once the change set is written, the declared post-hook runs as a plain process. It
receives the literal arguments its declaration names and no file list: the harness
does not guess which paths the generator needs, and the hook does not have to parse a
request to learn them.

The working copy is snapshotted before the hook starts and after it exits
successfully, and the difference — computed through Git, with ignores respected —
becomes part of the turn's change set. A hook that fails, times out, or is cancelled
aborts the change set: the caller reports a failed generator instead of committing
artifacts nobody can reproduce.

### 3.3 Verification

Verification runs the declared `[project.verify]` command with its own wall-clock
budget and stops at the first failure, because a later command would run on top of a
change set the earlier one already rejected. Cancellation stops the pipeline
immediately and rolls the turn back.

A failing command's combined output is parsed into typed diagnostics — file, line,
column, severity, code, and message — and those diagnostics are what enters the
healing loop. The transcript never does. A repository with nothing to build verifies
cleanly with no steps, so the gate does not fail a project that has nothing to
compile.

### 3.4 Targets and legacy phase resolution

A verification request names a target: all, check, test, or lint. The declared
`[project.verify]` command is authoritative for every target, because the repository
described its own lifecycle.

A repository that declares nothing keeps the legacy resolution, where phases run in a
fixed order — check, then lint, then test — and stop at the first failure. Each phase
resolves its command in turn:

1. its own typed section, when that section names a command;
2. the legacy project-wide `check_command`;
3. deterministic auto-detection from the build manifests the repository carries.

A phase that names no command keeps auto-detection, so declaring a section never
disables a phase by accident. A phase switched off with `enabled = false` keeps its
declaration but contributes no command at all, and a phase-local `timeout_secs`
overrides the global budget for that phase alone.

### 3.5 Failure semantics

- **Format failure** — the authored content is kept and the turn continues.
- **No declared formatter** — the file is written exactly as authored; this is the
  normal outcome for documentation and configuration.
- **Post-hook failure** — the change set is aborted and the turn fails with the
  hook's diagnostic instead of a misleading compile error.
- **Verification failure** — the diagnostics enter the healing loop; nothing is
  committed until verification passes.
- **Disk formatter failure** — the snapshotted content of the files it claimed is
  restored, and a formatter that touched a file outside the change set has that file
  reverted, so a best-effort pass cannot smuggle a surprise diff into the commit.
- **Cancellation** — a cancelled phase stops immediately; an interrupted turn rolls
  the working tree back instead of committing a partially processed tree.

### 3.6 Timeouts and process isolation

Every phase is one invocation with a wall-clock budget, an isolated process group, and
a teardown that reaps the child. A formatter that spawns a compiler, a hook that
spawns a generator, and a verification command that spawns a test binary are torn
down together when the budget expires or the developer cancels.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a shell pipeline.** Every phase declares an executable and literal
  arguments; nothing is interpreted by a shell.
- **It is not a single pass.** Formatting, side effects, and verification have
  different inputs and different moments.
- **It is not unconditional.** A repository declares which phases exist; a missing
  declaration means the phase is skipped rather than faked.
- **It is not a formatter-only protocol.** A project with no formatter still has a
  post-hook and a verification command.
- **It is not a replacement for verification.** Formatting cannot prove correctness,
  and a formatted file is not a verified file.
- **It is not a hidden rewrite.** In-memory output still travels through staging and
  the normal commit path, and a failed disk formatter restores the verified content
  instead of leaving a surprise diff.
- **It is not language-specific.** The engine names paths and declared commands; it
  never names a compiler.
- **It is not a best-effort commit of a failed generator.** A failed post-hook ends
  the change set.

---

## 5. Related Chapters

- [The TauqeProjectProtocol](tauqe-project-protocol.md) — the command contract these
  phases speak.
- [Structured Diagnostics and Verification Healing](verification-healing.md) — what
  verification produces and who repairs it.
- [Git Safety & Transactions](git-transactions.md) — what happens after a clean
  verification.
- [Configuration and Project Preparation](../ch05-system/configuration.md) —
  formatter, hook, command, and verification declarations.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — where
  verification and healing sit in the turn.
