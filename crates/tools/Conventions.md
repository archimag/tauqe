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
   transaction marker and a commit range created entirely by the harness. For
   a born base the range is every commit between the recorded base and HEAD,
   with the base still an ancestor of HEAD; for an unborn base it is every
   commit reachable from HEAD. A foreign commit is never rewritten.
   Recovery additionally requires the recorded writer to be provably gone,
   the same decision the staging reclamation applies. A marker attributed to
   a process that still runs describes a live transaction: recovery reports
   the skip diagnostic, keeps the marker for a later startup, and rewrites no
   history. A marker that names no writer — one written by an earlier
   revision — proves nothing about ownership and recovers exactly as before.
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

---

## 7. Blocking Work

1. **No Blocking I/O on Async Worker Threads:** an `fsync`-bounded write, a cold
   metadata read, or a `git` subprocess runs through `run_blocking` from synchronous
   code or `run_blocking_async` from async code.
2. **Reentrant and Failure-Aware:** both helpers degrade to a direct call outside
   a multi-threaded runtime, and a failed offload is always logged. Only a
   cancelled task — one the runtime shut down before it ran — is completed inline
   by its caller; a task that panicked already ran the work and crashed
   deterministically, so its payload resumes instead of the work being retried.

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
