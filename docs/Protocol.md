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

## 3. Концепция семантического протокола взаимодействия

Протокол взаимодействия строится вокруг идеи **семантического шага (Semantic Turn)**:
1. **Типизированный исход вместо слепого текста:** каждый ответ модели воспринимается средой как структурированный исход (`Answer`, `Edit`, а в дальнейшем — запрос контекста `NeedContext`, `Review`, `Clarification`).
2. **Отказ от Tool Calling для кода:** нативные вызовы функций (`tool_call`) показали себя хрупкими при стриминге правок, конфликтуют с reasoning/thinking-режимами современных моделей (DeepSeek R1, Claude thinking) и нестабильно поддерживаются провайдерами через API. Протокол взаимодействия консолидируется вокруг двух надежных форматов:
   - **`xml` (основной):** потоковый псевдо-XML, естественно совмещающий свободные рассуждения модели и структурированные блоки (`<workbench_edits>`, `<edit>`, `<create>`, `<delete>`, `<search>`, `<replace>`). Валидируется на лету без блокировки вывода.
   - **`structured` (схемный):** строгий JSON Schema (Structured Output), потоково парсимый через токенизатор без буферизации всего ответа.
3. **Целостность и идемпотентность:** среда транслирует клиенту семантические события по мере их распознавания в потоке, но фиксация на диск и в Git происходит атомарно только после полной успешной валидации всех изменений.

## 4. Transport independence

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

## 5. Message model

Protocol строится как request/response + events.

Концептуально:

```text
Request
Response
Event
```

Используется модель JSON-RPC 2.0.

## 6. Request envelope

```json
{
  "id": 42,
  "method": "project/runAction",
  "params": {
    "action": "check"
  }
}
```

## 7. Response envelope

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
    "message": "Cannot change workflow, edit protocol or model while a model operation is in progress"
  }
}
```

Domain failure, например failed tests, не является protocol error.

## 8. Event envelope

```json
{
  "method": "run/finished",
  "params": {
    "runId": "run-184",
    "exitCode": 1
  }
}
```

## 9. Versioning and initialize

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
  "available_edit_protocols": ["xml", "structured"],
  "available_models": ["anthropic/claude-3.5-sonnet", "openai/gpt-4o"]
}
```

Поле `model` — текущая активная модель, `available_models` — список моделей, между которыми можно переключаться (`[models].available` в конфигурации; если не задан — состоит из `default`).

До `1.0` protocol может intentionally break compatibility.

## 10. Configuration methods & events

### 10.1 config/get
Возвращает текущие настройки выполнения и поддерживаемые списки:
```json
{
  "model": "anthropic/claude-3.5-sonnet",
  "available_models": ["anthropic/claude-3.5-sonnet", "openai/gpt-4o"],
  "workflow": "toolchain",
  "edit_protocol": "xml",
  "available_workflows": ["toolchain", "git", "naive"],
  "available_edit_protocols": ["xml", "structured"]
}
```

### 10.2 config/set
Изменяет текущий рабочий процесс, протокол редактирования или активную модель:
```json
{
  "method": "config/set",
  "params": {
    "workflow": "git",
    "edit_protocol": "structured",
    "model": "openai/gpt-4o"
  }
}
```
*Инвариант:* если в данный момент выполняется генерация модели, сервер возвращает ошибку `OPERATION_IN_PROGRESS`.
Значение `model` валидируется по `available_models`; неизвестная модель приводит к ошибке `INVALID_MODEL`. Выбранная модель используется последующими вызовами `model/ask`.

### 10.3 config/changed
Событие рассылается всем клиентам при изменении настроек:
```json
{
  "method": "config/changed",
  "params": {
    "workflow": "git",
    "edit_protocol": "structured",
    "model": "openai/gpt-4o",
    "available_models": ["anthropic/claude-3.5-sonnet", "openai/gpt-4o"],
    "available_workflows": ["toolchain", "git", "naive"],
    "available_edit_protocols": ["xml", "structured"]
  }
}
```

## 11. Repository methods

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

## 12. Context methods & events

```text
context/get
context/add
context/addPattern
context/remove
context/setAccess
context/clear
```

### 12.1 context/addPattern
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

### 12.2 context/changed
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

## 13. Model operation methods

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

## 14. Model & Reasoning events

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
- `model/textDelta`: чистый текст ответа без служебной разметки правок.
- `model/usage`: оперативные данные о токенах и точной стоимости (`cost` в USD) за запрос и суммарно за сессию (`session_total_cost`).

## 15. Structured Edit Streaming events

Во время генерации кода сервером клиенту отправляются семантические события:

### 15.1 edit/started
Сигнализирует о начале блока правок в ответе модели.

### 15.2 edit/fileStarted
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

### 15.3 edit/hunk
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

### 15.4 edit/fileDone
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

### 15.5 edit/fileRetrying
Сигнализирует о повторной попытке исправления несошедшихся правок для конкретного файла (Patch Retry Loop):
```json
{
  "method": "edit/fileRetrying",
  "params": {
    "operation_id": "op-42",
    "path": "crates/core/src/edits.rs",
    "attempt": 1,
    "max_retries": 2,
    "reason": "Search block not found (0 matches). Check indentation and line endings."
  }
}
```

### 15.6 edit/finished
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

## 16. Toolchain events

В воркфлоу `toolchain` транслируются события детерминированной проектной проверки:

### 16.1 toolchain/started
```json
{
  "method": "toolchain/started",
  "params": {
    "operation_id": "op-42",
    "command": "cargo check"
  }
}
```

### 16.2 toolchain/result
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

## 17. Git methods & events

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

## 18. History methods & events

### 18.1 history/get
Запрос семантических записей истории с поддержкой пагинации:
```json
{
  "method": "history/get",
  "params": {
    "limit": 10,
    "before_id": 42
  }
}
```
Ответ:
```json
{
  "items": [
    {
      "id": 41,
      "kind": "assistant",
      "text": "Добавил поддержку тега delete в парсер.",
      "commit_hash": "a1b2c3d",
      "summary": "Support delete tag in XML parser",
      "files": ["crates/core/src/edits.rs"]
    }
  ],
  "total_count": 50,
  "has_more": true
}
```

### 18.2 history/entryAdded
Событие рассылается клиентам при добавлении новой семантической записи (сообщения пользователя, ответа модели, отката коммита или резюме):
```json
{
  "method": "history/entryAdded",
  "params": {
    "item": {
      "id": 43,
      "kind": "user",
      "text": "Добавь валидацию путей",
      "files": ["crates/core/src/context.rs"]
    }
  }
}
```

## 19. В protocol нет agent shell

Это фундаментальный инвариант архитектуры.
В протоколе намеренно отсутствуют методы вида `agent/runShell`, `model/runCommand`, `agent/exec`.
Модель может действовать исключительно через семантические правки файлов и зарегистрированные проектные действия.
