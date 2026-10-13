# 5. Keyboard-First Interface

TAUQE's terminal UI combines the design philosophies of **Magit** (structured, live document state) and **Org-mode** (collapsible outline hierarchy).

---

## 5.1 Primary Views and Universal Navigation

To guarantee universal compatibility across modern and legacy terminal emulators alike, all primary view modes and key actions support dual bindings (both modifier chords and standard function keys):

Throughout this chapter, `Ctrl+<key>` denotes the abstract **Primary Command Modifier** (`C-<key>`). It defaults to `Ctrl` and can be remapped to `Alt` in `tui.toml` (see section 5.5). Function keys (`F1`..`F6`) and arrow keys are universal fallbacks that stay active on every terminal.

- **`F1` / `Alt+1` / `Ctrl+1` (Develop View):**
  - The central engineering cockpit and execution canvas where coding turns take place.
  - Displays turn progression as a structured, live document: collapsible reasoning blocks (`Ctrl+R`), streaming markdown explanations, and interactive proposed file diffs.
  - **Dual-Focus Architecture:** Switch focus seamlessly between the prompt editor and the response/diffs viewport using `Ctrl+Space` or `Shift+Tab`.
  - Multi-line intent editor with auto-growing height and Emacs navigation (`Ctrl+A`, `Ctrl+E`, `Alt+B`, `Alt+F`, `Ctrl+K`, `Ctrl+Y`, `Ctrl+W` / `Alt+Backspace` to kill word backward, `Tab` for 2-space indentation).
  - Persistent prompt history: Use `Ctrl+P` / `Ctrl+N` (or `Alt+P` / `Alt+N`) to cycle through previous prompt inputs. History is saved to `.tauqe/prompts.history` and persists across sessions.
  - Newline insertion: `Shift+Enter`, `Alt+Enter`, or `Ctrl+J`.
  - Response clipboard: Press `Alt+C` or `Alt+Y` while idle to copy the model's markdown response to the system clipboard.
  - Undo AI commit: Press `Ctrl+Z` or `Alt+U` to trigger safe rollback. If no AI commit exists, TAUQE displays a status notice without opening unnecessary dialogs.
  - Active LLM selection: Press `Alt+M` or `Ctrl+M` from any screen (or click the model in the header) to open the model picker. See section 5.7 for the role model.
  - Contextual help: Press `Ctrl+H` / `Alt+H` from any view or `?` in navigation views to open the interactive help dialog.
  - Configuration reload: Press `Ctrl+O` to reload `tui.toml` settings without restarting TAUQE.

### What Happens in Develop When You Submit a Prompt
When you submit an engineering intent, TAUQE does not simply invoke a single-shot completion. Instead, it guides the model through an **autonomous, multi-round bounded turn**:
1. **Exploration & Discovery Rounds:** The model analyzes the task against the Tree-sitter symbol map. If critical type definitions, helper modules, or project documentation are missing from context, it autonomously requests them via protocol tags (`<context_request>`, `<doc_request>`). The footer reflects the active discovery round (`Round 1`, `Round 2`...). Control does not bother the user with context logistics; the model gathers facts until the architectural picture is crystal clear.
2. **Code Generation & Virtual Staging:** Once equipped with the necessary context, the model drafts minimal, coherent changes (search/replace blocks, new files, renames). These edits are applied to an **in-memory virtual staging tree** first, ensuring zero disk corruption from speculative failures.
3. **Patch Retry Loop (Staging Resilience):** LLMs occasionally emit code hunks with slight indentation shifts, mismatched whitespace, or stale surrounding context lines that cannot be cleanly matched against disk. Instead of aborting the turn or dumping broken patches, TAUQE preserves all successfully staged files, pinpoints the failing files, and prompts the model with targeted patch error feedback to regenerate only the problematic hunks (up to `max_retries`).
4. **Deterministic Toolchain Verification & Self-Healing:** After in-memory staging converges and changes are written to disk, TAUQE automatically triggers deterministic project toolchains (`cargo check`, `npm test`, linters). If compile or lint errors are detected, the raw compiler diagnostics are packaged and fed back to the model for an immediate **verification healing cycle**, correcting syntax or type errors before the developer even inspects the code.
5. **Git Transaction & Rollback:** The entire successful sequence is recorded as an isolated Git commit. If the outcome deviates from the developer's intent, pressing `Ctrl+Z` or `Alt+U` opens a confirmation dialog to immediately and atomically restore the repository to its exact pre-turn state.

### Live Reasoning Display During Streaming
Reasoning output during an active turn is a progress indicator, not a document that must be read while it grows. TAUQE uses this distinction to keep the interface responsive even when the reasoning stream is long and contains draft code:

- **Sliding window (Reasoning Tail Stream):** While reasoning is actively streaming, the Develop view renders only a compact slice of the most recent lines (ten lines by default) below the `[REASONING]` badge. The badge shows the running total line count (`Thinking… (N lines)`), so forward progress stays visible without re-wrapping the entire, ever-growing transcript on every update.
- **Deferred deep rendering:** The tail uses lightweight per-line styling. The full Markdown parse and `syntect` code-block highlighting are not restarted against the still-open stream; they run exactly once, when the reasoning stage finalizes.
- **Finalized transcript:** When reasoning completes, the sliding window is replaced by the complete reasoning transcript rendered through the standard Markdown pipeline, including syntax-highlighted code fences. The `Ctrl+R` fold/unfold toggle applies to both the live tail and the finalized transcript.

- **`F2` / `Alt+2` / `Ctrl+2` (Context View):**
  - Displays Pinned, User, and Auto context layers with token metrics.
  - Quick keys: `e` (add file as editable), `r` / `a` (add file as read-only), `t` (toggle access Editable ↔ Read-Only), `u` (promote an Auto file to persistent User), `c` / `C` (clear all auto-requested files), `d` / `x` / `Del` (remove a file from context).
  - Navigation and folding: `n` / `p` (or `j` / `k`, `↑` / `↓`) move the selection; `Space` / `Tab` or `Enter` fold or unfold a section; `Enter` also promotes an Auto file to User. `Esc` / `q` return to the Develop view.
- **`F3` / `Alt+3` / `Ctrl+3` (Review View):**
  - Dedicated code review workspace for static architectural and security audits.
  - Unified navigation: `n` / `p` (or `↑` / `↓`), with `Ctrl+N` / `Ctrl+P` for accordion inspection of the next / previous finding.
  - Interactive findings checklist: fold/unfold items (`Tab` / `Space`), execute the selected finding or every Todo finding in the session (`x`, with confirmation), and discuss the finding or the whole session in Develop (`d`).
  - Intentional status assignment: Press `t` or `s` to open the status selection dialog (`1` Discussion, `2` Todo, `3` InProgress, `4` Fixed, `5` Rejected).
  - Filtering and bulk folding: `f` toggles between all findings and hide-closed (FIXED / REJECTED); `a` folds or unfolds every session and finding.
  - Run and refresh: `r` starts a new review of the current context; `Shift+R` refreshes the session list from the server.
  - Clipboard and lifecycle: `c` / `y` copy the selected finding or session; `Delete` deletes a session (with confirmation); `Ctrl+R` folds or unfolds the thinking stream.
- **`F4` / `Alt+4` / `Ctrl+4` (Plans View):**
  - Workspace for managing tactical task hierarchies and execution roadmaps, split into an immutable Concept section (system invariants) and an Implementation section (executable steps).
  - Unified navigation: `n` / `p` (or `↑` / `↓`), with `Ctrl+N` / `Ctrl+P` for accordion inspection of the next / previous item.
  - Interactive tree of tasks with explicit statuses: Discussion, Todo, InProgress, Done, Blocked, and Cancelled.
  - Execution: `x` executes the target step, group, or entire plan (with confirmation); `Enter` executes a leaf step and folds/unfolds on groups and plan headers.
  - Discussion and refinement: `d` discusses the selected concept/step or the whole plan in Develop; `R` deep-refines plan architecture and steps with the Architect.
  - Intentional status assignment: Press `t` or `s` to open the status dialog (`1` Discussion, `2` Todo, `3` InProgress, `4` Done, `5` Blocked, `6` Cancelled).
  - Bulk folding (`a`), plan refresh (`r`), deletion (`Delete`, with confirmation), and Markdown copy (`c` / `y`).
- **`F5` / `Alt+5` / `Ctrl+5` (History View):**
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
- **Emacs navigation:** `Ctrl+A` / `Ctrl+E` for start/end of line, `Alt+B` / `Alt+F` for words backward/forward, `Ctrl+K` to kill to end of line, `Ctrl+Y` to yank from kill-ring, `Ctrl+W` / `Alt+Backspace` to kill word backward.
- **Persistent prompt history:** `Ctrl+P` and `Ctrl+N` cycle backward and forward through previous prompt inputs.
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

- **Commands require modifiers:** Destructive or state-changing actions use explicit modifiers (`Ctrl+Z` / `Alt+U` for Undo, `F6` / `Ctrl+S` for Squash, `Alt+C` for copying responses).
- **Draft protection on exit:** Pressing `Ctrl+Q` prompts for confirmation whenever a prompt draft is non-empty, a model turn is active, or an unapplied squash diff exists, preventing accidental loss of uncommitted work.
- **Onboarding isolation:** During initial configuration (`ViewMode::Onboarding`), view switching keys (`F1..F5`, `Ctrl+1..5`, `Alt+1..5`) and header mouse clicks are locked until setup is completed, preventing accidental bypass of credential verification.
- **Contextual help:** Press `Ctrl+H` / `Alt+H` from any view or `?` in navigation views to open the contextual help dialog.

---

## 5.4 Modal Dialog Architecture & Interaction Standard

Modal dialogs follow a unified, three-tier layout standard designed for keyboard reliability, safety, and layout neutrality:
- **Structure:** Every dialog consists of a distinct **Header** (title and context), a central **Body** (content, form, or options), and an **Actions / Keybindings Footer**.
- **Destructive Action Safety:** Confirmation dialogs (Quit with draft, Undo AI commit, Git squash, Cancel generation, Delete plan, Clear history) default focus securely to `[ Cancel ]`.
- **Button Navigation:** Switch focus between buttons using `Tab`, `Shift+Tab`, or arrow keys (`←`, `→`). Press `Enter` or `Space` to activate the focused button.
- **Fast Accelerators:** In confirmation dialogs, pressing `y` / `Y` confirms immediately, while `n` / `N`, `q`, or `Esc` cancels.
- **Intentional Status Dialogs:** In Plans and Review, task and finding statuses are selected through explicit modal dialogs rather than accidental cycling. Plans offer Discussion, Todo, InProgress, Done, Blocked, and Cancelled (`1`..`6`); Review offers Discussion, Todo, InProgress, Fixed, and Rejected (`1`..`5`). Choose a target status using direct numeric keys, or navigate with `n` / `p` and confirm with `Enter` / `Space`.
- **Git Squash Safety:** Within the squash dialog (`F6` / `Ctrl+S`), `Enter` or `Space` folds/unfolds changed files in the file list, and `Enter` in the message editor inserts a newline. Triggering the squash requires `Ctrl+Enter` or `Ctrl+S`, which opens an explicit confirmation dialog summarizing affected commits.

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
# This only changes the modifier used for input-level actions. Alt-based
# Readline word navigation (Alt+B, Alt+F) and universal function keys
# remain available regardless of the chosen modifier.
primary_modifier = "ctrl"

# Optional universal character mapping (langmap) for any non-Latin layout:
# Supports two formats:
# 1. Parallel string mapping: "from_chars;to_chars" (e.g. Vim langmap style)
# 2. Pairs list: "йq,цw,уe" or "й:q,ц:w"
# langmap = "йцукенгшщзхъ;qwertyuiop[]"

[theme]
# Color theme mode: "dark", "light", or "auto" (default: "auto").
# When "auto" is set, TAUQE checks COLORFGBG or terminal environment hints.
# Automatically detects 24-bit truecolor support and quantizes to ANSI-256 where required.
mode = "auto"

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

---

## 5.6 Long-Running Turn Notifications

Engineering tasks (discovery rounds, toolchain verifications, test suites) can take dozens of seconds. TAUQE ensures developers never miss the completion of their work when switching to another window or workspace:

- **Urgency Alert & Terminal Bell:** Emits a standard ASCII `BEL` (`\x07`), prompting modern terminal emulators (Kitty, WezTerm, iTerm2) to raise window urgency flags in your desktop taskbar or window manager.
- **Zero-Dependency Desktop Notifications:** Utilizes standard terminal OSC protocols (`OSC 777` and `OSC 9`) to display native desktop banners with turn summaries or verification statuses. Clicking the notification immediately focuses the terminal window.
- **Smart Duration Threshold:** Notifications fire only if execution equals or exceeds `min_duration_seconds` (default: 5 seconds), eliminating noise during rapid iterative dialogs.

### 5.6.1 Turn versus Batch Signal Granularity

The natural unit of waiting differs between an interactive Develop turn and a batch supervisor:

- **Single interactive turn (Develop):** One prompt maps to one turn, so the notification fires as soon as that turn finishes. Nothing is aggregated away.
- **Batch supervisors (Plan Execution, Review Execution):** One launch runs many steps, and the developer waits for the *whole batch*, not for each individual step. To avoid per-step noise, intermediate batch steps never raise desktop banners or sound alerts.

### 5.6.2 Aggregated Batch Notifications

Batch supervisors emit a typed completion event (`plan/batch_finished`, `review/batch_finished`) once the internal `BatchExecution` loop ends. This event carries the final status (`Completed`, `Failed`, `Blocked`, `Cancelled`), the executed and scheduled step counts, and the elapsed time. The TUI marks batch turns, suppresses the usual per-turn signal on each step's `MODEL_FINISHED`, and fires exactly one aggregated notification from this final event:

- **Normal completion (`Completed`):** A single notification reports the finished status together with the step tally, so the developer learns the outcome of the entire operation at a glance instead of reconstructing it from dozens of individual signals.
- **Failure or block stop (`Failed` / `Blocked`):** When the batch halts early on a failed or blocked step, the aggregated notification reports the halted status rather than a plain success. The offending step and any blocker reason remain inspectable in the Develop view; the notification only signals that the batch did not run to completion.
- **Cancellation (`Cancelled`):** A cancelled batch reports the `Cancelled` status through the same single notification.

The aggregated notification is gated by the same `tui.toml` settings as single-turn signals (`sound`, `desktop`, `desktop_protocol`, `only_unfocused`, and `min_duration_seconds`), so batching changes only *when* the signal fires — never *how* it is delivered.

---

## 5.7 Model Roles and Model Selection

Tasks differ in their nature: interactive dialogue, conceptual planning, concrete code generation, analytical review, and routine history compaction each demand a different competence. Instead of making the developer remember model identifiers, TAUQE assigns a model to each **semantic role** in the `[models]` section of `tauqe.toml`:

```toml
[models]
scribe = "~anthropic/claude-haiku-latest"      # history compaction, commit messages
coder = "~deepseek/deepseek-flash-latest"      # concrete code generation
architect = "~google/gemini-flash-latest"      # conceptual design, plan refinement
advisor = "~google/gemini-flash-latest"        # interactive engineering dialogue
reviewer = "~anthropic/claude-sonnet-latest"   # automated code review
auto_level_up = true                           # escalate the coder ladder on failure

# Optional per-role reasoning depth: "low", "medium", "high", or "xhigh".
[models.effort]
architect = "high"
```

- **Selection model:** The active choice is either a role (`Scribe`, `Coder`, `Architect`, `Advisor`, `Reviewer`) or one specific model. A role is resolved to a concrete model by the server, so the TUI never guesses the mapping. When a role is omitted, it is inferred through a deterministic proximity fallback chain instead of failing configuration load.
- **Coder ladder:** The `coder` value is polymorphic. A bare model string or a `{ model, effort }` table yields a one-rung ladder; an ordered array expresses an explicit escalation ladder, where each rung pairs a model with an optional reasoning depth:
  ```toml
  coder = [
    "~deepseek/deepseek-flash-latest",
    { model = "~deepseek/deepseek-flash-latest", effort = "medium" },
    "~anthropic/claude-sonnet-latest",
  ]
  ```
- **Picker (`Alt+M` / `Ctrl+M`, or a click on the model in the header):** The first screen lists the semantic roles with their resolved models. Selecting `Others...` opens the full catalog to pin one specific model. `Esc` closes the picker.
- **Header:** A semantic role is rendered as its title only, for example `Coder`. The concrete model name is deliberately omitted to keep the header free of provider-level noise; the resolved role-to-model mapping stays visible in the picker. A pinned specific model is shown by its own name instead (truncated on narrow terminals).
- **Background work:** Squash commit messages and history compaction always use the Scribe role. They are routine tasks, so they should not spend the budget of a stronger model.
- **Level Up:** When `auto_level_up = true`, a plan step or review fix that still fails after verification healing is rolled back to the Git checkpoint taken before the step. Execution then advances one rung along the **coder ladder** within the same Coder role — it never switches to the Architect role. Deepening reasoning on the same model and promoting to a stronger coder tier are both intra-role moves, so the strict What / How separation is preserved. Files found during discovery stay in context, so the stronger coder tier does not search again. When the ladder is exhausted, or Level Up is off, the normal Fail-Fast stop applies. Develop never escalates automatically: the developer's explicit choice in the picker is always respected.

---

## 5.8 Plan Stepper in Develop (Batch Step Progress)

When a Develop turn belongs to a plan execution (a single step or a server-orchestrated batch), the Develop view renders a compact **Plan Stepper** above the model output. Its role is to keep the developer oriented during a multi-step scope without leaving Develop and without switching to the Plans view (`F4`).

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
