# Application Flow Conventions

Flow coordinates scenarios and owns their session lifecycles. These rules are
review criteria, not an inventory of current RPC routes or event fields.

## Admission and ownership

- Claim exclusive work before validation or other side effects that belong to
  it. Hold the claim through terminal publication and recovery; a batch owns
  the gaps between its members as well as each member's execution.
- Keep reads and explicitly permitted non-mutating interactions available
  during an exclusive operation. Reject stale task or watchdog actions rather
  than letting them affect a newer owner.
- Give callers typed scenario inputs and outcomes. Keep preparation details,
  operation leases, and task supervision inside the application boundary.

## Cancellation and completion

- Hard cancellation interrupts current work and takes precedence over other
  outcomes. Soft stop settles the active batch member before the next is
  scheduled; it must preserve verified work already committed.
- Never release ownership while a transaction is still unwinding. A stalled
  operation must have a bounded recovery path that does not abandon a live
  transaction or publish success for incomplete work.
- Close every started edit or batch lifecycle exactly once, including refusal,
  stream replacement, cancellation, and infrastructure failure. A withdrawn
  stream must not leave stale text or previews visible.

## Scenario boundaries

- Pass session services and typed inputs to plan and review workflows at entry.
  Neither these workflows nor their shared execution paths may hold or call
  back into `Application`, directly or through a host or closure. Observers
  receive events; they do not execute parts of the scenario.
- Migrated scenarios must not fall back to legacy execution. Keep admission,
  continuation and accepted-round registration with workflows; a technical
  executor freezes and executes one request without choosing its successor.
- Preserve the same task state and budget across technical and document
  corrections. Settle the technical transaction before document correction,
  and finish recovery and terminal publication before releasing ownership.
- Reuse core execution mechanisms instead of copying turn logic into each
  scenario. The scenario determines its target, permissions, continuation,
  and durable result; the engine executes the prepared work.
- Publish the model and effort of an isolated attempt from its prepared
  operation-bound execution, never by resolving a second configuration snapshot.
  Preview reports the complete audit window; only full-request admission may
  refuse its size, and a failed group remains failed even if later groups run.
- Reuse queue mechanics where sequencing is the same. Keep plan and review
  decisions about readiness, failure, replanning, and closure with their
  respective scenarios rather than imposing one domain's rules on another.
- Validate document mutations and configuration changes before persisting
  them. Reads must not mutate stored state as a hidden side effect.
- Treat usage, history, context, and published outcomes as one observable
  scenario: publish coherent snapshots after the corresponding state changes,
  without dropping reports from failed attempts or publishing a terminal
  result before its recovery completes.
- Keep transport concerns in the server. Publish typed semantic events;
  do not serialize RPC payloads, configure logging sinks, or wait on a TUI
  from inside a scenario.
