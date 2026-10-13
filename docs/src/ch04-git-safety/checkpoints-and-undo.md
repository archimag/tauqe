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

## 4.3 Deterministic Undo

Reverting the most recent model commit is instant and deterministic:
1. The undo command in the TUI (`Ctrl+Z`) opens a confirmation dialog.
2. TAUQE verifies that HEAD is a Tauqe commit — detected by the co-author trailer, a known body marker, the author, or a `tauqe-step:` / `AI:` subject prefix. If HEAD looks like a human commit, the undo is refused for safety.
3. The AI commit is unwound with a mixed reset, and only the files touched by that commit are reverted. Unrelated working-tree changes are left intact.
4. If a developer checkpoint preceded the turn, it is unwound (`git reset HEAD~1`), returning the developer's exact uncommitted files to the working directory without data loss.

---

## 4.4 Crash Recovery on Startup

If a workflow is interrupted mid-turn (process termination or power loss), the turn's synthetic commits would otherwise linger at HEAD. On startup, TAUQE detects and unwinds them deterministically:
- Dangling intermediate `tauqe-step:` commits are reset away (hard reset), discarding half-applied model changes.
- A trailing `tauqe-checkpoint:` commit is unwound (mixed reset), returning the developer's uncommitted work to the working tree.

This restores the repository to the exact state it held before the interrupted turn, with no manual cleanup required.

---

## 4.5 Interactive Squash & Scribe Commit Messages

The squash dialog collapses a contiguous run of commits into one clean commit. The developer chooses the base to squash down to:
- **Session Base:** the most recent non-AI commit preceding the current run of AI commits, so only this session's work is squashed.
- **Upstream Base:** the detected (or configured) upstream branch, for squashing everything not yet published.
- **Custom Base:** an explicit ref supplied by the developer.

The dialog previews the commits ahead of the base together with the cumulative diff and per-file changes, letting the developer confirm scope before history is rewritten. The generate action asks the **Scribe** model to compose a Conventional Commit message from the commit list and the cumulative diff, keeping the wording aligned with the repository's commit conventions while leaving the final text editable by the developer.

Applying the squash soft-resets to the base and records a single commit authored by the developer, so the resulting history is indistinguishable from one written by hand.
