# Structured Code Review

Code review in TAUQE is a structured engineering discipline, not a conversation
about a diff. An isolated analytical pass inspects the selected files and emits typed
findings; the developer triages them; approved findings are resolved in bounded turns.
Findings that are symptoms of one shared root cause are never resolved one by one:
they form an architectural group resolved by one coherent plan that the Architect
synthesizes just in time. The Review workspace is where that evidence becomes visible
and where a root cause becomes an executable plan.

---

## 1. Motivation & Invariants ("Why")

### 1.1 The generation / verification asymmetry

A model writes code far faster than a human can read it line by line. Once that
asymmetry is accepted, requiring a manual inspection of every hunk stops being a
safety mechanism and becomes review fatigue, where the human rubber-stamps changes
without comprehension. The opposite extreme — generate a large change set and inspect
nothing — invites architectural drift, logic regressions, and security debt.

The asymmetry is not a defect of the model; it is a defect of *who verifies*. A
verification step performed by the same generative process that produced the code
inherits its assumptions, so it confirms rather than checks.

### 1.2 Two phases that never share a context

Review is split into two independent phases:

1. **Analytical audit** — a read-only Reviewer pass produces typed findings. It
   cannot modify files; its output is evidence.
2. **Finding resolution** — the approved findings are resolved by the Coder in
   bounded turns: an independent defect on its own, and a cluster of symptoms of one
   root cause through the single plan of its architectural group.

The audit never reuses the Develop conversation, and the fix phase never escalates
into the Architect role.

### 1.3 A symptom is not a defect to fix on its own

An architectural defect — a missing abstraction, a violated layering rule, a
duplicated responsibility — manifests in several places at once. Fixing each
manifestation in isolation produces three predictable failures:

- the root cause survives every individual fix, so the defects return;
- two isolated fixes disagree about the shape of the shared interface and collide;
- the resulting change set has no owner and no coherent design, so nobody can state
  what it achieves.

The remediation of a root cause is therefore a *plan*, not a patch, and its unit of
completion is the whole plan. That is the entire reason the group model exists.

### 1.4 The invariants

1. **The audit is isolated.** No Develop history, no repository map — only the
   selected file content, so no confirmation bias enters the verdict.
2. **The audit never writes.** It runs in the read-only Reviewer role.
3. **A defect and its remediation are separate statements.** What is wrong and what
   fixes it are distinct fields.
4. **Line coordinates are never stored.** Findings name files, not fragile line
   ranges.
5. **An architectural group is resolved only by its whole plan.** Its findings are
   symptoms, so executing one of them alone is refused, and the group is complete only
   when the whole plan is verified.
6. **An independent finding is resolved in one verified bounded turn.** Marking
   `Fixed` is tied to the verified result of a turn, not to an opinion.
7. **A remediation plan is synthesized just in time.** The harness asks the Architect
   for the group horizon when execution starts; there is no manual decomposition
   command and no intermediate status waiting for one.
8. **The audit fixes *what*; the fix owns *how*.** A resolving turn never re-audits
   the finding and never substitutes the designed structure, the interfaces the finding
   relies on, or an invariant it states.
9. **Human triage gates autonomous execution.** A finding under discussion is never
   executed.
10. **Status is developer-owned.** A discussion explains a proposed change; it never
    rewrites persisted review state.
11. **Verification precedes execution.** A repository that does not build is refused
   before any token is spent.
12. **A plan is a branch, not a symptom.** The unified plan of a group owns its own
    branch below the group header; its steps and the findings it fixes are separate
    bands of the tree, never one flat list.
13. **Only open findings are planning material.** The Architect synthesizes and
    reprojects a group horizon from the findings that are still `Todo`; a finding the
    developer already closed never generates a step.
14. **An `architecture` finding is executed only through a plan.** Its type is a
    deterministic requirement: the post-analysis gives it an architectural group, and
    the execution queue refuses to run it as a bare turn.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Session, group, finding

A **review session** is a persisted audit. A **group** (`ReviewGroup`) is one
operational slice of it. A **finding** (`ReviewIssue`) is one defect inside it,
deliberately split into a diagnosis and a remediation.

| Session field | Meaning |
| --- | --- |
| `id` | UUIDv7 identity |
| `title` | concise title produced by the reviewer |
| `created_at` | the instant the audit was produced |
| `model` | the model that performed the audit |
| `description` | the executive summary, falling back to the prompt |
| `user_prompt` | the raw audit instructions |
| `target_files` | the audited repository-relative paths |
| `items` | the discrete findings |
| `groups` | the audited slices and the derived groups |
| `post_analysis` | the Architect classification layer, if any |
| `launch_config` | the configuration that launched the session |
| `raw_markdown` | the rendered audit report, kept for copying |

| Group field | Meaning |
| --- | --- |
| `id` | identifier, unique within a session |
| `title` | human-readable title rendered by clients |
| `paths` | repository-relative paths this group audits |
| `prompt` | instructions that apply to this group only |
| `description` | the shared root cause, when there is one |
| `kind` | the class that decides how the group executes |
| `subplan` | the unified remediation plan of the group |
| `subplan_architect` | the configuration that produced `subplan` |
| `subplan_last_replanned_step` | the most recently replanned step |

| Finding field | Meaning |
| --- | --- |
| `id` | numeric identity inside the session |
| `uuid` | stable UUIDv7 identity of the finding |
| `group` | identifier of the group the finding belongs to |
| `title` | concise defect title |
| `type` | the problem category |
| `status` | the lifecycle state |
| `description` | what is wrong, and why |
| `recommendations` | the ordered remediation steps |
| `files` | paths to modify when resolving it |
| `readonly_files` | contracts to rely on but never change |
| `note` | resolution note, skip rationale, or diagnostics |
| `model` | optional role or model override for resolution |

### 2.2 The group dichotomy

`ReviewGroupKind` is a stored field, never a property derived from the shape of an
identifier: a prefix is a guess, not a contract.

| Class | Collects | Execution |
| --- | --- | --- |
| `Simple` | independent defects | one bounded turn per finding |
| `Architectural` | symptoms of one root cause | only the group `subplan` |

Two identifiers always appear in a resolved session:

- **`default`** — the implicit group of a single-audit session that declares no
  explicit groups. Consumers resolve it through `effective_groups` instead of
  branching on "legacy or grouped".
- **`local`** — the group the post-analysis always materializes, even when empty,
  collecting the findings that share no root cause with another finding and need no
  plan before they are executed. Every finding belongs to exactly one group, so an
  independent defect is a simple group of one member. An `architecture` finding never
  lands here: its type requires a plan, and the post-analysis mints the group that
  carries it.
- **`arch-N`** — a deterministic ordinal inside one classification (`arch-1`, `arch-2`,
  ...), naming an architectural group: a cluster the post-analysis derived from one
  shared root cause, or the single-element group minted for an `architecture` finding
  that the classification left independent. A random identifier would make two
  identical analyses produce two different sessions.

A finding of an architectural group carries the group identifier in `group` and
nothing else about remediation: the plan belongs to the group, and the finding merely
describes what the group manifests.

### 2.3 Why the diagnosis and the cure are separate

An undivided finding body conflates the two. A model can then dilute a precise root
cause with generic advice, and a human cannot tell whether the defect was actually
understood. Two independent statements give the developer a clean acceptance
criterion and the Coder a bounded mandate.

**Why `readonly_files` exists.** Some defects can only be fixed safely against an
immutable boundary: a trait definition, a public interface, a serialization format,
an architectural invariant. Naming those artifacts explicitly lets a fix rely on
their guarantees instead of silently changing a contract the rest of the system
depends on. A protected contract is context, never a target.

### 2.4 Problem categories

Findings are classified into categories an engineer can act on, instead of a shallow
severity ladder:

| Type | Meaning |
| --- | --- |
| `bug` | incorrect behavior, panic, data corruption, resource leak |
| `security` | a security flaw or an unsafe boundary |
| `architecture` | a structural or runtime risk that must be decomposed |
| `performance` | a costly hot path or wasted resources |
| `quality` | maintainability, readability, dead-code cleanup |

A non-actionable informational level is never exposed to the reviewer. The model
emits the actionable category, so the audit vocabulary stays aligned with the work
that can follow.

### 2.5 Status vocabulary

```text
            triage               execute                verified
[DISCUSSION] -----> [TODO] ---------------> [IN_PROGRESS] -----> [FIXED]
     |                 |                          |
     |                 +--------------------------+--> [SKIPPED]
     +--> [SKIPPED] | [REJECTED]
```

- **DISCUSSION** — the initial state. Human triage, feasibility assessment, or
  architectural discussion is required; it cannot be executed autonomously.
- **TODO** — approved, executable, and the only status that feeds planning: the
  Architect synthesizes and reprojects a group horizon from the open findings alone.
- **IN_PROGRESS** — a bounded turn, or the plan of the finding's group, is resolving
  it.
- **FIXED** — resolved, committed, and verified by the toolchain.
- **SKIPPED** — already addressed, made obsolete, or not reproducible; the reason
  stays in `note`.
- **REJECTED** — a false positive, an intentional trade-off, or non-actionable.

There is no intermediate state between triage and execution. A finding broader than
one turn is not parked in a "needs decomposition" limbo, because decomposition is not
a developer ritual but a phase the harness runs when execution starts (§3.7, §3.8).

**Progress counting.** Session progress is `fixed / total`, where `total` counts only
actionable findings: `SKIPPED` and `REJECTED` are terminal and are excluded from the
denominator. A session whose remaining findings are all resolved therefore reports a
full bar instead of a fraction that can never reach one, and a session with no
actionable findings reports `0/0`.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Starting an audit

`review/start` carries the audit instructions, an optional model selection, and an
optional list of target directory or glob patterns. The server resolves every pattern
against tracked files and **merges** the matches with the files already in context,
de-duplicated, for that run only. The developer's Context layer is never cleared or
rewritten: a scoped audit can cover code never explicitly added, while control over
the working set stays untouched. Because matched files are read from the working tree,
the audit always inspects current source.

The launch dialog previews the pattern locally before a request is sent, and the
preview is a contract, not a decoration: both the directory list and the match counter
are computed from the repository file list that arrived with the handshake, never
touching disk, Git, or the network. Directories are offered as sorted, unique
prefixes, each terminated with `/`, because that is the pattern form that selects
every path beneath it. The match counter applies the same `GlobPattern` the server
applies, so a preview can never predict a selection the server resolves differently. A
blank pattern narrows nothing and the launch runs unrestricted; a pattern that matches
nothing is an honest zero, and the launch is refused before a request is sent.

`review/preview` projects the same window server-side: the context contribution, the
matched files, the accepted files and their estimated tokens, and the budget
breakdown, per group for a preset launch. `ReviewPreviewWindow::is_truncated` is the
single predicate a client branches on, so a window that drops nothing needs no consent
step and a truncated one asks before launching.

Pattern expansion is bounded by the same budget that governs discovery: the expansion
stops at the first file that would push the loaded window past
`context.files_token_budget`, and each matched file is priced as its content plus the
wrapper its own `<file>` element renders, so the estimate covers what the audit
actually reads. The number of matched files is never a limit: it is reported, not
enforced, and a selection wider than any fixed ceiling is audited in full when the
budget pays for it. Matches the budget cannot pay for are not inspected. The
`review/started` event reports how many matched files were omitted, the daemon records
a warning, and the interface raises one, so a silently narrow audit is impossible. A
pattern that matches no tracked file is refused outright: launching anyway would audit
a different file set than the one the developer believes the pattern chose. A partial
selection stays a lawful run, reported through the omitted count.

While the run is in flight, the Review view mirrors its progress in the info strip. A
grouped session audits its slices one at a time, and the strip names the group that is
currently streaming: its one-based position in the supervisor's sequential traversal,
the total group count once the mirrored session is known, and the group title. The
session is published only after the first group finishes, so the first badge degrades
honestly to an ordinal without a denominator instead of inventing a total. When the
Architect post-analysis layer classifies the findings, the strip raises an analysis
badge; a standalone classification request is a turn of its own and settles the
interface when its terminal event arrives, while the automatic classification inside a
grouped run stays subordinate to the run's own terminal event.

### 3.2 Why the audit is isolated

The reviewer receives the pure content of the selected files — no Develop history and
no repository map. Three reasons make that deliberate:

- **No confirmation bias.** Sharing the development narrative imports its
  rationalizations, and the reviewer starts agreeing instead of inspecting.
- **Context hygiene.** Omitting long histories and global outlines keeps the prompt
  compact and spends the audit budget on the code itself.
- **Model heterogeneity.** The audit runs in the read-only Reviewer role, so an
  analytical model can audit while a fast model drives Develop.

### 3.3 Storage and identity

- **One file per session.** `.tauqe/reviews/{uuid7}.json`, so one session cannot
  corrupt another.
- **Chronological identity.** A UUIDv7 encodes the creation instant in its leading
  bits, so identities are lexicographically time-ordered and collision-resistant: a
  sorted directory listing reproduces creation order.
- **Active pointer.** `.tauqe/reviews/active` records the last inspected session for
  client reconnect. It is a view pointer only and never steers a mutation.
- **Auto-migration.** A legacy single-file dump is migrated into per-session files
  once at daemon startup, so ordinary reads stay free of write side effects.
- **Legacy field names.** Older on-disk shapes stay readable: severity for type, body
  for description, resolution notes for note. Migration on read keeps an older audit
  usable without editing it by hand.

### 3.4 Triage and status ownership

Statuses are chosen through the status dialog, strictly separate from execution. A
discussion that concludes a status should change *explains* the change; the developer
applies it. Persisted plan and review state never moves as a side effect of a
conversation, which keeps the Reins invariant intact: durable state changes on an
explicit developer decision or a verified execution.

### 3.5 Post-analysis: from findings to groups

A flat list of findings does not show which defects share a cause. The post-analysis
layer asks the Architect to classify the findings of one stored session at once:

- findings that stand on their own (`independent_issues`);
- clusters that manifest one architectural root cause (`architectural_groups`), each
  naming the cause, the findings that manifest it, and the unified plan that resolves
  them.

The layer references findings by `uuid` and never rewrites them: replacing the
classification discards the previous layer and any remediation horizon it carried,
while every finding keeps its identity and status. Regrouping materializes the `local`
group unconditionally, so a classified session keeps one uniform shape in which every
finding belongs to exactly one group. Each cluster becomes an `Architectural` group
carrying the root cause in `description`, and records the Architect configuration that
produced its horizon in `subplan_architect`.

An `architecture` finding is a deterministic requirement rather than a label: it is
planned before it is executed, and the only sanctioned carrier of a plan is an
architectural group. The classification therefore cannot decide otherwise. A finding
it names independent, or does not name at all, receives a single-element architectural
group minted after the clusters, numbered in the stored order of the findings and
titled after the finding itself, so no group of the layer is ever empty and `local`
stays strictly a group of independent findings that need no plan. Grouping is the duty
of this stage, not a manual decomposition ritual, and the execution queue refuses an
`architecture` finding that reached a simple or unknown group instead of substituting a
bare turn (§3.7).
`PostAnalysisMode` decides when the layer is produced:

| Mode | Behaviour |
| --- | --- |
| `automatic` | classify once every group of the run has finished |
| `manual` | classify only when the developer asks after the groups finish |
| `none` | never classify; the findings stay a flat list |

`Shift+A` runs the classification manually over the session the cursor rests on, so a
stored audit can be classified later or re-analyzed after new findings. The request is
refused while a turn, an audit, or another classification is in flight, and the layer
it produces replaces the previous one wholesale.

### 3.6 Discussion

`d` anchors the REPL dialogue on one finding, on one tree section, or on the whole
session and opens the review discussion contour. A review focus deliberately keeps the
editing surface: the developer holds the full finding context and can ask for the fix
to be applied directly, so edits are staged, verified, and committed exactly as in an
ordinary turn, with no splitting across a screen switch.

The prompt presents every `readonly_files` path as an immutable contract, and it
states the status rule: proposed status changes are explained, never applied. See
*Discussion Focus* for the anchoring mechanics and the read-only plan variant.

### 3.7 Execution contracts

Everything a batch may run is a **member**: `{session}:{item}` addresses one finding,
and `{session}:g:{group}` addresses one group plan. The member identifier is the
addressing contract the supervisor decodes, so a group member never reaches the
finding path and a finding never reaches the group path.

#### Independent findings and simple groups

`x` or `Enter` on a finding row sends `review/execute` with a session and item
identity. The server marks the finding `InProgress`, preloads every path in `files` as
editable auto context, formats the isolated execution prompt, and runs one bounded
turn with the toolchain gate. **Completion contract:** the finding becomes `Fixed`
only if the turn succeeds *and* actually applies verified changes; the turn's edit
summary is recorded in `note` when the finding has none.

A finding of a simple group is resolved exactly the same way. `x` on the header of a
simple group queues its actionable findings as separate isolated turns: its defects
are independent, so grouping them changes presentation, never execution.

Before any token is spent, a request is refused when:

- the finding is in `DISCUSSION` — triage has not happened (`ITEM_IN_DISCUSSION`);
- the finding is `REJECTED` — a false positive is not work (`ITEM_REJECTED`);
- the finding is already `FIXED` or `SKIPPED` (`NOTHING_TO_EXECUTE`);
- the finding belongs to an architectural group — the group plan is the unit of
  remediation (`ARCHITECTURAL_GROUP_EXECUTION_REQUIRED`);
- the finding is an `architecture` finding with no architectural group to plan it — the
  missing piece is the group, not the work, so the refusal names the post-analysis
  (`ARCHITECTURAL_GROUP_EXECUTION_REQUIRED`).

The isolated prompt speaks the vocabulary of the active protocol. The XML contour
carries its marker-suffixed completion, skip, block, and discovery tags, while the
structured contour names its response fields instead — `outcome` for completion or a
declared skip, `blocked` for a conflict with the fixed decision, and
`context_requests` for discovery — and never advertises a tag it would not emit.

The prompt fixes the decision on two levels. The `IsolatedStep` system contract
states that the resolving turn owns the implementation only, and the execution prompt
restates it beside the finding: the designed data structures, the interfaces and
contracts the finding relies on, and the components it selected are not the turn's to
replace, while every path in `readonly_files` is a protected contract that is read and
never modified.

**A blocked finding.** A turn that cannot resolve the finding inside that decision —
because the fix would need a different data structure, a changed interface, a
substituted component, or a relaxed invariant — reports the conflict with the blocked
outcome instead of improvising one. The review lifecycle has no separate blocked
status, so the outcome follows the domain-failure path: the finding becomes `Skipped`
with the blocker recorded in `note`, and the queue advances.

**Managed skip.** When the model determines that the defect was already addressed by
earlier work or is not reproducible, it emits the typed skip outcome with a reason —
the skip tag in the XML contour, `outcome` with status `skip` in the structured
contour. The server records the explanation in `note`, transitions the finding to
`Skipped`, and closes the turn cleanly without requiring code changes.

#### Architectural groups

`x` on the header of an architectural group addresses the group itself: the request
carries `group_id` and never an `item_id`, because one root cause is fixed by one
coherent change set. The server resolves the group and the walk executes its unified
`subplan` leaf by leaf, with every leaf step preloading its files and passing the same
pre-flight and verification gates as a single finding.

The whole plan confirms as a binary card and runs on the configured roles: it spans
several bounded turns, so a per-run Coder override would apply to none of them
coherently. A targeted step keeps the Coder selector and the directives editor,
because it is exactly one bounded turn.

**Just-in-time synthesis.** A stored group may not carry a plan yet. Rather than
refusing the run or asking the developer to decompose the finding by hand, the harness
hands the Architect the group description and every finding of the group, receives a
decomposed step graph, persists it on the group, and walks it in the same run. The
decomposition reuses the plan engine instead of a parallel scheduler, because finding
decomposition and plan refinement are the same operation — both ask the Architect for
a horizon — so it inherits the same invariants: target files are preloaded read-only
before the first round, contracts must be requested instead of guessed from contours,
the resilient discovery loop tolerates re-requested files, and a contradiction yields
a blocker. A synthesized horizon that would be dead on arrival — a dependency naming a
step that does not exist, or a step set whose ready set is empty — is refused before it
reaches storage, so the group keeps its previous plan and the Architect receives the
typed `INVALID_PLAN_TREE` diagnostic instead of a generic save failure.

**A targeted step.** `x` on a step of the group plan sends `group_id` together with
`step_id` and runs exactly that leaf step. The target is validated before the walk
starts: a group without a plan refuses a step target with `INVALID_PARAMS`, because
the horizon does not exist yet, and the shared plan scope evaluator refuses a step
that is `DONE`, `CANCELLED`, `BLOCKED`, blocked by unfinished dependencies or
blockers, or still in `DISCUSSION`. A review finding never accepts a step target: it
executes as one isolated turn and owns no plan.

#### Session batch

`x` on a session header queues every actionable finding of the session plus one group
member per architectural group that owns at least one actionable finding. The group
member takes the position of the group's first actionable finding, so the traversal
order a client rendered is preserved, and the confirmation card states both halves of
the queue: the per-finding work and the number of root-cause plans. Findings still in
`DISCUSSION` are not queued, and a session with nothing actionable is refused with
`NOTHING_TO_EXECUTE`. An `architecture` finding that no architectural group carries is
scheduled by neither form: it is refused, and a queue that would hold only such findings
reports how many await the post-analysis instead of claiming the session has no work.

#### Section batch

`x` on a section — a root-cause group or the independent-issues group — queues the
statuses that can still produce work. The audited selection travels on the wire as an
explicit, client-ordered list of finding ids, so the run can never silently widen to
the whole session behind a dialog that promised less. Ids that went stale between the
confirmation and the launch — a finding already fixed, skipped, or sent back to
discussion — are dropped instead of executed, and an empty selection is refused before
any token is spent. Provenance groups keep the session-wide scope of their header;
sections fold like every other tree node, and `d` on one anchors the discussion on the
findings it renders.

#### Cross-session batch

`X` queues every actionable finding and every actionable root-cause plan across all
sessions, in `{session}:{item}` and `{session}:g:{group}` order, advancing the active
session pointer as it moves, with an aggregated confirmation that states the total
scope first. It terminates with one aggregated batch-finished event.

### 3.8 The lifecycle of a group plan

The unit of completion of an architectural group is its **whole plan**, and the
status of its findings follows that unit:

1. **Before the plan starts** every finding of the group is `Todo`.
2. **When the walk starts** the group and every finding it collects move to
   `InProgress`, so the tree shows one unit of work in flight instead of several rows
   pretending to progress independently.
3. **When the last step is `Done` and verified** the findings of the group transition
   to `Fixed` atomically, and `note` records the executed root-cause plan, so the
   remediation stays inspectable after the plan itself scrolls out of view.
4. **When a step fails or blocks** the completed steps keep their `Done` status and
   their verified changes, the remaining horizon is replanned by the Architect, and
   the walk continues around the failure. `develop.max_replans` bounds the waves.

Planning material is exactly the set of open findings. The Architect synthesizes the
horizon from the group description and its `Todo` findings, and a replan uses the same
set: a finding the developer already resolved, skipped, or rejected names work the
walk must not redo, so it never generates a step. `InProgress` is transient execution
state that no planning call observes, because a failing walk returns its unfinished
findings to `Todo` before the Architect is asked again. One server-side filter resolves
that set, so the admission gate, the Architect prompt, the architectural context of a
step, and its file preload all read the same findings, and a group with no open finding
is refused with `NOTHING_TO_EXECUTE` before any provider call.
An exhausted budget, a cancellation, or a blocker reported by the Architect returns
the unfinished findings to `Todo` with the diagnostic report in `note`. Nothing is
thrown away: finished steps and their commits stay in the tree, exactly as the plan
runner keeps completed work. A finding is never marked `Fixed` for work that was
cancelled halfway, and an interrupted member is rolled back to its pre-step
checkpoint.

Escalation follows the same lazy rule as implementation plans. The first replan of a
failing sub-step keeps the current Architect configuration; only a repeated failure of
the same sub-step — recognized through `subplan_last_replanned_step` — advances the
`architect` ladder and writes the new rung onto the group. Escalation saturates on the
last rung, and a configuration absent from project configuration restarts from the base
rung.

### 3.9 Pre-flight verification

Before resolving anything — one finding, one group plan, or a whole batch — the server
runs the same toolchain pipeline the core runs after every fixing turn. A repository
that does not build or whose tests fail is refused synchronously, before any model
request and before any status change, at zero token cost, with the compiler or test
output attached and the statuses untouched. Repositories without a detected toolchain
skip the gate transparently.

### 3.10 Failure classification: skip versus abort

A failure does not stop the queue unconditionally. The server classifies the cause:

- **Infrastructure failure — abort.** A provider or network error, a timeout, a
  missing key, or an explicit cancellation. The fault is external to the member, so
  every further step would hit the same wall.
- **Domain failure — skip and continue.** Unhealed compiler errors, an unappliable
  patch, exhausted retries, a turn that applies nothing without emitting a skip, or a
  semantic blocker. The failed turn is rolled back through the Git checkpoint, the
  finding becomes `Skipped`, the diagnostic is recorded in `note`, and the queue
  advances.

Work resolved earlier in the batch is never rolled back. One hard member therefore
does not block the remaining independent ones, and the skip rationale stays
inspectable in the view.

#### The terminal report

An aborted queue ends with one typed `review/batch_finished` event whose
`BatchFinishSummary` carries the owner, the terminal status, the executed and
scheduled member counts, the elapsed time, and the cause of the stop in `error`. The
cause is a first-class field of the summary, not something a client reconstructs from
member journals, so a failure never degrades into a bare status word.

Members deliberately leave the shared turn open: a member's own error event records
its diagnostic without tearing down the queue, and the supervisor's terminal event is
the single point that settles the turn. The Develop canvas then stops its spinner,
projects the status onto the turn — `completed` onto `Done`, `failed` and `blocked`
onto `Error`, `cancelled` and `stopped` onto `Cancelled` — and writes the first
usable line of `error` into the response pane, exactly as a single failed turn does.
The response pane therefore carries a compact reason, while the whole report stays in
the diagnostic of the member that stopped, which is where a compiler or provider
transcript belongs. The same aggregate raises one in-app notification with the member
counters, the elapsed time, and that compact reason, so a stopped queue is explained
wherever the developer looks.

#### The failed member

The report names the member the stop is attributed to, and the identifier it carries
is the one the queue was minted with: `{session}:g:{group}` is decoded through the
sanctioned group parser and reported as a review group, while `{session}:{item}` is
reported as the finding it addresses. A group member never wears a finding ordinal it
does not have, so no message accuses finding `#0` of a failure that a root-cause plan
hit.

#### The durable diagnostic

A stop is also a durable fact of the session. Before the queue reports an
infrastructure failure, the server writes the reason into the `note` of the finding
the member addressed, under a fixed batch-stop prefix that marks the entry as a queue
diagnostic rather than a resolution note. A note the finding already carried — a
discussion resolution or a skip rationale — is preserved above the new diagnostic,
while a diagnostic left by an earlier stop is superseded instead of accumulating. The
session is published through `review/updated`, so the Review tree explains the stop
immediately and keeps explaining it after the daemon restarts.

A finding cut short this way keeps its `InProgress` status: the queue stopped, the
finding did not fail on its own terms, and a later run resumes it. Expanding the
finding row in the Review tree reveals the recorded reason, which is why the
diagnostic lives on the incomplete finding and not only in the daemon journal.

A group member records its own interruption: a group that exhausted its replanning
budget returns its unfinished findings to `Todo` with the diagnostic in `note`, as
§3.8 describes, and the terminal summary names the group.

Cancellation is classified separately and acts immediately. The batch waits on a
resolving step and on the cancellation channel together, so a stop signal ends the
batch as cancelled without waiting for the current turn to reach its natural end.
Inside the step the channel is read at every phase boundary that starts work — before
verification, between healing rounds, before the staged result is committed — and a
hard stop additionally tears the verification subprocess group down. An interrupted
member is rolled back to its pre-step checkpoint, so a finding is never marked `Fixed`
for work that was cancelled halfway. A soft stop is different: the current member
finishes its turn, verification, and commit before the queue halts, and the footer
marks the pending stop so a graceful drain is distinguishable from an abort.

### 3.11 Multi-file findings and the end of line drift

Binding a finding to one path and a line range fails in practice: coordinates drift
with every preceding edit, and architectural defects routinely span a trait
definition, its implementations, and the caller sites. Two structural decisions
replace it:

1. **A list of files, no coordinates.** A finding declares related paths only,
   harmonizing with plan step files. Legacy single-path findings are migrated on read.
2. **Batch context preloading.** Every declared path is loaded as editable auto
   context before the fixing turn begins, so the model receives the whole surface on
   round zero instead of spending discovery latency on it.

### 3.12 Model assignment and escalation

A finding may carry a role or an explicit model, rendered as a badge beside the
status, so a simple defect runs on a cheap model while a hard one uses a stronger one.
A finding without an assignment resolves to the Coder role, never to whatever model
happens to be selected in Develop.

Resolution is intra-role escalation only. With `models.auto_level_up` enabled, a turn
that fails verification is first rolled back to its pre-step checkpoint and then
restarted on the next coder rung, with discovered files kept in context. When the
ladder is exhausted, the domain-failure policy applies: the tree stays rolled back, the
unit becomes `Skipped` for a finding or the group returns its unfinished findings to
`Todo` with the diagnostic in `note` for a plan, and the batch continues. Escalation
never promotes a turn into the Architect role; a compile failure is a *how* problem.
The Architect ladder advances planning only — a replanned horizon — never
implementation.

### 3.13 The review tree

The Review view renders a session as a tree whose leading indentation is derived from
nesting depth, never from literal whitespace inside a renderer. Three bands describe a
session:

1. **The session band** — the session header, and below it the Executive Summary and the
   group headers.
2. **The plan band** — the unified remediation plan a group owns: one branch header per
   plan, carrying the leaf progress (`[x/y steps]`, cancelled leaves excluded) and the
   Architect configuration that produced it, with its steps rendered strictly deeper
   than the header.
3. **The findings band** — the findings the group manifests, rendered as a separate list
   after the plan branch.

A plan is the execution mechanism of a root cause (`How`) and the findings are the
symptoms that manifest it (`What`), so the tree never flattens them into one list. The
plan branch and the findings sit on the same band below the group header, and neither is
nested inside the other: the steps render deeper than the branch, and the findings form a
sibling band of rows. Folding the branch hides its steps and leaves the findings
untouched, folding the group hides both, and a group whose plan is synthesized just in
time simply gains a branch when the synthesis lands. Executing the group header runs the
whole plan as one binary confirmed card, executing a step row runs exactly that leaf, and
both keep the contracts of §3.7.

### 3.14 Review view commands

| Command | Action |
| --- | --- |
| `n` / `p` (also `↑` / `↓`) | move through sessions, sections, findings, and steps |
| `Home` / `End` (`Alt+<` / `Alt+>`) | first / last row |
| `PgUp` / `PgDn` | page through the tree |
| `Ctrl+N` / `Ctrl+P` | accordion walk to the next / previous finding |
| `Tab` / `Space` | fold or unfold the selected node |
| `a` | fold or unfold the whole tree |
| `Enter` / `x` | execute the selected finding, group, plan step, or session |
| `X` | execute every finding and root-cause plan across all sessions |
| `Shift+A` | classify the selected session with the Architect post-analysis |
| `t` / `s` | open the status dialog |
| `N` | start a new audit of the current context |
| `d` | anchor the discussion on the selected row |
| `f` | set the discussion focus without opening the prompt |
| `Shift+F` | hide or show the findings whose status is terminal |
| `g` | re-read sessions from the daemon |
| `c` / `y` | copy the finding, session, group, or plan step |
| `Delete` | delete the session, with confirmation |
| `Ctrl+L` | recenter the selected row |
| `Esc` / `q` | return to the Develop view |

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a chat about a diff.** The audit produces typed findings with a
  lifecycle, not an opinion.
- **It is not a second opinion from the same context.** Isolation is what makes the
  verdict independent; sharing the Develop history destroys its value.
- **It is not a writer.** The reviewer reports; resolution is a separate coder turn.
- **It is not a line-based tool.** Findings name files, never coordinates, so they
  survive consecutive edits in a batch.
- **It is not an autonomous queue without triage.** `DISCUSSION` blocks execution on
  purpose.
- **It is not a shortcut past the group plan.** An architectural group is resolved by
  its whole plan; executing its findings one by one fixes manifestations and leaves the
  cause in place.
- **It is not one flat list of a plan and its symptoms.** The plan owns a branch of its
  own; mixing its steps with the findings it fixes erases which band is the mechanism and
  which is the manifestation.
- **It is not planning by closed findings.** A finding already resolved, skipped, or
  rejected never generates a step: only the open findings of a group are planning
  material.- **It is not per-item execution inside an architectural group.** A member finding of
  such a group is refused, not silently scheduled, and the batch subsumes it into its
  group member.
- **It is not an isolated turn for an `architecture` finding.** Its type means it is
  planned before it is executed, so the post-analysis gives it an architectural group and
  the queue refuses the bare turn instead of improvising one.- **It is not a manual decomposition ritual.** The harness synthesizes the horizon
  just in time; there is no hand-run decompose command and no intermediate status
  waiting for one.
- **It is not a re-audit by the fixer.** The finding and the group description state
  what must hold; a resolving turn implements it and reports a conflict through the
  blocked outcome rather than redefining the finding.
- **It is not a status mutator.** Discussions explain proposed changes; the developer
  applies them.
- **It is not a way to weaken verification.** Pre-flight and per-turn gates come from
  project configuration, not from the model.
- **It is not a batch that hides failure.** A domain failure becomes a recorded skip;
  an infrastructure failure aborts the queue and is reported through the typed
  summary, the settled turn, and the durable diagnostic on the finding.

---

## 5. Related Chapters

- [Local Plans](plans.md) — the plan engine a group horizon reuses.
- [Discussion Focus](../ch02-perception/discussion-focus.md) — anchoring a dialogue on
  a finding.
- [Model Roles and the Coder Ladder](../ch01-foundations/model-roles.md) — the
  read-only Reviewer mandate and intra-role escalation.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — the
  turn a finding resolution runs.
- [Structured Diagnostics and Verification Healing](../ch03-project/verification-healing.md) —
  how a failing gate becomes typed diagnostics.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — the checkpoint a
  failed finding or group step returns to.
- [The Context Model](../ch02-perception/context-model.md) — why a target
  pattern widens one run without mutating the developer's layer.
- [Keyboard-First Interface](../ch05-system/tui-ux.md) — the authoritative command
  matrix and the batch cancellation dialog.
