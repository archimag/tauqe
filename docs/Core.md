# Core

## 1. Назначение

Core — независимое от интерфейса ядро системы. Его назначение — **делать работу**.

Core ничего не знает о TUI, Emacs, WebView, terminal widgets, keybindings, mouse или конкретном способе отображения.

Server поверх core обеспечивает long-lived sessions, transports, concurrency, subscriptions/events, client connections, voice/audio ingress и model/network adapters.

Клиенты общаются с server через protocol.

## 2. Архитектурная граница

Рекомендуемая структура Rust workspace:

```text
crates/
  core/
  protocol/
  server/

  git/
  project-model/
  repo-map/
  actions/
  context/
  edits/
  models/
  voice/
  persistence/

  tui/

clients/
  emacs/
```

Точное разбиение может измениться.

Ключевой invariant:

> TUI не вызывает `core` напрямую. TUI является настоящим клиентом protocol.

Это необходимо, чтобы reference client проверял архитектурную границу с первого дня.

## 3. Domain model

Минимальные основные сущности:

```text
Repository
ProjectModel
ProjectAction
Task
Plan
Context
ContextItem
Run
Diagnostic
Evidence
ModelOperation
GitSnapshot
GitChange
PermissionSet
WorkspaceTrust
```

## 4. Repository

```text
Repository {
    id
    root_path
    current_branch
    head_commit
    status
    trust_state
}
```

Git repository является основной единицей работы.

Полноценное редактирование без Git не является нормальным режимом. Допустимы предложение инициализировать Git или read-only analysis mode.

## 5. Git subsystem

Git — не внешняя convenience integration, а часть domain model.

Core должен уметь:

- определить repository root;
- получить HEAD;
- получить branch;
- получить status;
- получить diff;
- получить changed files;
- получить commit diff;
- создать checkpoint;
- создать AI commit;
- выполнить безопасный undo;
- читать history;
- отслеживать commits, созданные системой.

### 5.1 Turn snapshot

Перед каждой AI operation:

```text
TurnSnapshot {
    head_commit
    git_status
    dirty_files
    context_revision
    timestamp
}
```

## 6. Защита dirty changes

Если AI собирается изменить файл, уже содержащий незакоммиченные изменения пользователя, эти изменения не должны смешиваться с AI change без возможности разделения.

До AI edit core должен создать checkpoint существующих изменений в target files согласно policy.

```text
HEAD
 |
 + existing user changes
 |
checkpoint commit
 |
 + AI changes
 |
AI commit
```

Нельзя автоматически коммитить несвязанные dirty files «заодно».

## 7. AI commits

После успешно применённого edit система может автоматически создать commit.

Commit должен по возможности включать только изменения данной AI operation.

```text
KnownAICommit {
    commit_hash
    parent_hash
    operation_id
}
```

Commit message может генерироваться дешёвой моделью.

## 8. Undo

Undo не должен генерироваться LLM.

Core использует Git state.

Безопасный automatic undo возможен только когда core может доказать связь current state с известной AI operation. При сложном состоянии требуется явное разрешение или ручное разрешение конфликта.

## 9. ProjectModel

Core строит детерминированную модель проекта.

```text
ProjectModel {
    repository
    packages
    workspaces
    dependency_graph
    files
    symbols
    actions
    tests
    plans
    instructions
    constraints
}
```

Она строится из Git, project manifests, task runner files, parsers, symbol index и project semantic files.

LLM не является основным механизмом построения ProjectModel.

## 10. Project file discovery & Ecosystem Profiles

Core не должен раздуваться hardcoded Rust-кодом под каждую существующую экосистему и пакетный менеджер. Вместо специфических плагинов в ядре используется **Generic Discovery Engine** и **стандартная библиотека декларативных профилей (Ecosystem Profiles / Recipes)**.

### 10.1 Принцип декларативных профилей

Профиль описывает правила обнаружения технологии и сопоставления её возможностей со стандартными семантическими действиями (`ProjectAction`):

```toml
# Пример: profiles/node-pnpm.toml
[detection]
markers = ["pnpm-lock.yaml", "pnpm-workspace.yaml"]
priority = 20

[manifest]
type = "json"
path = "package.json"
scripts_field = "scripts"

[workspace]
config = "pnpm-workspace.yaml"
filter_pattern = "pnpm --filter {package} run {action}"

[actions.mapping]
check    = ["pnpm exec tsc --noEmit", "pnpm run typecheck", "pnpm check"]
test     = ["pnpm test"]
lint     = ["pnpm run lint", "pnpm lint"]
format   = ["pnpm run format"]
build    = ["pnpm run build"]
```

### 10.2 Стандартная библиотека профилей

Система поставляется со встроенными профилями для популярных экосистем:
- **Rust:** `Cargo.toml`, `Cargo.lock` (`cargo check`, `cargo test`, `cargo clippy`, `cargo fmt`).
- **Node/TS (pnpm):** `pnpm-lock.yaml`, `pnpm-workspace.yaml`, `package.json`.
- **Node/TS (npm / yarn / bun):** соответствующие lock-файлы и раннеры.
- Другие экосистемы (Python/uv/poetry, Go и т.д.) добавляются декларативно без изменения бинарника ядра.

### 10.3 Роль Core и роль LLM

1. **Детерминированное обнаружение (Core):**
   - Движок сопоставляет маркеры в репозитории с профилями.
   - Извлекает воркспейсы, пакеты и скрипты из манифестов.
   - Регистрирует готовые, строго типизированные и валидированные `ProjectAction`.
2. **Семантический выбор (LLM):**
   - Модель получает список зарегистрированных действий и их семантических ролей.
   - На запрос пользователя («проверь типы во фронтенде») модель выбирает нужный `action_id` (например, `pnpm --filter web run typecheck`), но **не придумывает произвольную shell-команду**.

### 10.4 Проектные переопределения

Если проект использует нестандартный workflow, декларативное описание задаётся или переопределяется в:
```text
.<tool-name>/project.toml
```

> **Infer first, declare only what cannot be inferred.**

## 11. ProjectActionRegistry

Все executable capabilities проекта представлены как именованные actions.

```text
ProjectAction {
    id
    display_name
    category
    command_spec
    working_directory
    target_scope
    source
    trust
}
```

Примеры:

```text
check
test
test-unit
test-integration
lint
format
build
generate
```

## 12. Критический invariant: arbitrary shell запрещён агенту

> **У agent/model layer нет `shell(command)` capability.**

Модель не может построить произвольную команду и выполнить её.

Допустима только схема:

```text
model
  ↓
request project action
  ↓
permission check
  ↓
ProjectActionRegistry
  ↓
trusted predefined command
```

Действие должно одновременно:

1. существовать в trusted ProjectModel;
2. быть разрешено policy текущей operation.

### Не существует `AgentCommand`

Domain model может различать:

```text
ProjectAction
UserCommand
```

Но не должен существовать:

```text
AgentCommand
```

`UserCommand` — explicit команда, которую человек сознательно просит выполнить.

## 13. Никакого скрытого shell fallback

Если action отсутствует:

```text
"typecheck action not available"
```

это не означает:

```text
ask LLM to invent a command
```

Система сообщает, что project model не содержит нужной capability.

Пользователь может определить action, разрешить новый action или выполнить explicit user command.

## 14. Изменение project actions не даёт права их запускать

Если модель имеет write access к `package.json`, `Justfile` или другим executable definitions и добавляет новый script/action, этот action **не становится автоматически trusted и executable**.

Изменённые или новые executable definitions должны повторно пройти trust boundary.

Это защищает от обхода capability model через self-modification.

## 15. Workspace trust

До trust:

- project files можно индексировать;
- source можно читать;
- executable actions из repository не запускаются.

После explicit trust разрешённые project actions могут исполняться, но всё равно применяется per-operation permission model.

## 16. Runs

Каждое выполнение action создаёт persistent объект:

```text
Run {
    id
    action_id?
    command_display
    cwd
    started_at
    finished_at?
    exit_code?
    stdout
    stderr
    truncated
    diagnostics
}
```

Non-zero exit code — не internal error. Failed tests — успешный `Run`, результат которого сообщает о failing tests.

## 17. Diagnostics

Структурированные diagnostics:

```text
Diagnostic {
    tool
    severity
    path?
    range?
    message
    raw_output_ref
}
```

Parsers для известных инструментов могут извлекать diagnostics из output.

## 18. Evidence

Evidence — context item, представляющий наблюдаемый факт.

Типы:

```text
RunOutput
DiagnosticSet
GitDiff
CommitDiff
TestFailure
TypecheckFailure
LintFailure
LogFragment
UserProvidedText
```

Фраза «исправь эти ошибки» может резолвиться к последнему relevant `TypecheckFailure`.

## 19. Context

Context — first-class persistent state.

```text
ContextState {
    revision
    editable
    read_only
    evidence
    plans
    instructions
    repo_map_policy
    history_summary
}
```

Каждый item имеет provenance:

```text
USER
SYSTEM
MODEL_REQUEST
AUTO_REPO_MAP
VOICE_RESOLUTION
PLAN
```

## 20. Knowledge scope и write scope

Это разные сущности.

```text
Knowledge Scope:
    schema.sql
    auth.rs
    token.rs
    typecheck #184

Write Scope:
    auth.rs
    token.rs
```

Presence in context не означает permission to edit. Presence in repo map тем более не означает permission to edit.

## 21. Repo map

Repo map — компактная structural representation codebase.

Core строит её без LLM.

Источники:

- Git tracked files;
- Tree-sitter;
- LSP/index metadata;
- definitions;
- references;
- file relations;
- project dependency graph.

Минимальная информация:

```text
path
symbol
kind
definition location
reference relationships
```

## 22. Repo map ranking

Использовать graph ranking, PageRank-like подход или эквивалент.

Увеличивать вес:

- symbols/files, явно упомянутые пользователем;
- items в explicit context;
- references из explicit-context files;
- active plan scope;
- files, затронутые current diff;
- recent diagnostics;
- distinctive identifiers.

Уменьшать вес:

- ubiquitous/common symbols;
- generated files;
- vendor code;
- low-information nodes.

Repo map всегда ограничивается token budget.

## 23. Repo map representation

Repo map — structural hint, не authoritative full source.

```text
crates/protocol/src/voice.rs
    enum VoiceEvent
    struct Transcript
    fn normalize_transcript(...)

crates/core/src/intent.rs
    enum Intent
    fn resolve(...)
```

Для модификации source модель должна получить authoritative editable source.

## 24. Project vocabulary

Core поддерживает vocabulary:

```text
paths
filenames
directories
symbols
package names
workspace names
actions
test suites
branches
recent commits
plans
```

Он используется voice/text resolver.

## 25. Voice pipeline

Infrastructure может делать:

```text
microphone
→ audio
→ VAD
→ STT
```

Core делает:

```text
transcript
→ project-aware normalization
→ reference resolution
→ discourse resolution
→ intent classification
```

## 26. Intent model

Минимальный набор:

```text
AskModel
RequestEdit

AddContext
RemoveContext
SetReadOnly
SetEditable
ClearContext

RunProjectAction
RunExplicitUserCommand

ShowDiff
ShowStatus
ShowContext
ShowHistory

Commit
Undo

ActivatePlan
ShowPlan

OpenInIDE
StopOperation
```

Если intent deterministic, coding model не вызывается.

## 27. Intent resolution priority

Порядок:

1. exact deterministic parse;
2. project vocabulary match;
3. active client object/focus;
4. recent discourse references;
5. small resolver model;
6. ambiguity request.

LLM resolver не имеет права invent entities.

## 28. Tasks

Task — текущий logical work context.

```text
Task {
    id
    title?
    repository_id
    start_commit
    active_plan?
    context_revision
    runs
    model_operations
}
```

Task не обязан соответствовать branch.

## 29. Plans

Core должен уметь:

- enumerate plans;
- parse metadata;
- activate plan;
- expose scope;
- expose gates;
- expose done criteria;
- include plan in context.

Plan metadata не должна автоматически давать write permission; она задаёт semantic scope.

## 30. Project instructions

Project instructions — отдельная domain сущность.

Они могут ссылаться на Markdown files.

Позже возможны scoped instructions:

```text
scope = "crates/protocol/**"
instruction = "Protocol types must remain transport-neutral."
```

## 31. ModelGateway

Core использует provider-neutral interface:

```text
complete(request)
stream(request)
```

Model provider adapters живут отдельно. Core не должен быть связан с конкретным API vendor.

## 32. Model roles

Возможные роли:

```text
CodingModel
ResolverModel
CommitModel
SummaryModel
ArchitectModel?
EditorModel?
```

Не обязательно использовать все роли в MVP.

## 33. Prompt assembly

Prompt собирается слоями:

```text
1. system behavior contract
2. project/platform metadata
3. project instructions
4. active plan
5. summarized history
6. repo map
7. read-only context
8. editable files
9. evidence
10. current request
11. output contract reminder
```

Стабильные части идут раньше для возможного provider caching.

## 34. Authoritative source rule

Текущий file content, переданный core в request, является единственным authoritative source для модели.

Старые fragments из conversation history не считаются актуальным source state.

## 35. Coding system contract

Функциональная семантика:

```text
- Modify only explicitly editable files.
- Use read-only files and repo map only for understanding.
- Do not invent missing file contents.
- Request additional context instead of guessing.
- Follow the task narrowly.
- Prefer the smallest coherent change.
- Do not perform unrelated cleanup.
- Do not claim commands passed unless actual Run results were supplied.
- Use only the structured edit protocol.
- Do not assume shell access exists.
```

## 36. Structured model result

```text
ModelResult =
    Answer
    | ContextRequest
    | EditProposal
```

### Answer

```json
{
  "kind": "answer",
  "text": "..."
}
```

### ContextRequest

```json
{
  "kind": "context_request",
  "requests": [
    {
      "type": "file",
      "path": "crates/core/src/task.rs",
      "preferred_access": "read_only",
      "reason": "Need to inspect Task lifecycle"
    }
  ]
}
```

### EditProposal

```json
{
  "kind": "edit",
  "summary": "Preserve event order during transcript finalization",
  "edits": []
}
```

## 37. Edit operations

Минимальный набор:

```text
Replace
Create
Delete
Rename
```

### Replace

```text
path
old_text
new_text
base_hash?
```

`old_text` должен совпасть ровно один раз.

- 0 matches → stale/malformed;
- >1 matches → ambiguous.

Это выполняет роль optimistic concurrency check.

## 38. Edit authorization

Каждый target path должен:

- находиться внутри repository;
- пройти path normalization;
- не выходить наружу через symlink;
- иметь write permission;
- соответствовать current editable scope.

Новые files требуют отдельного разрешённого operation type.

## 39. Atomic edit application

Все edits сначала валидируются. Только если валидны все, применяется transaction.

Нельзя оставлять partially applied proposal.

## 40. Stale source

Если source изменился после model request:

- hash/search validation fails;
- proposal не применяется;
- core может refresh context и ограниченно retry;
- force apply запрещён.

## 41. Reflection retries

Edit formatting/apply failure может быть отправлена модели для исправления.

Retries строго ограничены, например:

```text
max_automatic_repair_attempts = 2
```

Никаких бесконечных autonomous loops.

## 42. Validation gates

После edit можно запускать project actions согласно policy или explicit user request:

```text
check
test
lint
```

Неудачный gate становится Evidence.

Система не обязана автоматически чинить failures.

## 43. Conversation history

Conversation history хранит goals, architectural decisions, rejected approaches, unresolved issues и important constraints.

Она не должна быть долговременным storage старых source code copies.

Git хранит change history.

## 44. History summarization

При превышении budget старая история суммаризируется дешёвой моделью.

Summary не должна содержать устаревший source как authoritative code.

## 45. Cost accounting

Каждая model operation хранит:

```text
model
input tokens
output tokens
cached tokens
estimated cost
context revision
result
edited files
commit
```

Полезные метрики:

```text
tokens per accepted change
model calls per accepted change
reverted operations
failed edit applications
validation success rate
```

## 46. Secret filtering

Core не добавляет автоматически в model context `.env`, keys, credentials, known token stores и secret files.

Secret filtering конфигурируется.

## 47. Ignore rules

Учитываются `.gitignore`, generated directories, vendor dependencies, build output и application-specific ignore.

Git tracked state имеет высокий приоритет для repository membership.

## 48. Binary files

Binary files не включаются как text context.

Vision/binary understanding не входит в начальный core.

## 49. Branch and remote operations

Модель не получает capability:

```text
checkout
merge
rebase
reset
push
force-push
```

Такие операции могут появиться позже как explicit user actions.

## 50. External editor / IDE

Core/protocol поддерживает semantic requests:

```text
OpenLocation
OpenSymbol
OpenDiff
```

Конкретный client решает, как их реализовать.

## 51. Core state machine

```text
IDLE
 ↓
USER_INPUT
 ↓
RESOLVE_INTENT
 ├── deterministic → EXECUTE_ACTION → UPDATE_STATE → IDLE
 │
 └── model task
       ↓
    BUILD_CONTEXT
       ↓
    CALL_MODEL
       ↓
    VALIDATE_RESULT
       ├── Answer → STORE/DISPLAY
       ├── ContextRequest → UPDATE CONTEXT
       └── EditProposal
              ↓
          VALIDATE EDITS
              ↓
          CHECKPOINT DIRTY INPUT
              ↓
          APPLY ATOMICALLY
              ↓
          OPTIONAL GATES
              ↓
          COMMIT
              ↓
          UPDATE PROJECT MODEL
              ↓
             IDLE
```

## 52. MVP Core

Минимальный self-hosting core:

- Git open/status/diff/history;
- Cargo project model;
- explicit context;
- read-only/editable separation;
- repo map;
- plan loading;
- model call;
- structured search/replace edits;
- dirty-file checkpoint;
- AI commit;
- undo;
- action registry;
- `check`, `test`, `clippy`, `fmt`;
- Run/Evidence;
- intent resolution;
- voice transcript input;
- operation history.

## 53. Golden rules

Повторяются намеренно.

> **Git defines reality.**

> **Project files define what the project can do.**

> **Context defines what the model knows.**

> **Permissions define what the model may change or execute.**

> **The agent has no arbitrary shell.**

> **Never ask the model a question the environment can answer exactly.**

> **The application owns state.**

> **The model performs semantic work inside boundaries created and enforced by the environment.**
