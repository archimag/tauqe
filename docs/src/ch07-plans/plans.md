# 7. Local Plans

While high-level roadmaps and project backlogs are best committed to repository documentation, engineering work frequently involves complex, multi-step tactical tasks (such as protocol additions, database migrations, or subsystem refactorings).

TAUQE provides a dedicated **Local Plans** subsystem designed around structured task execution and the "What" vs "How" duality.

---

## 7.1 Architecture & Storage

- **Isolated & Persistent:** Tactical plans are stored locally in `.tauqe/plans/<id>.json`. Because `.tauqe/` is ignored by Git, plans never pollute repository commits or branch history.
- **Immune to History Compaction:** Unlike plain conversational chat where instructions fade after compaction rounds, local plans survive session restarts and compaction cycles intact.
- **Typo-Tolerant Identifier Matching:** Plan identifiers (slugs like `jwt-auth` or `tui-refactor`) are resolved using Levenshtein distance and prefix matching. Minor typos made by the developer or model resolve seamlessly to the intended plan.

---

## 7.2 Model Interaction Protocol

The model interacts with plans deterministically via structured tags:

1. **Creating or replacing a plan:**
   ```xml
   <plan action="save" id="jwt-auth" title="JWT Authentication Refactor">
     <summary>Migrate session cookies to rotating JWT tokens</summary>
     <item id="1" status="done" title="Data models">Add JwtClaims and RefreshPayload structs</item>
     <item id="2" status="in_progress" title="Refresh endpoint">
       <item id="2.1" status="done" title="Token storage" />
       <item id="2.2" status="todo" title="Handler implementation" />
     </item>
     <item id="3" status="todo" title="Unit and integration tests" />
   </plan>
   ```

2. **Updating item statuses during work:**
   ```xml
   <plan action="update" id="jwt-auth">
     <item id="2.2" status="done" />
     <item id="3" status="in_progress" />
   </plan>
   ```

All `<plan>` tags are automatically filtered out during streaming to prevent visual noise in the terminal UI, and are stripped from history entries.

---

## 7.3 Workflow: Focusing on "What"

1. **Formulate the Plan:**
   In the Develop view (`Ctrl+1`), ask TAUQE to design an implementation plan:
   > *"Draft an execution plan for adding rate-limiting middleware."*
   The model responds with an architectural overview and a structured `<plan>` tag. The plan instantly appears in the Plans view.

2. **Select Focus Tasks:**
   Switch to the Plans view (`Ctrl+4`). Use `j` / `k` to navigate and press `x` to check specific tasks:
   - `[x] 2.2 Handler implementation`
   - `[x] 2.3 Middleware wiring`

3. **Context Injection:**
   Checked items are automatically formatted and injected into the model's prompt as `<active_plan_context>`.

4. **Execute in Develop:**
   Return to Develop (`Ctrl+1`) and command the harness:
   > *"Implement the checked tasks."*
   The model focuses exclusively on the selected steps, stages changes, passes verification, and updates item statuses upon completion.

---

## 7.4 Plans View Keybindings

| Keybinding | Action |
|---|---|
| `n` / `p`, `j` / `k`, or `↑` / `↓` | Move selection through the plan hierarchy |
| `x` | Toggle checkbox (mark item for injection into prompt context) |
| `t` / `s` / `d` | Open status selection dialog (`1` Todo, `2` InProgress, `3` Done, `4` Cancelled) |
| `Space` / `Enter` | Fold or unfold child items |
| `Tab` / `Shift+Tab` | Switch between active plans |
| `c` / `y` (or `Alt+C`) | Copy entire plan to clipboard in Markdown format |
| `Ctrl+H` / `?` | Open contextual Help dialog |
| `Ctrl+1..5` | Quick switch between views |
