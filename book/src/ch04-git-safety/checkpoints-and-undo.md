# 4. Git Safety & Transactions

Git is the authoritative source of reality in TAUQE. Every turn is transactional and safe.

---

## 4.1 Checkpoints: Protecting Developer Work

Before applying any model modifications, TAUQE inspects the Git working tree:
- **Dirty Tree Protection:** If uncommitted changes exist, TAUQE automatically creates an isolated developer checkpoint commit: `tauqe-checkpoint: uncommitted user changes`.
- **Author Isolation:** Model edits are committed separately with strict AI authorship: `Tauqe AI <ai@tauqe.dev>`. Developer and AI changes never merge into an indistinguishable diff.

---

## 4.2 Deterministic Undo (`u`)

Reverting an AI action is instant and deterministic:
1. Pressing `u` in the TUI triggers a confirmation dialog.
2. TAUQE verifies that the HEAD commit was authored by `Tauqe AI`.
3. The AI commit is reset.
4. If a developer checkpoint preceded the turn, it is softly unwound (`git reset HEAD~1`), returning the developer's exact uncommitted files to the working directory without data loss.

---

## 4.3 Crash Recovery on Startup

If a workflow is interrupted mid-turn (due to process termination or power loss), TAUQE's startup routine automatically detects dangling step or checkpoint commits, cleanly unwinds them, and restores the working directory to its consistent state.
