# The TauqeProjectProtocol

Sovereign concept chapter: the Unix CLI contract that every project operation
obeys, the single `tauqe.toml` surface that declares it, and the process boundary
that keeps the core engine technology-agnostic.

A harness that knows how to run `rustfmt`, how to invoke `tsc`, and which verifier
a repository prefers has smuggled a language runtime into its engine. TAUQE refuses
that coupling. Every operation that depends on the concrete technology of a code
base — formatting, disk-side code generation, and verification — is declared as a
plain command line and executed as an isolated process.

---

## 1. Motivation & Invariants ("Why")

### 1.1 A language-aware engine does not scale

Embedding `rustfmt`, `tsc`, `go vet`, and every future toolchain into the engine
makes the engine responsible for a moving target. Each new language forces an
engine change, a formatter upgrade becomes a harness release, and a polyglot
repository needs one engine per language. Worse, a formatter crash becomes a
harness crash: the line between *what the harness guarantees* and *what a toolchain
happens to do* disappears.

### 1.2 One surface, two audiences

Harness policy and project commands answer different questions. Policy answers *how
patient, how safe, and how expensive* the environment is: discovery rounds, retry
budgets, model roles, the verification gate. A project command answers *how this
repository is built*: which formatter claims a file, which generator must run after
a write, which command decides that the tree is correct.

Both live in `tauqe.toml`, in different sections, and the section boundary is
load-bearing:

- A formatter upgrade never requires a review of harness policy, and tightening a
  retry budget never rewrites the build.
- One file means one diff to review and one place to answer the only question that
  matters about a change set: what does this repository run?

### 1.3 The CLI is the universal interface

A process contract is expressible in every ecosystem: an executable, literal
arguments, an exit status, and — for a formatter — a stream on standard input and
output. `cargo`, `npm`, `go`, `make`, and a `scripts/` wrapper all satisfy it
without an SDK, a wire format, a handshake, or a daemon lifecycle. Two consequences
follow:

- **Operability.** A developer can run the exact command the harness ran and observe
  the same result, so a failure stays reproducible outside the harness.
- **No protocol drift.** There is no second process to keep in sync, no message
  schema to version, and no session state that survives an invocation.

### 1.4 The invariants

1. **The core is technology-agnostic.** It names a declared operation, a phase, and a
   path. It never names a compiler flag.
2. **One configuration surface.** The `[project]` declaration of `tauqe.toml` is the
   only description of what the harness may run for this repository.
3. **Literal arguments.** A command line is spawned directly, never through a shell,
   so a path with spaces or metacharacters is never interpreted.
4. **Verification is mandatory.** A repository that declares no verification command
   has no accepted change set.
5. **One request, one process.** Every phase is one invocation with its own
   wall-clock budget and its own process group; nothing survives it.
6. **The exit status is the verdict.** Zero means the operation succeeded; a non-zero
   status is a failure whose output is parsed into typed findings.
7. **Declarative routing.** The declaration decides which command owns a phase, so
   the engine never branches on a language.
8. **Nothing runs that resolution refused.** A blank command, an unusable reference,
   an undeclared target, or a reference cycle invalidates the declaration before a
   process is spawned.
9. **The change set is harness-owned.** The turn's authoritative changed paths reach
   the verification command on its standard input, one path per line. The command
   never derives that list from Git itself.
10. **Selection narrows work, never the gate.** A dispatcher may route the change set
    to fewer checks, but an empty list, a terminal on standard input, or a path no
    rule recognizes selects the complete suite.

---

## 2. Mental Model & Ontology ("What")

### 2.1 Terms

- **Project command** — a command line the repository declares: an executable,
  literal arguments, and an optional timeout.
- **Resolved command line** — the concrete executable and arguments left after
  reference resolution.
- **Reference** — an executable written as `@` plus the name of another declared
  command, meaning "run that entry instead".
- **Phase** — one moment of the modification lifecycle: `format`, `post_hook`, or
  `verify`.
- **Stream filter** — a command that rewrites content read from standard input onto
  standard output and touches no file.
- **Post-hook** — a command that runs on the working copy after the change set is
  written.
- **Verification command** — the mandatory command whose exit status decides whether
  the change set is accepted.
- **Change set** — the turn's authoritative changed paths: the model's edits, the
  post-hook artifacts, the files a repair round touched, and the working-copy
  differential of the transaction.
- **Change-set delivery** — the change set written to the verification command's
  standard input, one repository-relative path per line.
- **Named command** — an entry of the allowlist the model may address as a typed
  tool.
- **Task runner** — an external tool that exports the project's commands from its
  own declarative file instead of declaring each one in `tauqe.toml`.
- **Recipe** — one command a task runner exports, addressed by name.
- **Allowlist group** — the group a recipe must declare to become reachable; on
  Just it is `[group('tauqe')]`.
- **Runner manifest** — the commands a runner exports, split into the verification
  lifecycle and the named allowlist.
- **Exit status** — the only success signal; output is evidence, never a verdict.
- **Typed diagnostic** — a finding parsed from a failing command: file, line,
  column, severity, code, and message.

### 2.2 The declaration

| Section | Declares | Required |
| --- | --- | --- |
| `[project.runner]` | external task runner: `just` or `none` | optional |
| `[project.verify]` | the verification command line | mandatory (unless runner exports `verify`) |
| `[project.format]` | the in-memory stream filter | optional |
| `[project.post_hook]` | the disk-side step after the write | optional |
| `[project.commands]` | named allowlisted commands | optional |
| `[project.boundaries]` | repository glob patterns no turn may modify | optional |

Every entry shares one shape:

| Field | Required | Meaning |
| --- | --- | --- |
| `command` | yes | Executable to run, or `@` plus the name of a declared command. |
| `args` | no | Literal arguments passed to that executable. |
| `timeout_secs` | no | Wall-clock budget of one invocation. |

A reference carries no arguments of its own, and an entry that lists `args`
together with a reference is rejected: the arguments belong to the command that
runs, so an entry can never silently drop the arguments it lists.

`timeout_secs` is the one field a reference may declare, because it bounds a single
invocation instead of defining the command line. The outermost declared budget wins
along a reference chain, and a reference that declares none inherits the budget of
the command it names, so a stage can tighten a shared command without restating it.
The `[project.boundaries]` subsection follows neither shape: `protected` is a list of
repository glob patterns rather than a command line. A pattern names a path the harness keeps
tracked and readable while refusing every turn that would create, rewrite, move, or delete
it; *Protected Paths and Trust Boundaries* specifies that rejection.
### 2.3 Phases and moments

| Phase | Invocation shape | Moment | Failure contract |
| --- | --- | --- | --- |
| **Format** | staged text on stdin, rewritten text on stdout | before any byte reaches disk | the authored content is kept |
| **Post-hook** | a plain process on the working copy | after the write, before verification | the change set is aborted |
| **Verify** | a plain process with a declared timeout, the change set on stdin | after every phase above | typed diagnostics feed the healing loop |

The order is not negotiable: a formatter that ran after the write would create
diffs nobody proposed, a generator that ran before the write would read stale
input, and verification that ran before the generator would judge a tree that is
not final.

### 2.4 Verification targets

A verification request names a target: all, check, test, or lint. The declared
`[project.verify]` command is authoritative for every target, because the
repository described its own lifecycle; the target is a label for the interface,
never a command selector.

The harness keeps no legacy chain and no manifest auto-detection: a repository
that declares no verification command resolves no step, so the change set is
reported as unverified instead of running a command nobody declared.

### 2.5 The explicit runner choice

A repository may delegate its command surface to an external task runner instead of
declaring every command by hand. The choice is explicit and binary:

```toml
[project]
runner = "just"   # or "none", the default
```

There is deliberately no `auto`. Detection would let a stray `justfile` on disk
change what the harness executes, so a project either opts into a runner or relies
solely on its own declarations.

Just exports its whole model as a JSON dump, so the harness reads structured data and
never parses the justfile grammar. Three conventions turn that dump into a harness
surface:

- **The allowlist group is the only door.** A recipe is reachable only when it
  carries `[group('tauqe')]`. Administrative, release, and destructive recipes stay
  invisible at parse time and can never be addressed by the model — the *No Arbitrary
  Shell* invariant applied to a task runner.
- **The mapping is declarative.** `[doc('...')]` (or the recipe's doc comment) becomes
  the tool description, `metadata('timeout=...')` becomes the invocation's
  `timeout_secs`, and the parameters a recipe declares become its typed contract, so
  the harness knows which named values a command accepts. A recipe that cannot be
  invoked by name — a private one — is skipped so the published list never advertises
  a command the harness cannot run.
- **Two recipes fill the lifecycle.** A recipe named `verify` becomes the
  verification command and a recipe named `format` fills the in-memory filter; every
  other admitted recipe becomes a typed custom tool invoked as `just <recipe>`.

### 2.6 Configuration over exports

The runner supplements the declaration; it never overrides it. A `verify` recipe or a
named recipe exported by Just fills a slot only when `tauqe.toml` leaves that slot
empty. An explicit `[project.verify]`, or an explicit entry in `[project.commands]`
with the same name, always wins, so a recipe added to the justfile can never silently
replace a command the build owner declared.

### 2.7 Who decides which checks run

The harness owns exactly one list of changed paths and delivers it to the
verification command; what that list *means* belongs to the project. This
repository delegates the decision to a dispatcher: the declared `verify` recipe
reads the paths from standard input and composes the checks the change requires —
the workspace build for Rust sources and workspace manifests, the markdown filter
for documentation, both when the set mixes them or carries a path no rule
recognizes, and the complete suite when the list is empty or the script was
started from a terminal.

The composition happens entirely inside the project. The dispatcher reads no Git
state of its own, because the working copy also holds post-hook artifacts and the
developer's uncommitted work while the transaction is open; and it duplicates no
command line, because it only invokes declared recipes. The recipes behind it stay
internal — they carry no allowlist group — so the model can narrow the work of the
mandatory gate but can never address a second, lighter verification channel
beside it.

---

## 3. Mechanics & Lifecycle ("How it behaves")

### 3.1 Resolution

Resolution turns a declaration into one concrete invocation, or into a refusal.

- The declared verification command wins over every legacy phase, for every target.
- A reference chain is followed entry by entry, and every name it passes through is
  recorded, so a cycle is detected instead of looping forever.
- A blank executable, an undeclared target, an unusable reference name, an argument
  that cannot reach a process (a NUL byte), and a cycle are all rejected while the
  configuration is validated. No process is spawned for a declaration resolution
  already refused.
- A declared entry that fails to resolve at verification time is reported and the
  repository keeps deterministic resolution, so a broken declaration never turns
  into a silently skipped verification.

### 3.2 One invocation

An invocation spawns the resolved executable with its literal arguments, in its own
process group, so a timeout or a cancellation tears down the whole descendant tree
instead of orphaning the compiler or test binary the command started. Nothing is
passed through a shell: a path containing a space, a semicolon, or a glob character
reaches the process exactly as written.

The verification invocation additionally receives the turn's authoritative change
set on its standard input, one repository-relative path per line. The list is the
harness's, not the command's: it spans the model's edits, the post-hook artifacts,
the files a repair round touched, and the working-copy differential of the
transaction, so a command that read Git instead would judge work the turn never
proposed. An empty change set sends no input at all, and a dispatcher that meets an
empty list, a terminal on standard input, or an unrecognized path falls back to the
complete suite. A child process that never reads its standard input is harmless:
the broken pipe is reported as a diagnostic, never as a failed turn.

The wall-clock budget comes from the entry's `timeout_secs` and falls back to the
harness default of 600 seconds. A real test suite routinely exceeds a minute,
which is why the default is generous.

### 3.3 Exit status and typed diagnostics

The exit status is the verdict, and nothing else is. A successful command
contributes a pass. A failing command contributes a failure whose combined output is
parsed into typed diagnostics — file, line, column, severity, code, and message —
covering structured compiler records and rendered lines alike.

The harness never forwards a raw transcript into the healing context: a compiler
banner is written for a terminal, and a model pays for every escape sequence and
every duplicated note in it. Output that no parser recognizes still yields one
unstructured finding, which is better than discarding the only evidence of a
failure.

### 3.4 Timeouts, cancellation, and teardown

Cancellation is observed before the process is spawned and while its output is
awaited. A timeout or a cancellation kills the process group, reaps the command, and
reports a typed outcome: the caller learns that the invocation timed out or was
cancelled instead of receiving a partial result.

A command that exits without producing output is still a completed invocation.
Success and failure are decided by the exit status alone, so a quiet formatter that
did its job is never mistaken for a crash.

### 3.5 The stream filter contract

The declared in-memory formatter is a pure stream: it receives the staged text of
one file on standard input and returns the rewritten text on standard output, so no
byte reaches disk and the filter cannot touch the working tree. Three outcomes are
distinguished:

- **rewritten** — the filter processed the content; its output is kept;
- **absent** — the repository declares no filter; the authored content is kept;
- **failed** — the filter exited non-zero, timed out, or was cancelled; the authored
  content is kept and the reason is reported.

A filter that emits nothing while the staged text was not empty counts as a failure
to rewrite, because keeping the authored text is safer than erasing code.

### 3.6 The post-hook contract

The declared post-hook runs on the working copy strictly after the change set is
written and before verification. It receives no file list — the standard-input
delivery belongs to verification alone — so the declaration names its own literal
arguments and the harness captures whatever the hook produced through Git.

The working copy is snapshotted immediately before the hook starts and once more
after it exits successfully, so the difference is exactly the set of files the hook
created, rewrote, or deleted, with `.gitignore` respected. Those artifacts join the
turn's change set, which is what makes them part of verification and of the final
atomic commit instead of an untracked residue.

A hook that fails or is interrupted aborts the change set: committing a tree whose
generator failed would record artifacts nobody can reproduce.

### 3.7 Named commands

`[project.commands]` declares a closed allowlist, and the harness publishes it to
the model as typed tools. An invocation addresses one command by name; it cannot
compose, chain, extend, or redirect a command line, and it adds no arguments of its
own, because the declaration already names them.

A named command receives no change set either: standard input carries a file list
only for the verification lifecycle, so one delivery channel exists instead of a
second one the model could address.

The invocation is answered with the real result: its exit status and the head of its
output join the turn journal, so the next round reasons about what the command
actually printed instead of the assumption that it ran. When no declared command
covers the required operation, the correct answer is a semantic blocker rather than
an improvised command.

### 3.8 Validation and refusal

Configuration is validated when it is loaded: the model roles must resolve, and the
`[project]` declaration must name a verification command whose reference chain is
sound. Rewriting `tauqe.toml` is validated a second time, in memory, while the
change set still lives in staging, so a declaration the harness could not load on
its next start never reaches the working tree.

### 3.9 Runner loading and precedence

When `runner` is not `none`, the harness queries the runner once through its own
declarative dump and merges the result into the project configuration. The query is an
ordinary project process: literal arguments, its own process group, the caller's
wall-clock budget, and the turn cancellation signal, so a missing or hung runner is
reported as a typed failure instead of stalling the turn.

A runner that is absent, fails, or answers with an unreadable dump is not treated as
one that exports no command: it yields a typed error and never a partially trusted
command list. Merging then follows one rule — *configuration wins*:

- A declared `[project.verify]` already fills the verification lifecycle, so the
  runner's `verify` recipe is ignored.
- A declared `[project.format]` already fills the in-memory filter — the single
  formatting surface of the staging contour — so the runner's `format` recipe is
  ignored.
- A declared entry in `[project.commands]` keeps its slot, so a recipe exported under
  the same name is ignored.
- Every remaining admitted recipe joins the named allowlist.

Because the merge is deterministic, the same `tauqe.toml` and the same justfile always
produce the same tool list in the same order, so the published tools and therefore the
prompt bytes stay stable across turns.

---

## 4. Boundaries & Anti-Patterns ("What it is NOT")

- **It is not a wire protocol.** There is no request envelope, no handshake, and no
  daemon to keep alive: the contract is a process, its arguments, its streams, and
  its exit status.
- **It is not a shell.** A command line is a declared executable with literal
  arguments; nothing is passed through a shell, so a path with spaces or
  metacharacters is never interpreted.
- **It is not a language server.** Each phase is a fresh invocation that keeps no
  session state, so a format request, a post-hook, and a verification are three
  independent processes.
- **It is not a build-system parser in the core.** The engine runs a declared
  command; it never reads `Cargo.toml`, `package.json`, or `go.mod`, and it never
  detects a toolchain from them.
- **It is not a second manifest.** One file declares both harness policy and project
  commands; a separate engineering manifest would reintroduce two owners, two review
  cadences, and a second place to look.
- **It is not hidden autodetection.** A task runner runs only when `runner` names
  it; a `justfile` on disk is inert until the declaration opts in.
- **It is not a second command surface.** A runner supplements `[project.commands]`;
  an exported recipe never overrides a declared command.
- **It is not harness policy.** Timeouts, the verification gate, and retry budgets
  belong to the harness sections; the `[project]` declaration holds only what runs.
- **It is not an optional optimization.** Formatting and verification are delegated
  through it, not duplicated beside it.
- **It does not trust the declaration blindly.** Every invocation is bounded by a
  timeout, isolated in a process group, validated against the declaration, and
  answered with its real exit status.
- **It is not the owner of the change set.** The harness delivers the turn's changed
  paths on standard input; a command that read Git instead would also see post-hook
  artifacts and the developer's uncommitted work.
- **It is not a second verification channel.** The internal checks a dispatcher
  composes stay unpublished, so the mandatory gate can be made cheaper but never
  swapped for a lighter one.

---

## 5. Related Chapters

- [The Three-Phase Modification Lifecycle](three-phase-lifecycle.md) — the three
  phases in the order a modification uses them.
- [Protected Paths and Trust Boundaries](protected-paths.md) — the trust boundary a staged
  operation is checked against first.
- [Structured Diagnostics and Verification Healing](verification-healing.md) — what
  a failing verification produces.
- [Git Safety & Transactions](git-transactions.md) — what happens to the tree after
  verification.
- [Configuration and Project Preparation](../ch05-system/configuration.md) —
  `tauqe.toml`, the `[project]` declaration, and the self-preservation invariant.
- [The Autonomous Bounded Turn FSM](../ch01-foundations/bounded-turn-fsm.md) — where
  the phases are scheduled.
