# Git Safety & Transactions

Sovereign concept chapter: how a bounded turn protects uncommitted developer
work, records temporary progress, finalizes one isolated commit, and restores the
repository when it must not commit at all.

Git is the authoritative source of reality in TAUQE. Every turn is transactional:
the working tree either converges into one clean, isolated commit or is restored
to its exact pre-turn state. Developer work is never lost, and model changes
never blend into the developer's uncommitted edits.

---

## 1. Motivation & Invariants ("Why")

### 1.1 Git is reality

A turn that modifies files must be reversible without asking the developer to
clean up afterwards. Git already provides the primitives — commits, resets,
checkouts — but using them naively destroys work: a hard reset discards
uncommitted changes, a commit that includes them blurs authorship, and a crash
mid-turn leaves synthetic commits at the tip of the branch.

### 1.2 The two dangers

- **Losing developer work.** The tree may be dirty when a turn begins. Any rollback
  that assumes a clean tree destroys that work.
- **Rewriting history the harness did not create.** A crash, a branch switch, or a
  hand-written commit that resembles a harness commit must never be reset away.

### 1.3 The invariants

1. **Nothing is committed before the whole change set stages and verifies.**
2. **Uncommitted developer work is captured before any edit touches disk.**
3. **Intermediate step commits are temporary.** They exist so healing can converge
   phase by phase and are squashed away.
4. **A failed or cancelled turn restores the exact pre-turn state.**
5. **A finalized commit is ordinary history.** The developer inspects, amends, or
   reverts it with standard Git tooling.
6. **Recovery requires proof.** It runs only with an open transaction marker whose
   recorded writer is provably gone, and only against a commit range created
   entirely by the harness. The ownership proof comes first: a marker naming a
   process that still runs describes a live transaction, so recovery preserves it
   and rewrites nothing until that writer has exited, while a marker that names
   no writer keeps the earlier behavior. The phase the marker records then
   selects the second proof: a settled transaction must show a commit range
   created entirely by the harness — for a born base, every commit between the
   recorded base and HEAD, with the base still an ancestor of HEAD; for an unborn
   base, every commit reachable from HEAD — while a marker still recording a
   capture in progress is resolved against HEAD and the recorded base alone,
   because the capture it describes has not produced a range yet.
7. **No Git child outlives the turn.** Every invocation runs under the
   `[git].timeout_secs` budget inside its own process group, and the checkpoint
   capture and the step commit also observe the turn's cancellation signal.
---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Checkpoint** — the recorded pre-turn state: the base — a commit hash or
  an unborn branch — and whether a checkpoint commit was created.
- **Checkpoint commit** — a temporary commit that captures the developer's
  uncommitted work.
- **Step commit** — a temporary commit that records one applied change set so
  verification can heal against a reproducible revision.
- **Final AI commit** — the single squashed commit a successful turn leaves
  behind.
- **Transaction marker** — the on-disk record of an open transaction.
- **Crash recovery** — the startup routine that unwinds a provably interrupted
  transaction.
- **Git budget** — the wall-clock budget `[git].timeout_secs` declares; every Git
  command of a transaction runs under it, in its own process group, so a hook or a
  signing agent that never returns is torn down instead of occupying the turn.
- **Turn cancellation** — the signal a bounded turn raises when the developer
  cancels. The checkpoint capture and the step commit observe it; finalization and
  rollback deliberately do not.
- **Undo** — the explicit reversal of the most recent harness commit.
- **Change set** — every repository-relative path the turn produced: the paths the
  patch wrote, the artifacts the post-hook generated, the writes a repair round left
  behind, and the files the toolchain rewrote during verification.
- **Worktree baseline** — the working-copy snapshot taken once, immediately after the
  first change set reached disk; the differential against it is what detects the
  paths the turn produced outside the proposal.
- **Squash** — collapsing a contiguous run of commits into one.
- **Commit message** — the `commit_message` the turn's success outcome declares.
  The isolated commit takes it verbatim as its subject, and `Conventions.md` §6
  fixes its shape: an English Conventional Commits header, imperative mood,
  lower case, no trailing period, at most 72 characters. The developer-facing
  answer travels in `message` and never becomes a commit message.

The transaction machinery — checkpoints, step commits, the squash, crash recovery,
and the proof that the protected paths stay excluded — is deterministic substrate
and lives in `crates/tools/src/git/`. The engine owns the harness-specific inputs
the project declares: the `[git].bypass_hooks` declaration, which it resolves into
the decision every committing entry point receives, and the `[git].timeout_secs`
budget, which bounds every Git child the substrate starts.
### 2.2 Commit kinds

| Kind | Subject | Lifetime |
| --- | --- | --- |
| Checkpoint | `tauqe-checkpoint: uncommitted user changes` | temporary |
| Step | `tauqe-step: <phase>` | temporary |
| Final | the `commit_message` of the success outcome | permanent |

Every harness commit carries the trailer
`Co-authored-by: Tauqe AI <noreply@tauqe.dev>`, so the harness can recognize its
own work deterministically.

### 2.3 The transaction marker

The marker lives at `.tauqe/active-transaction.json`, inside the gitignored
`.tauqe/` directory, and records:

- `previous_head` — the base the transaction started from: the hash of the
  commit the branch already held, or `null` when the branch was unborn;
- `created_checkpoint_commit` — whether uncommitted work was captured in a
  checkpoint commit;
- `phase` — where the capture stands: `capturing` while the capture is still in
  progress, `established` once its outcome is recorded;
- `writer_pid` — the process id of the writer that opened the transaction, or
  `null` when an earlier revision wrote the marker without recording a writer
  at all.

A clean tree still opens a transaction: the marker is written with
`created_checkpoint_commit = false`. The marker is cleared when the turn is
finalized, rolled back, undone, or recovered.

An unborn base always implies `created_checkpoint_commit = false`: the tree
was clean, and a dirty tree would have produced the root checkpoint commit
whose hash the marker records instead. Markers written by earlier revisions
recorded the moving ref `HEAD` for an unborn branch; they migrate to the
   unborn base on read, so a marker never hides an open transaction.

A dirty tree writes the marker twice, and the `phase` field is what tells a
reader which of the two writes it observes. The field records a phase of the
transaction, never an outcome of it: `capturing` is written before the work is
staged, and `established` is written once the checkpoint commit exists and the
marker can name it. The base still travels under the `previous_head` key earlier
revisions wrote, now as a typed value — a commit hash or the unborn state — and a
marker that carries no `phase` key decodes as `established`, keeping the settled
meaning that revision gave it.

The writer travels the same way. `writer_pid` names the process that opened the
transaction, and a marker that carries no such key — one written by an earlier
revision — decodes as unattributed rather than failing to decode, because an
undecodable marker would hide an open transaction.

The recorded writer also decides who may recover the transaction. Only a writer
that is provably gone authorizes the history-rewriting branches, exactly as only
a gone writer authorizes reclaiming a staging sibling; both decisions read the
same recorded process id through one shared helper, so the marker gate and the
sibling sweep cannot drift. A marker that names a live writer is skipped with the
marker preserved, and a marker that names none keeps the previous behavior.

The marker is persisted through the substrate's atomic-write policy end to
end. The directories a first write needs are created durably, with the parent
of every created directory synced, so a fresh `.tauqe/` entry is durable in the
repository root before the marker exists. The document is then staged in a
sibling file, flushed and `fsync`ed, renamed atomically over the marker, and the
directory that holds it is synced. A crash or a concurrently running recovery
pass therefore observes either the previous complete marker or the new complete
marker, never a truncated document that would decode as an absent transaction
while one is open.

Removal follows the same policy. Clearing the marker is a durable unlink that
syncs the directory which recorded it, and an already absent marker is success,
so a finished transaction cannot be resurrected by a crash or a power loss after
the harness returned. That durability guards the single artifact that authorizes
history rewriting: a marker a crash could bring back would let recovery prove a
settled transaction against the delivered squash commit — whose trailer it
carries — and reset the finished work away. A clear that cannot be proven
durable therefore reaches its caller. The operational paths — finalization, the
isolated commit, rollback, and the recovery branches that have not touched
history yet — abort on it with the diagnostic, so nothing continues on top of a
marker that still records an open transaction. The post-reset path of recovery
reports the failed removal and still returns, because the tree already sits at
the recorded base and the surviving marker makes the next startup retry an
idempotent recovery.

Neither crash window loses captured work, because the `capturing` marker is
durable before the capture starts and recovery resolves it against HEAD. A crash
before the checkpoint commit leaves a marker whose recorded base is still HEAD
while the developer's changes are still in the working tree: recovery returns
the index to the recorded base, which undoes the `git add -A` the capture ran
before it crashed, leaves every byte of the working tree untouched, and clears
the marker. A crash after the commit but before the
rewrite leaves a `capturing` marker plus a checkpoint commit whose parent is the
recorded base: recovery unwinds that commit and returns the captured work to the
working tree. Any other shape is refused, exactly as an unproven settled
transaction is.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Opening a checkpoint

Before any edit touches the working tree, the harness inspects the repository:

- **Clean tree** — no checkpoint commit is needed; the marker records the
  current HEAD — or an unborn base when the branch holds no commit yet —
  `created_checkpoint_commit = false`, the settled `established` phase, and the
  process id of the writer that opened it.
- **Dirty tree** — the marker is persisted in the `capturing` phase first, so
  the capture is announced before it starts; the uncommitted changes are then
  staged and captured in one checkpoint commit, and the marker is rewritten in
  the `established` phase recording the new HEAD and
  `created_checkpoint_commit = true`; both writes name the writer that opened
  the transaction in `writer_pid`.

The checkpoint step also ensures that `.tauqe`, `credentials.toml`,
`.credentials.toml`, and the staging suffixes are ignored through
`.git/info/exclude`. That keeps secrets and runtime state out of the checkpoint
without modifying tracked project files.

Installing those rules is fail-closed. A rule that cannot be appended aborts the
checkpoint instead of proceeding, and the installed rules are then proven with one
batched `git check-ignore` call over the protected paths:
`.tauqe/credentials.toml`, `credentials.toml`, and `.credentials.toml`.
`git add -A` stages everything Git does not consider ignored, so a checkpoint that
cannot prove the exclusion would otherwise carry the API key into a temporary
commit — an object that survives in the object database and the reflog long after
the checkpoint is unwound. A project `.gitignore` outranks `.git/info/exclude`
and can re-include one of those paths, which is why the exclusion is queried
rather than assumed.
The capture is also bound to the turn. A capture that begins after the turn was
cancelled is refused before the first probe, and a cancellation that arrives while
the capture runs tears the commit down and reports the cancellation. Section 3.10
states why the refusal comes first.
### 3.2 Temporary step commits

Each applied change set is recorded as a step commit so verification heals
against a reproducible revision. The checkpoint is created exactly once per
transaction: repeated healing applications reuse it instead of advancing its
base onto the previous step commit, which would make a later rollback or
finalization unable to return the tree to its true pre-turn state. A step commit
observes the turn's cancellation signal exactly as the capture does, so a cancelled
turn records no further progress (section 3.10).
### 3.3 Finalization

On success the harness:

1. soft-resets to the recorded base, so every step commit becomes staged work;
   an unborn base holds no commit to reset to, so its soft reset deletes the
   branch ref while leaving the index untouched, and the step commits
   collapse into the single squashed root commit;
2. unwinds the checkpoint commit so user uncommitted changes stay in
   the working tree;
3. clears the transaction marker;
4. stages exactly the turn's change set;
5. commits them with the `commit_message` the success outcome declared and the
   co-author trailer.

The change set is resolved before the reset, from three sources read off the real
tree: the paths the patch wrote, the artifacts the declared post-hook produced, and
the working-copy differential against the baseline captured when the first change set
reached disk. That differential is what keeps a lock file refreshed by the
verification command or a snapshot regenerated by a test run inside the commit
instead of leaving it unaccounted for in the working copy. It is also why the
baseline is captured once per transaction rather than per healing round: the
differential never advances past an artifact the turn already produced.

Repository history therefore exposes one isolated commit, not the healing noise of
a recovered turn.

The message of that commit is not composed by the harness. It is the
`commit_message` the turn's success outcome declared, taken verbatim, which is
why the field is named for this consumer rather than for the answer that
accompanies it. `Conventions.md` §6 already fixes the shape for every commit in
the repository — a Conventional Commits header, imperative mood, lower case, no
trailing period, at most 72 characters — and the model-facing rule repeats that
contract, so the header stays English whatever language the turn answered in.

### 3.4 Rollback

A failed, blocked, or cancelled turn:

1. hard-resets to the recorded base — an unborn base returns to the unborn
   state instead, removing exactly the paths the transaction tracked;
2. cleans untracked files while preserving `.tauqe` and credential files;
3. clears the marker;
4. unwinds the checkpoint commit so developer work returns to the working tree;
5. removes orphaned staging artifacts.

The marker is cleared before the checkpoint is unwound, so a failed unwind can
never strand an open transaction. The failure travels back as a diagnostic
naming the partial state — the developer's pre-turn work stays recoverable
inside the `tauqe-checkpoint:` commit at HEAD — and the rollback-retry loop of
the engine can attempt the restore again.

A checkpoint commit normally has the pre-turn base as its parent, and the unwind
moves the branch back to it. When the transaction opened on an unborn branch the
checkpoint has no parent at all, because it is the repository root, so resetting
to `HEAD~1` cannot work. The unwind recognizes that shape and returns the branch
to the unborn state instead: `git update-ref -d HEAD` deletes the branch ref and
`git read-tree --empty` clears the index. The developer's captured files stay in
the working tree exactly as before the turn — untracked on an unborn branch —
and the temporary checkpoint commit simply stops being reachable.

The result is the exact pre-turn state: the same commits, the same uncommitted
changes, and no temporary files.
### 3.5 Crash recovery

Recovery runs at startup and is deliberately conservative:

1. orphaned staging artifacts are cleaned up unconditionally, in both the
   working tree and the harness state directory (`.tauqe/`); recovery removes or
   restores only names that parse as staging siblings (`.tauqe-tmp` or
   `.tauqe-bak`), so a crashed plan, review, history, or reference write does not
   accumulate, while credentials, the tracked registry, and regular state files
   are never touched;
2. without a transaction marker, the repository is left untouched — a
   hand-written commit that merely shares the step-commit subject prefix is never
   mistaken for a dangling transaction;
3. a marker attributed to a writer that still runs describes a live transaction,
   not a crashed one: recovery reports the skip diagnostic naming that process
   id, preserves the marker, and rewrites nothing, so a later startup retries
   once the writer has exited; a marker that names no writer — one written by an
   earlier revision — proves nothing about ownership and keeps the previous
   behavior;
4. a marker still recording the `capturing` phase describes a crash inside the
   capture itself and is resolved against HEAD and the recorded base instead of
   a commit range: a HEAD that still equals the base means no checkpoint commit
   was created, so the index returns to the recorded base — undoing the
   `git add -A` the capture ran before it crashed — the working tree is left
   untouched, and the marker is cleared;
   a checkpoint commit whose parent equals the base — for an unborn base, a
   checkpoint commit that is the repository root — means the capture committed
   but crashed before the rewrite that records it, so that commit is unwound,
   returning the captured work to the working tree; any other shape is refused;
5. otherwise the settled marker's recorded base decides the proof range: for a
   born base, every commit between the base and HEAD, and the base must still be
   an ancestor of HEAD or recovery refuses and reports the reason; for an
   unborn base, every commit reachable from HEAD;
6. every commit in that range must carry the co-author trailer; a foreign
   commit makes recovery refuse instead of rewriting history;
7. if there are no step commits and no checkpoint commit, the marker is
   cleared;
8. otherwise the tree is returned to the base — a hard reset to the recorded
   commit for a born base, a return to the unborn state that removes exactly
   the tracked paths for an unborn one — the marker is cleared, and the
   checkpoint commit is unwound.

Recovery therefore restores an interrupted turn without ever discarding commits
the harness did not create, and it is total: once the destructive reset has run,
a failure to unwind the checkpoint commit and a failure to clear the marker are
both logged, and the call completes with a diagnostic naming the partial state
instead of aborting. The developer's pre-turn work stays recoverable inside the
`tauqe-checkpoint:` commit at HEAD when the unwind failed, and a marker that
survived the reset makes the next startup retry the recovery, which is
idempotent once the tree already sits at the recorded base. An interrupted turn
can therefore never leave a repository that fails recovery identically on every
later startup.

### 3.6 Undoing the last harness commit

Undo is an explicit developer action, not an automatic repair:

1. HEAD metadata is read and inspected;
2. a HEAD that does not look like a harness commit is refused;
3. the commit is unwound with a mixed reset;
4. the files it touched are restored from the new HEAD, and files it created are
   removed;
5. a checkpoint commit underneath it is unwound as well, returning the developer's
   uncommitted work;
6. the marker is cleared and the result reports the undone commit, whether a
   checkpoint was restored, and the new HEAD.

Once a commit is finalized, the harness ships no command that rewrites it. The
developer manages permanent history with standard Git tooling.

### 3.7 Interactive squash

The squash dialog collapses a contiguous run of commits into one. The developer
chooses the base:

- **Session base** — the most recent non-harness commit, so only this session's
  work is squashed;
- **Upstream base** — the detected or configured upstream branch;
- **Custom base** — an explicit ref.

The dialog previews the commits and the cumulative diff before history is
rewritten. The generate action asks the **Scribe** model for a Conventional
Commit message, which remains editable. Applying the squash soft-resets to the
base and records one commit authored by the developer.

### 3.8 Atomic disk commit

Writing a validated change set is all-or-nothing. New content is first staged in
a temporary sibling file, an existing target is moved aside to a backup sibling,
and the staged content is renamed into place. Every step is a rename on a single
filesystem, so permissions, timestamps, and symlinks survive, and a failure rolls
the tree back. A path that escapes the repository through a symlink is rejected
before anything is written. Every atomic write the substrate performs — the
working-tree sibling commit and the state-document replacement (`write_atomic`)
alike — stages under the same shared markers, so the Git exclude rules and the
crash-recovery walk recognize every sibling either of them can leave behind.

### 3.9 Authorship

Harness commits preserve the developer's configured Git identity and fall back to
a built-in identity only when Git cannot resolve one. Project hooks are bypassed
with `--no-verify` when `[git].bypass_hooks` is enabled, which defaults to true so
an autonomous turn never stalls on a user hook. The co-author trailer keeps every
harness-touched commit identifiable without taking authorship away from the
developer.
### 3.10 Budget and cancellation of the transaction layer

Every Git command the transaction layer runs is bounded twice.

- **Wall-clock budget.** `[git].timeout_secs`, 300 seconds by default, covers the
  wait for the child and the drain of its output. The child leads its own process
  group, so an expiry tears the hook, the signing agent, and every binary they
  spawned down together, and the invocation is reported as failed instead of
  occupying the blocking thread the turn is waiting on.
- **Turn cancellation.** The checkpoint capture and the step commit observe the
  bounded turn's cancellation signal, so cancelling the turn releases the commit a
  user hook is holding instead of waiting for the hook to finish. The signal
  travels into the transaction layer as the receiver the turn already owns;
  nothing is polled and no second channel is opened.

Finalization and rollback stay cancel-blind on purpose. Both settle an outcome the
turn already decided, and finishing them under the budget is what leaves the tree
in a known state; a cancellation that interrupted finalization would strand the
very transaction the developer asked to end. The server's deferred force-abort is
built on the same guarantee: it waits, bounded, for the open transaction to close,
because a settling command is expected to finish on its own.

A capture that begins after the turn was cancelled is refused before the first
probe. The capture derives its base and its cleanliness from `rev-parse HEAD` and
`git status`, and the substrate answers a probe it could not run the way that probe
answers for a repository without history: a failed `rev-parse HEAD` reads as an
unborn base and a failed status reads as a clean tree. A probe torn down by the
cancellation therefore describes a repository that does not exist, so the refusal
comes first and no rollback can ever trust such a base. Once the capture is under
way the substrate answers for itself: the interrupted child fails, the command
reports the cancellation, and the marker the capture already wrote is resolved by
crash recovery, which distinguishes a capture that committed from one that did not.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a stash.** The checkpoint is a real commit with a recorded base, so
  it survives a crash and is recoverable from the marker.
- **It is not a history rewriter for finalized commits.** Once a turn is
  committed, permanent history belongs to the developer's Git tooling.
- **It is not a background daemon.** The transaction lives for one turn; the
  marker exists so an interrupted turn can be recovered, not so a process can
  keep working.
- **It is not safe by assumption on crash.** Recovery refuses when a born base
  is not an ancestor or when the proof range contains a foreign commit.
- **It is not a recovery of a live writer's transaction.** A marker attributed to
  a process that still runs is skipped with the marker preserved, and no branch
  is reset; only a provably gone writer, or a marker that names none, authorizes
  the history-rewriting branches.
- **It is not an unbounded wait.** Every Git child of a transaction runs under
  `[git].timeout_secs` inside its own process group, and the capture and the step
  commit also observe the turn's cancellation.
- **It is not interrupted while it settles.** Finalization and rollback complete
  under the budget alone, because an outcome the turn already decided has to reach
  a known state.
- **It is not a checkpoint without a proven exclusion.** A checkpoint whose rules
  cannot be installed, or whose protected paths Git reports as not ignored, aborts
  before `git add -A` runs.
- **It is not a way to hide AI involvement.** The co-author trailer is always
  present.
- **It is not a substitute for verification.** A commit is created only after the
  change set stages and verifies.
- **It is not an excuse to discard uncommitted work.** A rollback restores the
  checkpoint rather than resetting past it.
- **It is not a commit of the proposal alone.** A file the toolchain rewrote while it
  verified the tree is committed when the turn succeeds and disappears with the tree
  when it does not.
- **It is not a commit message the harness writes.** The subject of the isolated
  commit is the `commit_message` the success outcome declared, never a summary
  the harness derives from the paths it staged.

---

## 5. Related Chapters

- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) —
  where checkpoint, staging, verification, and finalization sit.
- [The Three-Phase Modification Lifecycle](three-phase-lifecycle.md) — what runs
  before a commit is allowed.
- [Structured Diagnostics and Verification Healing](verification-healing.md) —
  why a failed check always rolls back.
- [Local Plans](../ch04-workflows/plans.md) — step execution and batch
  cancellation.
- [Configuration and Project Preparation](../ch05-system/configuration.md) —
  `[git] bypass_hooks`, `[git] timeout_secs`, and upstream resolution.
