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
    "code": "OPERATION_IN_PROGRESS",
    "message": "Cannot change workflow or edit protocol while a model operation is in progress"
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

```json
{
  "protocol_version": "0.1.0",
  "client_name": "workbench-tui",
  "client_version": "0.1.0"
}
```

Server отвечает:

```json
{
  "protocol_version": "0.1.0",
  "server_name": "workbench-server",
  "server_version": "0.1.0",
  "repository": {
    "root": "/path/to/repo",
    "branch": "main",
    "head": "abc1234",
    "dirty": false
  },
  "model": "anthropic/claude-3.5-sonnet",
  "workflow": "toolchain",
  "edit_protocol": "xml",
  "available_workflows": ["toolchain", "git", "naive"],
  "available_edit_protocols": ["xml", "whole_file", "tool_call"]
}
```

До `1.0` protocol может intentionally break compatibility.

## 9. Configuration methods & events

### 9.1 config/get
Возвращает текущие настройки выполнения и поддерживаемые списки:
```json
{
  "workflow": "toolchain",
  "edit_protocol": "xml",
  "available_workflows": ["toolchain", "git", "naive"],
  "available_edit_protocols": ["xml", "whole_file", "tool_call"]
}
```

### 9.2 config/set
Изменяет текущий рабочий процесс или протокол редактирования:
```json
{
  "method": "config/set",
  "params": {
    "workflow": "git",
    "edit_protocol": "tool_call"
  }
}
```
*Инвариант:* если в данный момент выполняется генерация модели, сервер возвращает ошибку `OPERATION_IN_PROGRESS`.

### 9.3 config/changed
Событие рассылается всем клиентам при изменении настроек:
```json
{
  "method": "config/changed",
  "params": {
    "workflow": "git",
    "edit_protocol": "tool_call",
    "available_workflows": ["toolchain", "git", "naive"],
    "available_edit_protocols": ["xml", "whole_file", "tool_call"]
  }
}
```

## 10. Repository methods

```text
repository/getState
repository/listFiles
```

`repository/getState` возвращает:
- `root`: абсолютный путь к корню;
- `branch`: имя текущей ветки;
- `head`: сокращенный хэш HEAD;
- `dirty`: boolean-признак наличия незакоммиченных изменений.

`repository/listFiles` возвращает плоский список всех отслеживаемых файлов репозитория.

## 11. Context methods & events

```text
context/get
context/add
context/addPattern
context/remove
context/setAccess
context/clear
```

### 11.1 context/addPattern
Пакетное добавление файлов по glob-маске или префиксу директории:
```json
{
  "method": "context/addPattern",
  "params": {
    "pattern": "crates/core/src/*.rs",
    "access": "read_only"
  }
}
```
Ответ:
```json
{
  "added_count": 8,
  "added_tokens": 12450,
  "state": { ... }
}
```

### 11.2 context/changed
Событие рассылается клиентам при любом изменении состава или прав файлов контекста:
```json
{
  "method": "context/changed",
  "params": {
    "state": {
      "revision": 5,
      "total_estimated_tokens": 14200,
      "items": [
        {
          "path": "crates/core/src/lib.rs",
          "access": "editable",
          "size_bytes": 1024,
          "estimated_tokens": 256
        }
      ]
    }
  }
}
```

## 12. Model operation methods

```text
model/ask
model/cancel
model/clearHistory
```

### model/ask
```json
{
  "method": "model/ask",
  "params": {
    "prompt": "Добавь валидацию путей в context manager"
  }
}
```

## 13. Model & Reasoning events

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

- `model/reasoningDelta`: стриминг рассуждений модели (thinking/reasoning).
- `model/textDelta`: чистый текст ответа без сырых тегов разметки правок.
- `model/usage`: оперативные данные о токенах и точной стоимости (`cost` в USD) за запрос и суммарно за сессию (`session_total_cost`).

## 14. Structured Edit Streaming events

Во время генерации кода сервером клиенту отправляются семантические события:

### 14.1 edit/started
Сигнализирует о начале блока правок в ответе модели.

### 14.2 edit/fileStarted
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

### 14.3 edit/hunk
Потоковое получение готового чанка search/replace:
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

### 14.4 edit/fileDone
Завершение обработки файла и результат промежуточной валидации в памяти сервера:
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
При ошибке сопоставления передаются `status: "error"` и `error: "..."`.

### 14.5 edit/finished
Финальное событие атомарного применения изменений:
```json
{
  "method": "edit/finished",
  "params": {
    "operation_id": "op-42",
    "applied": true,
    "changed_files": ["crates/core/src/edits.rs"],
    "commit_hash": "a1b2c3d"
  }
}
```

## 15. Toolchain events

В воркфлоу `toolchain` транслируются события детерминированной проектной проверки:

### 15.1 toolchain/started
```json
{
  "method": "toolchain/started",
  "params": {
    "operation_id": "op-42",
    "command": "cargo check"
  }
}
```

### 15.2 toolchain/result
```json
{
  "method": "toolchain/result",
  "params": {
    "operation_id": "op-42",
    "command": "cargo check",
    "success": true,
    "output": "   Compiling workbench-core v0.1.0\n    Finished dev [unoptimized + debuginfo] target(s) in 1.42s"
  }
}
```

## 16. Git methods & events

### Methods:
- `git/getDiff`: получение unified diff между коммитами или рабочего дерева.
- `git/undo`: детерминированный откат последнего AI-коммита.
  *Инвариант:* если в данный момент выполняется генерация модели, возвращается ошибка `OPERATION_IN_PROGRESS`.

Ответ `git/undo`:
```json
{
  "undone_commit": "a1b2c3d",
  "restored_checkpoint": true,
  "new_head": "e4f5a6b",
  "message": "Undid AI commit a1b2c3d and restored original uncommitted changes."
}
```

### Events:
- `git/stateChanged`: обновление статуса репозитория (ветка, HEAD, dirty).
- `git/commitCreated`: фиксация AI-коммита (`commit_hash`, `summary`, `changed_files`).
- `git/undoCompleted`: уведомление об успешном откате коммита и восстановлении чекпоинта.

## 17. В protocol нет agent shell

Это фундаментальный инвариант архитектуры.
В протоколе намеренно отсутствуют методы вида `agent/runShell`, `model/runCommand`, `agent/exec`.
Модель может действовать исключительно через семантические правки файлов и зарегистрированные проектные действия.
