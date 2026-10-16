# 5. Keyboard-First Interface

TAUQE's terminal UI combines the design philosophies of **Magit** (structured, live document state) and **Org-mode** (collapsible outline hierarchy).

---

## 5.1 Primary Views and Universal Navigation

To guarantee universal compatibility across modern and legacy terminal emulators alike, all primary view modes and key actions are bound to portable Emacs/Readline chords instead of function keys:

Throughout this chapter, `Ctrl+<key>` denotes the abstract **Primary Command Modifier** (`C-<key>`) and `Alt+<key>` the abstract **Meta Command Modifier** (`M-<key>`). Both are independent personal workstation settings that can be swapped in `tui.toml` (see section 5.5). Function keys (`F1`..`F12`) are never bound: window managers, terminal multiplexers, and laptop firmware intercept them unpredictably, and 60% and compact keyboards omit them entirely, so every action lives in the main alphanumeric block or on a dedicated navigation key.

- **`Ctrl+1` / `Alt+1` (REPL View):**
  - The central engineering cockpit and execution canvas where coding turns take place.
  - Displays turn progression as a structured, live document: collapsible reasoning blocks (`Ctrl+R`), streaming markdown explanations, and interactive proposed file diffs.
  - **Dual-Focus Architecture:** Switch focus seamlessly between the prompt editor and the response/diffs viewport using `Ctrl+Space` or `Shift+Tab`.
  - Multi-line intent editor with auto-growing height and Emacs navigation (`Ctrl+A`, `Ctrl+E`, `Alt+B`, `Alt+F`, `Ctrl+P`, `Ctrl+N` to move between lines, `Ctrl+K`, `Ctrl+Y`, `Ctrl+W` / `Alt+Backspace` to kill word backward, `Tab` for 2-space indentation).
  - Persistent prompt history: Use `Alt+P` / `Alt+N` to cycle through previous prompt inputs. History is saved to `.tauqe/prompts.history` and persists across sessions.
  - Newline insertion: `Shift+Enter`, `Alt+Enter`, or `Ctrl+J`.
  - Response clipboard: Press `Alt+C` or `Alt+Y` while idle to copy the model's markdown response to the system clipboard.
  - Undo AI commit: Press `Ctrl+Z` or `Alt+U` to trigger safe rollback. If no AI commit exists, TAUQE displays a status notice without opening unnecessary dialogs.
  - Active LLM selection: Press `Alt+M` or `Ctrl+M` from any screen (or click the model in the header) to open the model picker. See section 5.7 for the role model.
  - Contextual help: Press `Ctrl+H` / `Alt+H` from any view or `?` in navigation views to open the interactive help dialog.
  - Configuration reload: Press `Ctrl+O` to reload `tui.toml` settings without restarting TAUQE.
  - Theme selector: Press `Alt+T` (`M-T`) to open the interactive theme picker and preview built-in presets live (see section 5.10).

### What Happens in REPL When You Submit a Prompt
When you submit an engineering intent, TAUQE does not simply invoke a single-shot completion. Instead, it guides the model through an **autonomous, multi-round bounded turn**:
1. **Exploration & Discovery Rounds:** The model analyzes the task against the Tree-sitter symbol map. If critical type definitions, helper modules, or project documentation are missing from context, it autonomously requests them via protocol tags (`<context_request>`, `<doc_request>`). The footer reflects the active discovery round (`Round 1`, `Round 2`...). Control does not bother the user with context logistics; the model gathers facts until the architectural picture is crystal clear.
2. **Code Generation & Virtual Staging:** Once equipped with the necessary context, the model drafts minimal, coherent changes (search/replace blocks, new files, renames). These edits are applied to an **in-memory virtual staging tree** first, ensuring zero disk corruption from speculative failures.
3. **Patch Retry Loop (Staging Resilience):** LLMs occasionally emit code hunks with slight indentation shifts, mismatched whitespace, or stale surrounding context lines that cannot be cleanly matched against disk. Instead of aborting the turn or dumping broken patches, TAUQE preserves all successfully staged files, pinpoints the failing files, and prompts the model with targeted patch error feedback to regenerate only the problematic hunks (up to `max_retries`).
4. **Deterministic Toolchain Verification & Self-Healing:** After in-memory staging converges and changes are written to disk, TAUQE automatically triggers deterministic project toolchains (`cargo check`, `npm test`, linters). If compile or lint errors are detected, the raw compiler diagnostics are packaged and fed back to the model for an immediate **verification healing cycle**, correcting syntax or type errors before the developer even inspects the code.
5. **Git Transaction & Rollback:** The entire successful sequence is recorded as an isolated Git commit. If the outcome deviates from the developer's intent, pressing `Ctrl+Z` or `Alt+U` opens a confirmation dialog to immediately and atomically restore the repository to its exact pre-turn state.

### Live Reasoning Display During Streaming
Reasoning output during an active turn is a progress indicator, not a document that must be read while it grows. TAUQE uses this distinction to keep the interface responsive even when the reasoning stream is long and contains draft code:

- **Sliding window (Reasoning Tail Stream):** While reasoning is actively streaming, the REPL view renders only a compact slice of the most recent lines (ten lines by default) below the `[REASONING]` badge. The badge shows the running total line count (`Thinking… (N lines)`), so forward progress stays visible without re-wrapping the entire, ever-growing transcript on every update.
- **Deferred deep rendering:** The tail uses lightweight per-line styling. The full Markdown parse and `syntect` code-block highlighting are not restarted against the still-open stream; they run exactly once, when the reasoning stage finalizes.
- **Finalized transcript:** When reasoning completes, the sliding window is replaced by the complete reasoning transcript rendered through the standard Markdown pipeline, including syntax-highlighted code fences. The `Ctrl+R` fold/unfold toggle applies to both the live tail and the finalized transcript.

- **`Ctrl+2` / `Alt+2` (Plans View):**
  - Workspace for managing tactical task hierarchies and execution roadmaps, split into an immutable Concept section (system invariants) and an Implementation section (executable steps).
  - Unified navigation: `n` / `p` (or `↑` / `↓`), with `Ctrl+N` / `Ctrl+P` for accordion inspection of the next / previous item.
  - Interactive tree of tasks with explicit statuses: Discussion, Todo, InProgress, Done, Blocked, and Cancelled.
  - Execution: `x` executes the target step, group, or entire plan (with confirmation); `Enter` executes a leaf step and folds/unfolds on groups and plan headers.
  - Discussion and refinement: `d` discusses the selected concept/step or the whole plan in REPL; `R` deep-refines plan architecture and steps with the Architect.
  - Intentional status assignment: Press `t` or `s` to open the status dialog (`1` Discussion, `2` Todo, `3` InProgress, `4` Done, `5` Blocked, `6` Cancelled).
  - Bulk folding (`a`), plan refresh (`r`), deletion (`Delete`, with confirmation), and Markdown copy (`c` / `y`).
- **`Ctrl+3` / `Alt+3` (Review View):**
  - Dedicated code review workspace for static architectural and security audits.
  - Unified navigation: `n` / `p` (or `↑` / `↓`), with `Ctrl+N` / `Ctrl+P` for accordion inspection of the next / previous finding.
  - Interactive findings checklist: fold/unfold items (`Tab` / `Space`), execute the selected finding or every Todo finding in the session (`x`, with confirmation), run cross-session execution across all review sessions (`X` / `Shift+X`, with confirmation), and discuss the finding or the whole session in REPL (`d`).
  - Intentional status assignment: Press `t` or `s` to open the status selection dialog (`1` Discussion, `2` Todo, `3` InProgress, `4` Fixed, `5` Rejected).
  - Filtering and bulk folding: `f` toggles between all findings and hide-closed (FIXED / REJECTED); `a` folds or unfolds every session and finding.
  - Run and refresh: `r` starts a new review of the current context; `Shift+R` refreshes the session list from the server.
  - Clipboard and lifecycle: `c` / `y` copy the selected finding or session; `Delete` deletes a session (with confirmation); `Ctrl+R` folds or unfolds the thinking stream.
- **`Ctrl+4` / `Alt+4` (Context View):**
  - Displays Pinned, User, and Auto context layers with token metrics.
  - Quick keys: `e` (add file as editable), `r` / `a` (add file as read-only), `t` (toggle access Editable ↔ Read-Only), `u` (promote an Auto file to persistent User), `c` / `C` (clear all auto-requested files), `d` / `x` / `Del` (remove a file from context).
  - Navigation and folding: `n` / `p` (or `j` / `k`, `↑` / `↓`) move the selection; `Space` / `Tab` or `Enter` fold or unfold a section; `Enter` also promotes an Auto file to User. `Esc` / `q` return to the REPL view.
- **`Ctrl+5` / `Alt+5` (History View):**
  - Paginated audit log of semantic turns, code review runs, AI commit hashes, and file modifications.
  - Collapsible items: All entries are folded by default, displaying the role badge, commit summary, and preview lines.
  - Press `Tab`, `Space`, or `Enter` to fold/unfold the active entry and inspect the full response and modified files.
  - Item navigation & clipboard: Use `n` / `p` or `[` / `]` to navigate turns, `l` to recenter the active item, `c` to copy a code block, and `y` to copy the item text to the clipboard.
  - Scroll keys `↑` / `↓` (or `k` / `j`) fetch older turns as the top of the log is reached; `Home` / `End` jump to the oldest / newest message.
  - Header visibility guarantee: Active entry headers are automatically kept visible in the viewport during navigation.

---

## 5.2 Dual-Focus Architecture in Develop (Editor vs. Viewport)

In the Develop view, TAUQE resolves the tension between text editing and inspection through a clean two-focus model without modal traps:

### 1. Editor Focus (Default Mode)
- **Typing always types:** Every printable character is unconditionally inserted into the text buffer. Single-letter command keys are never intercepted during text entry.
- **Emacs navigation:** `Ctrl+A` / `Ctrl+E` for start/end of line, `Alt+B` / `Alt+F` for words backward/forward, `Ctrl+P` / `Ctrl+N` for moving between lines, `Ctrl+K` to kill to end of line, `Ctrl+Y` to yank from kill-ring, `Ctrl+W` / `Alt+Backspace` to kill word backward.
- **Persistent prompt history:** `Alt+P` and `Alt+N` cycle backward and forward through previous prompt inputs.
- **Switching focus:** Press `Ctrl+Space` or `Shift+Tab` to move focus to the response and diffs viewport.

### 2. Viewport Focus (Response & Diffs Inspection)
- **Visual indicator:** The prompt border dims to dark gray and displays `[VIEWPORT]`, while the currently inspected file shows an active cursor (`▶ `).
- **File selection:** Use `Ctrl+N` / `Ctrl+P` or arrow keys (`↑` / `↓`) to navigate through modified files. If no files are present, `Ctrl+N` and `Ctrl+P` scroll the assistant response.
- **Folding diffs:** Press `Tab`, `Enter`, or `Ctrl+T` to fold or unfold diff hunks for the active file.
- **Scrolling response:** `PgUp` and `PgDn` scroll the response content by full pages.
- **Zero-loss return to typing:** Pressing any printable character (letters, digits, symbols, or unshifted space) immediately returns focus to the prompt editor and inserts that character at the text cursor without dropping the key event.
- **Explicit return:** Press `Esc`, `Ctrl+Space`, or `Shift+Tab` to return focus to the prompt editor without modifying text.

---

## 5.3 Input Safety and Onboarding Protection

- **Commands require modifiers:** Destructive or state-changing actions use explicit modifiers (`Ctrl+Z` / `Alt+U` for Undo, `Ctrl+S` / `Alt+S` for Squash, `Alt+C` for copying responses).
- **Universal abort chord (`C-g`):** Pressing `Ctrl+G` (Emacs `keyboard-quit`) is normalized to `Esc` before any handler runs, so a single chord aborts the active generation, dismisses the top-most modal, returns from an auxiliary view, or detaches the active discussion focus exactly as `Esc` would. It is never wired to `SIGINT`, so the terminal's standard `Ctrl+C` behavior is preserved.
- **Draft protection on exit:** Pressing `Ctrl+Q` prompts for confirmation whenever a prompt draft is non-empty, a model turn is active, or an unapplied squash diff exists, preventing accidental loss of uncommitted work.
- **Onboarding isolation:** During initial configuration (`ViewMode::Onboarding`), view switching keys (`Ctrl+1..5`, `Alt+1..5`) and header mouse clicks are locked until setup is completed, preventing accidental bypass of credential verification.
- **Deterministic onboarding sequence:** Initial setup advances through a fixed order — Git, Configuration, Credentials, Layout, Command Modifier, Ready — derived solely from completion flags. Optional phases the developer deliberately skips are recorded, so the flow never loops back and no step is silently dropped.
- **Contextual help:** Press `Ctrl+H` / `Alt+H` from any view or `?` in navigation views to open the contextual help dialog.

---

## 5.4 Modal Dialog Architecture & Interaction Standard

Modal dialogs follow a unified, three-tier layout standard designed for keyboard reliability, safety, and layout neutrality:
- **Structure:** Every dialog consists of a distinct **Header** (title and context), a central **Body** (content, form, or options), and an **Actions / Keybindings Footer**. The footer has two bands: an interactive action bar carrying the buttons, and a non-interactive **docked help panel** directly below it.
- **Docked Help Panel:** The hint line does not float in the gap between the buttons and the border. It forms a full-width, `overlay`-filled strip docked to the bottom inner row of the card, immediately above the bottom border. Docking the hint to its own filled row separates persistent guidance from the interactive controls and visually closes the dialog card. The strip is non-interactive: it takes no focus and exposes no selectable controls.
- **Semantic Hint Typography:** Hints are assembled from structured spans rather than a flat string: chords render in `text_primary` (emphasized), action verbs in `text_secondary`, and separators (`/`, `•`) in `text_muted`. This keeps keys scannable and legible at any color depth and follows the semantic design system of section 5.10.
- **Hit-Testing Synchronization:** The geometry helpers that report clickable button rectangles derive their coordinates from the same `Layout` formula the render path uses, so shifting the action bar (or adding the docked panel below it) never desynchronizes mouse clicks from the drawn buttons: the buttons you see are always the buttons you can click.
- **Destructive Action Safety:** Confirmation dialogs (Quit with draft, Undo AI commit, Git squash, Cancel generation, Delete plan, Clear history) default focus securely to `[ Cancel ]`.
- **Button Navigation:** Switch focus between buttons using `Tab`, `Shift+Tab`, or arrow keys (`←`, `→`). Press `Enter` or `Space` to activate the focused button.
- **Fast Accelerators:** In confirmation dialogs, pressing `y` / `Y` confirms immediately, while `n` / `N`, `q`, or `Esc` cancels.
- **Intentional Status Dialogs:** In Plans and Review, task and finding statuses are selected through explicit modal dialogs rather than accidental cycling. Plans offer Discussion, Todo, InProgress, Done, Blocked, and Cancelled (`1`..`6`); Review offers Discussion, Todo, InProgress, Fixed, and Rejected (`1`..`5`). Choose a target status using direct numeric keys, or navigate with `n` / `p` and confirm with `Enter` / `Space`.
- **Git Squash Safety:** Within the squash dialog (`Ctrl+S` / `Alt+S`), `Enter` or `Space` folds/unfolds changed files in the file list, and `Enter` in the message editor inserts a newline. Triggering the squash requires `Ctrl+Enter` or `Ctrl+S`, which opens an explicit confirmation dialog summarizing affected commits.
- **Batch Cancellation Dialog (Three-Way Stop):** When a running plan or review batch receives a cancellation request (`Ctrl+C`), the dialog offers three explicit actions instead of a binary confirm: `Continue`, `Soft Stop`, and `Hard Stop`. `Soft Stop` lets the current step finish its bounded turn, compiler verification, and Git commit before the queue halts; `Hard Stop` aborts the current step immediately. `Continue` holds the safe default focus, so an accidental keypress never discards already-committed work. Direct accelerators `c` (Continue), `s` (Soft Stop), and `h` (Hard Stop) choose an action without moving focus, while `Tab`, `←` / `→`, `Enter` / `Space`, and `Esc` follow the standard dialog navigation rules above. While a soft stop is pending, the footer shows `[Stopping after current step]`, so a graceful drain is never confused with an immediate abort.

---

## 5.5 User Configuration (`tui.toml`)

In accordance with XDG standards, developer-specific terminal interface preferences are kept strictly separated from project configuration (`tauqe.toml`).

User settings reside in `$XDG_CONFIG_HOME/tauqe/tui.toml` (or `~/.config/tauqe/tui.toml`):

```toml
[input]
# Keyboard layout mapping preset:
#   "none"      - No character translation (default). Safe and layout-neutral.
#   "ru_jcuken" - Maps Cyrillic ЙЦУКЕН to QWERTY for navigation (j/k, x, t)
#                 and Ctrl/Alt shortcuts (Ctrl+B, Ctrl+Z, etc.).
layout = "none"

# Abstract Primary Command Modifier for text-input contexts:
#   "ctrl" - Commands are formulated as C-<key> (default).
#   "alt"  - Commands are formulated as Alt-<key> (M-<key>).
# This only changes the modifier used for input-level actions.
primary_modifier = "ctrl"

# Abstract Meta Command Modifier for text-input and navigation chords:
#   "alt"  - Commands are formulated as Alt-<key> (M-<key>, default).
#   "ctrl" - Commands are formulated as Ctrl-<key> (C-<key>).
# The primary and meta modifiers are independent and symmetric, so a
# terminal without a dedicated Meta key can freely swap the two chords.
meta_modifier = "alt"

# Optional universal character mapping (langmap) for any non-Latin layout:
# Supports two formats:
# 1. Parallel string mapping: "from_chars;to_chars" (e.g. Vim langmap style)
# 2. Pairs list: "йq,цw,уe" or "й:q,ц:w"
# langmap = "йцукенгшщзхъ;qwertyuiop[]"

[theme]
# Named built-in preset; it takes precedence over `mode` when set:
#   "tauqe-dark" (default) | "tauqe-light" | "catppuccin" |
#   "gruvbox" | "solarized-dark"
name = "tauqe-dark"

# Legacy light/dark mode, used as the fallback when `name` is unset.
# "auto" (default) probes the terminal background via COLORFGBG:
# mode = "auto"

# Terminal color depth for RGB styles. "auto" (default) detects it from
# COLORTERM/TERM; "truecolor" forces 24-bit, "ansi256" forces the xterm
# 256-color cube, and "ansi16" forces the attribute-based fallback:
# color_depth = "auto"

[notifications]
# Terminal audio bell (\x07 / BEL). In Kitty, WezTerm, Alacritty, and modern desktop
# window managers, this triggers window urgency hints or tab highlighting:
sound = true

# Native desktop notifications via terminal OSC sequences:
desktop = true

# Desktop notification protocol: "osc9" (default), "osc777", or "both":
desktop_protocol = "osc9"

# Only notify when the terminal window is unfocused:
only_unfocused = true

# Minimum turn/review execution duration in seconds before triggering notifications
# (prevents notification spam on fast sub-second interactions, default: 5):
min_duration_seconds = 5

# Optional shell command hook executed on long turn completion:
# command = "paplay /usr/share/sounds/freedesktop/stereo/complete.oga"
```

### Why C- and M- chords instead of function keys

The two modifier parameters express one symmetric idea. `primary_modifier` selects the abstract `C-<key>` chord; `meta_modifier` selects the abstract `M-<key>` chord. They exist because a terminal has no dedicated Meta key: modern terminals send `M-` as `Alt`, legacy setups send it as `Ctrl`, and a developer whose `Alt` is consumed by the window manager can swap the two chords freely. No view hardcodes `KeyModifiers::CONTROL` or `KeyModifiers::ALT`; every chord is matched through `InputConfig::is_primary(...)` and `InputConfig::is_meta(...)`.

TAUQE deliberately binds no function keys (`F1`..`F12`). Function keys are the least portable part of a keyboard: window managers, terminal multiplexers, and laptop firmware intercept them, some are reserved by the terminal itself, and 60% and compact keyboards omit the row entirely. Anchoring the interface on Emacs/Readline chords (`C-` / `M-`) and dedicated navigation keys (arrows, `Home`/`End`, `PgUp`/`PgDn`) keeps every action reachable on any keyboard and lets a cramped or non-standard layout be remapped without relearning the interface.

---

## 5.6 Long-Running Turn Notifications

Engineering tasks (discovery rounds, toolchain verifications, test suites) can take dozens of seconds. TAUQE ensures developers never miss the completion of their work when switching to another window or workspace:

- **Urgency Alert & Terminal Bell:** Emits a standard ASCII `BEL` (`\x07`), prompting modern terminal emulators (Kitty, WezTerm, iTerm2) to raise window urgency flags in your desktop taskbar or window manager.
- **Zero-Dependency Desktop Notifications:** Utilizes standard terminal OSC protocols (`OSC 777` and `OSC 9`) to display native desktop banners with turn summaries or verification statuses. Clicking the notification immediately focuses the terminal window.
- **Smart Duration Threshold:** Notifications fire only if execution equals or exceeds `min_duration_seconds` (default: 5 seconds), eliminating noise during rapid iterative dialogs.

### 5.6.1 Turn versus Batch Signal Granularity

The natural unit of waiting differs between an interactive Develop turn and a batch supervisor:

- **Single interactive turn (REPL):** One prompt maps to one turn, so the notification fires as soon as that turn finishes. Nothing is aggregated away.
- **Batch supervisors (Plan Execution, Review Execution):** One launch runs many steps, and the developer waits for the *whole batch*, not for each individual step. To avoid per-step noise, intermediate batch steps never raise desktop banners or sound alerts.

### 5.6.2 Aggregated Batch Notifications

Batch supervisors emit a typed completion event (`plan/batch_finished`, `review/batch_finished`) once the internal `BatchExecution` loop ends. This event carries the final status (`Completed`, `Failed`, `Blocked`, `Cancelled`, `Stopped`), the executed and scheduled step counts, and the elapsed time. The TUI marks batch turns, suppresses the usual per-turn signal on each step's `MODEL_FINISHED`, and fires exactly one aggregated notification from this final event:

- **Normal completion (`Completed`):** A single notification reports the finished status together with the step tally, so the developer learns the outcome of the entire operation at a glance instead of reconstructing it from dozens of individual signals.
- **Failure or block stop (`Failed` / `Blocked`):** When the batch halts early on a failed or blocked step, the aggregated notification reports the halted status rather than a plain success. The offending step and any blocker reason remain inspectable in the REPL view; the notification only signals that the batch did not run to completion.
- **Cancellation (`Cancelled`):** A cancelled batch reports the `Cancelled` status through the same single notification.
- **Graceful stop (`Stopped`):** A soft stop lets the running member finish its turn, verification, and commit, then halts the queue, so the batch reports the `Stopped` status through the same single notification — distinguishing a graceful drain from an immediate abort.

The aggregated notification is gated by the same `tui.toml` settings as single-turn signals (`sound`, `desktop`, `desktop_protocol`, `only_unfocused`, and `min_duration_seconds`), so batching changes only *when* the signal fires — never *how* it is delivered.

---

## 5.7 Model Roles and Model Selection

Tasks differ in their nature: interactive dialogue, conceptual planning, concrete code generation, analytical review, and routine history compaction each demand a different competence. Instead of making the developer remember model identifiers, TAUQE assigns a model to each **semantic role** in the `[models]` section of `tauqe.toml`:

```toml
[models]
# `primary` is mandatory and must list at least one model; its first entry is the
# idle active model and the deterministic default for the other roles.
primary = ["~google/gemini-flash-latest"]
scribe = "~anthropic/claude-haiku-latest"      # history compaction, commit messages
# Optional specialized lists; when omitted, coder, architect and reviewer inherit
# the entire primary list. The `coder` list doubles as the escalation ladder.
coder = [
  "~deepseek/deepseek-flash-latest",
  { model = "~google/gemini-flash-latest", effort = "medium" },
]
architect = [{ model = "~google/gemini-flash-latest", effort = "high" }]
reviewer = ["~anthropic/claude-sonnet-latest"]
auto_level_up = true                           # escalate the coder ladder on failure
```

- **Selection model:** The active choice is either a role (`Primary`, `Scribe`, `Coder`, `Architect`, `Reviewer`) or one specific model. A role is resolved to a concrete model by the server, so the TUI never guesses the mapping. `primary` is mandatory; an omitted specialized role inherits the entire `primary` list instead of failing configuration load.
- **Coder ladder:** The `coder` value is polymorphic. A bare model string or a `{ model, effort }` table yields a one-rung ladder; an ordered array expresses an explicit escalation ladder, where each rung pairs a model with an optional reasoning depth:
  ```toml
  coder = [
    "~deepseek/deepseek-flash-latest",
    { model = "~deepseek/deepseek-flash-latest", effort = "medium" },
    "~anthropic/claude-sonnet-latest",
  ]
  ```
- **Picker (`Alt+M` / `Ctrl+M`, or a click on the model in the header):** The first screen lists the semantic roles with their resolved models, shown by their `alias` when set and otherwise by provider and name. Selecting `Others...` opens the full catalog to pin one specific model. `Esc` closes the picker.
- **Header:** A semantic role is rendered as its title only, for example `Coder`. The concrete model name is deliberately omitted to keep the header free of provider-level noise; the resolved role-to-model mapping stays visible in the picker. A pinned specific model is shown by its own name instead (truncated on narrow terminals).
- **Background work:** Squash commit messages and history compaction always use the Scribe role. They are routine tasks, so they should not spend the budget of a stronger model.
- **Level Up:** When `auto_level_up = true`, a plan step or review fix that still fails after verification healing is rolled back to the Git checkpoint taken before the step. Execution then advances one rung along the **coder ladder** within the same Coder role — it never switches to the Architect role. Deepening reasoning on the same model and promoting to a stronger coder tier are both intra-role moves, so the strict What / How separation is preserved. Files found during discovery stay in context, so the stronger coder tier does not search again. When the ladder is exhausted, or Level Up is off, the normal Fail-Fast stop applies. REPL never escalates automatically: the developer's explicit choice in the picker is always respected.

---

## 5.8 Plan Stepper in REPL (Batch Step Progress)

When a REPL turn belongs to a plan execution (a single step or a server-orchestrated batch), the REPL view renders a compact **Plan Stepper** above the model output. Its role is to keep the developer oriented during a multi-step scope without leaving REPL and without switching to the Plans view (`F2`).

- **Title-only by design:** The stepper shows only step identifiers, a concise status (`Done`, `InProgress`, `Todo`), and short step titles, fitting in a 2–4 line header. It never renders expanded diffs, detail text, or concept nodes. Keeping the header terse preserves the reasoning and code output as the visual focus and avoids the noise of a full plan tree.
- **Zero overhead for ad-hoc turns:** The stepper appears only when the turn carries plan-step metadata. A plain ad-hoc prompt renders exactly as before, with no extra header.
- **Diff scoping invariant:** The Proposed Edits section and the streaming file list belong strictly to the *current active step*. Files that an earlier step already committed are removed from the working area when the batch advances, so the visible diffs always describe one step rather than an accreted history. What is shown is what this step changed; earlier steps are already safe in Git.

---

## 5.9 Markdown Rendering in the Viewport

Assistant responses, reasoning transcripts, plan details, and history entries are rendered through a single Markdown pipeline tuned for the terminal, so the same document reads consistently across every view.

- **Adaptive tables:** Column widths are derived from the widest cell in each column and then fitted to the available width. Cells wrap on word boundaries, keeping words intact and splitting only a word wider than its column on Unicode scalar boundaries, so no multibyte character is ever cut. Vertical borders stay on the same terminal column across every wrapped physical line, and one horizontal separator (`├───┼───┤`) is drawn between consecutive data rows, so wide, multi-row tables remain readable.
- **Column alignment:** Markdown column alignment (`:---`, `:---:`, `---:`) is honored, so left-, center-, and right-aligned cells line up as authored.
- **Block highlighting:** Fenced code blocks render borderless with a background band, a language label, and a `[📋 Copy]` button in the header line. Syntax highlighting is applied per language through `syntect`, and the active color theme is matched so the code band stays legible in both dark and light modes.
- **Theme and color depth:** The renderer detects 24-bit truecolor support from the environment and quantizes to ANSI-256 where truecolor is unavailable, so highlighting degrades gracefully on legacy terminals. Reasoning output uses a distinct, dimmed blue palette to keep it visually separate from the final answer.

---

## 5.10 Semantic Theme System and Color Depth

TAUQE owns its color space instead of borrowing the terminal's. The interface renders through a small semantic design system, so one declarative choice restyles every view without touching component code.

### 5.10.1 Semantic Roles Instead of Raw ANSI Colors

A view that paints itself with literal ANSI names (`Color::DarkGray`, `Color::White`) inherits the terminal's *remappable* palette. Under schemes such as Solarized the host reassigns the low color slots, so a gray border can collapse into its background and an element simply disappears.

TAUQE therefore never names raw colors in its views. Every surface resolves its foreground, background, and border from a **semantic role** in `ThemePalette` — for example `border`, `surface`, `canvas`, `text_primary`, `button_active_bg`, `status_error`, or `diff_added`. A view asks for the role; the design system decides the color. Because a role never depends on the host palette, the interface stays legible and internally consistent on any terminal.

### 5.10.2 Three-Tier Adaptive Color Degradation

Presets are authored once in 24-bit sRGB. The rendering engine projects each role onto the depth the terminal actually supports, so one palette serves modern and legacy terminals without per-view branching:

- **Truecolor:** roles are emitted as exact 24-bit RGB.
- **ANSI 256:** each role is quantized to the nearest entry of the xterm 256-color cube. The projection is confined to indices `16..=255` (the fixed color cube plus the grayscale ramp); the low slots `0..=15` are deliberately never emitted, because terminals remap them to host-configured colors — precisely the Solarized failure mode this design system exists to avoid.
- **ANSI 16:** color is dropped entirely, and surfaces are conveyed by attributes instead. Filled controls and panels use a reversed band (`REVERSED`), so buttons and overlays stay distinguishable even where no color can be trusted.

The active depth is detected from `COLORTERM`/`TERM` on startup and can be forced when detection is wrong (see section 5.10.4). Degradation is monotonic: each tier steps down to a coarser rendering, never to a broken one.

### 5.10.3 Built-in Calibrated Presets

TAUQE ships five complete, hand-calibrated presets. Each defines every semantic role, so text, borders, buttons, statuses, and diffs stay legible *together*; they are not partial palettes layered over a guess.

| Preset (`name`) | Character |
| --- | --- |
| `tauqe-dark` (default) | Signature graphite-blue, a restrained Tokyo Night accent set. |
| `tauqe-light` | Clean paper canvas with muted pastels. |
| `catppuccin` | Catppuccin Mocha. |
| `gruvbox` | Warm, retro-inspired dark scheme. |
| `solarized-dark` | Solarized Dark, recalibrated in true color so borders and buttons keep reliable contrast. |

Every preset is covered by automated WCAG contrast tests in both the truecolor and the ANSI-256 projections. A palette whose text or buttons fall below the required contrast ratio fails the build, so calibration is enforced by tests rather than by eye.

### 5.10.4 Configuration and Hot Reload

Theme selection is a personal workstation preference, so it lives in the user configuration and never in the project `tauqe.toml`. In `$XDG_CONFIG_HOME/tauqe/tui.toml`:

```toml
[theme]
name = "tauqe-dark"
# mode = "auto"
# color_depth = "auto"
```

- `name` names a built-in preset and takes precedence whenever it is set.
- `mode` (`"auto"`, `"dark"`, or `"light"`) is the legacy fallback used only when `name` is unset; `"auto"` probes the terminal background.
- `color_depth` (`"auto"`, `"truecolor"`, `"ansi256"`, or `"ansi16"`) forces a color depth when detection is wrong.

Switching is seamless. Press `Alt+T` (`M-T`) to open the interactive theme selector, where moving the cursor previews each preset live; `Enter` saves the choice and `Esc` restores the previous one. Alternatively, edit the file and press `Ctrl+O` to reload. Both paths apply the new theme immediately, without restarting TAUQE and without interrupting an active turn.

---

## 5.11 Command Map and Categorized Help

Every keyboard binding is declared exactly once, and both the input dispatcher and the Help window are generated from those same declarations, so the keys documented in Help cannot drift from the keys the application actually intercepts.

### 5.11.1 Command Specifications as a Single Source of Truth

A command is an executable specification: an abstract chord, a typed action, a cognitive category, and a help description. Handling code never carries a scattered string description or a hardcoded key check; each view owns a static command map and derives both key resolution and Help rendering from it. The maps are grouped by scope:

- the global map — application-wide shortcuts reachable from any view;
- the editor map — the shared text editor's Emacs/Readline bindings;
- per-view maps — the commands of Plans, Review, Context, History, and the REPL;
- the dialog map — the universal modal-dialog vocabulary.

### 5.11.2 Semantic Key Chords

A chord names intent independently of the physical keyboard, so one specification describes the developer's actual modifiers:

- `C-` (**Primary**) and `M-` (**Meta**) are the two symmetric command modifiers, resolved from the configured `primary_modifier` and `meta_modifier` (section 5.5). No view hardcodes a physical modifier.
- A chord reachable on either command modifier stays reachable on a terminal without a dedicated Meta key.
- **Bare** keys are active only in structural navigation and are unconditionally blocked while a text editor is capturing keystrokes. This is what guarantees the unconditional character insertion rule of section 5.2.
- **Shift** combinations (`Shift+Tab`, `Shift+C`, `Shift+X`) carry reverse motion, destructive actions, and batch operations, kept separate from typed characters.
- Raw `Ctrl` and `Alt` chords are used only where the physical key must never be remapped — the `Ctrl+G` keyboard-quit and the `Ctrl+C` cancel chord.
- No function key (`F1`..`F12`) is ever bound.

### 5.11.3 Categorized Help

The Help window (`Ctrl+H` / `Alt+H`, or `?` in navigation views) derives its contents from the command maps and ranks them by cognitive weight through three tiers:

1. **Essential** — costly operations that spend tokens or touch Git: sending and cancelling turns, executing a step or batch, squashing commits.
2. **Interface** — presentation, navigation, and view state: switching views, tree navigation, folding, focus toggling.
3. **Helpers** — auxiliary operations that do not change interface state: copying to the clipboard, filtering, recentering.

Tiers keep their canonical order and any empty tier is omitted. Each command belongs to the tier that matches its daily cognitive weight, never to the order in which it was implemented.

### 5.11.4 Text Editor and Dialog Help

The shared text editor backs the REPL prompt, the squash message field, the review prompt, the file picker, and the onboarding fields. Its Emacs/Readline bindings are documented once on a dedicated Text Editor Help tab, next to the active mode's own shortcut tab, so the editor vocabulary is never duplicated into every mode's list. Modal dialogs follow the same principle: their footer hints are generated from the shared dialog command map with the configured `C-` / `M-` labels, so a footer advertises exactly the chords the dialog intercepts. That hint is rendered as the docked help panel described in section 5.4, with its chords, verbs, and separators styled through the semantic text roles.
