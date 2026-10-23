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

### 2.3 Verification target and phases

A request names a target:

- **all** — check, then lint, then test;
- **check** — the build only;
- **test** — the test suite only;
- **clippy / lint** — the lint pass only.

The target decides which phases run, in which order, and the pipeline stops at
the first failing phase. A repository without a detected toolchain skips the gate
transparently instead of failing the turn.

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

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Running a check

The pipeline resolves its phases from configuration, runs each command with its
own wall-clock budget, and stops at the first failure. On Unix the command runs
in an isolated process group, so a timeout or a cancellation tears down the
compiler and every binary it spawned.

The result carries the command, its output, and the typed diagnostics parsed by
the harness. A command that reports only raw text still yields a result; the
difference is that its findings are not typed.

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
| `auto_verify` | when enabled, the configured target is authoritative; a model request can only choose the success report mode |
| `auto_heal` | gates the repair loop; disabled means one failed check ends the turn |
| retry budget | bounds the number of healing rounds |
| cancellation | stops verification and rolls the turn back immediately |
| blocked outcome | bypasses healing entirely and rolls the tree back |

In an isolated step or a batch, verification is strictly determined by project
configuration. The model has no authority to configure, weaken, or skip it.

### 3.5 Clean build, missing toolchain, exhausted budget

- **Clean build** — the turn is finalized as one isolated commit. A configured
  best-effort formatter registry runs first over the changed files, and a
  formatter failure restores the verified content rather than blocking the
  commit.
- **No toolchain** — verification succeeds with an explicit note that no build or
  test toolchain was detected. The turn is not failed for a repository that has
  nothing to build.
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
