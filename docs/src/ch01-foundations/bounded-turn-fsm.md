# 3. The Autonomous Bounded Turn FSM

A turn is not a conversation with a model. It is a finite-state machine that
carries one stated objective through discovery, proposal, staging, verification, and
healing, and then either commits the result or restores the repository.

---

## 3.1 Motivation & Invariants ("Why")

### 3.1.1 Why an unbounded loop fails

An agent loop that decides its own next action has no terminal state and no
guarantee that a step is reversible. Every imperative instruction added to such a
loop compounds the probability of a wrong branch, and a wrong branch usually
manifests as a mutated repository rather than as an error. A declarative boundary
inverts the economics: the process is fixed, so a failure is a known state with a
known recovery path.

### 3.1.2 The invariants

1. **Nothing reaches disk before the whole change set stages cleanly.** Edits are
   applied to a virtual tree first; a mismatch is a repair request, not a corrupted
   file.
2. **Every turn terminates in exactly one declared outcome** — success, error,
   blocked, or skip.
3. **A turn is transactional.** It ends as one verified commit, or the working tree
   returns to its pre-turn state.
4. **Every phase is bounded.** Discovery rounds, patch retries, healing rounds, and
   replanning cycles all have finite budgets.
5. **The process is deterministic; the content is generative.** The model chooses
   what to write, never which stage runs next.
6. **Volatile data never moves the cached prefix of the prompt.** Which is what
   keeps a multi-round turn economically viable.

---

## 3.2 Mental Model & Ontology ("What")

### 3.2.1 TurnIntent

Every turn runs in exactly one regime, declared as a typed value before the first
token is generated:

- `InteractiveRepl` — free interactive dialogue started from the REPL,
  including a dialogue anchored on a review finding;
- `PlanDiscussion` — architectural discussion of a plan;
- `IsolatedStep` — execution of one plan step or one review finding.

The intent is derived from typed request parameters, never from prompt text. It
narrows the capability set, chooses the plan mutation mode, and decides whether
the turn enters session history. The transport is declared separately from the
intent: a specialised plan discussion pins the structured discussion transport,
while a develop or step turn keeps the transport configured in `tauqe.toml` —
`structured` by default, `xml`, or `mixed`. The transport changes the encoding
only, never the capability surface: an XML turn and a structured turn advertise
and decode the very same operational commands. No default value exists, so a
forgotten argument cannot silently substitute a mode.

### 3.2.2 TurnPhase

The five phases are surfaced to the interface as turn-phase events:

| Phase | Repeats | Meaning |
| --- | --- | --- |
| `Discovery` | yes, bounded | widen the working set until contracts are known |
| `Proposal` | once per model interaction | produce minimal coherent edits |
| `Staging` | yes, bounded | validate edits against a virtual tree |
| `Verification` | once per applied proposal | run the project toolchain |
| `Healing` | yes, bounded | repair what verification rejected |

Discovery, Staging, and Healing carry a round counter and a maximum, because they
repeat; Proposal is emitted for every model interaction and Verification runs once
per applied proposal.

### 3.2.3 Capabilities and commands

A protocol declares what it supports as a set of capabilities — text message, file
edits, discovery, documentation requests, toolchain verification, plan creation,
plan update, scratchpad, outcome, blocked, abort, user language, project custom
tools. A capability set is the
profile mask naming the capabilities enabled for one role or mode, and role and
mode profiles are the only sanctioned way to narrow a protocol.

The `abort` capability closes a turn unconditionally when the remaining work
needs an action only the developer can perform on the host system, such as
installing a package or starting a service. The coder ladder never retries such a
turn, because no stronger tier can supply a capability the host lacks.

The tags named in this chapter and in the tool category table below are the
spellings of the XML contour. The structured contour expresses the very same
commands as fields of its JSON Schema, so the capability surface never depends on
the configured encoding.

Each capability belongs to a phase (`Discovery`, `Execution`, `Verification`,
`Outcome`). Model output is decoded into a normalized command list — text, edit,
context request, repository search, documentation request, verification, plan,
scratchpad, outcome, blocked, user language, protocol error — so no consumer ever
inspects raw markup.

Capabilities are implemented as modules. One module owns the canonical tag it
publishes, the aliases it still accepts, the prompt fragments it contributes, the
decoder that produces its commands, and the sanitizer that removes its tags from
conversational text. Because the same module supplies the tag, the instruction, and
the decoder, the prompt can never advertise a command the decoder rejects, and a
disabled capability leaves no trace in either the prompt or the command stream.

Each capability also declares how its output reaches the harness. `LiveStreaming`
marks the contours consumed while tokens arrive — conversational text and file
edits, which the interface renders as the model generates. `RoundDeferred` marks
everything that only becomes meaningful once the round finishes — discovery,
documentation, verification, plans, scratchpad, outcome, blockers, language, and
project tool payloads. Streaming scanners and round-level decoders read that
declared property instead of matching capability names, so delivery stays
orthogonal to phase: the execution phase carries both.

The code modification contour publishes the canonical `edits` container; the older
`tauqe_edits` spelling remains a declared alias, so transcripts, sessions, and
model output written before the rename stream and decode exactly like the
canonical tag.

### 3.2.4 Tool Categories: Terminal vs. Diagnostic Inspection

Tools and operations accessible to the model are partitioned into three distinct
functional categories that govern their lifecycle within the bounded turn:

| Tool Category | Operations / Tags | Lifecycle Effect | Terminates Turn? |
| --- | --- | --- | --- |
| **Perception (Discovery)** | `<context_request>`, `<repo_search>`, `<doc_request>`, `<dir_list>`, `<repo_dirs>` | Widens context or provides authoritative reference | **No.** Triggers next Discovery round. |
| **Inspection (Diagnostic Execution)** | Declared project commands (`CustomTool`, e.g. integration tests, linters, benchmarks) | Executes isolated CLI process; streams stdout/stderr back into the turn journal | **No.** Triggers next Proposal round for model reasoning. |
| **Mutation (Terminal Outcome)** | `<edits>`, `<success>`, `<error>`, `<blocked>`, `<skip>` | Commits patches to virtual staging or signals turn conclusion | **Yes.** Closes the turn upon verification and Git commit. |

Calling an inspection tool (such as running full integration tests) is an
exploratory, diagnostic act: it implies that the turn is **not finished**. The model
must receive the command's exit code, stdout, and stderr to analyze failures,
inspect stack traces, and adapt its subsequent code edits.

### 3.2.5 Terminal outcomes

Four typed tags close a turn:

- success — the objective was implemented, staged, and verified;
- error — an insurmountable contradiction or unresolvable failure;
- blocked — a deliberate semantic assertion that the step's preconditions cannot be
  satisfied as stated;
- skip — the finding was already addressed or is not reproducible.

A blocked outcome is not a failure to retry: the transaction is rolled back, no
patch retry or healing cycle runs, and the owning supervisor may replan instead of
repeating.

### 3.2.5 ApplyPolicy

`ApplyPolicy` isolates *how* validated edits reach disk from the state machine. It
exposes four operations: staging, apply temporary, apply final, and rollback. Two
implementations cover the supported transaction models: a direct policy that writes
straight to disk for naive mode and tests, and a Git policy that checkpoints,
commits a temporary step, and finalizes or rolls back.

### 3.2.6 The record, its status, and the three stores

A session keeps three separate stores, and the harness never treats one as a
stand-in for another:

- **The model history** — `.tauqe/history.jsonl`, an append-only JSON-lines
  journal of turns, responses, reviews, and compactions. It is the sole source of
  the `<history>` block, so what is absent from it is absent from the model's
  dialogue memory.
- **The developer chronicle** — the History view. It renders the journal together
  with the record of the turn that is still running and the record of the turn
  that was rolled back, neither of which the journal holds for long.
- **The editor prompt history** — `.tauqe/prompts.history`, the prompt buffer's
  own recall list. It stores submitted prompts only, newest last, deduplicated and
  capped, and is written with owner-only permissions. It is a typing convenience,
  not a dialogue: it is never rendered into a prompt and never reaches the model.

The journal answers what the harness and the model exchanged; the editor file
answers what the developer typed. Because they are different files, a failed turn
cannot lose a prompt from the editor and cannot leak it into the model's context.

Every journal record also carries a typed lifecycle status, and the status is
never inferred from the text of a message:

| Status | Meaning | Wire form |
| --- | --- | --- |
| `Active` | the turn is running; the record is visible but not yet authoritative | `"active"` |
| `Committed` | the turn's result is durable and belongs to the model context | omitted |
| `RolledBack` | the turn ended without a result, so the record left the journal | `"rolledBack"` |

The committed state is the default and adds no field, so a record written before
the status existed still deserializes as committed.

---

## 3.3 Mechanics & Lifecycle ("How it behaves")

```text
      objective
          |
   [Discovery] --requests--> context, search, docs, outlines
          |
   [Proposal]  --edits-----> virtual tree
          |
   [Staging] --mismatch----> [Patch Retry] --bounded-->
          |                        |
          |                     converged
          v                        v
      apply to disk --> [Verification] --errors--> [Healing] --bounded-->
                              |                          |
                           clean                    escalated coder
                              v                          v
                        finalize commit  <--or-->  roll back to checkpoint
```

### 3.3.1 Discovery

Discovery widens the working set until the model reasons about contracts rather
than contours. The model inspects the repository outline and emits typed requests:
open a file into context, search the tracked tree by regular expression, render the
folder tree, outline one subsystem, list the tracked files of one directory, or
request an authoritative documentation chapter.

Each request is executed by its own registered handler, so a capability owns its
validation, disk work, and feedback text. The round resolves repository facts once,
folds every handler's findings into one outcome, and halts as soon as the files
token budget would be breached — no later capability runs against a context that was
never widened.

Two details make the loop cheap. Files preloaded for a step bypass the budget,
because the caller already decided they are required. And findings that are not
repository files — search reports, listings, outlines, documentation — never enter
the context block: they accumulate in a turn-scoped discovery journal that extends
the user message and survives the whole turn, so a staging retry or a healing round
keeps the knowledge discovery already paid for.

A round reports whether it made progress. That single question lets the machine
distinguish a productive round from a dead end without inspecting individual request
types, and it is bounded by `context.max_discovery_rounds`.

### 3.3.2 Proposal

The protocol assembles the system prompt, streams lightweight events to the
interface, and decodes the finished answer into normalized commands. Edits are
validated against a virtual staging tree that tracks staged content per path, so a
second hunk against the same file sees the first hunk's result.

A file that is present but read-only may still be modified when the task genuinely
requires it: the patch is applied and the file's access is promoted in place, which
keeps the declared permission a knowledge hint rather than a silent refusal. Files
absent from context cannot be edited at all — the model must request them first.

Clear, compact, and indivisible work is executed right here: the answer carries its
edit block and the turn moves on to staging. A broad request — one that touches
several independent subsystems or contracts, decomposes into dependent stages, or
needs those stages verified separately — is not patched on the spot in Develop.
Such a round explains instead why decomposition is warranted and offers the
developer a choice between drafting a plan and performing the whole change at once.
That offer is text only: an edit block and a plan payload may not share the answer,
because a proposal that mutated storage would settle a question the developer has
not answered yet. The round still closes with its terminal success tag, so an
accepted or declined offer is a completed turn rather than an unfinished one, and
the plan itself is written only after explicit confirmation (*Local Plans*).

### 3.3.3 Project Command Execution & Feedback Loop

When a proposal invokes a declared project command (or a recipe from the external
task runner's `tauqe` group) without code modifications, the harness treats it as a
diagnostic investigation step rather than a terminal completion.

1. **Validation & Isolation:** The invocation is checked against the declared
   allowlist. It runs in its own process group with strict timeout enforcement
   and literal arguments.
2. **Structured Journal Feedback:** The execution outcome (`exit status`, `stdout`,
   and `stderr`, truncated to safe scalar boundaries) is recorded into the
   turn-scoped `TurnDiscoveryJournal` inside a structured `<project_command_feedback>`
   block.
3. **Next Proposal Round:** Instead of terminating the turn with an empty answer,
   the FSM requests an answer round from the model with the updated journal. The
   model perceives the real command output, evaluates test failures or runtime
   panics, and either proposes corrective code edits, invokes another inspection tool,
   or emits an explicit terminal outcome (`<success>` / `<blocked>`).
4. **Shared Round Budget:** Diagnostic tool executions consume the turn's discovery
   round budget (`context.max_discovery_rounds`), preventing unbounded agentic loops
   while ensuring models have sufficient feedback to diagnose complex failures.

### 3.3.4 Patch retry

Matching is deterministic and tolerant of what models actually produce: indentation
shifts, tab versus space variance, and trailing whitespace are normalized before a
hunk is applied. What cannot be tolerated — a missing anchor, an ambiguous anchor,
or a stale surrounding block — is reported precisely.

A staging failure never aborts the turn and never discards work. Already staged
files stay staged, the failing files and their matching errors are formatted into a
report, and the model is asked to re-emit only the failing hunks. The repair log is
append-only, so attempt *n* carries the history of attempts `1..n` instead of
replacing it, and the loop is bounded by `develop.max_retries`.

### 3.3.5 Verification

Once staging converges, edits are applied atomically to disk and verification runs
through the declared project commands in three phases: in-memory formatting of
staged source text, disk-side post-hooks after the write, and the mandatory
verification command. The suite
reports typed diagnostics — file, line, column, severity, message — instead of a
compiler transcript, which is what lets the healing round reason about a location
instead of paying for ANSI escapes in its context window.

A verification request names its target (all, check, test, or a lint pass); a
repository without a detected toolchain skips the gate transparently instead of
failing.

### 3.3.6 Healing and intra-role escalation

A failed check enters a targeted healing round: the diagnostics are formatted into
a clean report, missing error paths are staged into the context, and the prompt goes
to the active role. Healing repeats while the budget lasts.

When it does not converge, the harness does not abandon the *what*. With ladder
escalation enabled, the failed transaction is rolled back, the transient discovery
context is pruned, and the turn advances one rung of the coder ladder — deeper
reasoning on the same model first, a stronger coder afterwards, saturating on the
final rung. Escalation never leaves the coder role; *Model Roles and the Coder
Ladder* owns that boundary. A blocked report bypasses the ladder entirely.

### 3.3.7 Finalize or roll back

A converged, verified turn is finalized as one isolated commit authored by the
developer, marked with a co-author trailer so the harness can recognize its own
work. Intermediate step commits exist only so healing can converge phase by phase
and are squashed away, so history never exposes the healing noise of a recovered
turn.

Anything else — an unresolved failure, an exhausted ladder, a cancellation, a
blocked report — restores the working tree to the checkpoint taken before the turn
and discards the temporary commits. If the process dies mid-turn, recovery reads the
transaction marker written when the checkpoint was opened and rewinds only when
every commit in the recorded range carries the harness trailer; a foreign commit in
that range makes recovery refuse and report instead of rewriting history.

**The session journal is transactional in the same way.** The prompt is appended
to `.tauqe/history.jsonl` and published as `Active` the moment the turn starts, so
the running task is visible before any result exists. A turn that produces a
result appends it and publishes both records as `Committed`.

**A rolled-back record is deleted, not annotated.** When the turn ends any other
way — a provider failure, a cancellation, an exhausted ladder, a blocked report —
`rollback_entry` removes the prompt from the journal and rewrites the file, then
reports the discarded record with its `RolledBack` status and the interruption
reason. Three properties follow from deletion rather than from a status update:

1. **The model history stays pure.** The next turn assembles `<history>` from the
   journal, so a prompt whose turn never committed is simply not there. The model
   never reads a task it produced no result for, and never has to reconcile a
   half-finished turn with code that was rolled back.
2. **The frozen prefix is restored, not rewritten.** The deletion returns the
   journal to exactly the bytes it held before the turn began, so the history block
   of the following turn is identical to the one that would have been rendered had
   the prompt never been sent.
3. **The chronicle stays honest.** The deletion is scoped to the journal. The
   client receives the discarded record with its status and reason and keeps it on
   screen, so the developer sees that the attempt happened and why it ended.

The in-process publisher reports the three structurally different mutations
explicitly — appended, compacted, and rolled back — so a rollback never
masquerades as a trailing append and a compaction never masquerades as a message.

### 3.3.8 The five pipeline layers

The machine is decomposed so that each layer owns one reason to change, which is
what makes it testable without a network:

| Layer | Responsibility |
| --- | --- |
| Execution mode | lifecycle of one turn or of a supervised queue, role escalation, cost accounting |
| Task and loop | the phase cycle itself, with steps that declare whether they repeat |
| Prompt protocol | prompt assembly, capability declaration, decoding into commands |
| Apply policy | staging, temporary commit, finalize, rollback |
| Provider | transport, streaming, cancellation; a retrying decorator wraps every provider |

A scripted provider satisfies this contract without a network call, so multi-round
scenarios — discovery, a compiler failure, a successful healing patch, a
cancellation — run deterministically in milliseconds.

Batch supervision sits above single turns. It owns the queue, synchronizes entity
statuses, accumulates usage, and stops in two distinct ways: a hard cancellation
aborts the running member immediately, while a soft stop is observed strictly
between members, letting the current turn finish its verification and commit before
the queue halts. Hard cancellation wins when both are pending, and the two modes
surface as distinct terminal statuses.

### 3.3.9 Prompt layout policies across rounds

Every round re-sends the whole assembled prompt, and providers cache the longest
unchanged token prefix. Turning that prompt into a message sequence is a policy
behind one trait, so the same rendered material can become different sequences:

- `SemanticLayout` groups the material by kind: one system message carrying the
  contract, the outline, the whole `<context>` block, the active plan, the language
  directive, the `<history>` block, and the scratchpad.
- `ChronologicalLayout` turns the same material into an append-only transcript: a
  byte-stable head, one message per history record with the context files that
  byte-stable head, one message per history record with the context files that
  record already observed placed right after it, and a volatile tail.

Both policies place the regime delta — the contract rules that only the active
intent states — on the user side of the sequence. Under the semantic order it is
the leading block of the final user message; under the chronological order it is
the first block of the trailing user message, ahead of the active plan, the
context manifest, and the scratchpad. Round diagnostics (patch retry reports,
verification failures) are appended after it, so a retry never cuts the cached
prefix a second time. Inside one turn the delta is byte-stable, because the
intent does not change between rounds.

The head renders the main capability list of the intent and the shared part of
the regime trunk, so it is byte-identical for the free REPL, the plan discussion,
and the review discussion. What a dialogue narrows further — the plan mutation
delta of a plan discussion, and the empty profile delta of a review discussion —
travels in the regime delta instead. The invariant that a capability is
advertised exactly when it is decoded survives, because head and delta are read
from the same typed profile.

| Material | Semantic | Chronological |
| --- | --- | --- |
| Contract, instructions, metadata | system head | system head |
| Repository outline | system head | system head |
| Language directive | system head | system head |
| Context files | one `<context>` block | `<context>` groups in the transcript |
| History | one `<history>` block | one message per record |
| Active plan | system tail | trailing user message |
| Regime delta | leading block of the final user message | first block of the trailing user message |
| Context manifest | absent | trailing user message |
| Scratchpad | system tail | trailing user message |

The head carries only what is identical from round to round. The repository map is
generated once per turn and frozen, and it stays in the head under both policies:
exploration and planning dominate a turn, and the map is the largest block, so
moving it into the tail would pay a full reprocessing pass on every round. The price
is documented rather than hidden — a map regenerated between turns excludes the
files that entered the context, so the head itself changes and the whole tail is
invalidated.

Growth inside a turn stays append-only. Discovery appends an element at the tail of
the context block, or appends the file after the last record of the transcript, and
a promotion rewrites a single `access` attribute in place. The chronological order
comes from the insertion watermark every context entry carries, so a file picked up
between two turns appears between them, and a file no record observed yet closes the
transcript.

The tail carries everything that legitimately changes: the regime delta first,
then the active plan, the context manifest of the chronological policy — paths and
access, never content — and the turn scratchpad, while round feedback (discovery
findings, patch retry reports, verification diagnostics) is appended after the
delta.
fixed shape: task first, accumulated journal next, phase diagnostics last. A staging
retry therefore differs from its own earlier attempt only in the trailing
diagnostics.

One typed resolution decides which policy a turn runs under: a declared layout wins,
otherwise an interactive REPL turn takes the chronological transcript and a
discussion or an isolated step takes the semantic order, because those turns carry
no growing history to interleave.
### 3.3.10 Transport resilience and output ceiling

Transport failures that say nothing about the request — a timeout, a reset
connection, a rate-limit or server-error status, a stream that breaks before its
terminator — are retried by a decorator around every provider, with exponential
backoff and jitter, bounded attempts, and a ceiling on any single pause. Fatal
statuses are reported at once and consume no attempt. Every second of a pause is
reported as a connection-retry event so the interface shows a live countdown, and
the pause races the cancellation signal so an interrupt aborts it immediately.

A failure before any token is transparent; a failure after partial output resets the
accumulated fragments, so two attempts are never stitched into one answer.

A retry policy guards a transport that fails, not one that keeps working. A model
caught in a reasoning loop streams indefinitely, so one attempt's cumulative output
is metered against a hard ceiling. Crossing it aborts the attempt, discards the
partial output, and reports a terminal error that is deliberately not retried: the
same request would reproduce the same loop.

### 3.3.11 The process container

The turn runs inside a daemon that is a child of the client. The daemon opens no
socket, keeps no lock file, and registers no singleton: it reads requests from
standard input and answers on standard output. It is spawned in its own process
group, so closing a terminal window kills only the client and the daemon observes
the resulting end of input instead of dying mid-turn with an unfinished
transaction.

End of input is the one managed death channel; `SIGHUP` and `SIGTERM` converge on
the same path — cancel the active operation, drain in-flight work within the budget,
exit — and the cause is recorded as typed data. The drain budget and the client's
wait are declared together, with the wait strictly greater than the budget, so the
client never kills a daemon that is still rolling back. A detached lifetime is
deliberately absent: it would introduce a second lifecycle that no turn depends on.

### 3.3.12 Guards and budgets

| Guard | Setting |
| --- | --- |
| Discovery rounds per turn | `context.max_discovery_rounds` |
| Context files budget | `context.files_token_budget` |
| Repository outline budget | `context.repomap_token_budget` |
| Patch retries | `develop.max_retries` |
| Automated replans | `develop.max_replans` |
| Outlines per discovery round | fixed request cap |

No operational limit is hardcoded, and no budget silently discards work: a breached
budget halts with a diagnostic that asks for a narrower scope, so the developer
learns what to decompose instead of watching a turn fail anonymously.

---

## 3.4 Boundaries & Anti-Patterns ("What it is NOT")

- **It is not an agent loop.** The machine never asks the model what to do next; it
  asks the model to do the current step.
- **It is not a single-shot completion.** Discovery, retry, and healing rounds are
  first-class states, not error handling bolted onto a request.
- **It does not own project knowledge.** Formatters, post-hooks, and verification
  commands belong to the `[project]` declaration; the machine only decides when they
  run.
- **It does not retry forever.** Every loop has a configured budget and a saturated
  terminal state.
- **It does not infer the mode from text.** Intent is a typed value; a prompt that
  merely resembles an internal heading cannot change the regime.
- **It does not create a plan on its own initiative.** A broad request produces an
  offer inside the answer, never a plan document; storage changes only after the
  developer confirms.
- **It does not hide failure.** An exhausted budget, a blocked step, and a rolled
  back turn are all reported as distinct, named outcomes.
- **It does not keep an uncommitted prompt in the model context.** A turn that
  produced no result leaves no record behind, so the model never reads a task whose
  outcome it cannot see.
- **It does not infer a record's status from its text.** Active, committed, and
  rolled back are typed states, never a phrase inside a message.
- **It does not confuse prompt recall with dialogue memory.** The editor list is a
  typing convenience backed by its own file, not a second transcript.

---

## 3.5 Related Chapters

- [Philosophy: The Semantic Harness](semantic-harness.md) — the invariants this
  machine enforces.
- [Model Roles and the Coder Ladder](model-roles.md) — intra-role escalation.
- [Turn Scratchpad](turn-scratchpad.md) — the volatile tail of the prompt.
- [The TauqeProjectProtocol](../ch03-project/tauqe-project-protocol.md) — what
  verification delegates to.
- [The Three-Phase Modification Lifecycle](../ch03-project/three-phase-lifecycle.md) —
  where formatting, post-hooks, and checks run.
- [Structured Diagnostics and Verification Healing](../ch03-project/verification-healing.md) —
  the shape of a failing check and the loop that repairs it.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — checkpoints,
  step commits, and crash recovery.
