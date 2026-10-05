# Protocol

## 1. Назначение

Protocol определяет публичную границу между server и clients.

Он должен позволять независимо реализовывать:

- TUI;
- Emacs;
- future editor clients;
- remote clients;
- automation clients.

Protocol не должен содержать TUI/Emacs-specific concepts. Он описывает domain operations и events.

## 2. Архитектурный принцип: модель SLIME (Swank)

```text
Client
  ↓
Protocol
  ↓
Server
  ↓
Core
```

Client owns interaction.

Server owns semantics.

Ключевым концептуальным источником вдохновения для протокола является **SLIME (Swank)**:
- **Никакого скрейпинга текста (No text/terminal scraping):** клиент никогда не парсит сырой консольный вывод, ANSI-коды или смешанный текстовый поток модели в поисках путей к файлам или диффов. Вся коммуникация ведется строго типизированными семантическими сообщениями.
- **Презентационные потоки (Presentation Streams):** объекты в ответе (файлы, чанки изменений, ошибки, диагностики, запуски) передаются клиенту как первоклассные сущности предметной области со своим состоянием и доступными операциями.
- **Out-of-band и асинхронные события:** долгие операции (генерация LLM, фоновая компиляция, валидация правок) транслируют промежуточные события в реальном времени, не блокируя канал управления.
- **Сервер как независимый рантайм:** сервер держит сессию проекта независимо от клиентов; клиенты могут переподключаться или работать параллельно.

Client не собирает prompts, не вызывает LLM напрямую, не управляет Git lifecycle и не принимает semantic решения о project model.

## 3. Transport independence

Protocol types не зависят от transport.

Начальные transports:

1. `stdio`;
2. Unix domain socket.

Возможные будущие:

3. named pipes;
4. WebSocket;
5. TCP/TLS;
6. HTTP streaming.

Semantic protocol должен оставаться одинаковым.

## 4. Message model

Protocol строится как request/response + events.

Концептуально:

```text
Request
Response
Event
```

Используется модель JSON-RPC 2.0.

## 5. Request envelope

```json
{
  "id": 42,
  "method": "project/runAction",
  "params": {
    "action": "check"
  }
}
```

## 6. Response envelope

```json
{
  "id": 42,
  "result": {
    "runId": "run-184"
  }
}
```

Ошибка protocol/server:

```json
{
  "id": 42,
  "error": {
    "code": "ACTION_NOT_PERMITTED",
    "message": "Action 'check' is not permitted for the current task."
  }
}
```

Domain failure, например failed tests, не является protocol error.

## 7. Event envelope

```json
{
  "method": "run/finished",
  "params": {
    "runId": "run-184",
    "exitCode": 1
  }
}
```

## 8. Versioning and initialize

Client и server выполняют handshake через `client/initialize`.

Client сообщает:

```text
protocolVersion
clientName
clientVersion
capabilities
```

Server отвечает:

```text
protocolVersion
serverVersion
capabilities
session/workspace state
```

До `1.0` protocol может intentionally break compatibility.

## 9. Client capabilities

Пример:

```text
supportsAudioCapture
supportsAudioStreaming
supportsTTS
supportsOpenFile
supportsOpenSymbol
supportsDiffView
supportsRichText
supportsNotifications
```

## 10. Server capabilities

Пример:

```text
voice.serverCapture
voice.clientStreaming
voice.transcriptInput

project.plans
project.symbolIndex
project.repoMap

git.undo
git.history

models.streaming
edits.streaming
```

## 11. Session model

Client подключается к server и открывает repository/session.

```text
repository/open
session/create
session/attach
session/close
```

Server может поддерживать несколько clients на одну session.

## 12. Repository methods

```text
repository/open
repository/getState
repository/refresh
repository/listFiles
repository/findFiles
repository/findSymbols
repository/findReferences
```

`repository/getState` возвращает минимум:

```text
root
branch
HEAD
dirty state
changed files
trust state
active task/plan
```

## 13. Workspace trust

```text
workspace/getTrust
workspace/setTrust
```

Trust изменяется только explicit user action.

Model не может вызвать `workspace/setTrust`.

## 14. Project model methods

```text
project/getModel
project/listPackages
project/getPackage
project/listActions
project/getAction
project/listPlans
project/getPlan
project/activatePlan
project/getInstructions
```

## 15. ProjectAction protocol

### List

```text
project/listActions
```

### Run

```json
{
  "method": "project/runAction",
  "params": {
    "action": "check",
    "target": {
      "type": "package",
      "id": "core"
    }
  }
}
```

Server проверяет:

- action существует;
- project trusted;
- action разрешён;
- target допустим.

## 16. В protocol нет agent shell

Это намеренный invariant.

Не должно существовать методов:

```text
agent/runShell
model/runCommand
agent/exec
```

Model layer не получает arbitrary shell.

Если человек явно хочет выполнить command:

```text
user/runCommand
```

это отдельный user-authorized method с отдельной policy.

## 17. Context methods

```text
context/get
context/add
context/remove
context/setAccess
context/clear
context/pin
context/getRevision
```

Пример:

```json
{
  "method": "context/add",
  "params": {
    "path": "crates/core/src/task.rs",
    "access": "editable"
  }
}
```

## 18. Context item types

Начальный набор:

```text
File
Symbol
SourceRange
Plan
Instruction
Run
DiagnosticSet
GitDiff
CommitDiff
UserText
```

Access modes:
- `read_only`
- `editable`

## 19. Context events

```text
context/changed
context/budgetChanged
context/itemInvalidated
```

`context/changed` содержит актуальный `ContextState` и новую revision.

## 20. Task methods

```text
task/create
task/get
task/list
task/activate
task/updateTitle
task/close
```

Task является logical work boundary.

## 21. Intent methods

Два основных semantic entry points:

```text
intent/submitText
intent/submitTranscript
```

Пример:

```json
{
  "method": "intent/submitText",
  "params": {
    "text": "Добавь architecture read-only и запусти check."
  }
}
```

Server сам:

- резолвит references;
- классифицирует intents;
- исполняет deterministic части;
- вызывает coding model при необходимости.

## 22. Resolved intent events

```text
intent/resolved
intent/ambiguous
intent/executed
intent/failed
```

## 23. Model operation methods

```text
model/ask
model/cancel
model/clearHistory
```

## 24. Model & Reasoning events

```text
model/started
model/reasoningDelta
model/textDelta
model/usage
model/result
model/finished
model/cancelled
model/error
```

Текстовые дельты передают исключительно естественный язык модели (объяснения, рассуждения, ответы на вопросы). Блоки правок кода перехватываются потоковым фильтром сервера и транслируются в семантические события редактирования.

## 25. Structured Edit Streaming events

Во время генерации кода сервером клиенту отправляются события жизненного цикла изменений файлов:

### 25.1 edit/started
Сигнализирует о начале блока правок в ответе модели.

### 25.2 edit/fileStarted
Начало генерации изменений конкретного файла.
```json
{
  "method": "edit/fileStarted",
  "params": {
    "operation_id": "op-42",
    "path": "crates/core/src/edits.rs",
    "op_type": "replace"
  }
}
```
`op_type`: `"replace"`, `"create"`, `"delete"`.

### 25.3 edit/hunk
Потоковое получение готового чанка search/replace.
```json
{
  "method": "edit/hunk",
  "params": {
    "operation_id": "op-42",
    "path": "crates/core/src/edits.rs",
    "hunk_index": 0,
    "old_text": "fn old() {}\n",
    "new_text": "fn new() {}\n"
  }
}
```

### 25.4 edit/fileDone
Завершение обработки файла и результат промежуточной валидации в памяти сервера.
```json
{
  "method": "edit/fileDone",
  "params": {
    "operation_id": "op-42",
    "path": "crates/core/src/edits.rs",
    "status": "ok",
    "hunks_count": 1
  }
}
```
При ошибке сопоставления `search` (не найден или неоднозначен) передаются `status: "error"` и `error: "..."`.

### 25.5 edit/finished
Финальное событие атомарного применения всех изменений на диск.
```json
{
  "method": "edit/finished",
  "params": {
    "operation_id": "op-42",
    "applied": true,
    "changed_files": ["crates/core/src/edits.rs"]
  }
}
```

## 26. Git methods

```text
git/getStatus
git/getDiff
git/getCommitDiff
git/listHistory
git/commit
git/undo
```

Branch-changing operations не входят в начальный protocol.

## 27. Git events

```text
git/stateChanged
git/commitCreated
git/undoCompleted
git/externalChangeDetected
```

## 28. Runs

Methods:

```text
run/get
run/list
run/cancel
```

Events:

```text
run/started
run/stdout
run/stderr
run/diagnostic
run/finished
```

## 29. Voice architecture

Voice — first-class protocol subsystem.

Protocol поддерживает три режима:
- Server capture (`voice/startCapture`, `voice/stopCapture`);
- Client audio stream (`voice/startStream`, `voice/audioChunk`, `voice/stopStream`);
- Client transcript (`voice/submitTranscript`).

## 30. Spoken output

Server генерирует semantic events:

```text
speech/question
speech/notification
speech/error
speech/completion
```

Client решает показать text, произнести через TTS или проигнорировать.

## 31. Active object / focus context

```text
client/setFocus
```

Позволяет разрешать анафорические ссылки («это», «его», «эти ошибки»).

## 32. Open-in-editor requests

```text
client/openLocation
client/openSymbol
client/showDiff
```

## 33. Cancellation

```text
operation/cancel
```

Применимо к model calls, actions/runs, voice capture и indexing. При отмене частичные правки никогда не применяются.

## 34. Conformance & Compatibility

TUI является reference client и conformance implementation.
До `1.0` protocol может вносить breaking changes с обязательным bump версии.
