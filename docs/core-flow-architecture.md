# Execution architecture: executor and workflows

The REPL and isolated plan and review steps share a bounded edit turn.
Architect research and one-off structured requests use the same provider
transport without entering an edit turn. The dependency direction is
`server → flow → core → tools`; `protocol` is the RPC contract with the TUI.
The boundaries below are about who makes a decision, not where a type happens
to be defined or whether an object is mutable.

## Two kinds of decisions

**Core implements technical execution.** Before a particular workflow runs,
the application binds the configured provider, model transport, edit format,
application policy, verification machinery, request transport, and their
configuration. A workflow cannot silently replace these dependencies. Core,
under `PromptExecutor`, prepares the available-file catalog, repository outline,
external-document declarations, and contents of admitted files. A catalog entry
does not load an external document's text. Core provides the mechanisms for
constructing coherent prompts, streaming requests,
decoding results, and executing admitted technical operations. It does not
decide which scenario to run or what the scenario should do with a result.

**Flow owns workflows.** A workflow decides its task, context, available model
capabilities, whether to include history, which request comes next, how to
interpret an outcome, and when to persist or publish it. `ContextManager`,
`CapabilityTree`, and other changing inputs belong to the workflow even if
their types and deterministic operations are implemented in core. A workflow
may change them between requests; it must not mutate an in-flight request.
Flow also owns operation admission, cancellation policy, scenario transitions,
and durable plan or review decisions. Server translates RPC to application
operations and projects their events; it does not run workflows.

## PromptExecutor

The application factory binds the technical environment and gives a workflow
a `PromptExecutor`. This is the common execution capability for a model
request, not a workflow in disguise. The workflow supplies the current task,
context, optional history, capabilities or other typed request contract, and
semantic model selection. The executor freezes a **coherent snapshot** for
each request: instructions, advertised capabilities, response schema, stream
filter, and decoder must agree with the same grant. Changing or revoking a
capability takes effect on the next request, including a continuation; it
cannot leave an old decoder or instruction in place.

An isolated step prepares its model, effort, provider, policy and executor from
one operation-bound environment. The prepared attempt carries that binding and
the repository root, not a second configuration or a second model selection;
step publication reads the model and effort from the prepared binding. Reloading
configuration after admission cannot alter an in-flight request.

The executor admits the complete serialized provider input under
`request.max_input_tokens`: instructions, task, history, files, external
documents, repository outline, feedback, scratchpad, response schema, and
adapter framing all count. The estimate is `ceil(serialized_request_bytes / 4)`,
not an exact provider token count. Diagnostic components are rounded separately
and need not sum to the total. Overflow refuses the request before a provider
call or round charge without discarding accepted materials.

The executor's single operational method, `execute`, accepts semantic inputs
and returns `Result<Vec<LlmCommand>, ExecutionError>`. It owns the task's private
transcript and scratchpad, file snapshots, request construction, caching,
streaming, usage and decoding. A valid response with no commands is distinct
from a blank provider stream; cancellation, incomplete streams, transport
errors and protocol defects cannot execute partially decoded commands. A
workflow never assembles a provider request or reads raw provider JSON. The
executor never executes commands, schedules the next request, applies a plan
decision, or chooses a scenario-specific fallback. The factory resolves technical dependencies from
the operation's frozen environment; changing the selected model rung is
workflow intent, while resolving its provider and effort is factory work.

The interface must support both one-off structured requests and requests
inside a bounded turn without requiring every workflow to adopt the turn
lifecycle. Different typed request contracts are legitimate; different
provider paths and inconsistent rules for freezing inputs are not. The
executor can maintain a request cache, but it does not own the workflow's
context, history, permissions, round budget, or continuation. Flow supplies
intent and staging facts, including created paths; core assembles admitted
materials and keeps original and current file projections inside the task's
executor. Model rebinding within a task preserves the conversation; an
independent task receives a fresh executor.

## Workflows compose workflows

Flow's execution logic is organized around explicit, reusable scenarios,
not around a large `Application` facade or a collection of provider wrappers:

```text
Application::ask → repl::lifecycle → run_technical_turn → TurnWorkflow
       └─ workflows::round::run_round ── PromptExecutor::execute ── provider

Application::{execute_plan, execute_plan_pipeline}
  └─ PlanResources → plan batch → PlanAttempts → StepResources

Application::execute_review
  └─ ReviewResources → review batch → ReviewSequence → StepResources

StepResources → StepAttempt → run_technical_turn → TurnWorkflow → workflows::round::run_round → PromptExecutor::execute

Application::synthesize_plan → plan synthesis → Architect contour
  └─ workflows::round::run_round → PromptExecutor::execute → shared request transport

Application::{preview_review, start_review, analyze_review} → review::{audit, analysis}
  └─ preview reads the audit window; audit and analysis use bound model requests

One-off structured request (Scribe, audit, critique) → StructuredExecution::run
  └─ core::execution::structured::execute → shared request transport
```

The interactive route includes plan creation/modification and review decisions
inside a discussion. Standalone plan and review scenarios keep their entry
points and reach the same bounded turn through an isolated step attempt.

| Owner | Lifetime and responsibility | Does not own |
| --- | --- | --- |
| `Application` | Public entry points, binding of session services, and interactive admission | Plan/review phases, next model request or batch verdict |
| Plan and review workflows | Operation admission, readiness, continuation, document decisions and terminal publication | Provider transport or shared edit-turn mechanics |
| Shared leaf queue and `StepResources` | Queue and attempt mechanics, step supervision and recovery | Plan or review status and batch policy |
| `workflows::repl::run` | Interpretation of one interactive technical result and awaited document preparation, correction and save | Provider transport or independent document workers |
| `run_technical_turn` / `TurnWorkflow` | Retained transaction settlement; technical rounds, staging/edit state and continuation | Plan/review persistence decisions |
| `workflows::round::run_round` | One request, admitted-call budget charge and common command handling through research mechanisms; passes remaining commands in order | Task lifecycle, retry policy or transcript |
| `PromptExecutor::execute` | One admitted request, private task transcript, snapshots, decoding and sanitized recovery registration | Command execution, workflow context, budget or continuation |
| Server | RPC decoding, error translation and event projection | Scenario execution |

The edit-turn routes compose three workflow layers between application admission
and the executor. The infrastructure factory binds collaborators; it does not
decide when a scenario continues.

The free function `workflows::round::run_round` passes the workflow's semantic
inputs to the separately supplied executor, charges the budget only for an
admitted request, and handles common commands through the existing research
mechanisms with explicit context, journal, frozen project configuration and
cancellation. It removes handled commands while preserving the order of those
left for the scenario. It creates no task or lifecycle, cannot access the
transcript, and neither retries nor decides whether to continue.
The executor projects files and feedback provisionally and admits the complete
request before committing either projection or charging a call. It publishes
the round start before provider deltas; transport retries remain within that
call. Only a completed, structurally valid decoded payload (including a valid
empty command result) can register an accepted round; blank streams, protocol
defects, incomplete streams and transport failures cannot. Registration occurs
without an await gap before the recovery record. It is not successful edit or
document persistence. The recovery reader retains sanitized rounds and bounded
feedback, never private prompts, file bodies or scratchpad; accepted round IDs
identify the final response's source even after patch repair.
`TurnWorkflow` composes rounds into one bounded unit: research, rejected-patch
retry, verification healing, budget, staging, and transaction
settlement have one explicit lifetime. REPL and isolated steps both enter
`run_technical_turn`, a concrete function that accepts turn data rather than
computation or registration callbacks. It retains the policy while computation runs and returns
one technical report with settled file facts and an optional continuation
containing the turn state and executor. If computation stops or panics, the
supervisor confirms its exit and settles the policy before returning; neither
scenario starts a file worker or performs separate Git recovery. Only the
computation is registered for forced abort. A retained operation hold keeps
admission closed through settlement even if the caller stops waiting; cleanup
checks the task identity before clearing its abort handle. Transaction settlement
is a separate function running in the supervisor, not an abortable computation.
The supervised
REPL task owns its `TurnState`, focus, document interpretation and corrective
continuations. After
the universal TURN settles successfully, REPL selects the plan or review branch
and awaits `apply_plan_commands` or `apply_review_commands` in the same task;
TURN does not know about document storage. Plan preparation retains accepted
mutations in memory across corrective rounds on the same transcript and budget.
Only payload defects prompt correction; storage failures and authorization
refusals do not. Review decisions have no corrective loop.
`repl::lifecycle::run` relays provisional events, then waits for the task outcome
and the settled file facts from the same technical report. It chooses
whether to record a completed response or an interruption before publishing the
terminal outcome. A normal answer is recorded once before `Finished`. A plan
storage refusal produces a diagnostic, not a correction request or a saved plan
notification for the refused write. Cancellation takes precedence without hiding
writes that already completed. The workflow flushes usage and publishes settled
writes before terminal publication, while still holding the operation. History
failures suppress `Result` and `Finished`, not an already finalized commit. If
Git finalization fails after verification, the accepted reply stays among the
rounds and the response records the Git diagnostic, not a commit or a rollback.
Document persistence is a final awaited blocking call, not a separate queue.
`PlanStorage::commit_prepared_batch` and `ReviewStorage::commit_prepared_decisions`
commit prepared documents and active markers through reversible file replacements.
Cancellation before final acceptance restores the original bytes; a failed
restoration is reported explicitly. A blocking filesystem call already underway
cannot be interrupted by dropping its Tokio waiter: wait for it to settle and
perform only required cleanup on cancellation, not further preparation or
saving. Once committed, a later abort or cancellation of terminal publication
does not undo the saved documents; the typed result carries them for publication.
Git transaction recovery for code edits remains independent of this boundary.
Plan and review entry points pass scoped resources, not `Application` or a
callback to it. The scenarios retain readiness, escalation, status changes and
persistence. An isolated step starts a fresh turn with its own grant and no
dialogue history. Other scenarios can use
a single round or a typed structured request without adopting the turn lifecycle.

Composition is about contracts, not inheritance or mandatory names for Rust
types. Each workflow must have recognizable inputs, outcomes, and ownership of
its lifecycle. `Application` admits and supervises scenarios; it should not
be where their decision logic is scattered. Moving files into directories
without consolidating these decisions does not establish this architecture.

## Reading the plan and review execution paths

The public `tauqe_flow::api::Application` methods are the same entry points
used by the RPC adapter. Both launches claim the exclusive operation before
resolving the requested work and running preflight. The claim stays held
through attempts, recovery, gaps between queue members, and terminal
publication. Hard cancellation takes precedence over soft stop; soft stop
lets active work settle without undoing verified commits.

### Plan execution

1. Start at `Application::execute_plan` or `Application::execute_plan_pipeline`
   in [`application/plan_batch.rs`](../crates/flow/src/application/plan_batch.rs).
   [`workflows/plan.rs`](../crates/flow/src/workflows/plan.rs) resolves the plan
   scope or the ordered list of plans against stored documents. The
   [pipeline](../crates/flow/src/workflows/plan/queue.rs) runs each plan under
   the same operation and advances only after completion without interruption.
2. [`workflows/plan/run.rs`](../crates/flow/src/workflows/plan/run.rs) chooses
   ready leaves using core's dependency rules and sends them through the shared
   leaf queue. [`PlanAttempts`](../crates/flow/src/workflows/plan/attempt.rs)
   prepares each isolated attempt. When the queue
   returns, the plan scenario decides whether to reload the saved scope and
   continue, request an Architect refinement and save the replacement horizon,
   or stop. An explicitly targeted step or subtree cannot trigger replanning
   of the rest of the plan.
3. After the attempt settles its Git transaction, the plan scenario requests
   the leaf status and any confirmed commit or blocker through `Documents`.
   At batch end it reloads the stored plan for publication, publishes the
   plan and list snapshots when available, then emits `PlanBatchFinished`.

### Review execution

1. Start at `Application::execute_review` in
   [`application/review_batch.rs`](../crates/flow/src/application/review_batch.rs).
   [`workflows/review.rs`](../crates/flow/src/workflows/review.rs) determines
   which findings or groups are eligible and builds the member queue. A
   group runs its own stored implementation horizon, not the plan scenario.
2. [`workflows/review/sequence.rs`](../crates/flow/src/workflows/review/sequence.rs)
   runs the member's leaves through the same queue and attempt machinery. The
   review sequence keeps one model publication and usage window across the member's
   leaves and retries. Settled leaf writes record group progress or a finding's
   confirmed commit through `Documents`; recording a commit does not mark a
   finding `Fixed`.
3. [`workflows/review/member.rs`](../crates/flow/src/workflows/review/member.rs)
   interprets the sequence stop. Completion requests closure; for a group,
   core checks the stored horizon before closing its findings. A partial run
   preserves earlier commits without declaring the member fixed. A local
   block or exhausted retry budget can leave the member unresolved while the
   batch continues; an infrastructure failure stops it. The workflow publishes
   a stored review snapshot after settling the member verdict. `ReviewResources`
   publishes `ReviewBatchFinished` from the settled batch report.

Both routes use `workflows::step::{queue, attempts}` for leaf sequencing and
retry mechanics. Both converge at
[`StepResources::execute_prepared_single_step`](../crates/flow/src/workflows/step/run.rs)
and the same `StepAttempt → TurnWorkflow → workflows::round::run_round` path. That path
settles repository edits; it does not decide whether to write `Done` or `Fixed`.

Plan creation, refinement and replanning enter
[`plan/synthesis.rs`](../crates/flow/src/workflows/plan/synthesis.rs). Review
preview and launch enter [`review/audit.rs`](../crates/flow/src/workflows/review/audit.rs);
the same audit window rules serve preview and execution. Post-analysis enters
[`review/analysis.rs`](../crates/flow/src/workflows/review/analysis.rs) explicitly
or from a preset audit. These paths can use Architect research and one-off
structured requests without entering an edit transaction.

Plain audit expands the active context plus matched paths; preset audit expands
each configured group independently. Preview uses the same full expansion, with
no separate file-token allowance or silent truncation. The complete structured
request is admitted against `request.max_input_tokens` before transport; an
oversized audit fails rather than silently dropping readable files. A failed
group remains a failed member in the report: a local domain failure can permit
the next group, while an infrastructure failure stops the batch. Automatic
post-analysis follows only a successful group report; explicit post-analysis
remains an independent admitted scenario.

The application-owned [`Documents`](../crates/flow/src/application/documents.rs)
service handles document reads and writes, including
[execution-specific writes](../crates/flow/src/application/documents/execution.rs).
Scenarios address documents by ID. Core owns the transition checks and domain
projections, and its `PlanStorage` and `ReviewStorage` own document paths and
file persistence. SQLite is a possible future replacement for file persistence
behind this boundary, not an implemented backend.

## Consolidated mechanics and remaining session dependencies

The plan and review routes use the same `step::attempts::LeafExecutor` for
readiness, coder rung attempts and retry mechanics, and `step::queue` for leaf
sequencing. Their `LeafOperations` implementations still supply distinct task
prompts, document transitions and persistence. Plan synthesis owns its draft,
critique and reconciliation beats separately from directed replanning; both
reuse the Architect contour without merging their budgets. Audit and
post-analysis share `workflows::structured::run_accounted`, which supervises
one operation-bound structured request and charges its usage but does not close
the enclosing scenario's usage window or decide its durable result.

Core's request cache prepares project configuration, repository outlines and
context file contents for both interactive and isolated turns. Core's task
helpers confirm joins and bounded stops; flow decides what an orphaned task
means for its scenario. No common workflow dispatches between Plan, Review and
REPL or calls back into `Application` to execute part of a scenario.

The computational REPL continuation does not borrow `Application` or session services.
`Application::ask` binds the executor to an operation and spawns
the independent `repl::lifecycle::run` coordinator. That coordinator uses
`HistorySession` alongside context and execution session services for
history, usage and terminal publication; the infrastructure factory binds providers and operation
policy from the same session environment. No second supervisor is required
when the caller stops waiting after admission.

`HistorySession` owns the decision to compact and the protected registration
of the current prompt. The launch passes the operation, frozen configuration
and current usage binding to that call; Scribe is resolved lazily by execution
infrastructure only when compaction is needed, inside the active accounting
window. History holds no turn binding between calls. Registration returns an
`OpenInteractiveTurn`, the consumable right to complete or interrupt that
specific journal entry after execution settles.

The computational REPL continuation interprets responses and document corrections
on one turn state and returns its local context without post-edit cleanup.
`InteractiveTask` starts with its operation guard, serial lease, open journal
turn and round reader already owned. Its join waits for technical settlement
even when computation is aborted. Only after that report may REPL consume its
continuation for document correction or persistence; document calls are awaited
inside the REPL task, not drained by an independent worker. Document acceptance
is a separate boundary: a completed file transaction does not itself save a plan
or review decision, and late cancellation does not undo an already saved document.
The classifier pairs completed decisions with `CompleteTurn` and interrupted
decisions with `InterruptTurn`; `OpenInteractiveTurn` consumes exactly one such
decision. The journal moves only an eligible final accepted round into the
response, restoring it among the rounds if the response write fails. Empty
cancellation rolls back the prompt only without rounds, diagnostics or
confirmed writes.

After journal settlement, `ContextSession::settle_turn_context` merges context
from completed results (including diagnostic results), then alone clears
transient context and rotates the dialogue identity for a confirmed applied
edit, even without returned context or successful journal persistence.
Interrupted computation does not implicitly merge its context. Confirmed
commits and documents are published before usage closes. Cancellation is
checked before the journal decision and again after all asynchronous settlement
and usage work. The final snapshot chooses the terminal sequence: late
cancellation emits `Cancelled`, not `Result`/`Finished` or `Error`, without
undoing a saved response, commit or document. Usage and confirmed-write
notices remain visible. No await separates this snapshot from publication.

## Lifetimes and invariants

An operation owns one monotone `StopSource` in tools: `Running → Soft → Hard`.
Its `StopToken` reads the full state at scheduling boundaries; a cloned
`HardCancellation` receiver exposes only Hard to current work. A new observer
sees the latest state, and a closed source in Running or Soft leaves Hard waits
pending; a previously requested Hard remains observable after closure. Soft
stops the next plan step or pipeline plan, or the next outer review member,
while the active review member finishes its internal queue (which ignores Soft).
Hard interrupts active work and prevents the next member, but transaction
recovery, reap, usage drain, protected journal registration, and terminal
publication must still settle before ownership is released. The watchdog and
abort handle retain their separate forced-stop role. Process-local stop-on-drop
and transport shutdown are independent signals and never promote an operation.

1. **Request:** freeze the supplied context version, optional history, grant,
   and model selection for one provider call. Only an admitted physical call
   spends its request budget; transport retries are not new rounds. Streaming
   previews are provisional until the final typed response is decoded.
2. **Continuation:** the same turn retains its transcript, original file
   snapshots, research, round budget, and active transaction. The workflow
   supplies updated inputs and feedback; the executor rebuilds the entire
   request contract. A refused payload does not create a fresh turn or reset
   its budget. Staged bytes never escape an unverified transaction.
3. **New unit:** a new step or independent audit starts with a fresh transcript,
   budget, research journal, and staging state. The workflow may deliberately
   share approved context and an operation identity, never another unit's
   unverified work. Cancellation and failure must settle the active operation
   before ownership is released or success is published.

## Shared mechanisms and non-edit requests

Architect synthesis owns its context, research journal, and feedback for the
duration of one run. It prepares each request with the Architect grant and
decodes it with `PlanRefinePromptProtocol`. Model changes rebind the executor
without losing its technical cache. Flow decides whether to admit research,
retry an empty response, heal a schema error, escalate, or stop. No dialogue
history, staging, edit-turn registration, or disk transaction is involved.

One-off requests such as history compaction supply a `StructuredContract` to
the bound runner. Core uses the contract's schema, preview policy, completion
policy, and decoder without building a repository prompt or scheduling another
request. Both this runner and `PreparedPrompt` call `core::execution::request`
for provider streaming and event delivery.

Before registering a new REPL turn, Scribe compacts the eligible journal head
when its history budget requires it, preserving the retained tail. Full-request
admission follows; overflow in other input components does not trigger a
separate compaction retry. Accepted history is never silently truncated.

The interactive workflow owns one `TurnState` and the same executor across technical
and document corrections. After file transaction settlement, a defective plan payload is
retried by calling `workflows::round::run_round` directly with the same
`TurnState` and executor, but only its original plan operation, message,
scratchpad, blocked and abort capabilities. These requests spend the same
technical turn budget and retain the accepted conversation inside the executor.
The supervisor retains a narrow
history reader before task launch and reads it after join and recovery.

`PromptExecutor` privately holds `execution::transcript` with the frozen task,
first delivered file versions, discoveries, feedback and scratchpad, alongside
a separate accepted-round journal and read-only recovery handle. Created paths
are staging facts supplied to `execute`, not opening originals;
later staged changes, deletions and access changes are explicit current
versions. Architect requests are fresh, without dialogue replay or interactive
answer registration; evidence continuation projects frozen read-only evidence
without carrying conversation between phases. Architect research and tier
budgets remain independent of the technical turn's correction budget.
`workflow::research::step` executes one research operation against explicit
context, journal, configuration and cancellation inputs. `edits::staging`
owns cumulative virtual staging and reports new destinations for the caller
to record in its transcript. `edits::execution` applies and verifies one
change set and resolves diagnostic file candidates without authorizing context
expansion. `edits::transaction` settles finalization or rollback through the
existing apply policy, preserving an explicit commit error on failure.

Flow owns edit-turn continuation, research progress and healing admission;
none of these mechanisms decides the scenario's next request.

| Mechanism | Authoritative location |
| --- | --- |
| Request construction, private task transcript and provider transport | `core::execution::{executor, request, transcript}` |
| Cancellation, semantic event sink, response envelope | `core::execution::{support, outcome}` |
| Original files, discoveries, feedback and scratchpad | `core::execution::transcript`, owned by `PromptExecutor` |
| Sanitized accepted rounds and recovery reader | `core::execution::accepted_rounds`, owned by `PromptExecutor` |
| Research commands and feedback journal | `core::workflow::research` and `core::workflow::research::journal` |
| Shared command handling in a round | `flow::workflows::round::run_round` using `core::workflow::research::step` |
| One-off structured request | `core::execution::structured` |
| Virtual staging | `core::edits::staging` |
| Application and verification | `core::edits::execution` |
| Transaction settlement | `core::edits::transaction` and the existing apply policies |
| Deterministic patch, filesystem and Git operations | `tools` |

Application integration contracts cover repeated research, verification
healing and plan-payload correction against captured requests, semantic events,
usage, durable history and repository state. RPC cancellation coverage preserves
an accepted research round while excluding the unfinished attempt and private
scratchpad. Lower-level tests retain forced-abort recovery, budget admission,
grant reseeding and rollback coverage; existing plan, review, and structured
request tests protect their production paths.
