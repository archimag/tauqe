# Structured Diagnostics and Verification Healing

Sovereign concept chapter: how a failing verification step becomes typed
file/line/severity records, how those records render into a deterministic report,
and how the bounded healing loop consumes that report without paying for a
compiler transcript.

A compiler banner is written for a human at a terminal. ANSI escapes, tool
banners, duplicated notes, and dependency noise are useful there and expensive
here. The healing round needs the opposite: a short, stable list of located
failures that names the file, the line, and the problem.

---

## 1. Motivation & Invariants ("Why")

### 1.1 The transcript problem

Forwarding raw compiler output into a model prompt has three costs:

- **Token waste.** A single Rust error can carry a dozen note and help lines, and a
  failed workspace build can produce thousands of them.
- **Instability.** The same failure renders differently across toolchain versions,
  color settings, and terminal widths.
- **Noise.** A model asked to repair an error can spend its budget on an ANSI
  escape or a dependency warning instead of the offending line.

### 1.2 Why a typed finding

A diagnostic is a data record: file, line, column, severity, optional code, and
message. A record can be sorted, deduplicated, filtered, and rendered
deterministically. A transcript can only be truncated.

### 1.3 The invariants

1. **Diagnostics are typed, not scraped.** The harness parses command output into
   structured records.
2. **The report is deterministic.** The same findings render identically
   regardless of the order the driver reported them.
3. **No ANSI escapes reach the report.** Rendering rebuilds from typed fields.
4. **Duplicates collapse.** Two records that describe the same finding render
   once.
5. **Healing is bounded.** The retry budget is finite and configurable.
6. **A failed check never commits.** The working tree rolls back to its pre-turn
   checkpoint.
7. **The model cannot weaken verification.** When automatic verification is
   enabled, the target comes from configuration, not from the model.
8. **A blocked outcome bypasses healing.** A semantic refusal is not a compiler
   error to retry.
9. **Selection narrows work, never the gate.** The project may route the change set
   to cheaper checks, but an empty or unrecognized change set selects the complete
   suite.

---

## 2. Mental Model & Ontology ("What")

### 2.1 The diagnostic

| Field | Meaning |
| --- | --- |
| `file` | repository-relative path, when the toolchain provides one |
| `line` | one-based line inside `file` |
| `column` | one-based column inside `file` |
| `severity` | error, warning, note, or help |
| `code` | tool-specific code, such as a compiler error number |
| `message` | the human-readable problem |
| `rendered` | the command's own pre-rendered line, retained for fidelity only |

`rendered` is retained but never used for rendering: the report is rebuilt from
the typed fields, so a command cannot smuggle ANSI escapes or duplicated text into
the healing prompt.

### 2.2 Severity

Severity is a closed set with a canonical order: `error` precedes `warning`,
`warning` precedes `note`, and `note` precedes `help`. The order is the reporting
order, so the most actionable findings appear first.

An unreadable or absent severity resolves to `error`, never to a note. A
malformed record must not downgrade a real failure.

### 2.3 The declared verification lifecycle

The repository declares one verification command in `[project.verify]`, and that
command line *is* the pipeline: the harness knows no compiler, so it runs the
declared process once and takes its exit status as the verdict.

A verification request still names a target — all, check, test, or lint — but the
target travels as a label for the interface, never as a command selector. Because
the repository described its own lifecycle, the model cannot narrow the gate by
asking for a lighter target, and a repository that declares no command has nothing
to verify instead of a guessed toolchain.

The declared command may itself be a dispatcher that reads the delivered change set
and routes the work, but the harness never composes, narrows, or rewrites the
command line: it spawns what the declaration names and reads the exit status.

### 2.4 The healing round

A healing round is one bounded model interaction that receives the verification
report and is asked to correct only the offending code. It is not a new task and
not a new conversation: it is the same turn, re-entering the proposal phase with
a diagnostic tail.

### 2.5 Error-path auto-registration

The report is parsed deterministically for repository paths. Every path that is
present in the tracked file list and not yet editable is added to the editable
auto context layer, so the next healing round can patch it without a discovery
round. The paths are extracted by a deterministic scan, never guessed by a model.

### 2.6 The change set and the selected checks

The turn owns exactly one authoritative list of changed paths, and the harness
delivers it to the verification command on standard input, one repository-relative
path per line. The command never derives that list from Git itself: while the
transaction is open, the working copy also holds the post-hook artifacts and the
developer's uncommitted work, which the turn did not propose.

What the list *means* is decided by the project, not by the harness. This
repository delegates the decision to a dispatcher script, which selects the
declared recipes a change requires: the workspace build for Rust sources and
workspace manifests, the markdown filter for documentation, both when the set mixes
them or carries a path no rule recognizes, and the complete suite when the list is
empty or the script was started from a terminal. The engine runs one declared
command line and never learns which suffix belongs to which technology.

Selection narrows the amount of work, never the gate. Whatever the dispatcher
chooses, the exit status of the declared verification command remains the only
verdict, and failing it still returns the tree to its pre-turn checkpoint.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Running a check

The pipeline runs the declared `[project.verify]` command line with its own
wall-clock budget and stops at its exit status. On Unix the command runs in an
isolated process group, so a timeout or a cancellation tears down the compiler
and every binary it spawned.

The command also receives the turn's authoritative change set on standard input,
one repository-relative path per line, so a project dispatcher selects the checks
the change requires. An empty change set writes nothing and the command runs its
complete suite, because a narrowed list may reduce work but can never switch a
check off. When the change set touches only documentation, this repository runs the
markdown filter and never starts the workspace build, so a one-line fix does not pay
for a full compile.

The result carries the command, its output, and the typed diagnostics parsed by
the harness. A command that reports only raw text still yields a result; the
difference is that its findings are not typed.

A selected documentation check is one of those typed results: the markdown filter
prints `path:line: error: message` for each finding and exits non-zero, and the
harness parses those lines into located `error`-severity diagnostics. The offending
documentation enters the editable context through the same deterministic path scan
as a compiler error, so the healing round can repair it without a discovery round.

### 3.2 Rendering the report

The renderer sorts findings by severity, then places located findings before
unlocated ones, then orders by file, line, column, code, and message. Findings
that differ only in their pre-rendered text are reported once.

A finding renders as:

```text
path:line:column: severity[code]: message
```

Only the parts a finding actually has are rendered. The column appears only when
the line is known. A location without a file is dropped in favor of the bare
message. Message whitespace is collapsed onto one line. No field is truncated,
because output budgets belong to configuration, not to rendering.

The report contains no ANSI escapes, no tool banners, and no trailing newline.

### 3.3 The healing loop

1. The turn applies its staged edits and runs verification.
2. On failure, the report is rendered and the referenced error paths are
   registered as editable context.
3. The loop asks the model for corrected edits, replaying the turn journal so the
   knowledge discovery already paid for survives.
4. The repair edits are applied to the working tree and verification runs again.
5. A clean check finalizes the turn. An exhausted budget rolls the working tree
   back to its pre-turn checkpoint.

The retry prompt is protocol-neutral: it refers to the edit protocol and the
discovery protocol defined in the system instructions instead of hardcoding a tag
vocabulary. Healing therefore works with every protocol implementation.

Two variants keep the loop honest:

- **missing edits** — the previous answer explained the problem but proposed no
  edits, so the prompt restates that concrete edits are required;
- **protocol errors** — the proposed edit blocks could not be parsed, so the
  prompt names the syntax errors and asks for corrected blocks.

### 3.4 Guards

| Guard | Effect |
| --- | --- |
| mandatory verification | the declared lifecycle runs after every change set; no flag turns it off |
| report mode | a model request chooses only whether a passing run is reported |
| retry budget | bounds the number of healing rounds (`develop.max_retries`) |
| cancellation | stops verification and rolls the turn back immediately |
| blocked outcome | bypasses healing entirely and rolls the tree back |

In an isolated step or a batch, verification is strictly determined by project
configuration. The model has no authority to configure, weaken, or skip it.

### 3.5 Clean build, missing declaration, exhausted budget

- **Clean build** — the turn is finalized as one isolated commit.
- **No declared command** — verification reports that the project declares no
  verification command, so the change set stays unverified. The turn is not failed
  for a declaration the configuration gate would already have rejected.
- **Exhausted budget** — the working tree is restored to the pre-turn checkpoint,
  the error report is returned, and no commit is created.

### 3.6 Escalation

When healing does not converge and ladder escalation is enabled, the failed
transaction is rolled back and the turn advances one rung of the coder ladder:
deeper reasoning on the same model first, a stronger coder afterwards,
saturating on the final rung. Escalation never leaves the coder role; the
architect role is not a repair mechanism for a compile failure.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a compiler wrapper that forwards raw output.** The report is rebuilt
  from typed records.
- **It is not unbounded retries.** The loop has a configured budget and a
  saturated terminal state.
- **It is not a model-controlled verification gate.** With automatic verification
  enabled, the target comes from configuration.
- **It is not a linter replacement.** It consumes findings; it does not decide
  which rules a project should enforce.
- **It is not a text scraper for correctness.** Error paths are extracted
  deterministically from the report; the model never invents them.
- **It is not a commit path on failure.** A failing check always ends in rollback,
  never in a partial commit.
- **It is not a reason to hide failure.** An exhausted budget, a blocked step, and
  a rolled-back turn remain distinct, named outcomes.
- **It is not a fixed check list.** Which commands run for a change set is decided
  by the project dispatcher; the harness runs what the declaration names and reads
  the exit status.

---

## 5. Related Chapters

- [The Three-Phase Modification Lifecycle](three-phase-lifecycle.md) — where the
  check sits among the phases.
- [The TauqeProjectProtocol](tauqe-project-protocol.md) — the command contract that
  produces the output.
- [Git Safety & Transactions](git-transactions.md) — the checkpoint a failed
  healing round returns to.
- [Model Roles and the Coder Ladder](../ch01-foundations/model-roles.md) —
  intra-role escalation.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) —
  the verification and healing phases.
