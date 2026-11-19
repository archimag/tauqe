# Execution and scenario ownership

The runtime dependency direction is server → flow → core → tools. The server
also uses protocol for RPC.

## Three execution paths

| Path | Prepares the work | Executes it | Interprets and records the result |
| --- | --- | --- | --- |
| Interactive repair | Application captures history, focus, session, and policy. REPL supplies the dialogue brief and action surface. | The bounded turn admits each provider call, carries accepted staging across research and repair, and verifies edits through tools. | The REPL loop interprets the response envelope. Application finalizes the interaction and its history. |
| One plan step | The implementation host prepares the step prompt and context. Step execution supplies the isolated brief and omits dialogue history. | The same bounded turn owns the round budget, repair, verification and transaction. | The implementation workflow classifies the result and the plan scenario records progress or requests refinement. |
| Repair a review finding | The review scenario selects the finding. The implementation host prepares its work under the owning operation. | A finding with multiple steps uses the plan sequence loop. That loop runs each step and calls its persistence adapter between steps. | Review persistence writes the finding's progress. The review batch interprets the member outcome before advancing. |

## Migration milestones

1. **Implemented.** Share execution setup. Both entry points borrow a journal and input cache
   from one owner. They use one constructor for configured prompts and loop
   contexts. Exercise the same repair and reread through both entry points.
2. **Implemented.** The transcript owns original files in delivery order and
   computes the current file view from disk and virtual staging. Reseeding
   includes known originals in the new prefix. Each caller declares whether
   prose can complete its task. After rollback, REPL uses the last answer and
   spent round count to report exhaustion. Both step entry points use the
   same domain failure to report it to the owning workflow.
3. **Implemented.** The discussion contract owns the typed plan and review
   proposals used by schema composition, decoding and command dispatch. Plan
   proposals and stored plans share the same concept and implementation item
   types. Plan storage owns graph validation and lifecycle transitions. A review
   focus identifies the session for persistence and publication. The finding
   applies the proposed decision, and storage locks and saves its session.
4. **Implemented.** Application dispatches single turns and ordered sequences.
   The isolated turn executor accepts a task and returns its turn result. The
   sequence starts each member with a fresh budget, journal, transcript, and
   input cache. It carries the live repository context and cancellation signal
   across members. The review batch supplies the persistence adapter and settles
   findings. Batches, sequences, and implementation waves check cancellation
   before soft stop at their existing scheduling boundaries.
5. **Implemented.** Application owns the shared session, operation registry,
   and configuration and usage adapters. The RPC projection imports the domain
   values it exchanges through `api::types`. Each scenario selects the point
   at which a focus transition is published. Deleting a focused document
   persists the deletion, publishes the focus release, and then publishes its
   updated list. The registered operation supplies the identifier for batch
   acknowledgements and their events.
6. **Implemented.** `tauqe-flow` owns the application facade and scenarios. Cargo
   checks the boundary on every build. The workspace architecture test checks
   the allowed dependency graph, including the exceptions for test fixtures.

## Review scope after extraction

The workspace audit groups each crate by its `src` directory. This snapshot
counts Rust files and physical lines, including inline tests and comments.
Integration tests under `tests/` are outside these review groups.

| Crate | Rust files | Lines |
| --- | ---: | ---: |
| protocol | 42 | 12,704 |
| tools | 53 | 28,321 |
| core | 185 | 91,527 |
| flow | 84 | 22,105 |
| server | 50 | 23,682 |
| tui | 125 | 95,178 |

Before extraction the core group contained 268 files and 113,588 lines.
