# 4. Git Safety & Transactions

Git is the authoritative source of reality in TAUQE. Every turn is transactional: the working tree either converges into one clean, isolated commit or is restored to its exact pre-turn state. Developer work is never lost, and model changes never blend into the developer's uncommitted edits.

---

## 4.1 Checkpoints: Protecting Developer Work

Before applying any model modification, TAUQE inspects the Git working tree:
- **Dirty Tree Protection:** If uncommitted changes exist, TAUQE stages them and captures an isolated checkpoint commit: `tauqe-checkpoint: uncommitted user changes`.
- **Clean-Tree Fast Path:** An already-clean tree needs no checkpoint, so the turn proceeds without any synthetic commit.

A checkpoint is a temporary transaction marker, not an artifact the developer is meant to keep. It exists so a turn can be rolled back without discarding work that was already in flight when the turn began.

---

## 4.2 Step Commits and Atomic Finalization

While a turn executes, TAUQE records intermediate `tauqe-step:` commits so that verification healing can converge one phase at a time. When the turn succeeds, those intermediate commits are squashed into a single, isolated commit, so repository history never exposes the healing noise of a recovered turn.

**Authorship isolation:** model commits preserve the developer's configured Git identity (falling back to the built-in `Developer <developer@tauqe.dev>` when no identity is set) and mark AI involvement with an explicit `Co-authored-by: Tauqe AI <noreply@tauqe.dev>` trailer. The developer stays the author in their own repository, while every model-touched commit remains deterministically identifiable for later undo.

---

## 4.3 Transactional Turns and Pre-Commit Rollback

A turn is transactional from the inside: safety is guaranteed before any commit exists, not by rewinding history afterward.

- Every edit is staged and fuzzy-matched in memory before a single byte touches disk, so a mismatched or invalid patch never corrupts the working tree in the first place.
- If toolchain verification fails and healing cannot converge, or the developer cancels the turn, the working tree is restored to its exact pre-turn state and the temporary `tauqe-step:` commits are discarded. The checkpoint returns any uncommitted developer work that existed before the turn.
- Only a turn that converges and verifies cleanly is finalized into one isolated commit. That commit is ordinary history: the developer can inspect, amend, or revert it with standard Git tooling like any other.

TAUQE therefore ships no bespoke command to undo an already-finalized commit. Once a turn is committed, permanent history is managed exclusively through the developer's own Git tooling. This keeps repository history under the developer's full control and avoids heuristics that try to guess whether a commit is safe to rewrite.

---

## 4.4 Crash Recovery on Startup

If a workflow is interrupted mid-turn (process termination or power loss), the turn's synthetic commits would otherwise linger at HEAD. On startup, TAUQE recovers deterministically — but only when it can prove the commits belong to its own interrupted transaction.

When a checkpoint is opened, TAUQE writes a transaction marker under the gitignored `.tauqe/` directory recording the base revision (`previous_head`) and whether a checkpoint commit was created. Crash recovery reads that marker and acts only when it is present:

- Without a marker, recovery leaves the repository untouched. A commit whose subject merely begins with `tauqe-step:` — a hand-written commit or a step commit from another branch after a checkout — is never mistaken for a dangling transaction.
- With a marker, every commit between the recorded base and HEAD must carry the `Co-authored-by: Tauqe AI` trailer. If any commit in that range was not created by Tauqe, recovery refuses and reports the reason instead of rewriting history.
- A validated recovery resets to the recorded base, unwinds a temporary `tauqe-checkpoint:` commit (mixed reset), and clears the marker, returning the developer's uncommitted work to the working tree.

This restores the repository to the exact state it held before the interrupted turn, with no manual cleanup required — and no risk of discarding commits TAUQE did not create.

---

## 4.5 Interactive Squash & Scribe Commit Messages

The squash dialog collapses a contiguous run of commits into one clean commit. The developer chooses the base to squash down to:
- **Session Base:** the most recent non-AI commit preceding the current run of AI commits, so only this session's work is squashed.
- **Upstream Base:** the detected (or configured) upstream branch, for squashing everything not yet published.
- **Custom Base:** an explicit ref supplied by the developer.

The dialog previews the commits ahead of the base together with the cumulative diff and per-file changes, letting the developer confirm scope before history is rewritten. The generate action asks the **Scribe** model to compose a Conventional Commit message from the commit list and the cumulative diff, keeping the wording aligned with the repository's commit conventions while leaving the final text editable by the developer.

Applying the squash soft-resets to the base and records a single commit authored by the developer, so the resulting history is indistinguishable from one written by hand.
