# Execution and scenario ownership

This document describes the implemented execution boundary. The dependency direction is
`server → flow → core → tools`; the server also uses `protocol` for RPC. The
boundary is defined by ownership of decisions, not by whether an object is
mutable, technically complex, or shared by several scenarios. Flow may later
split into separate scenario crates without changing that boundary.

## Who owns a decision?

Before a workflow starts, the environment has already selected technical
dependencies such as the provider, edit format, application policy, verification
machinery, and their configuration. A factory assembles these into a prompt
executor. Workflows do not reconstruct this wiring or choose a convenient
substitute for a configured policy.

During execution, the workflow owns the decisions it may change: which context
and history to use, the model's available capabilities, what work to request
next, and how to interpret and persist the result. `ContextManager`,
`HistoryManager`, and `CapabilityTree` can be objects supplied to the factory
and then operated on by the workflow. Their concrete ownership and borrowing
rules are an implementation concern; the important property is that the
workflow can explicitly change their relevant state before the next request.
Using history at all is a scenario decision, not an executor default.

Mutability alone does not imply flow ownership: the executor's internal cache
changes, but its contents do not constitute a workflow decision. Conversely,
the technical implementation of a workflow-owned capability tree may live in
core. The test is: **who is authorized to decide its value, and can that
decision change during this workflow?**

The executor implements the decision: prompt assembly, coherent request
construction, caching, provider streaming, schema and decoder selection,
technical retries, bounded research and edit verification as applicable. It
returns typed outcomes, including refusal, cancellation, and infrastructure
failure; it does not schedule the next plan step, close a review finding, or
silently alter a workflow's permissions. Flow owns those transitions and the
durable effects that follow them. The server projects results onto RPC and
handles transport, logging setup, and final error translation.

## Factory and request boundary

The factory lives in `crates/flow/src/application/infrastructure/execution.rs`.
`ExecutionFactory::bind` freezes configuration and dialogue identity under an
admitted operation for interactive and single-step calls. A
`PlanExecutionEnvironment` shares one configuration snapshot across plan waves,
steps and review batches; auxiliary Scribe work uses that snapshot or an
operation-bound `CompactionEnvironment`. Provider kind, credentials, model
parameters, effort and session affinity are resolved from semantic selection
under the operation lease, including when a later rung changes providers.
The factory is configured independently of any particular workflow.
The workflow supplies its controllable entities to create an executor; it
does not pass a provider, edit policy, or application policy through its own
scenario code. Where two execution shapes need different contracts, the
factory can expose distinct typed creation methods rather than one bag of
optional flags.

Each call to the executor uses a **coherent snapshot** of workflow-controlled
state. The advertised capabilities, instructions, response schema, decoder,
and stream filter must describe that same snapshot. A change to context,
history, or capabilities while a call is in flight cannot partially change
that call; it affects the next call. This matters even if an implementation
constructs a protocol before the request. `DialogueExecution` rebuilds its
protocol for each grant, including corrective continuations; `EditExecution`
binds the step grant to its protocol. A read-only `StructuredContract` binds
instructions, response format, decoder, visible field filter and completion
rule for a structured request.

An executor call may itself contain bounded technical rounds (research,
rejected-patch retry, verification healing). These rounds implement one
requested unit of work. Whether to ask for another unit, switch capabilities,
advance a plan, or save a review belongs to the workflow. A scenario should
not need to know how cache entries, provider requests, or retries are made.

## Implemented units and scenario owners

- `DialogueExecution` calls core `ExecutionUnit::execute_bounded`; flow's
  `interactive_dialogue` interprets its commands and uses `continue_round` on
  the same unit after a rejected plan payload. Flow controls focus, context,
  optional history, permissions and durable plan or review decisions.
- Each isolated plan or review leaf obtains an `EditExecution` with a fresh
  `ExecutionUnit`. Flow's implementation pipeline owns readiness, coder ladder,
  replan budget, step status, commit association and review settlement. The
  step's technical rounds share a transcript and budget, but neighboring leaves
  never share staging. Isolated steps do not inherit dialogue history.
- `StructuredExecution` runs one read-only `StructuredContract` through core's
  shared request mechanism. `structured_task` supervises the operation-bound
  task and delivers filtered events; flow accounts for usage and interprets the
  typed result. Review audits, post-analysis, independent critique, locale,
  squash-message generation and compaction each retain their own acceptance
  policy.
- `ArchitectResearchExecution` holds a core `StructuredResearchUnit` across
  research rounds. Flow's `architect_synthesis` owns the round limit, schema
  healing, tier escalation, terminal verdict and transient-context cleanup;
  core retains its research journal and request cache, not plan transitions.

## Decision ownership and execution contract

| Decision or resource | Owner | Boundary rule |
| --- | --- | --- |
| Operation admission, task supervision, hard cancel, soft-stop boundary, usage and publication | Flow application infrastructure | Keep the operation claimed through recovery and terminal publication; a batch keeps the claim between members. The server only translates and publishes RPC. |
| Provider client, credentials, configured model parameters, session affinity, edit format, apply policy, verification and technical channels | Application execution factory | Resolve from one environment snapshot and the operation lease; no workflow passes `LlmProvider`, `PromptProtocol`, `ApplyPolicy`, patch strategy, or the whole `AppConfig` to shared execution. Preserve explicit configured strategies. |
| Semantic model selection and escalation decision | Flow workflow | Pass a role, explicit selection, or selected ladder rung as intent. The factory resolves the provider (including a change of provider on promotion), effort and affinity for each selected rung; the workflow does not rebuild a client or a protocol. |
| Context, optional history, capability tree, task and next unit of work | Flow workflow | Prepare and change these between calls; an isolated step has no dialogue history by default. Context admission and capability validation remain deterministic core operations, not a reason for core to select the next plan step. |
| One provider request or bounded technical unit | Core executor | Assemble coherent requests, bind the model-facing contract to its grant, stream and decode typed results, and perform allowed research, patch retry, verification healing and rollback. Internal cache and transcript are implementation state, not workflow decisions. |
| Readiness, domain correction, review verdict, document and history writes | Flow workflow | Interpret typed outcomes, decide whether to continue or replan, and persist only after the required verification/recovery boundary. Core may retain pure domain models, parsers and validators. |

The workflow supplies context, an *optional* history snapshot, capabilities,
semantic selection and task under an operation identity. The factory supplies
the frozen technical configuration. It binds dialogue and edit-capable bounded
units, structured single requests and persistent structured research separately;
it is not a universal bag of flags. A call returns decoded commands
or a typed payload with its verification/change-set metadata and the context it
actually widened, or a distinct semantic refusal, exhaustion, cancellation or
infrastructure failure. It must not turn a provider failure into a blocker or
publish an uncommitted edit as success. Flow interprets the payload and decides
which context layers to adopt or release, how to record history, and when to
publish the terminal result. A failed or cancelled call still reports usage and
recoverable research; an aborted task cannot discard the last shared context
snapshot or bypass transaction recovery.

In Rust, a workflow can own `ContextManager` and a capability value and pass
exclusive reborrows into a call; history may be a borrowed immutable snapshot.
The executor must finish borrowing before flow changes them for the next call.
When a task must be spawned, it can own the context snapshot and return it with
the outcome for flow to merge under the operation lease. `Arc<Mutex<_>>` is not
required for every entity; synchronization is needed only at actual concurrent
ownership boundaries. Changing permissions between calls must rebuild the
entire model-facing contract from the new tree, including revocation, rather
than merely changing a mutable field beside an old protocol.

There are three different lifetimes:

1. **Single request:** freeze the context version, optional history, model
   selection and capability tree for this provider call. A structured request
   uses its own typed contract and completion rule instead of a turn grant.
   Research admitted by a turn grant may widen context *between* rounds, never mutate an
   in-flight request. Count a physical provider call only when admitted, not on
   a rejected request; a transport retry inside one call is not another round.
2. **Continuation of the same unit:** after a domain payload refusal or a
   patch/verification correction, retain the unit's transcript, research and
   round budget, including any staged bytes only within the active edit
   transaction. A changed capability grant takes effect on the next request
   with a matching protocol; it does not reset the budget or replay stale
   permissions. Technical retry and healing budgets remain scoped to their
   respective operations.
3. **New isolated unit:** a new step or independent audit gets a fresh
   transcript, round budget, research journal and staging state. It may share
   the operation lease, selected provider session affinity, and approved
   workflow context, but never inherits another step's unverified staging.
   The batch owns scheduling and soft stop between units; hard cancel takes
   precedence everywhere. A workflow-level ladder or replan budget spans the
   units the workflow explicitly groups, not an accidental executor lifetime.

## Production routes and completion policies

Every production provider call reaches `core/control/request.rs`: bounded turns
use `ExecutionUnit`, structured calls use `structured::execute`, and Architect
research uses `StructuredResearchUnit`. The following routes identify the flow
scenario that makes each domain decision, rather than a second provider path.

| Scenario | Technical route and flow-owned result |
| --- | --- |
| REPL and focused discussion | `Application::ask` → `start_interactive_execution` → `interactive_dialogue::run` → `DialogueExecution`. Flow applies plan/review commands and corrective continuation on the same turn. |
| Plan steps and review repair | Flow's `PlanImplementationPipeline` and `review_sequence::run` schedule leaves through `implementation_host` / `step_run` and a fresh `EditExecution` per leaf. Flow persists step and review transitions; core verifies the bounded edit. |
| Plan synthesis and replan | `plan_two_phase` / `plan_horizon` call `architect_synthesis` through `ArchitectResearchExecution`; independent critique uses `StructuredExecution` and `structured_task`. Flow orders waves, escalation and plan writes. |
| Review creation, groups and post-analysis | `review_synthesis_execution`, `review_groups` and `review_analysis_execution` call `structured_task` with separate review contracts. Flow owns windows, findings, regrouping, publication and storage. |
| Auxiliary Scribe work | `locale_generation`, `git_squash` and `history_compaction` call `StructuredExecution`; flow owns validation, fallback or journal adoption. The later `apply_squash` Git rewrite makes no model request. |

The shared request transport is **not** a shared acceptance policy:

| Consumer | Completion and fallback |
| --- | --- |
| Single review | `single_review_contract` does not require `Done`. Empty output is refused; nonempty unstructured prose becomes an informational finding. Usage is published before the session is persisted. |
| Group review | `group_review_contract` also permits close without `Done`, but requires parseable findings: prose and empty output are local group failures; stream or task failure stops the batch. Completed groups remain saved. |
| Post-analysis | The typed regrouping validates identities and references after decoding; empty, parse and schema failures are distinct, and no regrouping write follows them. Usage precedes persistence. |
| Independent critique | Its silent contract requires `Done`; empty, incomplete or unreadable payloads spend the selected rung's schema-healing attempts and block issuance when exhausted. Provider failure and cancellation do not issue the draft. |
| Locale | Its silent contract requires `Done`. A completed empty or unusable translation map falls back per catalog key to width-checked English; incomplete streams and provider failures are errors, not fallback. |
| Squash message | Its silent contract requires `Done`. An unavailable provider or completed response without a usable message falls back to the first commit subject (or `Squashed commits`); an incomplete or failed started request is an error. Only started requests open a usage window. |
| History compaction | Its silent contract requires `Done`. The application compacts only an over-budget journal with a usable summary; empty, incomplete, cancelled or failed requests leave the journal unchanged. |

## Existing verification anchors

| Guarantee to retain | Existing tests |
| --- | --- |
| Protocol tracks the declared grant, frozen inputs and revocation on continuation | `crates/flow/src/application/turn_execution/tests.rs` (`protocol_starts_with_the_declared_contour_tree`, `compare_repl_and_plan_request_prefixes`, `dialogue_continuation_uses_its_new_capability_grant`); `crates/core/src/control/prompt.rs` (`changed_bytes_travel_after_the_frozen_context`). |
| Frozen configuration, provider promotion and operation admission | `crates/flow/src/application/infrastructure/execution.rs` (`frozen_credentials_and_affinity_survive_reload`, `next_rung_switches_provider_kind_without_reusing_the_previous_client`, `plan_environment_keeps_configuration_across_steps_and_provider_changes`, `reviewer_selection_preserves_the_exact_tier_and_review_affinity`); `crates/flow/src/application/turn_execution/tests.rs` (`foreign_interactive_and_step_operations_do_not_reach_the_provider`). |
| Continuation preserves its unit; neighboring steps isolate staging | `crates/flow/src/application/turn_execution/tests.rs` (`dialogue_continuation_uses_its_new_capability_grant`, `neighboring_step_units_do_not_inherit_failed_staging`, `failed_step_retains_research_without_dialogue_history`); `crates/core/src/control/prompt.rs` (`only_admitted_calls_publish_rounds_before_provider_deltas`). |
| Domain failure differs from infrastructure failure; soft stop retains completed work | `crates/core/src/control/turn/step_outcome_tests.rs` (`semantic_block_and_abort_keep_their_precedence_and_messages_in_both_loop_outcomes`, `commit_failure_takes_precedence_and_stops_the_run_as_infrastructure_failure`); `crates/flow/src/application/implementation_pipeline.rs` (`the_replanning_budget_is_bounded`, `a_soft_stop_requested_mid_run_keeps_the_completed_step`). |
| Completion policy and accounting stay with each scenario | `crates/core/src/control/structured.rs` (`completion_policy_and_empty_payload_are_distinct`); `crates/core/src/review/parser.rs` (`contracts_distinguish_single_fallback_from_group_json`); `crates/flow/src/application/review_synthesis_response.rs` (`empty_output_is_refused_and_nonempty_unstructured_output_is_retained`); `crates/flow/src/application/review_analysis_response.rs` (`decoding_keeps_tolerant_fallback_and_typed_parse_and_schema_refusals`); `crates/flow/src/application/review_synthesis_execution.rs` (`headless_execution_keeps_accounting_publication_and_history_order`); `crates/flow/src/application/review_analysis_execution.rs` (`headless_execution_accounts_before_persistence_and_records_history`). |
| Auxiliary fallback and no-write failures | `crates/core/src/locale.rs` (`translation_contract_is_silent_and_requires_completion`); `crates/core/src/prompt.rs` (`squash_contract_extracts_an_optional_message_from_the_existing_format`, `compaction_contract_requires_done_and_extracts_only_a_nonempty_summary`); `crates/flow/src/application/history_compaction.rs` (`provider_resolution_or_stream_failure_keeps_the_journal_unchanged`, `empty_incomplete_and_cancelled_summaries_leave_history_unchanged`). |
| No scenario provider bypass or RPC boundary leak | `crates/server/tests/architecture_tests.rs` (`scenarios_do_not_wire_provider_requests`, `core_provider_calls_use_the_shared_request_mechanism`, `workspace_dependencies_follow_module_boundaries`, `server_production_sources_use_application_ports_for_domain_io_and_mutations`). |

## Migration criterion

The boundary is established by production callers, not by the existence of a
factory facade. Reading a flow scenario must reveal its domain decisions and
state transitions; reading the core executor must not reveal plan scheduling,
review settlement or scenario-specific fallback. All production model requests
use the shared technical request mechanism, while configuration and task
supervision remain in flow infrastructure. Changing the next request's grant
must rebuild its model-facing contract without overriding the frozen environment
or losing the state of an intentional continuation.
