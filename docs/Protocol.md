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

## 2. Архитектурный принцип

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

Можно использовать JSON-RPC 2.0 или близкую модель. Необязательно буквально копировать LSP.

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
    "item": {
      "type": "file",
      "path": "crates/core/src/task.rs"
    },
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

Representation может быть:

```text
full
symbol
range
summary
```

MVP может реализовать не все варианты.

## 19. Context events

```text
context/changed
context/budgetChanged
context/itemInvalidated
```

`context/changed` должен содержать новую revision.

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

Пример ambiguity:

```json
{
  "method": "intent/ambiguous",
  "params": {
    "requestId": "intent-88",
    "question": "Which auth file?",
    "options": [
      { "id": "frontend", "label": "frontend/auth.ts" },
      { "id": "backend", "label": "backend/auth.ts" }
    ]
  }
}
```

Client может ответить через `intent/resolveAmbiguity`.

## 23. Model operation methods

Clients обычно используют intents, но protocol может экспонировать explicit semantic methods:

```text
model/ask
model/requestEdit
model/cancel
```

Это полезно для rich clients.

## 24. Model events

```text
model/started
model/textDelta
model/result
model/usage
model/finished
model/cancelled
model/failed
```

Structured edit не применяется из partial stream.

## 25. Context request from model

Когда модель требует additional context:

```text
model/contextRequested
```

Payload содержит operation ID, requested items, reason и preferred access.

Client policy может показать запрос пользователю, auto-add read-only или reject.

Server остаётся authority.

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

Protocol поддерживает три режима.

### 29.1 Server capture

Подходит local server.

```text
voice/startCapture
voice/stopCapture
```

Server сам захватывает microphone.

### 29.2 Client audio stream

Подходит remote server или client-controlled audio.

```text
voice/startStream
voice/audioChunk
voice/stopStream
```

Audio format negotiated during start.

### 29.3 Client transcript

Если client уже имеет STT:

```text
voice/submitTranscript
```

Server всё равно выполняет project-aware normalization, reference resolution, discourse resolution и intent resolution.

## 30. Voice events

```text
voice/listeningStarted
voice/partialTranscript
voice/finalTranscript
voice/normalizedTranscript
voice/stopped
voice/error
```

## 31. Spoken output

Server не обязан генерировать raw audio.

Предпочтительно server генерирует semantic events:

```text
speech/question
speech/notification
speech/error
speech/completion
```

Client решает показать text, произнести через TTS или проигнорировать.

Это сохраняет разделение semantics/presentation.

## 32. Example voice flow

```text
TUI → voice/startCapture

Server → voice/listeningStarted

Server → voice/partialTranscript
        "запусти те..."

Server → voice/finalTranscript
        "запусти те тесты"

Server → voice/normalizedTranscript

Server → intent/resolved
        RunProjectAction(protocol-tests)

Server → run/started
Server → run/stdout ...
Server → run/finished
        exitCode=1

Server → speech/error
        "Two tests failed."
```

## 33. Active object / focus context

Rich clients могут сообщать server semantic focus:

```text
client/setFocus
```

Например:

```text
Run #184
Diagnostic #3
Context item auth.rs
Plan voice-protocol
Git change protocol.rs
```

Это помогает разрешать слова «это», «его», «эти ошибки», «этот файл».

Focus — hint, а не authority.

## 34. Open-in-editor requests

Server может отправлять client request/event:

```text
client/openLocation
client/openSymbol
client/showDiff
```

Client capability negotiation определяет поддержку.

## 35. Subscription model

Для multi-client server полезна подписка:

```text
subscribe
unsubscribe
```

Categories:

```text
repository
context
git
runs
model
voice
tasks
plans
```

MVP может отправлять все events attached client без explicit subscriptions.

## 36. Cancellation

Все long-running operations должны иметь operation ID и cancellation:

```text
operation/cancel
```

Применимо к model calls, actions/runs, voice capture и indexing.

Cancellation partial edit никогда не применяется.

## 37. Error classes

Разделять:

### Protocol errors
Malformed messages, unsupported version.

### Authorization/policy errors
Action not permitted, workspace untrusted.

### Domain errors
Context item missing, stale edit.

### Operation results
Failed tests, compiler errors — не protocol errors.

## 38. Serialization

Начальная рекомендация — JSON.

Причины:

- легко отлаживать;
- легко реализовать Emacs client;
- human-readable;
- достаточно для local protocol.

Binary audio chunks можно сначала передавать base64 или вынести в отдельный framed channel позднее.

## 39. stdio transport

Подходит для child server, editor integration и debugging.

Framing options:

- JSON lines;
- Content-Length framing как LSP.

Для streaming events Content-Length framing надёжнее, JSON lines проще для MVP.

## 40. Unix socket transport

Основной local daemon transport.

Плюсы:

- несколько clients;
- server живёт независимо;
- reconnect;
- TUI и Emacs могут подключаться к одной session.

## 41. WebSocket transport

Future option для remote/browser-like clients.

Не должен влиять на domain protocol.

## 42. Protocol conformance

TUI является reference client и conformance implementation.

Нужны protocol-level integration tests:

- initialize;
- open repository;
- add context;
- run action;
- submit intent;
- receive model events;
- receive Git events;
- voice flow;
- cancellation.

## 43. Compatibility philosophy

До `1.0`:

- semantic correctness важнее compatibility;
- breaking protocol changes допустимы;
- version bump обязателен;
- server должен явно отклонять incompatible clients.

После `1.0` можно ввести стабильные guarantees.

## 44. Security principles

Повторяются намеренно.

- Protocol не предоставляет model-controlled arbitrary shell.
- Project actions проходят trust и permission checks.
- Client не может обойти core invariants.
- Newly modified executable project definitions не становятся trusted автоматически.
- External paths запрещены без explicit read-only grant.
- Secret files не попадают в context автоматически.

## 45. Основной принцип protocol

Protocol должен описывать **объекты проекта и операции над ними**, а не низкоуровневые UI-команды.

Хорошо:

```text
context/add
project/runAction
git/getDiff
plan/activate
intent/submitText
```

Плохо:

```text
ui/openLeftPane
terminal/sendKeys
emacs/showBuffer
```

> **Protocol — extension point всей системы.**
