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
9. **The post-analysis triages every finding before it is executed.** The routing is
   the first lifecycle decision: an unambiguous defect becomes `Todo`, and a finding
   whose resolution depends on a decision only the engineer can make stays `Discussion`
   together with the question that must be answered.
10. **Human triage gates autonomous execution.** A finding in `Discussion` is never
    executed.
11. **Status is developer-owned, and the dialogue records the decision.** A discussion
    never rewrites persisted review state on its own initiative: it records the decision
    the engineer states in the dialogue through the typed `review_update` payload. It can
    never declare a finding `Fixed`, because a fix becomes true only when the harness
    verifies it and commits the change.
12. **Verification precedes execution.** A repository that does not build is refused
    before any token is spent.
13. **A plan is a branch, not a symptom.** The unified plan of a group owns its own
    branch below the group header; its steps and the findings it fixes are separate
    bands of the tree, never one flat list.
14. **Only open findings are planning material.** The Architect synthesizes and
    reprojects a group horizon from the findings that are still `Todo`; a finding the
    developer already closed never generates a step.
15. **An `architecture` finding is executed only through a plan.** Its type is a
    deterministic requirement: the post-analysis gives it an architectural group, and
   the execution queue refuses it as a bare turn.
16. **A group is ready only when its questions are settled.** An architectural group
   whose findings still pose an open question — a finding in `Discussion` without a
   recorded engineering decision — is not ready for an autonomous run. The implicit
   queues defer such a group whole and count its findings as deferred, and an explicit
   launch is refused with the typed `GROUP_UNRESOLVED_QUESTIONS` diagnostic naming the
   findings that keep it unready. A recorded decision answers the question and lifts
   the deferral.
17. **An Architect failure is confined to its group.** A decomposition that cannot
   produce a horizon — a reported blocker, an exhausted discovery budget, or a payload
   that does not parse into a plan — follows the ordinary local-failure path: the
   diagnostic lands in the `note` of the findings the group did not finish, those
   findings return to `Todo`, and the member is counted as a local failure, so a night
   run is never lost to one group whose cause the Architect could not decompose.
18. **A reverted failure is a local skip; a dirty tree is the only stop.** A member that
   fails in code — unhealed compiler errors, a failing test, an unappliable patch, an
   exhausted retry budget, a semantic blocker — or that loses its transport is a local
   failure as long as its Git transaction was rolled back to the pre-step checkpoint:
   the diagnostic is kept in the session, the member is counted as a local failure, and
   the queue continues. The single fatal condition is a working tree that still carries
   unverified bytes (`turn_left_unreverted_changes`), because the next member would be
   built on lines nobody verified.
19. **The night run describes itself.** The terminal `review/batch_finished` event and
   the `BatchFinishSummary` it carries state what the queue achieved: the findings it
   repaired, the findings the readiness gate deferred for an engineering decision, and
   the members that failed locally with their diagnostics preserved. A morning review
   reads one typed record instead of replaying a journal.
20. **A blocker belongs to the group that has to resolve it.** A cause the Architect
    could not decompose, an exhausted discovery budget, and an exhausted replanning
    budget are recorded on the group itself instead of being scattered across the
    notes of its findings, so the diagnosis survives a reload and names the unit of
    work that is stopped.
21. **A recorded decision is a settled requirement.** Recording a decision clears the
    blocker, stamps the decision onto every finding the group collects, and moves the
    whole group to the decided status in one atomic write; an Architect that reports
    the same question again is refused with the typed `UNEXPECTED_STATUS` error
    instead of being answered.
22. **A running remediation does not narrate itself through finding statuses.** While
    a group horizon or a degenerate single-finding horizon runs, the findings keep the
    statuses the developer gave them: the horizon, not the status field, is the record
    of progress. A group closes once, when its aggregate status reaches `Done`, and
    that completion is one write under the storage lock: every open finding becomes
    `Fixed`, the manual verdicts `Skipped` and `Rejected` stay exactly as they are, and
    the blocker is cleared by the same write.

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
| `blocked_reason` | the diagnosis of the cause that currently stops the group |
| `resolution_notes` | the decision the engineer recorded for the group |
| `kind` | the class that decides how the group executes |
| `implementation.items` | the unified remediation horizon of the group |
| `implementation.architect` | the configuration that produced the horizon |
| `implementation.last_replanned_step` | the most recently replanned step |

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
| `note` | the decision recorded in discussion, a skip rationale, or a diagnostic |
| `model` | optional role or model override for resolution |

The `implementation` a review group owns is the same nested `PlanImplementation` a
local plan owns (*Local Plans* §2.1), so the algebra of a horizon — readiness by
dependency, aggregate status, leaf progress, leaf-only file normalization, structural
validation — is defined once and shared by both workflows. A plan and a group differ
in *what* they execute, never in how the step graph is represented or evaluated.

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

**Why a decision is recorded, not inferred.** When the resolution of a finding depends
on a choice only the engineer can make, the analysis leaves it in `DISCUSSION` and
states the question. The answer belongs to the engineer, so it is recorded as a
decision — the status the finding moves to, the reasoning behind it, and the
remediation agreed in the dialogue — and that decision becomes the binding directive the
Architect designs inside. Nothing in the chain infers the answer from the shape of the
finding.

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

- **DISCUSSION** — the initial state, and the routing the post-analysis chooses for a
  finding whose resolution is an open decision. Human triage, feasibility assessment, or
  architectural discussion is required; it cannot be executed autonomously.
- **TODO** — approved, executable, and the only status that feeds planning: the
  Architect synthesizes and reprojects a group horizon from the open findings alone.
- **IN_PROGRESS** — the developer's own marker that a finding is being worked on. The
  harness never assigns it: a running remediation reads its progress from the horizon,
  so the status of a finding stays exactly where the developer put it until the group
  closes.
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

### 2.6 The blocker and the decision a group carries

An architectural group is a statement about a cause, so it is also where the fate of
that cause is recorded. Two fields carry it, and neither is derived from the findings
the group collects:

- **`blocked_reason`** — the diagnosis of a cause the harness could not remove: a
  blocker the Architect reported, an exhausted discovery budget, an exhausted
  replanning budget, or an invariant conflict that stalled the plan. The diagnosis
  belongs to the group because the group is the unit of work that is stopped;
  restating it in the note of every symptom would state it once per manifestation and
  lose it as soon as a finding was closed.
- **`resolution_notes`** — the decision the engineer recorded, in their own words. It
  outlives the blocker it resolved, so a later reader still sees why the plan of the
  group took the shape it took, and the next decomposition reads the decision as a
  requirement instead of reopening the question.

A group is therefore *ready*, *blocked*, or *decided*, and that state is read from the
group's own record rather than reconstructed from its findings. The blocker is cleared
when a horizon is synthesized and again when the group plan completes, because a group
that owns an executable plan is no longer stopped.

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
produced its horizon in `implementation.architect`.

**The same payload triages every finding.** `triage` is the routing layer of the
classification: the analysis names the status each finding's resolution starts from, so a
defect whose correct behavior the code already decides is queued for execution (`todo`),
while a finding whose resolution depends on a decision only the engineer can make stays
`discussion` together with the question that must be answered. Routing never rewrites a
finding that already earned its state: one that is being executed, or that already
reached a terminal outcome, keeps it, and re-running the analysis never reopens a
decision that was already taken.

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
carries the decision contract of the anchored dialogue.

**A decision is recorded, and it is applied atomically.** The structured contour carries
the decision as a typed `review_update` field of the dialogue payload, naming the finding
(`item_id`), the status the engineer chose (`todo`, `skipped`, or `rejected`), the
reasoning behind it (`note`), and the remediation agreed in the dialogue
(`recommendations`). The dialogue is where the engineer actually decides what a finding
is worth, so the decision travels with the dialogue payload: the storage engine applies
it under its own lock at the round that decoded it, the session is published
immediately, and the transition is durable even when the rest of the turn fails. A
recommendation the finding already carries is never duplicated, and a finding the named
session does not hold is reported as absent rather than invented.

The vocabulary is deliberately narrower than the lifecycle. A discussion may queue a
finding for execution (`todo`), dismiss it as not reproducible (`skipped`), or reject it
as a false positive (`rejected`); it can never declare a finding `fixed`, because a fix
becomes true only when the harness verifies it and commits the change. See *Discussion
Focus* for the anchoring mechanics and the read-only plan variant.

**A decision taken for a group is recorded on the group.** When the anchored entity is
an architectural group, the same payload names the group (`group_id`) beside the
finding whose text carried the discussion, and the storage engine applies the decision
to the whole group instead of to one symptom: `blocked_reason` is cleared,
`resolution_notes` records the decision once, the same note is stamped onto every
finding the group collects, and the whole group moves to the decided status. The
transition is one atomic write of the session file, so a reader never observes a group
that is unblocked while its findings still wait, or findings that moved without their
cause. A decision must carry its own reasoning: recording that something was decided
without saying what is refused, because the next decomposition would inherit a blocker
it cannot interpret.

### 3.7 Execution contracts

Everything a batch may run is a **member**: `{session}:{item}` addresses one finding,
and `{session}:g:{group}` addresses one group plan. The member identifier is the
addressing contract the supervisor decodes, so a group member never reaches the
finding path and a finding never reaches the group path.

#### Independent findings and simple groups

`x` or `Enter` on a finding row sends `review/execute` with a session and item
identity. The request resolves to `ExecutionScope::ReviewFinding { review_id, item_id }`,
which builds a degenerate one-step horizon in memory: the finding is the single leaf, its
`files` are the files that leaf targets, its `readonly_files` are the contracts it reads,
and its description is the context of the step. The horizon is never persisted, so
launching a finding writes no status — the finding keeps the status the developer gave it
for the whole turn, and the shared pipeline executes this one leaf with the same
pre-flight and verification gates as any other step. **Completion contract:** the finding
becomes `Fixed` only if the turn succeeds *and* actually applies verified changes; the
turn's edit summary is recorded in `note` when the finding has none.

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
coherent change set. The request resolves to
`ExecutionScope::ReviewGroup { review_id, group_id }`, whose frame carries the group's own
`implementation` and renders every finding of the group into the concept tree, so the
description of each symptom reaches the context of every step it resolves. The shared
pipeline then walks the horizon leaf by leaf, with every step preloading its files and
passing the same pre-flight and verification gates as a single finding, while the finding
statuses stay frozen for the whole walk. The readiness gate runs before the
walk: while one of the group's findings still poses an open question, an explicit launch
is refused with the typed `GROUP_UNRESOLVED_QUESTIONS` diagnostic naming those findings,
and a targeted sub-step never bypasses that refusal.

The whole plan confirms as a binary card and runs on the configured roles: it spans
several bounded turns, so a per-run Coder override would apply to none of them
coherently. A targeted step keeps the Coder selector and the directives editor,
because it is exactly one bounded turn.

**Just-in-time synthesis.** A stored group may not carry a plan yet. Rather than
refusing the run or asking the developer to decompose the finding by hand, the
harness hands the Architect the group description and every finding of the
group, receives a decomposed step graph, persists it on the group, and walks it
in the same run. The decomposition reuses the plan engine instead of a parallel
scheduler, because finding decomposition and plan refinement are the same
operation — both ask the Architect for a horizon — so it runs through the same
structured Architect contour with the same invariants: target files are
preloaded read-only before the first round, contracts must be requested instead
of guessed from contours, the resilient discovery loop tolerates re-requested
files, an unparsable payload is healed within the rung's budget, and a
contradiction yields a blocker. A synthesized horizon that would be dead on
arrival — a dependency naming a step that does not exist, or a step set whose
ready set is empty — is refused before it reaches storage, so the group keeps its
previous plan and the Architect receives the typed `INVALID_PLAN_TREE`
diagnostic instead of a generic save failure. A synthesis that ends in a blocker
— a reported contradiction, an exhausted discovery budget, an exhausted rung
budget, or a payload that never satisfies the schema — is never fatal to the
run: the diagnosis is stored on the group itself, the findings the group did not
finish return to `Todo` with the diagnostic in `note`, and the member is counted
as a local failure, so the queue advances to its next member (§3.8).

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
`NOTHING_TO_EXECUTE`. A group the readiness gate defers takes no position in the
traversal: it is not queued as a member of its own, none of its findings is addressed by
the run, and the findings it collects are counted as deferred. An `architecture` finding
that no architectural group carries is scheduled by neither form: it is refused, and a
queue emptied by the gate or by such findings names what is missing — the engineering
decision that postpones the deferred findings, or the post-analysis that plans the
architecture findings — instead of claiming the session has no work.

#### Section batch

`x` on a section — a root-cause group or the independent-issues group — queues the
statuses that can still produce work. The audited selection travels on the wire as an
explicit, client-ordered list of finding ids, so the run can never silently widen to
the whole session behind a dialog that promised less. Ids that went stale between the
confirmation and the launch — a finding already fixed, skipped, or sent back to
discussion — are dropped instead of executed, and an empty selection is refused before
any token is spent. Provenance groups keep the session-wide scope of their header;
sections fold like every other tree node, and `d` on one anchors the discussion on the
findings it renders. A root-cause section inherits the readiness of its group: while a
finding of that group still poses an open question, the section contributes no member and
the findings it renders are counted as deferred.

#### Cross-session batch

`X` queues every actionable finding and every actionable root-cause plan across all
sessions, in `{session}:{item}` and `{session}:g:{group}` order, advancing the active
session pointer as it moves, with an aggregated confirmation that states the total
scope first. A root-cause plan the readiness gate defers contributes no member and its
findings are counted as deferred, so a cross-session run crosses out of a group whose
question is still open instead of executing it. It terminates with one aggregated
batch-finished event.

### 3.8 The lifecycle of a group plan

The unit of completion of an architectural group is its **whole plan**, and the
status of its findings follows that unit:

1. **Before the horizon starts** the findings of a runnable group are `Todo`: the
   readiness gate has already settled every open question, so no finding carries an
   inferred intermediate state.
2. **While the walk runs** those statuses stay frozen. The horizon, not the status
   field, is the record of progress: the tree already shows which step is running, which
   steps are `Done`, and which leaves remain, and a status that narrated every step
   would claim progress no verification has confirmed. The findings stay part of the
   context of every step through the group concept tree.
3. **When the last step is `Done` and verified** the group closes in one write: every
   open finding — `Discussion`, `Todo`, or `InProgress` — becomes `Fixed`, a finding
   that carries no note of its own receives the executed horizon as its resolution
   record, the manual verdicts `Skipped` and `Rejected` and an earlier `Fixed` stay
   exactly as they are, and the blocker is cleared by the same write. The remediation
   therefore stays inspectable after the plan itself scrolls out of view.
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

**A recorded decision is a settled requirement.** A decision the engineer recorded in
review discussion is no longer an open question, so the Architect receives it as a
binding directive beside the findings it decomposes: it designs inside the decision and
never restates it as a doubt, a blocker, or a reason to wait for guidance. A group whose
findings all carry a decision is settled, and a decomposition that reports `blocked` for
such a group is refused with a typed `UNEXPECTED_STATUS` error before the verdict
reaches storage, so a replanning wave can never reopen a decision the engineer already
took.
A synthesis that ends in a blocker or in an exhausted budget is a diagnosis of
the group, so it is recorded on the group itself in `blocked_reason`, and the
findings the group did not finish return to `Todo` with the diagnostic report in
`note`. A cancellation returns them the same way with the cancellation recorded.
A failed synthesis never stops the queue on its own: the member is counted as a
local failure and the walk advances to the next member, so one group whose cause
the Architect could not decompose never costs the run its other groups.
Nothing is thrown away: finished steps and their commits stay in the tree, exactly as the
plan runner keeps completed work. A finding is never marked `Fixed` for work that was
cancelled halfway, and an interrupted member is rolled back to its pre-step checkpoint.
The member stops the whole queue only when that rollback could not restore the working
tree.

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

The gate runs while the operation slot stays registered, and it inherits that
operation's cancellation channel. A hard cancel or a shutdown drain therefore tears
the verification subprocess group down instead of waiting the build or test suite
out, and the slot is released within a bounded interval: the request is answered
when the teardown completes, not when the suite would have finished. The gate
reports that outcome as a cancellation, never as a verification failure of a suite
that never ran to completion.
### 3.10 Failure classification: local skip versus fatal stop

A failing member does not stop the queue by itself. The queue asks one question: did the
member's Git transaction roll back to its pre-step checkpoint, so that the working tree is
exactly what the previous member committed?

- **Reverted failure — skip and continue.** The rollback restored the tree, so there is
  nothing unverified to build the next member on. A domain failure (unhealed compiler
  errors, an unappliable patch, exhausted retries, a turn that applies nothing without
  emitting a skip, a semantic blocker) belongs here, and so does a member that produced no
  result at all (a provider or network error the transport already retried, a timeout, a
  missing credential). The diagnostic is recorded in the `note` of the finding the member
  addressed — the finding closes as `Skipped` when the failure is a content verdict — and
  the member is counted as a local failure while the queue advances. A transport failure
  consumes no tier attempt, because the network layer already retried it transparently and
  a member that lost its transport produced no result to judge.
- **Unreverted failure — stop.** The rollback could not restore the working tree
  (`turn_left_unreverted_changes`), so it still carries the bytes of an aborted attempt.
  Retrying or replanning on a dirty tree would build the next member on lines nobody
  verified, so this — and only this failure — halts the queue. A cancellation is a
  deliberate end of the run, not a failure verdict, and it is classified separately.

Work resolved earlier in the batch is never rolled back. One hard member therefore
does not block the remaining independent ones, and the failure rationale stays inspectable
in the view.

#### The terminal report

A finished queue — completed, gracefully stopped, or aborted — ends with one typed
`review/batch_finished` event whose `BatchFinishSummary` carries the owner, the terminal
status, the executed and scheduled member counts, the elapsed time, the cause of a stop in
`error`, and the counters of the run: the findings the queue repaired, the findings the
readiness gate deferred for an engineering decision, and the members that failed locally
while the queue continued. The cause is a first-class field of the summary, not something a
client reconstructs from member journals, so a failure never degrades into a bare status
word. The counters are published the same way, so a morning review reads what the run
achieved — and exactly which findings still wait for a decision — without replaying a
single member journal. A counter a queue cannot classify stays absent instead of being
invented.

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

A local failure is also a durable fact of the session. The server writes the reason into
the `note` of the finding the member addressed, under a fixed diagnostic prefix that marks
the entry as a queue diagnostic rather than a resolution note. A note the finding already
carried — a discussion resolution or a skip rationale — is preserved above the new
diagnostic, while a diagnostic left by an earlier failure is superseded instead of
accumulating. The session is published through `review/updated`, so the Review tree
explains the failure immediately and keeps explaining it after the daemon restarts.
A failed member leaves its diagnostic on the finding rather than only in the daemon
journal: expanding the finding row in the Review tree reveals why the member did not
complete, so the morning review reads the reason where the work is, and the next run finds
the finding in exactly the state the failure left it.
A group member records its own interruption: a group that exhausted its replanning
budget returns its unfinished findings to `Todo` with the diagnostic in `note`, as
§3.8 describes, and the terminal summary names the group and counts the member as a local
failure instead of stopping the run.
Cancellation is classified separately and acts immediately. The queue is awaited
cooperatively: the supervisor hands the cancellation and soft-stop channels to the
member and keeps awaiting it, instead of racing the queue against a cancellation
branch and dropping the unfinished future. A hard cancel observed while a member
runs therefore unwinds the queue through that member itself — the provider races
the signal at every await and the toolchain tears its verification subprocess group
down at the cancel boundary — and the operation slot is never released while a
spawned task is still running. Dropping the future instead would detach the task
the member had already spawned and release the slot together with its abort handle,
leaving an unsupervised task holding the session lock and possibly an open Git
transaction. Inside the member the channel is read at every phase boundary that
starts work — before verification, between healing rounds, before the staged result
is committed. A cancel that no running phase observes on its own is resolved by the
deferred-abort watchdog, the sanctioned force-termination path: it defers the abort
while the task still holds an open Git transaction and aborts once that transaction
has closed. An interrupted member is rolled back to its pre-step checkpoint, so a
finding is never marked `Fixed` for work that was cancelled halfway. A soft stop is
different: the current member finishes its turn, verification, and commit before
the queue halts, and the footer marks the pending stop so a graceful drain is
distinguishable from an abort.

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
that fails verification is first rolled back to its pre-step checkpoint and re-asked
on the same coder rung, up to `develop.max_tier_attempts` attempts, with discovered
files kept in context. Only an exhausted tier advances one rung, and the new rung
starts with a fresh budget of its own. When the ladder itself saturates, the
domain-failure policy applies: the tree stays rolled back, the unit becomes `Skipped`
for a finding or the group returns its unfinished findings to `Todo` with the
diagnostic in `note` for a plan, and the batch continues. Escalation never promotes a
turn into the Architect role; a compile failure is a *how* problem. The Architect
ladder advances planning only — a replanned horizon — never implementation, and it
spends the generation budget its own rung declares before it moves to the next
one.

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

A plan step is projected from the shared `PlanImplementation` exactly as a local plan is:
leaf progress, the exclusion of cancelled leaves, and folding are computed by the same
algebra in both views, so a group step and a plan step cannot diverge in how they are
shown or navigated.

A group that cannot proceed states it on its own row. A blocked group carries the
`[BLOCKED]` badge beside its title, and unfolding it opens with the architectural
fork: the diagnosis, the chords that reach its resolution (`d` to discuss the cause,
`t` to record the decision), and the decision already recorded. The fork precedes the
root cause description, because a stopped cause has to be settled before its plan can
be read as executable. A group whose findings are all filtered out keeps that body
reachable: the fold icon reflects the fork, so the diagnosis is never hidden behind a
missing icon.

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
  purpose, and the post-analysis routes every finding before the queue ever sees it.
- **It is not a second execution engine.** A group horizon and a single finding resolve
  into an `ExecutionScope` and run through the same pipeline that executes a plan; the
  review supervisor only selects members, addresses them, and reports the run, so the
  former review execution runtime and its per-item step runner are removed rather than
  kept dormant behind a flag.
- **It is not a per-step status feed.** A running remediation does not stamp findings as
  it advances: the horizon is the progress record, the findings keep the statuses the
  developer gave them, and the group closes once.

- **It is not a queue that executes an unresolved group.** A root-cause group whose
  findings still pose an open question is deferred whole, and the run never answers the
  question on the engineer's behalf.
- **It is not a blocker buried in a note.** A cause that cannot be decomposed is
  recorded on the group itself, so the diagnosis survives a reload and marks the unit
  of work that is stopped instead of being restated once per symptom.
- **It is not a decision that can be reopened.** Recording a decision clears the
  blocker and stamps it onto every finding of the group; an Architect that reports the
  same question again is refused instead of answered.
- **It is not a shortcut past the group plan.** An architectural group is resolved by
  its whole plan; executing its findings one by one fixes manifestations and leaves the
  cause in place.
- **It is not one flat list of a plan and its symptoms.** The plan owns a branch of its
  own; mixing its steps with the findings it fixes erases which band is the mechanism and
  which is the manifestation.
- **It is not planning by closed findings.** A finding already resolved, skipped, or
  rejected never generates a step: only the open findings of a group are planning
  material.
- **It is not per-item execution inside an architectural group.** A member finding of
  such a group is refused, not silently scheduled, and the batch subsumes it into its
  group member.
- **It is not an isolated turn for an `architecture` finding.** Its type means it is
  planned before it is executed, so the post-analysis gives it an architectural group and
  the queue refuses the bare turn instead of improvising one.
- **It is not a manual decomposition ritual.** The harness synthesizes the horizon
  just in time; there is no hand-run decompose command and no intermediate status
  waiting for one.
- **It is not a re-audit by the fixer.** The finding and the group description state
  what must hold; a resolving turn implements it and reports a conflict through the
  blocked outcome rather than redefining the finding.
- **It is not a status mutator on its own initiative.** A discussion records the
  decision the engineer states in the dialogue; it never invents a status the engineer
  did not choose, and it never declares a fix.
- **It is not a way to weaken verification.** Pre-flight and per-turn gates come from
  project configuration, not from the model.
- **It is not a batch that hides failure.** A reverted failure becomes a recorded local
  skip with its diagnostic on the finding, and the queue advances; only an unreverted
  working tree aborts the run, and that stop is reported through the typed summary, the
  settled turn, and the durable diagnostic on the finding.
- **It is not an infinite retry of one rung.** A member spends its configured
  `develop.max_tier_attempts` attempts on the current coder rung before the ladder
  moves, and a saturated ladder ends in the domain-failure policy rather than another
  attempt.
- **It is not a burned rung per broken connection.** A transport failure consumes no
  attempt and advances no ladder, because the cause is external to the work; it is a local
  skip like any other cleanly reverted failure.

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
