# Protocol

## 1. Назначение

Protocol (`tauqe-protocol`) определяет публичную границу между server и clients.

Он позволяет независимо реализовывать:
- терминальный клиент (TUI);
- альтернативные клиенты (например, Emacs в перспективе).

Protocol не содержит UI-специфичных концептов. Он описывает исключительно операции доменной области и семантические события.

## 2. Архитектурный принцип: модель SLIME (Swank)

```text
Client
  ↓
Protocol (JSON-RPC)
  ↓
Server
  ↓
Core
```

Client owns interaction.
Server owns semantics.

Ключевые принципы протокола:
- **Никакого скрейпинга текста (No text/terminal scraping):** клиент никогда не парсит сырой консольный вывод, ANSI-коды или смешанный текстовый поток модели в поисках путей к файлам или диффов. Вся коммуникация ведётся строго типизированными семантическими сообщениями.
- **Презентационные потоки (Presentation Streams):** объекты в ответе (файлы, чанки изменений, ошибки валидации, статусы тулчейна) передаются клиенту как первоклассные сущности предметной области со своим состоянием.
- **Out-of-band и асинхронные события:** долгие операции (генерация LLM, фоновая проверка тулчейна, валидация правок) транслируют промежуточные события в реальном времени, не блокируя канал управления.
- **Сервер как независимый рантайм:** сервер держит сессию проекта независимо от клиентов.

## 3. Транспорт

Текущий транспорт:
1. `stdio` (запуск сервера как дочернего процесса клиента).

Перспектива:
2. Unix domain socket (фоновый демон).

Семантический протокол остаётся одинаковым независимо от транспорта.

## 4. Модель сообщений

Протокол строится по спецификации JSON-RPC 2.0 (Request, Response, Event).

### 4.1 Request envelope
```json
{
  "id": 1,
  "method": "model/ask",
  "params": {
    "prompt": "Добавь валидацию путей в context manager"
  }
}
```

### 4.2 Response envelope
```json
{
  "id": 1,
  "result": {
    "operation_id": "op-42"
  }
}
```

Ошибка сервера:
```json
{
  "id": 1,
  "error": {
    "code": "OPERATION_IN_PROGRESS",
    "message": "Cannot change workflow, edit protocol or model while a model operation is in progress"
  }
}
```

### 4.3 Event envelope
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

## 5. Инициализация (Handshake)

Клиент и сервер выполняют handshake через `client/initialize`.

Запрос клиента:
```json
{
  "protocol_version": "0.1.0",
  "client_name": "tauqe-tui",
  "client_version": "0.1.0"
}
```

Ответ сервера:
```json
{
  "protocol_version": "0.1.0",
  "server_name": "tauqe-server",
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

## 6. Конфигурация (Configuration)

### 6.1 config/get
Возвращает текущие настройки выполнения и поддерживаемые списки.

### 6.2 config/set
Изменяет рабочий процесс (`workflow`), протокол редактирования (`edit_protocol`) или активную модель (`model`):
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

### 6.3 config/changed
Событие рассылается клиентам при изменении настроек.

## 7. Репозиторий (Repository)

```text
repository/getState
repository/listFiles
```

- `repository/getState` возвращает `root`, `branch`, `head`, `dirty`.
- `repository/listFiles` возвращает список отслеживаемых Git файлов репозитория.

## 8. Контекст (Context)

```text
context/get
context/add
context/addPattern
context/remove
context/setAccess
context/clear
```

### 8.1 context/addPattern
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

### 8.2 context/changed
Событие рассылается клиентам при любом изменении состава или прав файлов контекста (`ContextState`).

## 9. Операции модели (Model Operations)

```text
model/ask
model/cancel
model/clearHistory
```

- `model/ask`: запуск запроса к модели с текстом промпта.
- `model/cancel`: принудительная отмена активной генерации.
- `model/clearHistory`: очистка персистентного файла истории `.tauqe/history.jsonl`.

## 10. События генерации и рассуждений

- `model/started`: начало операции (`operation_id`, `model`).
- `model/reasoningDelta`: стриминг хода рассуждений модели (thinking).
- `model/textDelta`: чистый текст ответа без служебной разметки правок.
- `model/usage`: оперативные данные о токенах и стоимости (`cost`, `session_total_cost`).
- `model/result`: итоговый результат выполнения (`ModelResult`: Answer или Edit).
- `model/finished`: успешное завершение генерации.
- `model/cancelled`: генерация прервана клиентом.
- `model/error`: ошибка во время исполнения.

## 11. События потоковых правок (Structured Edit Streaming)

- `edit/started`: обнаружен блок изменений в ответе модели.
- `edit/fileStarted`: начало генерации правок конкретного файла (`path`, `op_type`: replace, create, delete).
- `edit/hunk`: готовый чанк изменений (`hunk_index`, `old_text`, `new_text`).
- `edit/fileDone`: завершение файла и результат in-memory валидации (`status`: ok / error).
- `edit/fileRetrying`: повторная попытка исправления несошедшихся правок для файла в рамках Patch Retry Loop (`attempt`, `max_retries`, `reason`).
- `edit/finished`: финальный результат применения (`applied`, `changed_files`, `commit_hash`).

## 12. События тулчейна (Toolchain Events)

- `toolchain/started`: запуск проверки (`command`, например `cargo check`).
- `toolchain/result`: результат выполнения проверки (`command`, `success`, `output`).

## 13. Git-операции и события

### Методы:
- `git/getDiff`: получение unified diff между коммитами или для рабочего дерева.
- `git/undo`: детерминированный откат последнего AI-коммита и восстановление чекпоинта.

### События:
- `git/stateChanged`: обновление статуса репозитория (`repository`).
- `git/commitCreated`: фиксация AI-коммита (`commit_hash`, `summary`, `changed_files`).
- `git/undoCompleted`: подтверждение успешного отката коммита.

## 14. История (History)

### 14.1 history/get
Постраничная выборка семантической истории сессии:
```json
{
  "method": "history/get",
  "params": {
    "limit": 10,
    "before_id": 42
  }
}
```

### 14.2 history/entryAdded
Событие рассылается клиентам при появлении новой записи в истории:
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

## 15. Инвариант: отсутствие arbitrary agent shell

В протоколе принципиально отсутствуют методы вида `agent/runShell`, `model/runCommand` или `agent/exec`. Модель может действовать исключительно через семантические правки файлов и предопределённую команду верификации тулчейна.
