# The Mockups Sandbox

Sovereign concept chapter: the throwaway prototype area under `.tauqe/mockups/`, the
single Git answer that keeps it out of every commit, and the admission rule that
makes the sandbox fail closed.

A prototype is written to be read, argued about, and discarded. It is not part of the
project. Yet the model drafts it inside the same repository, during the same bounded
turn, that edits real code — so without a dedicated place to put it the draft either
lands in the tracked tree or never exists at all. The sandbox gives it that place
without relaxing an invariant the harness enforces elsewhere: nothing reaches a
commit, a verification run, or a post-hook capture unless the turn proposed it at a
tracked path.

---

## 1. Motivation & Invariants ("Why")

### 1.1 A prototype is not a contribution

A sketch has no maintainer. Committing one burdens the repository with an artifact
nobody owns, and a later refactoring is silently obliged to keep it compiling.
Writing one into the tracked tree is worse in the short term: the compiler sees it,
the linter judges it, and a verification failure raised by a file that was never
meant to compile blocks a change set that is otherwise correct.

The two failures differ in kind. One is a bookkeeping problem that surfaces months
later; the other blocks the turn that is running now. Both are avoided the same way —
by keeping the draft out of everything the harness treats as the project.

### 1.2 Why a Git-ignored area is not enough on its own

The harness already refuses to write a path Git ignores
(`EditError::IgnoredByGit`), for one reason: `git add` would refuse the path, and the
transaction would abort after the whole change set had been applied. A sandbox that
simply ended the rule there would be a hole in the invariant rather than a use of it,
because the same ignore answer decides both the admission of the write and the
exclusion of the file from the change set.

The sandbox therefore inverts that rule instead of bypassing it. It is writable
exactly while Git ignores it.

### 1.3 The invariants

1. **The sandbox path is declared once.** The directory constant and the predicate
   over normalized repository-relative paths live in one module, and every consumer
   names the sandbox through them instead of spelling the path itself.
2. **Isolation is proven, not assumed.** Whether a sandbox write is admitted is decided
   by the same batched `git check-ignore` answer that decides every other ignore
   question, so admission and refusal can never disagree about what Git ignores.
3. **A draft never joins the change set.** The paths a turn reports exclude the
   sandbox, so a draft cannot reach the atomic commit, the post-hook differential, or
   the change set the verification command receives.
4. **A draft never breaks the gate.** A change set that touches only the sandbox
   reports no changes; a change set that touches tracked files is verified exactly as
   if the sandbox did not exist.
5. **The sandbox does not widen the write boundary.** A file that already exists there
   is edited under the ordinary context rules: it must be in context like any other
   path.
6. **Isolation cannot be switched off.** No configuration flag relaxes the check. A
   repository whose ignore rules do not cover the sandbox refuses every sandbox write
   instead of recording a draft Git would track.
7. **A draft is expendable.** The harness never deletes it and never protects it. It is
   a sketch, not state the environment is obliged to preserve.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Sandbox** — the repository-relative directory `.tauqe/mockups/`: the only writable
  area of the repository that the project never compiles, verifies, or commits.
- **Mockup** — one throwaway artifact inside the sandbox: a prototype module, a scratch
  design, a generated sample.
- **Sandbox path** — the directory itself or anything beneath it. The predicate compares
  whole path components, so a sibling whose name merely starts with the same prefix
  (`.tauqe/mockups-archive/old.rs`) is not a sandbox path.
- **Isolation** — the property that Git ignores every sandbox path, which is what keeps
  a mockup out of a checkpoint commit, a step commit, a post-hook capture, and the
  final commit of a turn.
- **Admission** — the staging decision that allows a write into the sandbox. It is the
  inversion of the ordinary ignore refusal and the only path by which a mockup can
  appear on disk.
- **Change set** — the turn's authoritative changed paths. Sandbox paths never appear in
  it.

### 2.2 One declaration, one answer

The module owns two items: the directory constant and the predicate. Staging consults
the predicate to decide admission and to filter the reported change set, discovery
consults it to admit a file the tracked list will never name, and the prompt-side
contract names the sandbox through the same constant. A second spelling of
`.tauqe/mockups` anywhere in the engine would create a path the predicate does not
recognize, and every rule in this chapter would silently stop applying to it.

### 2.3 What a mockup is excluded from

| Surface | How the exclusion happens |
| --- | --- |
| Change set | the path list the disk commit reports is filtered through the predicate |
| Verification | the verification command receives that list, and no sandbox path is in it |
| Post-hook capture | artifacts are read off a Git differential, and Git does not report an ignored path |
| Commit | checkpoint, step, and final commits stage only what Git does not ignore |
| Context lifetime | the entry retains the file for the rest of the turn and is swept with the Auto layer |

### 2.4 The draft outlives the turn, the entry does not

Two lifetimes meet at the sandbox. The file on disk belongs to the working copy: the
harness writes it and never deletes it, and a rollback that restores the tracked tree
leaves the harness-owned `.tauqe/` directory in place, so a draft survives a failed
turn. The context entry belongs to the turn: it is an ordinary Auto-layer entry, and
the sweep at the end of the turn removes it exactly like every other discovery
addition.

The asymmetry is deliberate. Context is a working set with a cost, and nothing
justifies paying for a sketch in the next turn. Bytes on disk cost nothing until
someone reads them again.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Admission during staging

Virtual staging resolves the ignore question for the whole change set in one batched
`git check-ignore` call, then decides each write against that answer:

- outside the sandbox, an ignored path is refused with `EditError::IgnoredByGit`,
  because the change set could never be committed;
- inside the sandbox, the same answer is inverted: the write is admitted only while the
  path is ignored, and a path Git would track is refused with
  `EditError::MockupsNotIgnored`.

Both branches read one answer, so the admission of the write and the refusal of the
tracked alternative cannot drift apart. The check runs while the change set is still
purely in memory, before any byte reaches disk, so a refused draft costs nothing.

The admission covers the operations that produce a new path or a whole new body: a
create, an overwrite, and a move destination. A file that already exists in the sandbox
is otherwise edited under the ordinary rules, so a replacement or an anchored range
works exactly as it does anywhere else — provided the file is in context.

### 3.2 The sandbox never joins the reported change set

The disk commit and the context reconciliation both run over the complete staged state,
so the draft really is written and really does become visible to the model. What
changes afterwards is what the caller is told: the reported path list is filtered
through the predicate, so

- a change set that touches only the sandbox reports an empty list, and
- a mixed change set reports exactly its tracked paths.

Everything downstream reads that list. The verification command never receives a
sandbox path, the final commit never stages one, and the developer's review never shows
one.

### 3.3 Context reconciliation and the sweep

Every staged path is reconciled with the context layer: an entry that already exists
keeps the layer that owns it and is promoted to editable, while a path absent from
context joins the transient Auto layer. A draft the turn created therefore stays
readable and editable for the remaining rounds, so a follow-up round can rewrite the
prototype it just wrote instead of rediscovering a path Git does not list.

The sweep at the end of the turn then drops the entry with the rest of the Auto layer.
The file stays on disk; only the working set shrinks.

### 3.4 Discovery admits a mockup by its existence

A tracked file is admissible because the repository tracks it. A sandbox path can never
satisfy that test, so discovery applies the one property the sandbox actually has: the
file exists on disk inside the declared sandbox directory. An existing draft is
therefore requestable in a later turn, and a requested path that does not exist is
reported as missing like any other, because the sandbox grants no fabricated file — it
is a place to write, not a source of content.

### 3.5 Isolation is installed and proven once per transaction

Opening a transaction ensures that `.tauqe/` and the credential files are excluded
through `.git/info/exclude`, and then proves the exclusion with one batched
`git check-ignore` call over the declared paths. A rule that cannot be appended, or a
path Git reports as not ignored, aborts the checkpoint instead of proceeding: `git add
-A` stages everything Git does not ignore, so an unproven exclusion would carry runtime
state into a temporary commit.

The same proof is what makes a sandbox write admissible. A project `.gitignore` outranks
`.git/info/exclude` and can re-include a path, which is exactly the case the admission
rule catches: the file would be tracked, so the draft is refused while it is still in
memory.

### 3.6 Failure semantics

- **Sandbox write while Git does not ignore the path** — rejected during staging with a
  typed diagnostic naming the violated condition, so the model writes the draft where
  the project accepts it instead of retrying the same path.
- **Repository Git cannot inspect** — the ignore question degrades to "nothing is
  ignored". Ordinary writes still stage, and sandbox writes are refused. The direction
  of that degradation is deliberate: an unproven exclusion is never read as proof.
- **Draft produced by a declared tool** — a post-hook or a test run that writes into the
  sandbox contributes nothing to the change set, because the capture is a Git
  differential and Git does not report ignored paths. The artifact exists on disk and
  stays out of the commit, which is the answer the model gets for writing it directly.

### 3.7 Interaction with rollback and recovery

A failed or cancelled turn restores the tracked tree to its pre-turn checkpoint and
cleans untracked files while preserving `.tauqe/` and the credential files. A draft
sits inside the preserved tree, so it survives the rollback — a later turn can pick it
up again — while the code the turn proposed is gone with the rest of the working copy.
Crash recovery returns the repository to its recorded base the same way and never has
to consider a file it never tracked.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a build artifact directory.** `target/`, `node_modules/`, and generated
  output are toolchain products that belong where the toolchain puts them. The sandbox
  holds model-authored drafts, which is why the model may write it and the toolchain is
  never judged by it.
- **It is not a promotion channel.** Nothing about the sandbox makes a draft tracked. A
  draft enters the project only through an ordinary operation that targets a tracked
  path — a create, or a move destination — and that operation is judged like every other
  change: staging, verification, and the commit.
- **It is not a widened write boundary.** An existing draft is edited under the ordinary
  rules, so it must be in context first. The sandbox relaxes the ignore refusal for a
  path the turn creates; it never relaxes the context boundary.
- **It is not a switchable exemption.** No configuration flag turns the isolation check
  off. A repository that does not ignore `.tauqe/` refuses sandbox writes rather than
  recording a draft Git would track.
- **It is not a verification bypass.** The exclusion removes sandbox paths from the
  delivered change set; it does not remove the gate. A mixed change set is verified on
  its tracked paths exactly as before.
- **It is not durable storage.** Files there are expendable: the harness never deletes
  them, and nothing protects them from the next turn or from the developer. State that
  must survive belongs in the project, in Git, or in the harness's own storage, never in
  a sketch.
- **It is not the Refs cache.** The sibling `.tauqe/cache/refs/` shares the "exists on
  disk" discovery admission but is read-only by construction, because a snapshot the
  model could rewrite would stop being evidence. One directory holds drafts; the other
  holds material quoted from outside the repository.
- **It is not a place to hide a change.** A draft that must take effect is created at a
  tracked path; hiding it in the sandbox produces a file no compile, no test, and no
  commit will ever observe.

---

## 5. Related Chapters

- [The Three-Phase Modification Lifecycle](three-phase-lifecycle.md) — the ignore
  rejection the sandbox inverts.
- [Git Safety & Transactions](git-transactions.md) — the checkpoint, rollback, and
  recovery that leave the sandbox directory in place.
- [Structured Diagnostics and Verification Healing](verification-healing.md) — the
  change set the sandbox is filtered out of.
- [The TauqeProjectProtocol](tauqe-project-protocol.md) — the post-hook capture whose
  differential ignores the sandbox.
- [Discovery Engine](../ch02-perception/discovery-engine.md) — how an existing draft
  enters the context of a later turn.
- [The Context Model](../ch02-perception/context-model.md) — the Auto layer
  the entry is swept with.
