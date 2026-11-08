# Deterministic Substrate Conventions

This document specifies mandatory architectural and operational conventions for the
Tauqe deterministic substrate (`crates/tools`).

The crate owns every operation the harness performs without model judgement. It
receives the shared wire contract and explicit arguments from the engine, performs
the operation, and reports a typed result. It holds no cognitive state and drives no
turn.

---

## 1. Boundary and Dependency Direction

1. **One-Way Dependencies:**
   - `tauqe-tools` depends on `tauqe-protocol` and on the system crates its
     operations require. It never depends on `tauqe-core`, on a provider client, or
     on the turn machine.
   - The hierarchy is strictly `protocol -> tools -> core -> server/tui`. The
     compiler enforces the boundary: a cognitive type cannot even be named here.
2. **Declared Inputs, Not Discovered State:**
   - Harness configuration and the decisions it carries reach the substrate as
     explicit arguments: `ProjectConfig` names the command lines, the staged change
     set names the paths, and the commit hook policy arrives as a `GitHooks` value.
   - The substrate never reads `tauqe.toml`, never selects a project command, and
     never decides which verification target a project runs.
3. **What Stays in the Engine:** decoding model output, prompt assembly, context and
   history management, plan and review persistence, provider communication, and the
   bounded turn FSM. The substrate answers a call; it never initiates one.

---

## 2. Virtual Staging

1. **No Byte Before the Whole Change Set Stages:** an edit is applied to memory
   first. The working tree receives content only after every member of the change set
   has been resolved and formatted.
2. **Member Atomicity:** a refused member restores exactly the paths it addressed. A
   member that converged earlier keeps its own effect, so a retry builds on what the
   attempt already delivered instead of starting over.
3. **Bottom-Up Anchor Application:** every anchored range is validated against one
   snapshot of the file — inside the file, never running backwards, never sharing a
   line with another range — and the validated ranges are applied from the bottom of
   the file upwards.
4. **Ignored Paths Fail Fast:** a patch that would create, overwrite, or move onto a
   path excluded by `.gitignore` or `.git/info/exclude` is rejected with
   `EditError::IgnoredByGit` before any byte reaches disk and before `git add` runs.
   The check is batched through `git check-ignore`, so one subprocess answers for the
   whole change set, and a repository Git cannot inspect degrades to "nothing is
   ignored" rather than blocking an otherwise valid turn. Post-hook artifacts are
   exempt by construction: they are discovered through a Git status differential that
   honors the same ignore rules, so a generated build file is absent from the change
   set instead of breaking the transaction.
5. **Trust Boundaries Are Asked on Both Sides:** a protected pattern is checked at the
   source and at the destination of every move, so a move can neither carry a guarded
   file out of its directory nor drop one into it.
6. **Directory Moves Are Expanded, Never Enumerated:** a directory rename names the
   source and the destination directory, and the engine expands it into the per-file
   moves of the tracked files under the source. A destination inside the source, a
   source with no tracked file, and an occupied destination are refused before the
   first file moves.
7. **The Sandbox Is Admitted Only When Git Ignores It:** `.tauqe/mockups/` accepts
   writes because the harness keeps `.tauqe/` out of Git, so a draft never joins the
   change set, the verification input, or the commit.
8. **Relocations Carry Bytes Verbatim:** a move carries the payload of its
   source exactly as it lies in the working tree, so a file that does not decode
   as text — an image, a font, or a serialized asset — travels like any other. A
   source this attempt has not staged is read as bytes, and only the text
   pipeline (replacement, anchors, in-memory formatting, prompt rendering, and
   the streaming preview) restricts itself to entries that decode losslessly as
   UTF-8. A source that is missing or is not a regular file is refused as a
   missing file, while a genuine read failure stays an I/O rejection.
9. **A Destination Is Created Only Beneath Directories:** a write admitted by the
   change set has a reachable destination, not merely a symlink-safe one: a
   component that already exists as a regular file blocks every path beneath it,
   so the destination is refused and the component that blocks it is named while
   the change is still purely in memory. Without the check the disk commit fails
   with `ENOTDIR` and rolls the whole change set back with an opaque filesystem
   error, instead of the typed, per-member refusal virtual staging exists to
   produce.

---

## 3. Atomic Disk Commit

1. **Staged Siblings Only:** new content is synced into a process-unique sibling
   file, an existing target is moved aside to a backup sibling, and the staged content
   is renamed into place.
2. **All-or-Nothing:** a failure at any step restores every path the commit already
   published, removes the directories the commit created, and drops the staged content
   of the paths it never reached.
3. **Durability:** each rename is flushed to its parent directory, so a crash
   publishes either the previous complete file or the new complete file.
4. **Pruning Runs Last:** a directory a move or a deletion emptied is pruned only
   after the whole change set reached disk, because a directory that still receives a
file of the same change set has to survive.
5. **One Staging-Marker Contract:** every staging sibling the crate creates — for
the atomic replacement of a state document and for the sibling commit of a
working-tree file alike — is named exclusively by `STAGING_TEMP_MARKER` or
`STAGING_BACKUP_MARKER`. The Git exclude rules, the sibling-name parsers, and
crash recovery derive their names from those two markers instead of inventing
one of their own. Recovery reclaims orphaned siblings everywhere they can
appear — the working tree and the harness state directory — and touches only
names that parse as staging siblings whose recorded writer is provably gone. A
working-tree sweep descends one directory level at a time and asks Git to
classify the next level in one batched `git check-ignore` probe, so a tree the
ignore rules cover is never entered: no admitted change set can leave a sibling
there, while walking it would stat every entry of a build or dependency tree at
every recovery and every rollback. A level Git cannot classify keeps its whole
set, so an uninspectable repository degrades to the previous reach instead of
skipping a directory that may hold a sibling.
A sibling a still-running writer owns is untouchable: deleting its staged
content or moving its live backup aside corrupts the commit that writer is
about to publish. A legacy pid-less name records no writer and keeps the
earlier reclaim behavior, and on a platform without a process probe every
attributed sibling reports a live writer.

A recorded writer whose process id lies outside the probe's target domain is
reported as not provably gone and keeps ownership of its sibling, exactly as a
writer on a platform without a process probe does. A value that does not fit
the platform's `pid_t` names no process once converted, because the conversion
wraps it negative and a negative id addresses a whole process group; the
reserved low values address the caller's own group or the init process
instead of the writer. The probe therefore validates its target before it
signals, since an answer about a target that is not the writer — least of all
"no such process" — is the answer that authorizes the reclaim sweep to delete
staged content or move a live backup aside.

---

## 4. Process Execution

1. **Literal Arguments, No Shell:** a declared command line is spawned as a program
   with its argument vector. Nothing is split, expanded, or interpreted.
2. **Isolated Process Groups:** every command runs in its own process group, so a
   timeout or a cancellation tears the compiler, the generator, and every binary they
   spawned down together.
3. **Cancellation Is Authoritative:** the wait for a child races its natural exit
   against the caller's cancellation signal. A cancelled wait kills the group and
   reaps the child instead of waiting the command out.
   The group is signalled only while the direct child is unreaped, and the reap
   follows the final group signal: a reaped pid may be recycled into a new
   process group, and a negative-pid signal that names it would tear that
   unrelated group down. The grace window of a polite termination is therefore
   waited out with the leader unreaped, and the forceful signal that closes it
   is harmless to a group whose only remaining member is the zombie leader.
4. **Typed Diagnostics:** command output is parsed into file, line, column, severity,
   code, and message records. The report is rebuilt from those fields, so no ANSI
   escape, tool banner, or duplicated note reaches a model prompt.
5. **No Raw Dumps in the Journal:** failures are recorded as compact, UTF-8-safe
   summaries through the `tracing` facade.

---

## 5. Git Transactions

1. **Checkpoint Before Any Write:** uncommitted developer work is captured in a
temporary checkpoint commit before the first byte of a change set reaches disk.
The transaction marker that records this checkpoint lives through the atomic
disk commit policy of §3 end to end: the directories a first write needs are
created durably, the document is rewritten atomically, and its removal is a
durable unlink that treats an absent marker as success. A crash can therefore
neither leave a torn marker that hides an open transaction nor resurrect a
marker a finished transaction already cleared.
2. **The Exclusion Is Proven, Not Assumed:** the ignore rules covering `.tauqe`,
   `credentials.toml`, and the staging suffixes are installed fail-closed and then
   verified with one batched `git check-ignore` call before `git add -A` runs.
3. **Step Commits Are Temporary:** intermediate commits exist so verification can
   heal against a reproducible revision; finalization squashes them into one commit.
4. **Recovery Requires Proof:** a startup recovery runs only with an open
   transaction marker and a commit range created entirely by the harness. The
   proof is settled over one pinned identity: `HEAD` is resolved once, every
   evidence step addresses that commit, and the identity is re-asserted
   immediately before the reset or the return to the unborn state acts, so a
   repository that advanced while the proof ran is refused with the skip
   diagnostic instead of rewritten. For a born base the range is every commit
   between the recorded base and the pinned commit, with the base still an
   ancestor of it; for an unborn base it is every commit reachable from the
   pinned commit. A foreign commit is never rewritten.
   Recovery additionally requires the recorded writer to be provably gone,
   the same decision the staging reclamation applies. A marker attributed to
   a process that still runs describes a live transaction: recovery reports
   the skip diagnostic, keeps the marker for a later startup, and rewrites no
   history. A marker that names no writer — one written by an earlier
   revision — proves nothing about ownership and recovers exactly as before.
   The same ownership decision gates a clear the caller does not own. An
   isolated commit opens no transaction, so it refuses to clear a marker whose
   recorded writer still runs and keeps every other state — an absent marker, a
   marker attributed to a provably dead writer, and an unattributed legacy
   marker — clearing exactly as before.

   The same ownership decision gates the open of a transaction. A capture
   replaces the marker only when no other live writer owns it: a marker
   attributed to another process that still runs refuses the capture before the
   ignore rules are installed and before the first repository probe, because
   overwriting it would leave the work of the transaction that wrote it behind a
   marker that no longer names it. A marker this process wrote itself keeps the
   previous behavior, since no other process could have opened it, and so do a
   marker attributed to a provably dead writer and an unattributed legacy
   marker.
5. **The Hook Policy Arrives as an Argument:** whether a commit bypasses the
   repository hooks is resolved by the engine and passed in as `GitHooks`.
6. **The Child Environment Is Sanitized:** every `git` invocation is spawned
   through one helper that removes the repository-redirection and
   configuration-injection variables from the child — `GIT_DIR`,
   `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, `GIT_NAMESPACE`, the
   object store overrides, and the injected configuration family — so a daemon
   launched from a Git hook still addresses the directory the engine passed
   down. The identity variables (`GIT_AUTHOR_*`, `GIT_COMMITTER_*`) stay
   inherited, because the commit path probes and supplements them.
7. **Every Child Runs Under a Budget:** each `git` invocation reaches the
   repository through the bounded executor of `git::exec`. The child leads its
   own process group, one wall-clock budget covers the wait and the output
   drain, and the caller's cancellation signal races the natural exit. Expiry
   or cancellation kills the group and reaps the child, so a repository hook
   that never returns, a signing agent waiting for a passphrase, or a
   `check-ignore` that stops reading its candidate list can never occupy a
   blocking thread indefinitely. `run_git` carries the default budget with no
   call-site change, while `run_git_with_deadline` carries an explicit
   `GitDeadline`.
8. **Destructive Steps Act on a Pinned Identity:** the checkpoint unwind, the
   return to an unborn branch, and the finalize base return resolve `HEAD`
   exactly once, read the subject, the parent, and the tracked paths from that
   commit, and re-assert it immediately before they rewrite anything. The unwind
   resets to the explicit pinned parent and deletes a root checkpoint's branch
   ref through the conditional `git update-ref -d HEAD <pinned>`; the unborn
   return deletes the ref the same conditional way before it removes a path. The
   finalize return first proves the recorded base still accounts for the pinned
   commit — the base must remain an ancestor of it, and every commit in between
   must carry the Tauqe trailer — then re-asserts that commit before the soft
   reset and deletes an unborn base's branch ref conditionally on it. A `HEAD`
   that moved in between is a typed refusal that rewrites no history, no index
   entry, and no working-tree byte, so a commit a concurrent writer landed
   survives instead of being dropped by a proof that never saw it.

9. **The Index Lock Is Reclaimed by Proof, Not by Assumption:** Git refuses
   every command that writes the index while another process holds the index
   lock, so a lock a crashed invocation left behind blocks the whole
   transaction. The lock is addressed through `rev-parse --git-path
   index.lock`, never derived from a guess, because a linked worktree keeps its
   own index beside its private Git directory.
   No marker names the writer of a plain Git invocation, so the age of the lock
   is the whole proof there. Every child runs under a wall-clock budget and
   receives the termination grace period to release its locks before the
   executor tears it down, so a lock older than the sum of the two cannot
   belong to an invocation that still runs and is removed; a younger lock may
   belong to another harness process, a developer command, or a repository
   hook, and is left alone whatever holds it. A reclaimed lock is recorded at
   `warn` level, because it reports a process that died holding the repository
   index, while a removal that fails is recorded and answers as still held:
   reclaiming an artifact never fails the recovery that runs it, and the caller
   learns that no orphan was reclaimed rather than that the repository is
   unusable.
   A transaction marker supplies the stronger proof. When it attributes the
   interrupted transaction to a writer that provably no longer runs, recovery
   removes the lock that writer left behind even where the age rule would
   still wait, because a writer that no longer runs can never clean up after
   itself and every recovery command that records the index would fail on the
   surviving lock. A marker that names no writer proves nothing about the lock
   and keeps the previous behavior.
   `index_availability` answers the staleness question once, so an entry point
   that is about to write the index shares it instead of restating the rule: it
   answers with a free index, a reclaimed one, or a lock young enough that a
   running invocation may still own it. A lock whose age cannot be read answers
   as free, and the Git command the surviving lock obstructs reports the real
   reason on its own.
---

## 6. Repository Map

1. **Generated, Never Narrated:** symbols come from Tree-sitter over the Git-tracked
   file list. Nothing in the outline is produced by a model.
2. **Budgeted and Deterministic:** the outline is formatted at the richest compression
   level that fits the token budget, and identical trees produce identical bytes.
3. **Links Are Classified, Never Followed:** a tracked symbolic link is inspected with
   `lstat` and rendered with its target and status; only regular files reach the
   extractor.
4. **Frozen Within a Turn:** the engine generates the map once for a turn and reuses
   it, so a later round cannot rewrite the cached prompt prefix.
5. **The Symbol Cache Is Shared, Bounded, and Validated:** a pass serves an
   unchanged file from the cache left by an earlier pass, so a file whose
   modification time and size still match is parsed only once for the whole
   process. Entries are keyed by absolute path, so two repositories never
   collide; the cache holds a fixed number of entries and evicts the least
   recently used one once it is full; and a pass touches it only to read a
   validated entry or to publish a parsed one, so concurrent passes parse in
   parallel instead of serializing behind the lock. A file that changed,
   disappeared, or fell out of the bound is parsed again, and an answer that a
   file publishes no symbols is cached and reused like any other.

---

## 7. Blocking Work

1. **No Blocking I/O on Async Worker Threads:** an `fsync`-bounded write, a cold
   metadata read, or a `git` subprocess runs through `run_blocking` from synchronous
   code or `run_blocking_async` from async code.
2. **Reentrant and Failure-Aware:** `run_blocking` runs the closure directly
   outside a multi-threaded runtime, `run_blocking_async` offloads to the
   blocking pool on either runtime flavor and runs the closure directly only
   when no runtime is present, and a failed offload is always logged. A blocking
   closure cannot be cancelled, so a failed offload is never classified by the
   join error of its task: the closure owns the channel it reports through, a
   channel closed with no report proves the runtime dropped the closure before
   it ran, and only that case is completed inline by the caller. A closure that
   ran and panicked already performed the work deterministically, so its payload
   resumes instead of the work being retried.

---

## 8. Single In-Memory Formatting Contour

1. **One Formatting Surface:** formatting is declared once — by `[project.format]` in
   `tauqe.toml`, or by the `format` recipe the declared task runner exports when that
   entry is omitted — and it runs exclusively while the change set lives in virtual
   staging.
2. **No Disk Formatting Stage:** nothing reformats a verified working tree, and no
   compensation layer restores formatter modifications afterwards.
3. **Fail-Safe Outcome:** a non-zero exit status, a timeout, a cancellation, an
   unresolvable declaration, a filter that emits nothing while the staged text was
   not empty, or a rewritten document the staged-configuration gate rejects keeps
   the authored content and reports a compact reason.
4. **The File Context Travels Beside the Stream:** the filter learns which file
   it rewrites from the resolved declaration, never from the content it reads. The
   repository-relative path reaches the child through the `TAUQE_FILE_PATH`
   environment variable, and a declaration that names the parameter `path` or
   `file` receives the same path as one literal argument, which is how a polyglot
   dispatcher selects its filter by file type. A declaration that names neither
   keeps its argument list exactly as written, so a plain filter such as `rustfmt`
   still reads the staged text and nothing else, and an invocation without a known
   path supplies neither the variable nor the argument.
5. **Rationale (Why):** formatting improves the shape of a change; it never proves its
   correctness, so it cannot sit on the verification gate.

---

## 9. What This Crate Is Not

- **It is not a second engine.** It owns no turn, no phase, and no retry budget.
- **It is not a configuration reader.** Every declaration reaches it as a resolved
  value.
- **It is not a model client.** It never decodes a protocol tag, a JSON field, or a
  streamed delta.
- **It is not a domain store.** Plans, reviews, history, and context belong to
  `core`.
- **It is not a place for optimistic guesses.** If an answer can be derived from Git,
  an AST parser, or compiler output, it is computed here deterministically.
