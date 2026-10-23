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
6. **Recovery requires proof.** It runs only with an open transaction marker, a
   recorded base that is still an ancestor of HEAD, and a commit range created
   entirely by the harness.
7. **Foreign commits are never rewritten.**
8. **Authorship stays with the developer.** Harness commits carry the developer's
   identity and an explicit co-author trailer.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Checkpoint** — the recorded pre-turn state: the base revision and whether a
  checkpoint commit was created.
- **Checkpoint commit** — a temporary commit that captures the developer's
  uncommitted work.
- **Step commit** — a temporary commit that records one applied change set so
  verification can heal against a reproducible revision.
- **Final AI commit** — the single squashed commit a successful turn leaves
  behind.
- **Transaction marker** — the on-disk record of an open transaction.
- **Crash recovery** — the startup routine that unwinds a provably interrupted
  transaction.
- **Undo** — the explicit reversal of the most recent harness commit.
- **Squash** — collapsing a contiguous run of commits into one.

### 2.2 Commit kinds

| Kind | Subject | Lifetime |
| --- | --- | --- |
| Checkpoint | `tauqe-checkpoint: uncommitted user changes` | temporary |
| Step | `tauqe-step: <phase>` | temporary |
| Final | the turn summary | permanent |

Every harness commit carries the trailer
`Co-authored-by: Tauqe AI <noreply@tauqe.dev>`, so the harness can recognize its
own work deterministically.

### 2.3 The transaction marker

The marker lives at `.tauqe/active-transaction.json`, inside the gitignored
`.tauqe/` directory, and records:

- `previous_head` — the base revision the transaction started from;
- `created_checkpoint_commit` — whether uncommitted work was captured in a
  checkpoint commit.

A clean tree still opens a transaction: the marker is written with
`created_checkpoint_commit = false`. The marker is cleared when the turn is
finalized, rolled back, undone, or recovered.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Opening a checkpoint

Before any edit touches the working tree, the harness inspects the repository:

- **Clean tree** — no checkpoint commit is needed; the marker records the current
  HEAD and `created_checkpoint_commit = false`.
- **Dirty tree** — uncommitted changes are staged and captured in one checkpoint
  commit, and the marker records the new HEAD and
  `created_checkpoint_commit = true`.

The checkpoint step also ensures that `.tauqe`, `credentials.toml`,
`.credentials.toml`, and the staging suffixes are ignored through
`.git/info/exclude`. That keeps secrets and runtime state out of the checkpoint
without modifying tracked project files.

### 3.2 Temporary step commits

Each applied change set is recorded as a step commit so verification heals
against a reproducible revision. The checkpoint is created exactly once per
transaction: repeated healing applications reuse it instead of advancing its
base onto the previous step commit, which would make a later rollback or
finalization unable to return the tree to its true pre-turn state.

### 3.3 Finalization

On success the harness:

1. soft-resets to the recorded base, so every step commit becomes staged work;
2. unwinds the checkpoint commit, returning the developer's uncommitted changes to
   the working tree;
3. clears the transaction marker;
4. stages only the files the turn touched;
5. commits them with the turn summary and the co-author trailer.

Repository history therefore exposes one isolated commit, not the healing noise of
a recovered turn.

### 3.4 Rollback

A failed, blocked, or cancelled turn:

1. hard-resets to the recorded base;
2. cleans untracked files while preserving `.tauqe` and credential files;
3. unwinds the checkpoint commit so developer work returns to the working tree;
4. clears the marker;
5. removes orphaned staging artifacts.

The result is the exact pre-turn state: the same commits, the same uncommitted
changes, and no temporary files.

### 3.5 Crash recovery

Recovery runs at startup and is deliberately conservative:

1. orphaned staging artifacts are cleaned up unconditionally;
2. without a transaction marker, the repository is left untouched — a
   hand-written commit that merely shares the step-commit subject prefix is never
   mistaken for a dangling transaction;
3. with a marker, the recorded base must still be an ancestor of HEAD; otherwise
   recovery refuses and reports the reason;
4. every commit between the base and HEAD must carry the co-author trailer; a
   foreign commit makes recovery refuse instead of rewriting history;
5. if there are no step commits and no checkpoint commit, the marker is cleared
   without a hard reset, so legitimate uncommitted changes survive;
6. otherwise the tree is reset to the base, the checkpoint commit is unwound, and
   the marker is cleared.

Recovery therefore restores an interrupted turn without ever discarding commits
the harness did not create.

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
before anything is written.

### 3.9 Authorship

Harness commits preserve the developer's configured Git identity and fall back to
a built-in identity only when Git cannot resolve one. Project hooks are bypassed
with `--no-verify` when `[git].bypass_hooks` is enabled, which defaults to true so
an autonomous turn never stalls on a user hook. The co-author trailer keeps every
harness-touched commit identifiable without taking authorship away from the
developer.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a stash.** The checkpoint is a real commit with a recorded base, so
  it survives a crash and is recoverable from the marker.
- **It is not a history rewriter for finalized commits.** Once a turn is
  committed, permanent history belongs to the developer's Git tooling.
- **It is not a background daemon.** The transaction lives for one turn; the
  marker exists so an interrupted turn can be recovered, not so a process can
  keep working.
- **It is not safe by assumption on crash.** Recovery refuses when the recorded
  base is not an ancestor or when the range contains a foreign commit.
- **It is not a way to hide AI involvement.** The co-author trailer is always
  present.
- **It is not a substitute for verification.** A commit is created only after the
  change set stages and verifies.
- **It is not an excuse to discard uncommitted work.** A rollback restores the
  checkpoint rather than resetting past it.

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
  `[git] bypass_hooks` and upstream resolution.
