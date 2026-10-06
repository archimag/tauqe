# Core

## 1. Назначение

Core — независимое от интерфейса ядро системы. Его назначение — **делать работу**.

Core ничего не знает о TUI, Emacs, WebView, terminal widgets, keybindings, mouse или конкретном способе отображения.

Архитектурным ориентиром разделения обязанностей выступает **SLIME (Swank)**: ядро и сервер живут как независимый долгоживущий рантайм, хранящий сессионное состояние проекта, а клиенты подключаются к нему по семантическому протоколу.

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

Ключевой invariant:

> TUI не вызывает `core` напрямую. TUI является настоящим клиентом protocol.

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

Git repository является основной единицей работы. Полноценное редактирование без Git не является нормальным режимом.

## 5. Git subsystem & Checkpoints

Git — часть domain model. Core умеет:
- определять repository root, HEAD, branch, status, diff;
- создавать turn snapshot и checkpoints существующих незакоммиченных изменений;
- создавать изолированные AI commits;
- выполнять безопасный детерминированный undo.

### 5.1 Защита dirty changes через Checkpoints
Перед применением AI edit core создает чекпоинт (`create_checkpoint`) существующих изменений в целевых файлах:
- Если в рабочем дереве есть незакоммиченные изменения разработчика, они временно коммитятся в изолированный коммит `workbench-checkpoint: uncommitted user changes`.
- После успешного применения правок создается целевой коммит `Workbench AI <ai@workbench.dev>` с кратким саммари.
- Таким образом, пользовательские и модельные правки никогда не слипаются в один неразделимый diff.

### 5.2 Детерминированный Undo
Undo не генерируется LLM, а выполняется детерминированно через Git state:
- Проверяется, что коммит в `HEAD` действительно создан `Workbench AI`. Если коммит пользовательский — операция безопасно отклоняется.
- Выполняется `git reset --hard HEAD~1`.
- Если перед AI-коммитом был создан чекпоинт пользователя, выполняется мягкий откат чекпоинта (`git reset HEAD~1`), возвращающий незакоммиченные файлы разработчика в рабочее дерево без потерь.

## 6. Workflows (Рабочие процессы редактирования)

Ядро поддерживает подключаемые стратегии выполнения задач (`EditWorkflow`):

1. **`toolchain` (по умолчанию):**
   - Создает чекпоинт пользовательских изменений.
   - Атомарно применяет правки модели к файлам.
   - Запускает детерминированную проверку сборщика (`cargo check`, `npm test` и т.д.).
   - **Auto-healing feedback loop:** в случае ошибок компиляции ошибки перехватываются и передаются обратно модели в диалог для автоматического исправления.
   - При успехе проверки фиксирует изолированный AI-коммит.
   - При исчерпании попыток исправлений — безопасно откатывает рабочее дерево к исходному чекпоинту, гарантируя, что код проекта не останется в сломанном состоянии.
2. **`git`:**
   - Чекпоинт незакоммиченных изменений разработчика → атомарное применение правок → изолированный AI-коммит без вызова тулчейна.
3. **`naive`:**
   - Простое атомарное применение правок к файловой системе без создания Git-коммитов (для работы вне Git-репозиториев или тестовых сред).

Смена workflow блокируется во время активной генерации модели.

## 7. Протоколы взаимодействия и модификации кода (Interaction & Edit Protocols)

Ядро абстрагирует формат взаимодействия с моделью через фабрику `EditProtocol`:

1. **`xml` (по умолчанию):**
   - Строгий структурированный псевдо-XML протокол тегов `<workbench_edits>`, `<edit>`, `<create>`, `<delete>`, `<search>`, `<replace>`.
   - Потоковая фильтрация и валидация единственности сопоставления (`Search/Replace`) на сервере в реальном времени.
   - Естественно поддерживает свободный текст и рассуждения модели (reasoning) до и между тегами.
   - Создание и полная замена файлов покрываются тегом `<create path="...">`.
2. **`structured`:**
   - Строгий JSON Schema (Structured Output), задающий контракт `ModelResultProposal` (`message`, `changes`, `context_requests`, `suggested_actions`).
   - Потоковый парсинг через конечный автомат `struson` без ожидания полного завершения ответа.

*Отказ от Tool Calling для кода:* нативные tool calls (Function Calling) исключены из протоколов модификации кода из-за нестабильности стриминга аргументов у провайдеров и их конфликта с режимами рассуждений (thinking/reasoning).

Смена протокола блокируется во время активной генерации модели.

## 8. ProjectModel & Ecosystem Profiles

Core строит детерминированную модель проекта без использования LLM на базе декларативных профилей экосистем (`Cargo.toml`, `pnpm-workspace.yaml`, `package.json` и др.).

## 9. ProjectActionRegistry & Invariant: arbitrary shell запрещён

> **У agent/model layer нет `shell(command)` capability.**

Модель не может выполнять произвольные команды оболочки. Она может лишь запрашивать запуск строго типизированных именованных `ProjectAction`, объявленных в trusted project model.

## 10. Runs & Diagnostics

Каждое выполнение action порождает персистентный объект `Run`. Неуспешные проверки превращаются в структурированные `Evidence` и `Diagnostic`, которые можно передать в контекст модели по команде («исправь эти ошибки»).

## 11. Context: Knowledge Scope vs Write Scope

Контекст — first-class state.
- **Knowledge Scope:** файлы и evidence, доступные модели для чтения (`read_only` или `editable`).
- **Write Scope:** строго файлы с правами `editable`. Модель не имеет права предлагать изменения для `read_only` файлов.
- **Batch Pattern Add:** поддержка пакетного добавления файлов по glob-маскам и префиксам директорий (например, `src/*.rs`, `crates/core/`).

## 12. Structured Model Result & XML Protocol

Для взаимодействия с моделью используется строгий структурированный XML-протокол редактирования. Современные LLM нативно обучены XML-структурам, что исключает хрупкие эвристики и коллизии с git-конфликтами.

### Формат разметки

```xml
<workbench_edits summary="Concise commit message in imperative mood">
  <!-- Модификация существующего файла: -->
  <edit path="crates/core/src/task.rs">
    <search>
fn old_implementation() {
    do_something();
}
    </search>
    <replace>
fn new_implementation() {
    do_something_better();
}
    </replace>
  </edit>

  <!-- Создание нового файла или полная перезапись: -->
  <create path="crates/core/src/new_mod.rs">
pub fn helper() -> bool { true }
  </create>

  <!-- Удаление файла: -->
  <delete path="crates/core/src/obsolete.rs" />
</workbench_edits>
```

### Инварианты Search/Replace
1. Атрибут `path` обязан точно указывать на файл из `<editable_files>`.
2. Блок `<search>` должен сопоставляться **ровно один раз** в целевом файле с учетом отступов и переводов строк.
   - 0 совпадений → `NoMatch` (код устарел или ошибочен).
   - >1 совпадений → `AmbiguousMatch` (неоднозначность).
3. Поиск и замена выполняются компактными чанками без переписывания всего файла.

## 13. Потоковая фильтрация и двухфазная валидация

Сервер реализует потоковый фильтр (`XmlStreamFilter`):
1. **Фаза 1 (Стриминг и валидация в памяти):**
   - Текст рассуждений и комментариев передается клиенту в реальном времени (`model/textDelta`).
   - Блоки XML перехватываются: сырые теги не попадают в текстовый поток клиента.
   - По мере закрытия каждого тега `<edit>` / `<create>` / `<delete>` сервер отправляет семантические события (`edit/fileStarted`, `edit/hunk`, `edit/fileDone`).
   - Чанки валидируются в памяти на копии файла: если `search` не найден, клиент немедленно получает статус ошибки валидации файла.
2. **Фаза 2 (Атомарная транзакция на диск):**
   - Только если **все** правки всех файлов успешно прошли валидацию и модель завершила ответ, изменения атомарно записываются на диск (`apply_edit_proposal`).
   - Создаются или обновляются записи в `ContextManager`.
   - Если хотя бы один файл содержит ошибку — диск остается нетронутым, частично примененных правок не возникает.

## 14. Golden rules

> **Git defines reality.**

> **Project files define what the project can do.**

> **Context defines what the model knows.**

> **Permissions define what the model may change or execute.**

> **The agent has no arbitrary shell.**

> **Never ask the model a question the environment can answer exactly.**

> **The application owns state.**

> **The model performs semantic work inside boundaries created and enforced by the environment.**
