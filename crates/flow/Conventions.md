# Application Flow Conventions

`tauqe_flow::api::Application` exposes typed scenarios for the session. Clones share
the session and independently constructed applications have independent state.
Scenario inputs name the caller's intent, such as a prompt, target, document
identifier, or model selection. Preparation types, operation leases, and workflow
tasks are internal. Scripted providers and test timing overrides use explicit
test seams.

### Admission and cancellation

The server decodes parameters before invoking the scenario. An exclusive scenario
registers before validation, document loads, focus changes, or writes. One
operation owns a batch and all its steps, including the gaps between them. Its
guard remains live until the scenario queues terminal notifications. Reads and
permitted focus changes remain available while the slot is occupied.
The [RPC scenario inventory](../../docs/rpc-application-boundary.md) identifies
each route's response mode and registration policy.

Hard cancellation raises a cooperative signal and leaves the registration live
during recovery. The watchdog waits for the unwind grace before force-aborting a
stalled task, defers while a Git transaction is open, and confirms the task exit
against the task deadline. Soft stop lets the current member settle before the
next scheduling decision. Identity checks prevent a stale watchdog or task
handle from affecting a newer operation. Task deadlines passed to a spawn are
resolved before the task starts.

### Operations and publication

`ApplicationObserver` accepts typed events and offers asynchronous transport
readiness. Publication is synchronous. The observer may enqueue an event without
accessing the application, including while a usage heartbeat holds the accounting
lock. Flow constructs result metadata, usage and context snapshots, ready-to-send
document lists, edit outcomes, Git state, and terminal batch reports. Semantic
tracing belongs to the layer that performs the work. The server projects event fields onto the existing wire
contracts and manages backpressure, process signals, and logging sinks.

Interactive and step turns resolve their provider, apply policy, prompts,
capabilities, and session affinity in flow using core services. Stream resets discard text from
the withdrawn attempt and close any open edit cycle. Cancellation wins over task
and stream failure. A task failure triggers recovery. Each open edit cycle closes
once, including on abort, refusal, or stream failure. On successful interactive
completion flow flushes usage, merges context, publishes edit/Git completion,
persists the history outcome, and publishes context and document updates before
the model result and finished notice. A Git commit notice precedes the refreshed
repository state and edit completion.

Usage windows retain reports from failed attempts. Flushing folds a window into
session and project totals once and attempts persistence before the final
snapshot. The heartbeat derives the current duration under the accounting lock,
checks ownership there, and ceases publication after a window closes or is
replaced. A stale window cannot update its successor. History publications carry
the estimate and, for compaction, the count taken at the mutation.

Plan and review workflows validate queued targets and run preflight under the
same operation they use for execution. Implementation hosts, Architect and
critic calls, reviewer resolution, and automatic post-analysis use core execution services. A plan
failure ends its pipeline. Review retains local failure and skip handling.
Architectural findings require their group horizon, and unresolved questions
prevent that group from starting. Review preview is read-only and keeps preset
group budgets separate from the interactive context window.

### Repository, configuration, and documents

Flow initializes and rebinds the configuration, context, project usage, and
history under one operation. It resolves effective model configuration, roles,
history model, available choices, and readiness before returning a view.
Configuration changes validate before committing. Module-owned fields take
precedence over patch validation. Credentials preserve the active selection and
dialogue identity. Model changes rotate dialogue identity when the effective
model changes. File-writing commands report the path they wrote even if
rediscovery fails.

Flow orchestrates Git revision validation, squash base selection and message generation,
manual context editing, cache reconciliation, and history cleanup. The squash
preview keeps a consistent view under exclusive admission. History cleanup resets
dialogue identity and detaches focus even if journal removal fails. Document
commands validate identifiers and trees, resolve active and first-document
fallbacks for reads, enforce explicit review update targets, and release matching
focus after deletion. Reads leave active pointers intact. Document updates and
list notifications are prepared while holding admission. Storage and publication
preserve their best-effort behavior where the scenario allows recovery.
