# Protected Paths and Trust Boundaries

Sovereign concept chapter: the `[project.boundaries.protected]` declaration, the shared
repository glob matcher that resolves it, and the categorical staging rejection that keeps
a turn away from the artifacts its own change set is judged with.

A change set is judged by something. A `Justfile` says how the tree is verified, a
specification says what an implementation must satisfy, and a reference manifest says which
external documents the harness may fetch. Each of those files is the yardstick of a turn,
and a turn able to rewrite its own yardstick can make verification agree with itself.
Protection keeps the yardstick out of the turn's hands without taking it out of its sight: a
protected path stays tracked, stays readable, and stays impossible to modify.

---

## 1. Motivation & Invariants ("Why")

### 1.1 A judge that can be rewritten is not a judge

Verification is worth its exit status only while the thing it consults stays outside the
control of the change set it examines. The declared verification command is the extreme
case: a turn that may edit the recipe deciding whether the tree is correct makes the gate
compare the change set with itself.

The same argument reaches one step away from the gate:

- a specification states what an implementation must satisfy, so an implementation able to
  rewrite the specification can weaken the requirement instead of meeting it;
- a reference manifest declares which external documents the harness may fetch, so a turn
  able to rewrite it could choose its own sources;
- a generated source that a post-hook overwrites describes the contract its consumers
  compile against, so a turn able to edit it can move the goalposts of the very code it was
  asked to write.

None of those failures is a compile error, which is what makes them dangerous: the turn ends
green, and the disagreement surfaces much later as a review comment or an incident.

### 1.2 A convention in the prompt is not enforcement

The contract rule that publishes the declared boundaries is one sentence in a prompt, and a
probabilistic engine occasionally ignores a sentence. A rule that is only stated is an
instruction, not an invariant. Protection therefore lives in the deterministic half of the
harness: the path is refused while the change set is still in virtual staging, before a byte
reaches disk, before the compiler starts, and before Git stages anything.

### 1.3 How a boundary differs from an ignore rule

An ignore rule and a boundary rule look alike and answer opposite questions:

| Rule | Statement | Effect on a write |
| --- | --- | --- |
| `.gitignore` | Git must not track this path | refused with `EditError::IgnoredByGit`, because the change set could never be committed |
| `[project.boundaries.protected]` | Git tracks this path; a turn must not change it | refused with `EditError::ProtectedPath`, because the path is not the turn's to change |

An ignored path is invisible to review and to the commit. A protected path is tracked,
reviewed, and readable, and only the authorship of a turn is denied. Reusing the ignore rule
for the second purpose would either hide the file from Git or admit the write, so the two
rejections stay separate down to their typed errors.

### 1.4 The invariants

1. **One declaration.** `[project.boundaries.protected]` is the only source of a boundary;
   the harness ships no default protection for any project.
2. **One matcher.** Patterns resolve through the repository glob matcher that pattern-based
   context addition and review path selection already use, so one spelling of a path means
   one thing everywhere.
3. **The boundary is decided first.** An operation on a protected path is refused before the
   ignore rule, before context membership, and before existence is consulted.
4. **Every mutating operation is covered.** A create, an overwrite, a replacement, an
   anchored range, and a deletion are refused on the addressed path; a move is refused when
   either its source or its destination is protected.
5. **A refusal changes nothing.** The rejected operation never enters the staged state, never
   reaches disk, and never touches the file it named.
6. **A boundary hides nothing.** The path remains in the repository, remains listed by Git,
   and remains requestable as ordinary `read_only` material.
7. **Protectable is not protected.** A specification, a generator, or a manifest is protected
   only once a pattern names it; an absent or empty subsection protects nothing.
8. **A blank pattern is a defect.** Configuration validation rejects it, and the staging
   compiler skips it with a diagnostic instead of compiling a matcher that matches nothing.
9. **A boundary is a path rule, not a file rule.** A create beneath a protected prefix is
   refused exactly like a rewrite of an existing protected file, so a specification cannot be
   smuggled in beside the protected ones.
10. **The boundary is knowledge before it is a rejection.** Every declared pattern is
    published in the contract rule of the edit contour, in declaration order, so a turn reads
    what it may not touch instead of discovering it by failing.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Trust boundary** — a repository path a turn may read but never create, rewrite, move, or
  delete.
- **Declaration** — the `[project.boundaries]` subsection of `tauqe.toml`; its `protected`
  list holds the patterns and nothing else.
- **Pattern** — one repository-relative glob of that list.
- **Compiled boundary** — the matchers produced from the declaration once per change set.
- **Boundary rejection** — `EditError::ProtectedPath`, the typed failure of one staged
  operation whose normalized path matched a compiled pattern.
- **Reserved reference** — the shape in which a protected file is meant to enter a turn: as
  read-only context material, like any other file.

### 2.2 The declaration

```toml
[project.boundaries]
protected = ["Justfile", "docs/specs/**", ".tauqe/refs/refs.md"]
```

The subsection carries one list and no command line, which makes it the part of the
`[project]` declaration that runs no process. This repository declares exactly the three
patterns above: the `Justfile` owns the verification lifecycle, the specifications state what
an implementation must satisfy, and the reference manifest declares which external documents
the harness may fetch. Each of them judges a change set rather than being judged by one.

### 2.3 What a pattern covers

| Pattern | Covers | Does not cover |
| --- | --- | --- |
| `Justfile` | the entry `Justfile`, and anything beneath a directory of that name | `Justfile.bak` |
| `docs/specs/**` | every path beneath `docs/specs/`, at any depth | `docs/specs.md`, `docs/other/x.md` |
| `crates/*/src/generated.rs` | one segment in place of `*` | `crates/a/b/src/generated.rs` |
| `*.proto` | the basename `*.proto` at any depth | `api.proto.in` |

Three properties of the matcher matter to a boundary. A pattern and a path are both
normalized before matching, so a trailing separator, a backslash separator, or an interior
`.` segment never changes the answer. `**` matches zero or more whole segments, so
`docs/specs/**` covers `docs/specs/auth.md` without a second entry. A non-wildcard pattern
matches itself or acts as a directory prefix, so `Justfile` protects a `Justfile/` subtree
but not `Justfile.bak`.

The matcher is deliberately the shared one. A boundary compiled by a second, private matcher
would drift from the patterns a developer sees everywhere else, and one path would then mean
one thing in context selection and another in staging.

### 2.4 Four independent facts about a protected path

| Fact | Value for a protected path |
| --- | --- |
| Git tracking | tracked, reviewable, and committed by the developer like any other file |
| Context access | requestable; the contract rule asks for it as `read_only` |
| Discovery | admitted exactly like any tracked file, with no special case |
| Staging admission | no mutating operation is admitted |

The rows do not imply one another, and the last two are the ones most often conflated:
`access="editable"` describes what the context layer lets a turn propose, while the boundary
describes what staging accepts. A protected file a developer marked editable in the interface
is still refused by staging, because the boundary is the later and stronger answer.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 One read and one compile per change set

Staging reads `[project.boundaries]` through the harness's single section reader, so a change
set touching fifty files still costs one configuration read and the subsection is decoded
exactly like the rest of the configuration. Blank entries are filtered out with a warning
while the matchers are compiled, and every remaining pattern becomes one compiled glob
matcher.

The compile step is also the failure boundary. A declaration the reader cannot parse degrades
to the default subsection, and the configuration loader reports the reason as a diagnostic,
so a broken boundary declaration never blocks an otherwise valid turn. A degradation in that
direction is visible; the opposite one — silently protecting nothing while a turn believes
otherwise — is not, which is why the shipped template documents the subsection.

### 3.2 The order of answers

Staging answers several questions about one operation, and the boundary is asked first:

1. **Normalization** — the path must resolve inside the repository, because a pattern is
   matched against a repository-relative path.
2. **The boundary** — the normalized path is matched against the compiled patterns.
3. **The ignore rule** — a write to an ignored path is refused, while a write into the
   mockups sandbox inverts the same answer.
4. **Context membership** — a path absent from context is refused, unless the same change set
   created it.
5. **Existence and content** — the operation is applied to the in-memory snapshot.

The order is not cosmetic. A protected path absent from context reports
`EditError::ProtectedPath` rather than `EditError::NotInContext`, because the second answer
would tell the model to add the file to context and retry — advice that could never succeed.

### 3.3 What each operation matches

| Operation | Paths matched against the boundary |
| --- | --- |
| `create`, `overwrite`, `replace`, anchored range, `delete` | the addressed path |
| `move` | the source and the destination |

The destination half of a move is the easy one to forget: writing it would overwrite a path
the turn may never touch, so it is refused even when the source is ordinary.

### 3.4 What a rejection looks like

The refusal is one failed file edit in the staged result, carrying
`EditError::ProtectedPath` and the normalized path. Nothing else changes: the operation never
joins a per-file group, the path never reaches the staged state, no other file of the change
set is affected, and the working tree is untouched.

Because the failure is an ordinary typed error, it travels the ordinary path: the retry reason
repeats the message verbatim in either patch-strategy dialect, since a boundary rejection
carries no strategy vocabulary to reword, and the request then names the only admissible
remedy — ask for the path as read-only reference material instead of proposing a change to it.

### 3.5 How the model learns the boundary

Two channels, one fact:

- **Before the attempt** — the `Minimal Coherent Change` contract rule appends one sentence
  listing every declared pattern in declaration order, stating that editing them is
  categorically forbidden and that such a file is requested only as `read_only` reference
  material. A project that declares nothing publishes no sentence.
- **After the attempt** — the typed rejection names the path, the subsection, and the remedy.

Both channels render into the byte-stable head of the prompt, so the boundary reads
identically in the interactive dialogue and in a plan discussion.

### 3.6 Working next to a boundary

A turn that genuinely needs a different contract has one honest move: state it. A
specification the task contradicts, a verification recipe that cannot express the check the
change needs, and a manifest that must name a new source are semantic blockers, not edits. The
harness then replans around the boundary instead of the turn rewriting it.

Reading, by contrast, is unrestricted. A protected path enters the turn as read-only context
material through the ordinary discovery requests, and it is never promoted to editable,
because promotion happens after a staged change set applies and a boundary rejection is
exactly the case in which nothing applies.

### 3.7 Failure semantics

- **No subsection** — nothing is protected; a repository that declares no boundary keeps
  every path editable.
- **An empty list** — the same answer, stated explicitly.
- **A blank entry** — configuration validation rejects the declaration as a whole, and the
  staging compiler skips the entry with a diagnostic, so one unusable pattern never disables
  the remaining boundary.
- **An unreadable or unparsable section** — the reader degrades to the default subsection, the
  loader reports the reason, and the turn continues unprotected rather than blocked.
- **A path beneath a protected prefix that does not exist yet** — still refused, because the
  boundary is matched against a path rather than an existing file.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not the developer's protection.** The developer may edit a protected file in an
  editor and commit it; the harness refuses only the authorship of a turn. A boundary is a
  statement about a change set, never about repository immutability.
- **It is not an ignore rule.** `.gitignore` names what Git must not track; a boundary names
  what Git tracks and a turn must not change. Confusing the two either hides the file or
  admits the write.
- **It is not enforcement in the interface.** The terminal frontend adds files to context and
  switches an entry between read-only and editable without consulting the boundary; the
  refusal belongs to staging, the only place a change is admitted.
- **It is not a sandbox.** The mockups sandbox inverts the ignore rule to admit draft writes
  into an untracked directory. A boundary does the reverse: it refuses writes to tracked
  artifacts. One grants a place to sketch, the other withdraws a place to decide.
- **It is not automatic.** A conventions file, a specification directory, and a generator are
  candidates, not boundaries: until a pattern names one, it is editable.
- **It is not a glob engine of its own.** The boundary carries patterns, not a private
  matcher; filesystem walking, ignore handling, and negation stay outside it.
- **It is not a way to hide work.** A boundary never removes a path from the repository, from
  review, or from context.
- **It is not verification.** It removes one class of self-agreement; it proves nothing about
  the change set and never replaces the mandatory verification command.
- **It is not a permission on the context layer.** An `access="editable"` entry and an
  admitted write are different facts, and the boundary decides the second one.

---

## 5. Related Chapters

- [The Three-Phase Modification Lifecycle](three-phase-lifecycle.md) — the staging pass that
  raises the rejection.
- [The TauqeProjectProtocol](tauqe-project-protocol.md) — the declaration this subsection
  belongs to.
- [The Mockups Sandbox](mockups-sandbox.md) — the opposite admission rule in the same staging
  pass.
- [Git Safety & Transactions](git-transactions.md) — what happens to the tree after
  verification.
- [Configuration and Project Preparation](../ch05-system/configuration.md) — the rest of
  `tauqe.toml` and its living template.
- [The Context Model](../ch02-perception/context-model.md) — the read-only
  material a protected file enters through.
