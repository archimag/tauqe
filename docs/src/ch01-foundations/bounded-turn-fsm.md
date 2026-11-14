# 3. The Autonomous Bounded Turn FSM

A turn is not a conversation with a model. It is a finite-state machine that
carries one stated objective through research, proposal, staging, verification, and
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
3. **A turn is transactional.** It ends as one commit that carries the explicit
   verdict of its verification run, or the working tree returns to its pre-turn
   state.
4. **Every phase is bounded.** Research rounds, patch retries, healing rounds, and
   replanning cycles all have finite budgets.
5. **The process is deterministic; the content is generative.** The model chooses
   what to write, never which stage runs next.
6. **Volatile data never moves the cached prefix of the prompt.** Which is what
  keeps a multi-round turn economically viable.
7. **Silence is not an action.** A round the harness cannot act upon is a
   non-answer, whatever status it declares. It is re-asked, and a budget that
   runs out ends the turn with a visible error instead of a validated silence.

---

## 3.2 Mental Model & Ontology ("What")

### 3.2.1 The capability tree of a control loop

Every turn runs in exactly one regime, and the regime is declared by the loop function
that runs it rather than by a value handed in from outside:

- `run_repl_loop` — free interactive dialogue started from the REPL, including a
  dialogue anchored on a plan or on a review finding;
- `run_turn_loop` — execution of one plan step or one review finding;
- the architect refinement contour — the Architect projects the unfinished horizon of
  a plan from repository evidence.

Each of those functions owns the tree of its own surface and hands it to the shared
kernel through `LoopContext::capabilities`. `repl_capability_expr` in
`control/repl.rs`, `turn_capability_expr` in `control/turn.rs`, and the synthesis tree
in `control/refine.rs` are the only declarations of a capability surface: nothing else
constructs one, and no enumeration of turn intents exists to be consulted instead.

The tree is composed from typed request parameters, never from prompt text, and it
decides everything the regime implies — which capabilities narrow the protocol, whether
plan mutation is authorized, whether the assembled prompt carries the `<history>` block,
and which prompt layout the turn runs under. Narrowing is a typed intersection:
whatever a protocol advertises is cut down to the tree the loop declares, so the prompt
and the decoder observe one surface — a capability is never advertised while remaining
undecodable, nor decoded while hidden from the instructions. The interactive dialogue
loop carries the dialogue head without plan mutation, and the bounded turn carries no
plan permission at all. Plan mutation reaches a turn exclusively through the dialogue
loop, which adds it when the dialogue is anchored on a plan. Plan creation is not
mutation: it stays in the dialogue head, so a developer may request a plan at any
moment of a dialogue. Every turn uses the strict structured output protocol
(`response_format: json_schema`), so all operational commands are typed properties
within the strict schema, guaranteeing deterministic constrained decoding without
protocol discrepancies.

An Architect synthesis declares its own tree in `control/refine.rs`. Its surface is
the architect research profile — research, plan creation, plan update, the
scratchpad, and the abort escape hatch — with no file-edit capability and no
toolchain verification, because a synthesis produces a horizon rather than a
patch. The escape hatch travels with the profile because a synthesis runs
unattended exactly as an isolated step does: a condition that only the
developer can resolve on the host system cannot be improvised or downgraded into a
blocked answer. It renders no session history,
exactly as an isolated step does: neither loop continues the developer's
dialogue, so neither prompt carries the `<history>` block. Rendering is not
recording: a step journals the two-sided card of its own event (§3.2.8), and a
synthesis is recorded by the caller that started it, because only that caller
knows the projection it produced. The loop is what puts the synthesis on the
machine as every other turn: the structured Architect contour is a policy layer
over the shared turn kernel (§3.3.9), so it inherits the rounds, the prompt
head, the discovery journal, the corrective budget of an empty answer, and the
transport retries instead of re-implementing them.

The loop also owns the brief of its contour. It hands the assembly a typed
`TurnBrief` — the kind of the contour, the role the model fills, a short
statement of the paradigm, and the task — so the prompt renders what the loop
declared instead of selecting a text by a boolean flag. The flag survives only
where it governs behaviour the prompt does not own: hiding the active plan, the
dialogue session identifier, and the layout choice. The mode-specific rules of
the prompt are gated on the typed kind the brief carries, never on a heuristic
that infers a regime from the shape of a surface.

### 3.2.2 TurnPhase

The five phases are surfaced to the interface as turn-phase events:

| Phase | Repeats | Meaning |
| --- | --- | --- |
| `Research` | yes, bounded | widen the working set until contracts are known |
| `Proposal` | once per model interaction | produce minimal coherent edits |
| `Staging` | yes, bounded | validate edits against a virtual tree |
| `Verification` | once per applied proposal | run the project toolchain |
| `Healing` | yes, bounded | repair what verification rejected |

Research, Staging, and Healing carry a round counter and a maximum, because they
repeat; Proposal is emitted for every model interaction and Verification runs once
per applied proposal.

### 3.2.3 Capabilities and commands

A protocol declares what it supports as a set of capabilities — text message, file
edits, research, internal documentation requests, external project documentation
requests, toolchain verification, plan creation, plan update, scratchpad, terminal
signals (`commit_message`, `error_message`, `reason`), blocked, abort, project custom
tools. A capability expression is a typed tree of those capabilities. A control loop
declares its tree through its own `run_*_loop` function, and intersecting a protocol
surface with that tree is the only sanctioned way to narrow a protocol. The same tree
renders the response schema and decodes the round, so a capability is advertised
exactly when it is decoded, and the label of an operational branch is data the
contour sets rather than a value sniffed from the leaves.

The `abort` capability closes a turn unconditionally when the remaining work
needs an action only the developer can perform on the host system, such as
installing a package or starting a service. The coder ladder never retries such a
turn, because no stronger tier can supply a capability the host lacks. Both
escape hatches are the property of the autonomous step contour: a dialogue
declares neither `blocked` nor `abort` and carries no step-termination signal in
any of its branches, because an unattended step is the only contour that has to
report a condition its own turn cannot resolve.

All commands described in this chapter are typed operations within the strict JSON Schema,
guaranteeing that the capability surface is enforced at the grammar sampling level.
Each capability belongs to a phase (`Research`, `Execution`, `Verification`,
`Outcome`). Model output is decoded into a normalized command list — text, edit,
context request, repository search, documentation request, verification, plan,
scratchpad, a terminal signal, blocked, protocol error — so no consumer ever
inspects raw markup.

Each atomic `LlmCapability` maps directly in `capabilities/schema.rs`, `capabilities/decode.rs`, and `capabilities/rules.rs` to its `schemars`-backed schema fragment (`capability_schema`), its tree-walking decoder (`decode_capability_expr`), and its contract rule and instruction (`contract_rule`, `structured_instruction`). Because schema generation, prompt composition, and decoding all walk the same `CapabilityExpr` tree under `PromptRenderContext`, the prompt can never advertise a command the decoder rejects, and a disabled capability leaves no trace in either the prompt or the command stream. The strict schema is the single source of the response structure — field names, required-ness, enum values, and the descriptions it carries from the doc comments of the command types — while the prompt text states the direction of the work: the role, the discipline, the semantics of `blocked` and `abort`, and the order of the phases. A rule therefore never restates a structural requirement the schema already declares, and the instruction list is rendered from the same tree, so a narrowed surface withholds a property from the instructions exactly as it withholds it from the schema.

Each capability also declares how its output reaches the harness. `LiveStreaming`
marks the contours consumed while tokens arrive — conversational text and file
edits, which the interface renders as the model generates. `RoundDeferred` marks
everything that only becomes meaningful once the round finishes — research,
documentation, verification, plans, scratchpad, outcome, blockers, and project tool payloads.
declared property instead of matching capability names, so delivery stays
orthogonal to phase: the execution phase carries both.

A capability surface splits into one invariant ambient channel and mutually
exclusive operational branches (`research`, `edit`, `plan`, `review`). The turn scratchpad
accompanies every round, so it belongs to no branch. The conversational channel belongs to
the branches instead: every branch declares the single `message` field of the response, so a
wire carries exactly one `message` property because exactly one branch is selected. A
discovery or planning round reports through a short description of the action it performs,
while a round that answers or changes the tree carries the full explanation. The language of
the answer is not a channel either: the shared contract states it once, as an unconditional
rule that user-facing output follows the language of the request.
The discovery commands form the `research` branch, code modifications form the `edit` branch,
and plan mutations form the `plan` branch, while a dialogue anchored on a plan wraps both
plan capabilities in one `plan_action` union whose `kind` values are `create` and `modify`,
so `plan_create` and `plan_update` never share one level. One round follows exactly one
branch: it discovers, patches, or plans, never multiple in the same round.

The split is enforced grammatically via the `action` union envelope whose constant `kind`
discriminator names the branch a response follows, so a research round contains no `changes`
field at all, and an edit round contains no research requests.
### 3.2.4 Tool Categories: Terminal vs. Diagnostic Inspection

Tools and operations accessible to the model are partitioned into three distinct
functional categories that govern their lifecycle within the bounded turn:
| Tool Category | Operations / Fields | Lifecycle Effect | Terminates Turn? |
| --- | --- | --- | --- |
| **Perception (Research)** | `context_requests`, `repo_searches`, `dir_lists`, `repo_dirs`, `repo_maps`, `tauqe_doc_requests`, `outer_doc_requests` | Widens context or provides authoritative reference | **No.** Triggers next Research round. |
| **Inspection (Diagnostic Execution)** | Declared project commands (`custom_tools`, e.g. integration tests, linters, benchmarks) | Executes isolated CLI process; streams stdout/stderr back into the turn journal | **No.** Triggers next Proposal round for model reasoning. |
| **Mutation (Terminal Outcome)** | `changes`, flat terminal fields (`commit_message`, `message`, `error_message`, `reason`), `blocked`, `abort` | Commits patches to virtual staging or signals turn conclusion | **Yes.** Closes the turn upon verification and Git commit. |
### 3.2.5 Terminal outcomes

Four terminal outcomes close a turn, carried as flat fields on the response or its active branch:

- success — the objective was implemented, staged, and committed under an explicit
  verification verdict;
- error — an insurmountable contradiction or unresolvable failure;
- blocked — a deliberate semantic assertion that the step's preconditions cannot be
  satisfied as stated;
- skip — the finding was already addressed or is not reproducible.

The branch a round selects decides which payload its status carries: the branch
that applied a change set closes with `commit_message`, while a dialogue closes
with the `message` of its own branch, while a failure reports through `error_message`,
so no status names a field the selected branch does not declare.
A blocked outcome is not a failure to retry: the transaction is rolled back, no
patch retry or healing cycle runs, and the owning supervisor may replan instead of
repeating.

Every terminal status carries its own result: `success` carries the
`commit_message` of its change set, `error` carries its `error_message`, `skip` carries
its reason. The decoders substitute a deterministic payload for `error` and
`skip`, because for those two the status already means something. `success`
cannot be completed that way — only the model can say what the turn achieved —
so a payload-free success is a protocol defect rather than a result, and it
cannot close a turn.

The field is named for its consumer, not for the moment it is written.
`commit_message` is the Git commit message of the change set, so it is an
English Conventional Commits header — `type(scope): description`, imperative
mood, lower case, no trailing period, at most 72 characters — whatever language
the conversation uses. It travels as the flat `commit_message` field on the active
branch, because the field is named for the commit (§6): the harness aligns the wire
field with the standard it feeds instead of inventing a second one. The language
obligation therefore splits:
`message` answers the developer in the language of the dialogue, and
`commit_message` is an English machine contract that the language of the dialogue never reaches.

### 3.2.6 Result-Bearing Elements and Auxiliary Channels

A round is judged by what the harness can act upon, not by how much the model
wrote. One classification decides it:

| Channel | Elements | Advances the turn? |
| --- | --- | --- |
| Result-bearing | conversational text, file edits, research requests, plan updates, a verification request, a project command invocation, a protocol error, a terminal outcome carrying its payload, a blocked report, an abort | yes |
| Auxiliary | scratchpad notes | no |

An auxiliary channel is memory, not work: it records what the model is thinking.
It never widens the context, never stages a byte, and therefore never makes a
round actionable on its own. A round carrying only auxiliary material is as
empty as a round carrying nothing at all.

### 3.2.7 ApplyPolicy

`ApplyPolicy` isolates *how* validated edits reach disk from the state machine. It
exposes four operations: staging, apply temporary, apply final, and rollback. Two
implementations cover the supported transaction models: a direct policy that writes
straight to disk for naive mode and tests, and a Git policy that checkpoints,
commits a temporary step, and finalizes or rolls back.

### 3.2.8 The record, its status, and the three stores

A session keeps three separate stores, and the harness never treats one as a
stand-in for another:

- **The model history** — `.tauqe/history.jsonl`, an append-only JSON-lines
  journal of turns, responses, reviews, and compactions. It is the sole source of
  the `<history>` block, so what is absent from it is absent from the model's
  dialogue memory.
- **The developer chronicle** — the History view. It renders the journal together
  with the record of the turn that is still running and the record of the turn
  that was rolled back, neither of which the journal holds for long.
- **The editor prompt history** — the prompt buffer's own recall list, kept in the
  developer's private state directory (`$XDG_STATE_HOME/tauqe/prompts.history`,
  falling back to `$HOME/.local/state/tauqe/prompts.history`) and never inside a
  checkout. It stores submitted prompts only, newest last, deduplicated and capped,
  and is written with owner-only permissions. It is a typing convenience,
  not a dialogue: it is never rendered into a prompt and never reaches the model.

The journal answers what the harness and the model exchanged; the editor file
answers what the developer typed. Because they are different files, a failed turn
cannot lose a prompt from the editor and cannot leak it into the model's context.

A dialogue turn and an isolated step journal different cards of the same
event. A dialogue turn records what the developer asked and what the model
answered, which is the material the next dialogue reads. An isolated step
records a **two-sided step card**: the structured execution prompt that opened
it — the plan, the step identifier, its title, and its target files — and a
compact result that closed it, which carries the status of the step, the commit
it produced, the files it changed, and the first line of the reason or summary
it reported. The full reply of the model is never recorded for a step: the
chronicle needs the outcome of the step, not a transcript of it. Because a batch
may execute many steps, a step turn passes through the same budgeted compaction
as a dialogue (`history.budget_tokens`), so the journal is bounded by the
configured budget rather than by the length of the batch.

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
   [Research] --requests--> context, search, docs, outlines
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

### 3.3.1 Research

Research widens the working set until the model reasons about contracts rather
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
the context block: they accumulate in a turn-scoped research journal that extends
the user message and survives the whole turn, so a staging retry or a healing round
keeps the knowledge research already paid for.

A step arrives with more than its files. The external documents its leaf declares in
`outer_docs` are materialized and mounted into the read-only OuterDocs layer before the
turn starts, under the same `[context].token_budget` ceiling. A trust refusal ends the step
before any model work, because no round can widen a declaration that lives outside the
repository; a delivery failure travels as a diagnostic of the step prompt, and the model
may request the same document again in a later round.

A round reports whether it made progress. That single question lets the machine
distinguish a productive round from a dead end without inspecting individual request
types, and it is bounded by `context.max_research_rounds`.

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
A round that violates the branch exclusion — a change set together with a
discovery request — is a protocol defect rather than a command, and the kernel
resolves it without losing work. The change set proceeds to staging, because file
content the model already produced is work the turn already paid for, while the
discovery half is reported back as the defect to correct in a later round.
Precedence belongs to the change set: a proposal about file content is checked by
staging and verification, whereas a request for more material is checked by
nothing. Discarding the proposal instead would let a turn that produced code reach
the boundary looking like a turn that produced nothing, and would report as
silence exactly the round the machine refuses to accept as silence.

### 3.3.3 Silence Is Re-Asked, Never Accepted

The invariant is enforced once, immediately after a round is decoded and before
any phase consumes it.

A round whose buckets are all empty is a non-answer rather than a reply, so the
machine neither fails it nor records it. It issues a corrective request that
states the contract the round violated — the model owes at least one
result-bearing element, and a status tag alone does not discharge that debt — and
lets the model answer again. The corrective loop is bounded like every other
loop of the turn, so a re-ask can never grow into an open conversation.

Exhausting that budget is a failure, not a conclusion: the turn terminates with a
visible error that names the empty round, so the interface reports what happened,
the coder ladder may escalate to a stronger tier, and a batch supervisor records
a failed step. Three properties follow.

1. **A written answer is required.** Any result-bearing element advances the turn:
   text, an edit, a discovery request, a plan update, a verification request, a
   project command invocation, a blocked report, an abort, or a terminal outcome
   that carries its payload.
2. **A status tag is a claim, not a result.** A `success` without a stated
   `commit_message` is decoded into a command and then fails the same emptiness
   test as a blank round, so the model cannot close a turn by asserting that it
   closed.
3. **No phase observes silence.** Research, Staging, and Healing run only on
   rounds that already passed the test, so a later phase never has to distinguish
   "nothing to do" from "nothing was said".

### 3.3.4 Project Command Execution & Feedback Loop

When a proposal invokes a declared project command (or a recipe from the external
task runner's `tauqe` group) without code modifications, the harness treats it as a
diagnostic investigation step rather than a terminal completion.

1. **Validation & Isolation:** The invocation is checked against the declared
   allowlist. It runs in its own process group with strict timeout enforcement
   and literal arguments.
2. **Structured Journal Feedback:** The execution outcome (`exit status`, `stdout`,
   and `stderr`, truncated to safe scalar boundaries) is recorded into the
   turn-scoped `TurnResearchJournal` inside a structured `<project_command_feedback>`
   block.
3. **Iterative Answer Round:** Once the invocation completes, the FSM requests an
   answer round from the model with the updated journal. The
   model perceives the real command output, evaluates test failures or runtime
   panics, and either proposes corrective code edits, invokes another inspection tool,
or emits an explicit terminal outcome (`success` / `blocked`).
4. **Shared Round Budget:** Diagnostic tool executions consume the turn's discovery
   round budget (`context.max_discovery_rounds`), preventing unbounded agentic loops
   while ensuring models have sufficient feedback to diagnose complex failures.

### 3.3.5 Patch retry

Matching is deterministic and tolerant of what models actually produce: indentation
shifts, tab versus space variance, and trailing whitespace are normalized before a
hunk is applied. What cannot be tolerated — a missing anchor, an ambiguous anchor,
or a stale surrounding block — is reported precisely.

A staging failure never aborts the turn and never discards work. Already staged
files stay staged, the failing files and their matching errors are formatted into a
report, and the model is asked to re-emit only the failing hunks. The repair log is
append-only, so attempt *n* carries the history of attempts `1..n` instead of
replacing it, and the loop is bounded by `repl.max_retries`.

### 3.3.6 Verification

Once staging converges, edits are applied atomically to disk and the declared
project lifecycle runs in three phases: in-memory formatting of staged source
text, disk-side post-hooks after the write, and the verification command, when the
project declares one. The suite reports typed diagnostics — file, line, column,
severity, message — instead of a compiler transcript, which is what lets the
healing round reason about a location instead of paying for ANSI escapes in its
context window.

Verification is a declared capability, not a universal barrier. `[project.verify]`
is optional, so a repository may legitimately describe no verification at all: the
turn then runs no step, records the explicit `not_configured` verdict, and still
commits its change set, because the project stated its own choice. No verdict is
ever fabricated: a change set is reported as `passed`, `failed`,
`not_configured`, or `cancelled`, and only a real run produces the first two. Only
a declared gate that failed enters the healing loop.

A verification request names its target (all, check, test, or a lint pass); the
declared command is authoritative for every target, so a request can never weaken
the gate the project declared. A repository that declares nothing runs nothing and
skips the gate transparently instead of failing.

### 3.3.7 Healing and intra-role escalation

A failed check enters a targeted healing round: the diagnostics are formatted into a
clean report, missing error paths are staged into the context, and the prompt goes to
the active role. Healing repeats while `repl.max_retries` lasts, and it stays
inside one attempt of the current coder tier.

When it does not converge, the turn reports its outcome and stops there. Retry and
escalation belong to the step supervisor, not to the turn: the failed transaction is
rolled back to the step's pre-turn checkpoint, the transient research context is
pruned, and the step is re-asked on the same coder tier until
`repl.max_tier_attempts` content attempts are spent. Only an exhausted tier
advances one rung of the coder ladder — deeper reasoning on the same model first, a
stronger coder afterwards — and that rung starts with a fresh budget of its own.
Escalation never leaves the coder role and saturates on the final rung, where the
step ends as a reported failure; *Model Roles and the Coder Ladder* owns that
boundary. A blocked report consumes neither an attempt nor a rung and bypasses the
ladder entirely.

### 3.3.8 Finalize or roll back

A converged turn is finalized as one isolated commit. It is authored by the
developer, its subject is the `commit_message` the success outcome declared — the
harness never assembles one of its own — and it is marked with a co-author
trailer so the harness can recognize its own work. The same commit carries the
explicit verdict of its verification run: `passed` when the declared gate
accepted the tree, `not_configured` when the project declares no lifecycle.
Intermediate step commits exist only so healing can converge phase by phase and
are squashed away, so history never exposes the healing noise of a recovered turn.

Anything else — an unresolved failure, an exhausted ladder, a cancellation, a
blocked report — restores the working tree to the checkpoint taken before the turn
and discards the temporary commits. If the process dies mid-turn, recovery reads the
transaction marker written when the checkpoint was opened and rewinds only when
every commit in the recorded range carries the harness trailer; a foreign commit in
that range makes recovery refuse and report instead of rewriting history.

**The plan payload of the turn settles at the same boundary.** Finishing a turn no
longer applies plan mutations as a side effect: every terminal path runs the accumulated
payload through one settlement phase before the outcome is packed. A qualifying result —
a validated answer or an applied change set — applies the mutations and collects the
warnings the status barrier produced, while a non-qualifying result — a semantic block,
an abort, a terminal error, a patch failure, an edit proposal that applied nothing, an
empty round, or an exceeded context budget — abandons the payload with a single
`Abandoned` rejection that names the reason. A cancelled turn is the one intentional
silence. A payload the planning contract itself refuses buys exactly one corrective
exchange, and only the payload of that exchange is settled; a refusal the turn regime
forbids, or a payload that already left the turn, buys none, because repeating the turn
cannot change the verdict. The contract is documented in *Local Plans* §3.13.
**A plan discussion validates its payload before that boundary.** Applying a
refusal only at settlement would report it after the turn had closed, when the
dialogue could no longer repair it. A plan discussion therefore validates its
payload with a non-persisting probe of plan storage: the probe runs the very
mutation cycle the application runs — identity resolution, the status barrier,
and the structural barrier of the write path — against an in-memory snapshot of
the stored plans, under the same storage lock and off the async executor thread,
and it writes neither a plan document nor the active-plan marker. Because both
contracts run one barrier, the verdict of the probe is the verdict of the
application it predicts. A clean verdict costs nothing and leaves settlement as
the single application; a refusal buys the corrective exchange described above,
in which the refused mutations leave the turn state so the corrected document
replaces them instead of being accumulated beside them; and a payload that is
still refused when that round is spent travels to settlement, which refuses it
and reports it through the ordinary diagnostic surfaces. A repair of the barrier
— a status it normalized instead of refusing — never starts the round, because
the mutation was applied. The phase belongs to the plan-discussion regime alone:
a round that carries file edits beside a payload keeps the staging contour and
settles its payload after verification, and the interactive, review-discussion,
and isolated-step regimes keep their current semantics.

**The session journal is transactional in the same way.** The prompt is appended
to `.tauqe/history.jsonl` and published as `Active` the moment the turn starts, so
the running task is visible before any result exists. A turn that produces a
result appends it and publishes both records as `Committed`. The rule holds for
every turn the kernel journals: a dialogue turn and an isolated step alike open
their card before the first token and keep both cards only when the turn
produced a result, so a batch of steps is a sequence of two-sided cards rather
than an unrecorded stretch of work.

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

### 3.3.9 The five pipeline layers
The machine is decomposed so that each layer owns one reason to change, which is
what makes it testable without a network:

| Layer | Responsibility |
| --- | --- |
| Execution mode | lifecycle of one turn or of a supervised queue, role escalation, cost accounting |
| Task and loop | the phase cycle itself, with steps that declare whether they repeat |
| Prompt protocol | prompt assembly, capability declaration, decoding into commands |
| Apply policy | staging, temporary commit, finalize, rollback |
| Provider | transport, streaming, cancellation; a retrying decorator wraps every provider |

The cycle itself is one reusable kernel (`TurnKernel`). It carries the rounds,
the prompt assembly of each round, the research journal, the corrective budget
of an empty answer, and the observation of cancellation, so a contour supplies
only the prompt protocol and the policy of its own payload. The Architect
synthesis is exactly that: the same kernel driven by `PlanRefinePromptProtocol`
and the structured Architect contour, which adds Schema Healing of an unparsable
answer, escalation along the architect ladder, and typed terminal outcomes — a
synthesized horizon, a blocker, an exceeded context budget, or a cancellation.

A scripted provider satisfies this contract without a network call, so multi-round
scenarios — discovery, a compiler failure, a successful healing patch, a
cancellation — run deterministically in milliseconds.

Batch supervision sits above single turns. It owns the queue, synchronizes entity
statuses, accumulates usage, and stops in two distinct ways: a hard cancellation
aborts the running member immediately, while a soft stop is observed strictly
between members, letting the current turn finish its verification and commit before
the queue halts. Hard cancellation wins when both are pending, and the two modes
surface as distinct terminal statuses.

### 3.3.10 Prompt layout policies across rounds
Every round re-sends the whole assembled prompt, and providers cache the longest
unchanged token prefix. Turning that prompt into a message sequence is a policy
behind one trait, so the same rendered material can become different sequences:

- `SemanticLayout` groups the material by kind: one system message carrying the
  contract, the outline, the whole `<context>` block, the active plan, the `<history>` block, and the scratchpad.
- `ChronologicalLayout` turns the same material into an append-only transcript: a
  byte-stable head, one message per history record with the context files that
  record already observed placed right after it, and a volatile tail.

Both policies place the whole contract in the byte-stable head: the shared trunk and every rule the loop's capability tree authorizes, since that tree never changes between the rounds of one turn. Round diagnostics (patch retry reports, verification failures) travel as append-only extensions of the user message instead, so a retry never cuts the cached prefix a second time.

The head renders the whole contract of the loop's tree — its capability list, the shared trunk, and the rules that tree alone states — so every rule and every schema property derive from the tree the loop function returns, and the head is byte-identical for every round of one turn. What a loop adds reaches that same head: a dialogue anchored on a plan declares plan mutation there, while a free dialogue and a review dialogue declare exactly the interactive tree. The invariant that a capability is advertised exactly when it is decoded survives, because the head, the schema, and the decoder are all read from that one tree — an interactive turn intersects the advertised set with its own tree and rejects plan mutation even when its protocol declares it.

| Material | Semantic | Chronological |
| --- | --- | --- |
| Contract, instructions, metadata | system head | system head |
| Repository outline | system head | system head |
| Context files | one `<context>` block | `<context>` groups in the transcript |
| History | one `<history>` block | one message per record |
| Active plan | system tail | trailing user message |
| Context manifest | absent | trailing user message |
| Scratchpad | system tail | trailing user message |

The head carries only what is identical from round to round. The repository map is
generated once per turn and frozen, and it stays in the head under both policies:
exploration and planning dominate a turn, and the map is the largest block, so
moving it into the tail would pay a full reprocessing pass on every round. The price
is documented rather than hidden — a map regenerated between turns excludes the
files that entered the context, so the head itself changes and the whole tail is
invalidated.

Growth inside a turn stays append-only. Research appends an element at the tail of
the context block, or appends the file after the last record of the transcript, and
a promotion rewrites a single `access` attribute in place. The chronological order
comes from the insertion watermark every context entry carries: the policy
compares that watermark against the `context_watermark` each turn record
persisted and emits the file after the prompt record of the turn during which
the watermark was allocated. That slot is stable for the whole session, so the
next turn appends the response of the discovering turn and its own prompt behind
the arrival instead of repositioning it. A mid-turn arrival is therefore an
append-only event, never a fifth invalidation boundary beside the four documented
ones; a file no watermark-bearing turn bounds falls back to the recorded starting
context, and a file that carries no watermark at all closes the transcript.

The tail carries everything that legitimately changes: the active plan, the context manifest of the chronological policy — paths and access, never content — and the turn scratchpad, while round feedback (research findings, patch retry reports, verification diagnostics) is appended after the user message.
The user message keeps one fixed shape: task first, accumulated journal next, phase
diagnostics last. A staging retry therefore differs from its own earlier attempt
only in the trailing diagnostics.

One typed resolution decides which policy a turn runs under: a declared layout wins,
otherwise an interactive REPL turn takes the chronological transcript and a
discussion or an isolated step takes the semantic order, because those turns carry
no growing history to interleave.
### 3.3.11 Transport resilience and output ceiling

Transport failures that say nothing about the request — a timeout, a reset
connection, a rate-limit or server-error status, a stream that breaks before its
terminator — are retried by a decorator around every provider, with exponential
backoff and jitter, bounded attempts, and a ceiling on any single pause. Fatal
statuses are reported at once and consume no attempt. Every second of a pause is
reported as a connection-retry event so the interface shows a live countdown, and
the pause races the cancellation signal so an interrupt aborts it immediately.

A failure before any token is transparent; a failure after partial output resets the
accumulated fragments, so two attempts are never stitched into one answer.

Those retries are transport-level and neutral to the tier attempt budget. A turn that
ends with no result because its stream broke, its request timed out, or the provider
answered with a rate-limit or server-error status is classified as an infrastructure
failure: the decorator already answered it, no content attempt of the current tier is
consumed, and no rung of the ladder is advanced. Only a turn that produced a result
the harness judged as a failure spends an attempt.

A retry policy guards a transport that fails, not one that keeps working. A model
caught in a reasoning loop streams indefinitely, so one attempt's cumulative output
is metered against a hard ceiling. Crossing it aborts the attempt, discards the
partial output, and reports a terminal error that is deliberately not retried: the
same request would reproduce the same loop.

Cancellation reaches the completion path as well. The answer is already delivered,
so a phase that runs after the last delta is optional work: the generation-stats
fetch that resolves the reported cost races the operation's cancellation signal and
carries its own short per-request timeout. A cancelled, slow, or unreachable
endpoint costs the turn the enrichment and nothing else, because a best-effort
fetch can never be the reason a finished turn waits.
### 3.3.12 The process container

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

Cancellation is a property of the operation, not of the chat request. Every phase
an operation creates inherits it: the pre-flight verification gate runs while the
slot is registered, reads the same channel, and tears its toolchain subprocess
group down; the provider request and its completion path race the signal at every
await; and every task spawned under the slot keeps the registration — with its
abort handle — alive while the task unwinds. A supervisor that dropped the future
of a running step would break that last clause, so a queue unwinds through the
member rather than around it. A phase that cannot observe the signal on its own is
terminated by the deferred-abort watchdog, which defers while the operation still
holds an open Git transaction and aborts once that transaction has closed. The
abort is never treated as synchronous death: a task terminates only at its next
suspension point, so every abort the daemon raises is followed by a confirmation of
exit under one task-exit deadline, whether the task is reaped by the watchdog that
followed the turn or by the last-resort watchdog that targets the registered
operation directly. A watchdog that reaps a task it did not spawn resolves that
deadline where it is spawned and applies it inside itself. An expired confirmation
releases the registration with a diagnostic that names the orphan in the journal,
because a registration parked on a task that may never reach a suspension point
refuses every later request until the process exits. The orphan keeps its abort
delivered: it dies at its next suspension point and releases its session lock only
then. The raised signal classifies that termination, never the death of the task:
the turn ends as cancelled, so the client that asked for the cancellation is not
handed a spurious failure for the work it stopped.
### 3.3.13 Guards and budgets

| Guard | Setting |
| --- | --- |
| Research rounds per turn | `context.max_research_rounds` |
| Context token budget | `context.token_budget` |
| Repository outline budget | `context.repomap_token_budget` |
| Patch retries and healing rounds | `repl.max_retries` |
| Content attempts per coder tier | `repl.max_tier_attempts` |
| Generation attempts per architect rung | `attempts` on the architect entry (2 by default) |
| Automated replans | `repl.max_replans` |
| Outlines per research round | fixed request cap |
No operational limit is hardcoded, and no budget silently discards work: a breached
budget halts with a diagnostic that asks for a narrower scope, so the developer
learns what to decompose instead of watching a turn fail anonymously.

---

## 3.4 Boundaries & Anti-Patterns ("What it is NOT")

- **It is not an agent loop.** The machine never asks the model what to do next; it
  asks the model to do the current step.
- **It is not a single-shot completion.** Research, retry, and healing rounds are
  first-class states, not error handling bolted onto a request.
- **It does not own project knowledge.** Formatters, post-hooks, and verification
  commands belong to the `[project]` declaration; the machine only decides when they
  run.
- **It does not retry forever.** Every loop has a configured budget and a saturated
  terminal state.
- **It does not infer the mode from text.** The mode is a typed parameter of the loop
  that runs the turn; a prompt that merely resembles an internal heading cannot
  change the regime, and no boolean flag that resembles one selects the brief.
- **It does not create a plan on its own initiative.** A broad request produces an
  offer inside the answer, never a plan document; storage changes only after the
  developer confirms.
- **It does not hide failure.** An exhausted budget, a blocked step, and a rolled
  back turn are all reported as distinct, named outcomes.
- **It does not treat silence as a conclusion.** An auxiliary channel — the
  scratchpad, the language declaration — grants no right to be silent, and a
  status without its payload cannot close a turn.
- **It cannot delegate its verdict to the schema.** JSON Schema can require a
  field, not a non-empty string, so the schema and the prompt state the payload
  obligation while the authoritative test runs after decoding, on the typed
  commands. A schema never decides by itself whether a turn is finished.
- **It does not let one round both discover and patch.** A capability surface
  splits into invariant ambient channels and mutually exclusive operational
  branches, so a round that carries a change set carries no discovery request. A
  mixed round is a protocol defect, and the kernel resolves it in favor of the
  change set instead of discarding work the turn already paid for.
- **It does not let the dialogue language reach the commit.** `commit_message` is
  an English Conventional Commits header whatever language the turn answered in;
  the language of the answer follows the request, never the Git history.
- **It does not keep an uncommitted prompt in the model context.** A turn that
  produced no result leaves no record behind, so the model never reads a task whose
  outcome it cannot see.
- **It does not infer a record's status from its text.** Active, committed, and
  rolled back are typed states, never a phrase inside a message.
- **It does not confuse prompt recall with dialogue memory.** The editor list is a
  typing convenience backed by its own file, not a second transcript.
- **It does not fabricate a verification verdict.** A project that declares no
  lifecycle states its own choice, so the change set is reported as unverified
  instead of being dressed up as a passed gate.
- **An attempt on a tier is not an escalation.** Re-asking the same tier is finite
  and configured by `repl.max_tier_attempts`; only an exhausted tier advances the
  ladder, and a saturated ladder ends the step instead of retrying it again.
- **A broken connection does not burn a rung.** Transport failures are answered by
  the network layer and consume no content attempt, so a degraded link never promotes
  a model that has not failed on content.
---

## 3.5 Related Chapters

- [Philosophy: The Semantic Harness](semantic-harness.md) — the invariants this
  machine enforces.
- [Model Roles and the Coder Ladder](model-roles.md) — intra-role escalation.
- [Turn Scratchpad](turn-scratchpad.md) — the volatile tail of the prompt.
- [The Context Model](../ch02-perception/context-model.md) — the working set the
  rendered context block carries.
- [The TauqeProjectProtocol](../ch03-project/tauqe-project-protocol.md) — what
  verification delegates to.
- [The Three-Phase Modification Lifecycle](../ch03-project/three-phase-lifecycle.md) —
  where formatting, post-hooks, and checks run.
- [Structured Diagnostics and Verification Healing](../ch03-project/verification-healing.md) —
  the shape of a failing check and the loop that repairs it.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — checkpoints,
  step commits, and crash recovery.
