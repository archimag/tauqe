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
configuration. A workflow cannot silently replace these dependencies. Core
provides the mechanisms for constructing coherent prompts, streaming requests,
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

The executor streams provisional user-visible events and returns a typed
outcome: decoded response, semantic refusal, exhaustion, cancellation, or
infrastructure failure. It owns request construction, caching, provider
streaming and technical retries. A workflow never needs to assemble a
provider request or read raw provider JSON to decide its next step. The
executor never schedules that step, applies a plan decision, or chooses a
scenario-specific fallback. The factory resolves technical dependencies from
the operation's frozen environment; changing the selected model rung is
workflow intent, while resolving its provider and effort is factory work.

The interface must support both one-off structured requests and requests
inside a bounded turn without requiring every workflow to adopt the turn
lifecycle. Different typed request contracts are legitimate; different
provider paths and inconsistent rules for freezing inputs are not. The
executor can maintain a request cache, but it does not own the workflow's
context, history, or permissions.

## Workflows compose workflows

Flow's execution logic is organized around explicit, reusable scenarios,
not around a large `Application` facade or a collection of provider wrappers:

```text
Application::ask → workflows::repl::run
  └─ TurnWorkflow
       └─ RoundWorkflow ── PromptExecutor ── provider

Application::{execute_plan, execute_plan_pipeline}
  └─ PlanResources → plan batch → PlanAttempts → StepResources

Application::execute_review
  └─ ReviewResources → review batch → ReviewSequence → StepResources

StepResources → StepAttempt → TurnWorkflow → RoundWorkflow → PromptExecutor

Application::synthesize_plan → plan synthesis → Architect contour
  └─ PromptExecutor → PreparedPrompt::execute_decoded → shared request transport

Application::{preview_review, start_review, analyze_review} → review::{audit, analysis}
  └─ preview reads the audit window; audit and analysis use bound model requests

One-off structured request → StructuredExecution::run
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
| `workflows::repl::run` | Supervision of one interactive task, receipt settlement, history and terminal publication | Provider transport |
| `TurnWorkflow` | Technical continuation, staging/edit state and transaction settlement | Plan/review persistence decisions |
| `RoundWorkflow` | One admission, budget charge and accepted-response registration | Research execution or retries |
| `PromptExecutor` / `PreparedPrompt` | Technical cache and one frozen, single-use request | Mutable turn state, budget or continuation |
| Server | RPC decoding, error translation and event projection | Scenario execution |

The edit-turn routes compose three workflow layers between application admission
and the executor. The infrastructure factory binds collaborators; it does not
decide when a scenario continues.

`RoundWorkflow` prepares and admits one request from workflow-owned inputs,
uses the executor, and registers only a completed, structurally valid response.
It does not execute commands or decide whether to continue.
`TurnWorkflow` composes rounds into one bounded unit: research, rejected-patch
retry, verification healing, transcript, budget, staging, and transaction
settlement have one explicit lifetime. The supervised REPL task owns its
`TurnState`, focus, document interpretation and corrective continuations.
`repl::run` relays provisional events, then waits for a report containing the
task outcome and the independently settled disk and document writes. It chooses
whether to record a completed response or an interruption before publishing the
terminal outcome. A normal answer is recorded once before `Finished`. A plan
storage refusal produces a diagnostic, not a correction request or a saved plan
notification for the refused write. Cancellation takes precedence without hiding
writes that already completed. The workflow flushes usage and publishes settled
writes before terminal publication, while still holding the operation. History
failures suppress `Result` and `Finished`, not an already finalized commit. If
Git finalization fails after verification, the accepted reply stays among the
rounds and the response records the Git diagnostic, not a commit or a rollback.
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
and the same `StepAttempt → TurnWorkflow → RoundWorkflow` path. That path
settles repository edits; it does not decide whether to write `Done` or `Fixed`.

Plan creation, refinement and replanning enter
[`plan/synthesis.rs`](../crates/flow/src/workflows/plan/synthesis.rs). Review
preview and launch enter [`review/audit.rs`](../crates/flow/src/workflows/review/audit.rs);
the same audit window rules serve preview and execution. Post-analysis enters
[`review/analysis.rs`](../crates/flow/src/workflows/review/analysis.rs) explicitly
or from a preset audit. These paths can use Architect research and one-off
structured requests without entering an edit transaction.

The application-owned [`Documents`](../crates/flow/src/application/documents.rs)
service handles document reads and writes, including
[execution-specific writes](../crates/flow/src/application/documents/execution.rs).
Scenarios address documents by ID. Core owns the transition checks and domain
projections, and its `PlanStorage` and `ReviewStorage` own document paths and
file persistence. SQLite is a possible future replacement for file persistence
behind this boundary, not an implemented backend.

## Lifetimes and invariants

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

The interactive workflow owns one `TurnState` across technical and document
corrections. After transaction settlement, a defective plan payload is retried
directly through `RoundWorkflow`, with only its original plan operation,
message, scratchpad, blocked and abort capabilities. These requests spend the
same budget and retain the accepted transcript. The supervisor retains a narrow
history reader before task launch and reads it after join and recovery.

`execution::transcript`
stores originals, accepted rounds, discoveries, feedback and bounded private
notes, and provides a read-only accepted-history recovery handle.
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
| Request construction and provider transport | `core::execution::{executor, request}` |
| Cancellation, semantic event sink, response envelope | `core::execution::{support, outcome}` |
| Original files, accepted rounds, discoveries, feedback and recovery reader | `core::execution::transcript` |
| Research commands and feedback journal | `core::workflow::research` and `core::workflow::research::journal` |
| Research step in an edit turn | `core::workflow::research::step` |
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
