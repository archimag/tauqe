# Structured Code Review

Code review in TAUQE is a structured engineering discipline, not a conversation
about a diff. An isolated analytical pass inspects the selected files and emits typed
findings; the developer triages them; approved findings are resolved in isolated
bounded turns. The Review workspace is where that evidence becomes visible and where
a finding is promoted into an executable plan.

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
2. **Finding resolution** — each approved finding is resolved in its own bounded
   turn by the Coder, with pre-flight verification, its target files preloaded, and
   intra-role escalation.

The audit never reuses the Develop conversation, and the fix phase never escalates
into the Architect role.

### 1.3 The invariants

1. **The audit is isolated.** No Develop history, no repository map — only the
   selected file content, so no confirmation bias enters the verdict.
2. **The audit never writes.** It runs in the read-only Reviewer role.
3. **A defect and its remediation are separate statements.** What is wrong and what
   fixes it are distinct fields.
4. **Line coordinates are never stored.** Findings name files, not fragile line
   ranges.
5. **A finding is resolved only in a verified bounded turn.** Marking `Fixed` is tied
   to the verified result of the turn, not to an opinion.
6. **The audit fixes *what*; the fix owns *how*.** A resolving turn never re-audits
   the finding. Substituting the designed structure, the interfaces the finding
   relies on, or an invariant it states is not part of resolving it.
7. **Human triage gates autonomous execution.** A finding under discussion is never
   executed.
8. **An undecomposed *what* is never executed.** A finding awaiting decomposition is
   refused, not guessed at.
9. **Status is developer-owned.** A discussion explains a proposed change; it never
   rewrites persisted review state.
10. **Verification precedes execution.** A repository that does not build is refused
    before any token is spent.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Session and finding

A **review session** is a persisted audit. A **finding** (`ReviewIssue`) is one
defect inside it, deliberately split into a diagnosis and a remediation.

| Session field | Meaning |
| --- | --- |
| `id` | UUIDv7 identity |
| `title` | concise human-readable session title produced by the reviewer |
| `created_at` | the instant the audit was produced |
| `model` | the model that actually performed the audit |
| `description` | the executive summary, falling back to the request prompt |
| `user_prompt` | the raw audit instructions |
| `target_files` | the audited repository-relative paths |
| `items` | the discrete findings |
| `items` | the discrete findings |
| `groups` | the isolated slices the audit ran in, in launch order |
| `post_analysis` | the Architect classification layer, when one exists |
| `launch_config` | snapshot of the configuration that launched the session |

| Finding field | Meaning |
| --- | --- |
| `id` | numeric identity inside the session |
| `uuid` | stable UUIDv7 identity used by the post-analysis layer |
| `group` | identifier of the group that produced the finding |
| `title` | concise defect title |
| `type` | the problem category |
| `status` | the lifecycle state |
| `description` | what is wrong, and why |
| `recommendations` | the ordered remediation steps |
| `files` | paths to modify when resolving it |
| `readonly_files` | contracts the fix must rely on but never change |
| `plan` | an optional embedded decomposition plan |
| `note` | resolution note or skip rationale |
| `model` | optional role or model override for resolution |
| `subplan_architect` | the Architect configuration that produced the subplan |

### 2.2 Why the diagnosis and the cure are separate

An undivided finding body conflates the two. A model can then dilute a precise root
cause with generic advice, and a human cannot tell whether the defect was actually
understood. Two independent statements give the developer a clean acceptance
criterion and the Coder a bounded mandate.

**Why `readonly_files` exists.** Some defects can only be fixed safely against an
immutable boundary: a trait definition, a public interface, a serialization format,
an architectural invariant. Naming those artifacts explicitly lets a fix rely on
their guarantees instead of silently changing a contract the rest of the system
depends on. A protected contract is context, never a target.

### 2.3 Problem categories

Findings are classified into categories an engineer can act on, instead of a shallow
severity ladder:

| Type | Meaning |
| --- | --- |
| `bug` | incorrect behavior, panic, data corruption, resource leak |
| `security` | a security flaw or an unsafe boundary |
| `architecture` | a structural or runtime risk that must be decomposed before implementation |
| `performance` | a costly hot path or wasted resources |
| `quality` | maintainability, readability, dead-code cleanup |

A non-actionable informational level is never exposed to the reviewer. The model
emits the actionable category, so the audit vocabulary stays aligned with the work
that can follow.

### 2.4 Status vocabulary

```text
[DISCUSSION] --approve--> [TODO] --> [IN_PROGRESS] --> [FIXED] | [SKIPPED]
      |                      |                        (verified)
      v                      +--broaden--> [REFINE] --Architect--> [TODO] + plan
                            (decomposition needed)
```

- **DISCUSSION** — the initial state. Human triage, feasibility assessment, or
  architectural discussion is required; it cannot be executed autonomously.
- **TODO** — approved and executable.
- **IN_PROGRESS** — a bounded turn is resolving it.
- **REFINE** — broader than one bounded turn. It is a *what* that has not been
  decomposed into *how* steps, so neither the single executor nor the batch runner
  may start it.
- **FIXED** — resolved, committed, and verified by the toolchain.
- **SKIPPED** — already addressed, made obsolete, or non-reproducible; the reason
  stays in `note`.
- **REJECTED** — a false positive, an intentional trade-off, or non-actionable.

**Progress counting.** Session progress is `fixed / total`, where `total` counts only
actionable findings: `SKIPPED` and `REJECTED` are terminal and are excluded from the
denominator. A session whose remaining findings are all resolved therefore reports a
full bar instead of a fraction that can never reach one, and a session with no
actionable findings reports `0/0`.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Starting an audit

`review/start` carries the audit instructions, an optional model selection, and an
optional target directory or glob pattern. The server resolves the pattern against
tracked files and **merges** the matches with the files already in context,
de-duplicated, for that run only. The developer's Context layer is never cleared or
rewritten: a scoped audit can cover code never explicitly added, while control over
the working set stays untouched. Because matched files are read from the working
tree, the audit always inspects current source.

The launch dialog previews the pattern locally before a request is sent, and the
preview is a contract, not a decoration: both the directory list and the match
counter are computed from the repository file list that arrived with the
handshake, never touching disk, Git, or the network. Directories are offered
as sorted, unique prefixes, each terminated with `/`, because that is the
pattern form that selects every path beneath it. The match counter applies the
same `GlobPattern` the server applies, so a preview can never predict a selection
the server resolves differently. A blank pattern narrows nothing and the launch
runs unrestricted; a pattern that matches nothing is an honest zero, and the
launch is refused before a request is sent.

Pattern expansion is bounded by the same context budgets that govern discovery: at
most `context.max_pattern_batch` matched files are loaded per audit, and the
expansion stops at the first file that would push the loaded total past
`context.files_token_budget`. Matches beyond either cap are not inspected. The
`review/started` event reports how many matched files were omitted, the daemon
records a warning, and the interface raises one, so a silently narrow audit is
impossible. A pattern that matches no tracked file is refused outright: launching
anyway would audit a different file set than the one the developer believes the
pattern chose. A partial selection stays a lawful run, reported through the
omitted count.

While the run is in flight, the Review view mirrors its progress in the info
strip. A grouped session audits its slices one at a time, and the strip names
the group that is currently streaming: its one-based position in the
supervisor's sequential traversal, the total group count once the mirrored
session is known, and the group title. The session is published only after
the first group finishes, so the first badge degrades honestly to an ordinal
without a denominator instead of inventing a total. When the Architect
post-analysis layer classifies the findings, the strip raises an analysis
badge; a standalone classification request is a turn of its own and settles
the interface when its terminal event arrives, while the automatic
classification inside a grouped run stays subordinate to the run's own
terminal event. `Shift+A` launches the classification manually over the
session the cursor rests on, so a stored audit can be classified later or
re-analyzed after new findings: the request is refused while a turn, an
audit, or another classification is in flight, and the layer it produces
replaces the previous one wholesale while every finding keeps its identity
and status.

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
  for description, subplan for plan, resolution notes for note. Migration on read
  keeps an older audit usable without editing it by hand.

### 3.4 Triage and status ownership

Statuses are chosen through the status dialog, strictly separate from execution. A
discussion that concludes a status should change *explains* the change; the developer
applies it. Persisted plan and review state never moves as a side effect of a
conversation, which keeps the Reins invariant intact: durable state changes on an
explicit developer decision or a verified execution.

### 3.5 Discussion

`d` anchors the REPL dialogue on one finding or on the whole session and opens the
`d` anchors the REPL dialogue on one finding, on one tree section, or on the
whole session and opens the review discussion contour. A review focus
deliberately keeps the editing surface: the developer holds the full finding
context and can ask for the fix to be applied directly, so edits are staged,
verified, and committed exactly as in an ordinary
of splitting across a screen switch.

The prompt presents every `readonly_files` path as an immutable contract, and it
states the status rule: proposed status changes are explained, never applied. See
*Discussion Focus* for the anchoring mechanics and the read-only plan variant.

### 3.6 Execution contracts

### Single finding

`x` or `Enter` on a finding sends `review/execute` with a session and item identity.
Findings in `DISCUSSION` are refused (triage has not happened), `REFINE` findings are
refused explicitly (`ITEM_IN_REFINE`), and already terminal findings are refused. A
confirmation dialog shows the finding and its target files.

The server then marks the finding `InProgress`, preloads every path in `files` as
editable auto context, formats the isolated execution prompt, and runs one bounded
turn with the toolchain gate. **Completion contract:** the finding becomes `Fixed`
only if the turn succeeds *and* actually applies verified changes; the turn's edit
summary is recorded in `note` when the finding has none.

The isolated prompt speaks the vocabulary of the active protocol. The XML contour
carries its marker-suffixed completion, skip, block, and discovery tags, while the
structured contour names its response fields instead — `outcome` for completion or a
declared skip, `blocked` for a conflict with the fixed decision, and
`context_requests` for discovery — and never advertises a tag it would not emit.

The prompt fixes the decision on two levels. The `IsolatedStep` system contract
states that the resolving turn owns the implementation only, and the execution
prompt restates it beside the finding: the designed data structures, the interfaces
and contracts the finding relies on, and the components it selected are not the
turn's to replace, while every path in `readonly_files` is a protected contract that
is read and never modified.

**A blocked finding.** A turn that cannot resolve the finding inside that decision —
because the fix would need a different data structure, a changed interface, a
substituted component, or a relaxed invariant — reports the conflict with the
blocked outcome instead of improvising one. The review lifecycle has no separate
blocked status, so the outcome follows the domain-failure path: the finding becomes
`Skipped` with the blocker recorded in `note`, and the queue advances. A finding
that carries a subplan is different: a failed sub-step first triggers a replanning
wave bounded by `develop.max_replans`, and only an exhausted budget or a blocked
Architect leaves the finding `Skipped`.

**Managed skip.** When the model determines that the defect was already addressed by
earlier work or is not reproducible, it emits the typed skip outcome with a reason —
the skip tag in the XML contour, `outcome` with status `skip` in the structured
contour. The server records the explanation in `note`, transitions the finding to
`Skipped`, and closes the turn cleanly without requiring code changes.

### Session batch

`x` on a session header queues every `TODO` or `IN_PROGRESS` finding in order. A
session still holding findings in `DISCUSSION` is refused, because human triage is a
precondition, and `REFINE` findings are excluded from the queue rather than silently
executed.

### Section batch

`x` on an architectural section — a root-cause group or the independent-issues
### Section batch (§3.3)
statuses that can still produce work. The audited selection travels on the wire
as an explicit, client-ordered list of finding ids, so the run can never
silently widen to the whole session behind a dialog that promised less. Ids
that went stale between the confirmation and the launch — a finding already
fixed, skipped, or sent back to discussion — are dropped instead of executed,
and an empty selection is refused before any token is spent. Provenance groups
keep the session-wide scope of their header; sections fold like every other
tree node, and `d` on one anchors the discussion on the findings it renders.

### Cross-session batch

`X` queues every actionable finding across all sessions in `{session}:{item}` order,
advancing the active session pointer as it moves, with an aggregated confirmation
that states the total scope first. It terminates with one aggregated batch-finished
event.

### 3.7 Pre-flight verification

Before resolving anything — one finding or a whole batch — the server runs the same
toolchain pipeline the core runs after every fixing turn. A repository that does not
build or whose tests fail is refused synchronously, before any model request and
before any status change, at zero token cost, with the compiler or test output
attached and the statuses untouched. Repositories without a detected toolchain skip
the gate transparently.

### 3.8 Failure classification: skip versus abort

A failure no longer stops the queue unconditionally. The server classifies the cause:

- **Infrastructure failure — abort.** A provider or network error, a timeout, a
  missing key, or an explicit cancellation. The fault is external to the finding, so
  every further step would hit the same wall.
- **Domain failure — skip and continue.** Unhealed compiler errors, an unappliable
  patch, exhausted retries, a turn that applies nothing without emitting a skip, or a
  semantic blocker. The failed turn is rolled back through the Git checkpoint, the
  finding becomes `Skipped`, the diagnostic is recorded in `note`, and the queue
  advances.

Findings already resolved earlier in the batch are never rolled back. One hard
finding therefore does not block the remaining independent ones, and the skip
rationale stays inspectable in the view.

### 3.9 Multi-file findings and the end of line drift

Binding a finding to one path and a line range fails in practice: coordinates drift
with every preceding edit, and architectural defects routinely span a trait
definition, its implementations, and the caller sites. Two structural decisions
replace it:

1. **A list of files, no coordinates.** A finding declares related paths only,
   harmonizing with plan step files. Legacy single-path findings are migrated on read.
2. **Batch context preloading.** Every declared path is loaded as editable auto
   context before the fixing turn begins, so the model receives the whole surface on
   round zero instead of spending discovery latency on it.

### 3.10 Embedded subplans and Architect decomposition

A finding can be broader than one bounded turn. `r` dispatches it to the Architect,
which decomposes it into an embedded plan stored on the finding: each sub-step is a
plan item with the same status vocabulary, dependency evaluation, and leaf file
normalization as an implementation plan.

The decomposition reuses the plan engine rather than a parallel scheduler, because
finding decomposition and plan refinement are the same operation — both ask the
Architect for a horizon. It therefore inherits the same invariants: the concept and
target context arrive as immutable input, target files are preloaded read-only before
the first round, contracts must be requested instead of guessed from contours, the
resilient discovery loop tolerates re-requested files, and a contradiction yields a
blocker.

The synthesized horizon is merged with the finding's `Done` foundation: finished
sub-steps are preserved verbatim, colliding identifiers are dropped deterministically,
and a dependency cycle rejects the whole plan. A horizon that would be dead on arrival
— a `depends_on` naming a step that neither the foundation nor the new steps provide,
or a step set whose ready set is empty — is refused before it reaches storage, so the
finding keeps its previous subplan and the Architect receives the typed
`INVALID_PLAN_TREE` diagnostic instead of a generic save failure. Decomposition alone
writes no code; it turns an undecomposed *what* into executable *how* steps and
returns the finding to `TODO`.

Each decomposition records its Architect configuration on the finding and renders it
as a compact badge. Escalation follows the same lazy rule as implementation plans, so
both workflows stay symmetric: the first replan of a failing sub-step keeps the
current architect configuration, and only a repeated failure of the sub-step recorded
in `subplan_last_replanned_step` advances the `architect` ladder and writes the new
rung onto the finding. Escalation saturates on the last rung, and a configuration
absent from project configuration restarts from the base rung.

The marker is persisted with the finding together with the synthesized horizon, so a
decomposition or replan started manually from the interface behaves exactly like an
automated one. Parity is intentional rather than incidental: finding decomposition
and plan refinement both ask the Architect for a horizon, and they must not answer
the same repeated failure with two different escalation policies.

### 3.11 Execution of an embedded plan

A finding carrying a plan expands each leaf sub-step into its own bounded turn, with
files preloaded as editable and the same completion contract and escalation behavior
as a single finding. A sub-step that applies cleanly is persisted as `Done`
immediately, so finished work survives any later interruption.

Each sub-step prompt is built from the finding rather than from the subplan alone.
The finding body becomes the plan description, its recommendations become concept
nodes beside an invariant node restating the decision the audit fixed, and the whole
tree is rendered in the prompt. A sub-step therefore reads the reviewer's rationale
and the contract it must not break, instead of an orphaned list of file paths.

If a sub-step fails, blocks, or the turn is cancelled:

- completed sub-steps stay `Done` and the working tree keeps every verified change,
  so the codebase remains functional;
- the stop reason is recorded in `note`;
- the finding becomes `Skipped`, letting the developer inspect the cause and decide
  how to proceed without discarding finished sub-steps.

This mirrors the plan runner's rule: finished work is immutable, and a partial advance
is recorded rather than thrown away.

### 3.12 Model assignment and escalation

A finding may carry a role or an explicit model, rendered as a badge beside the
status, so a simple defect runs on a cheap model while a hard one uses a stronger
one. A finding without an assignment resolves to the Coder role, never to whatever
model happens to be selected in Develop.

Resolution is intra-role escalation only. With `models.auto_level_up` enabled, a
finding that fails verification is first rolled back to its pre-step checkpoint and
then restarted on the next coder rung, with discovered files kept in context. When
the ladder is exhausted, the domain-failure policy applies: the tree stays rolled
back, the finding becomes `Skipped` with the escalation diagnostic in `note`, and the
batch continues. Escalation never promotes the turn into the Architect role; a
compile failure is a *how* problem.

### 3.13 Review view commands

| Command | Action |
| --- | --- |
| `n` / `p` (also `↑` / `↓`) | move through sessions, findings, and subplan steps |
| `Home` / `End` (`Alt+<` / `Alt+>`) | first / last row |
| `PgUp` / `PgDn` | page through the tree |
| `Ctrl+N` / `Ctrl+P` | accordion walk to the next / previous finding |
| `Tab` / `Space` | fold or unfold the selected session, section, finding, or step |
| `a` | fold or unfold every session, section, and finding |
| `Enter` / `x` | resolve the selected finding, section, or session batch, with confirmation |
| `X` | resolve every open finding across all sessions as one batch |
| `r` / `R` | decompose the selected finding / every finding awaiting decomposition with the Architect |
| `Shift+A` | classify the findings of the selected session with the Architect post-analysis |
| `t` / `s` | open the status dialog |
| `N` | start a new audit of the current context |
| `g` | re-read sessions from the daemon |
| `c` / `y` | copy the finding, session, or subplan step |
| `Delete` | delete the session, with confirmation |
| `Ctrl+R` | fold or unfold the reasoning stream |

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
- **It is not a shortcut past decomposition.** A `REFINE` finding is a *what* without
  *how* and is refused until the Architect decomposes it.
- **It is not a re-audit by the fixer.** The finding states what must hold; the
  resolving turn implements it and reports a conflict through the blocked outcome
  rather than redefining the finding.
- **It is not a status mutator.** Discussions explain proposed changes; the developer
  applies them.
- **It is not a way to weaken verification.** Pre-flight and per-finding gates come
  from project configuration, not from the model.
- **It is not a batch that hides failure.** A domain failure becomes a recorded skip;
  an infrastructure failure aborts the queue and is reported.

---

## 5. Related Chapters

- [Local Plans](plans.md) — the plan engine an embedded subplan reuses.
- [Discussion Focus](../ch02-perception/discussion-focus.md) — anchoring a dialogue on
  a finding.
- [Model Roles and the Coder Ladder](../ch01-foundations/model-roles.md) — the
  read-only Reviewer mandate and intra-role escalation.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — the
  turn a finding resolution runs.
- [Structured Diagnostics and Verification Healing](../ch03-project/verification-healing.md) —
  how a failing gate becomes typed diagnostics.
- [Git Safety & Transactions](../ch03-project/git-transactions.md) — the checkpoint a
  failed finding returns to.
- [The Context Model](../ch02-perception/context-model.md) — why a target
  pattern widens one run without mutating the developer's layer.
- [Keyboard-First Interface](../ch05-system/tui-ux.md) — the authoritative command
  matrix and the batch cancellation dialog.
