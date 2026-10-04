# Vision

## 1. Назначение

Проект создаёт **AI-native среду разработки**, в которой разработчик сохраняет контроль над проектом, а модель используется как сильный семантический исполнитель внутри заранее заданных рамок.

Система не является IDE, расширением IDE или автономным coding agent. Она представляет собой **интеллектуальную среду управления проектом**, где:

- Git определяет текущее состояние и историю изменений;
- проектные файлы определяют, что проект умеет делать;
- явный контекст определяет, что модель знает;
- write scope определяет, что модель имеет право менять;
- task/plan scope определяет, какую проблему модель решает;
- project actions определяют, какие операции разрешено запускать;
- пользователь дешёво управляет этим голосом или текстом;
- модель выполняет работу внутри этих границ.

Главная цель — не убрать разработчика из цикла, а убрать **трение** между его намерением и системой.

> Не human-out-of-the-loop, а friction-out-of-the-loop.

## 2. Проблема

Типичная современная agent-first модель выглядит так:

```text
user request
    ↓
agent
    ↓
filesystem + search + shell + git + browser
    ↓
agent сам собирает контекст
    ↓
agent сам решает, что делать
    ↓
agent сам исполняет
    ↓
developer review
```

Такой подход делает модель одновременно интерпретатором требований, исследователем codebase, оператором shell, менеджером контекста, исполнителем, планировщиком и тестировщиком.

Проект исходит из противоположного предположения:

> **Если часть задачи может быть решена детерминированно, она не должна поручаться LLM.**

Примеры:

- Git знает, что изменилось — модель не должна это угадывать.
- `Cargo.toml` знает workspace — модель не должна заново выводить его структуру.
- `package.json` знает scripts — модель не должна изобретать команды.
- parser/LSP знает definitions — модель не должна искать их глазами.
- project plan знает scope — модель не должна самостоятельно расширять задачу.
- permission model знает, что можно менять — модель не должна сама решать, куда ей разрешено писать.

Модель должна применяться прежде всего там, где действительно нужна семантика: понять намерение, понять код, предложить изменение, объяснить проблему, разрешить неоднозначную человеческую ссылку, выполнить локальное преобразование внутри заданного пространства.

## 3. Основная гипотеза

Индустрия пытается уменьшить участие разработчика за счёт автономности агента.

Этот проект пытается уменьшить **стоимость участия разработчика**.

Вместо одного широкого запроса:

```text
"сделай feature"
```

целевая работа может выглядеть так:

```text
"Работаем над voice protocol."
"Добавь protocol crate и server crate в контекст."
"Architecture оставь read-only."
"Исправь только порядок событий."
"После изменения запусти check и protocol tests."
"Нет, этот файл верни."
"Покажи diff."
```

Если такие действия занимают секунды и не требуют ручного копирования файлов, команд, ошибок и контекста, разработчик может оставаться внутри процесса без существенного overhead.

## 4. Роль разработчика

Разработчик не превращается в диспетчера Jira-задач для цифровых исполнителей.

Он остаётся человеком, который:

- понимает устройство системы;
- формулирует границы;
- выбирает контекст;
- определяет write scope;
- задаёт архитектурные ограничения;
- принимает решения;
- видит последствия;
- выбирает проверки;
- контролирует Git;
- может в любой момент спуститься к исходному коду.

Модель освобождает прежде всего от ручного производства большого объёма кода и от части механической работы.

Система должна **сохранять situational awareness разработчика**.

> Высокая скорость генерации кода не должна покупаться ценой потери понимания собственного проекта.

## 5. Ключевые принципы

### 5.1 Git defines reality

Git является формальным источником истины о состоянии разработки: branch, HEAD, working tree, diff, commits, checkpoints, undo.

LLM не используется для того, что Git может сообщить точно.

### 5.2 Project files define capabilities

Проектные файлы описывают операционную модель проекта: packages/workspaces, dependencies, scripts, build actions, tests, lint, typecheck, formatting, generation.

Система должна извлекать максимум информации из `Cargo.toml`, `package.json`, workspace manifests, `Justfile`, `Makefile` и других поддерживаемых project files.

Если действие нельзя надёжно вывести автоматически, оно может быть описано в project configuration.

### 5.3 Context defines knowledge

Контекст является first-class state.

Пользователь должен видеть и контролировать editable files, read-only files, evidence, plans, instructions, repo map и conversation summary.

### 5.4 Permissions define power

**Способность модели и полномочия модели — разные вещи.**

Особенно:

> **У агента нет произвольного shell.**

Он может запускать только именованные project actions, уже существующие в trusted project model, и только если текущая policy это разрешает.

### 5.5 Never use reasoning where a deterministic abstraction exists

Если информацию можно получить из Git, parser, manifest, dependency graph, action registry, plan metadata или filesystem state, она должна получаться обычным кодом.

> **Model intelligence is not a substitute for system structure.**

### 5.6 The application owns state

Модель не является памятью системы. Состояние хранит приложение: context, tasks, plans, runs, Git snapshots, permissions, project model, operation history, model usage.

Каждый model call получает только необходимую проекцию состояния.

### 5.7 Minimal coherent change

Модель по умолчанию должна выполнять минимальное согласованное изменение, достаточное для решения задачи.

Новые abstractions, dependencies, config mechanisms или реорганизация не должны появляться без необходимости.

## 6. ProjectModel

Repository не рассматривается как просто каталог текстовых файлов.

Сервер строит структурированную модель:

```text
Project
  ├─ Repository
  ├─ Packages / Workspaces
  ├─ DependencyGraph
  ├─ Files
  ├─ Symbols
  ├─ Actions
  ├─ Tests
  ├─ GitState
  ├─ Plans
  ├─ Constraints
  ├─ Runs
  └─ Context
```

Это позволяет без LLM отвечать на вопросы вроде:

- какие packages затронуты diff;
- какие actions доступны package;
- где определён symbol;
- какие tests относятся к scope;
- какие files входят в plan;
- что зависит от изменённого crate;
- какие проверки уже прошли.

## 7. Semantic layer проекта

Существующие project files остаются machine truth.

Поверх них проект добавляет небольшой semantic layer, доступный человеку и системе:

```text
PROJECT.md
ARCHITECTURE.md

plans/
  voice-protocol.md
  emacs-client.md
  project-model.md

decisions/
  001-core-server-split.md
  002-no-arbitrary-shell.md
```

Дополнительно может существовать небольшой machine-readable manifest, например:

```text
.<tool-name>/project.toml
```

Он описывает только то, что нельзя надёжно вывести из существующего проекта.

> **Infer first, declare only what cannot be inferred.**

## 8. Plans как first-class objects

Plan — не скрытое состояние агента и не ephemeral chain-of-thought. Это обычный человекочитаемый проектный документ.

Он может содержать:

- goal;
- scope;
- constraints;
- affected areas;
- validation gates;
- done criteria;
- status.

Пример:

```markdown
+++
id = "voice-protocol"
status = "active"
scope = [
  "crates/protocol/**",
  "crates/voice/**"
]
gates = ["check", "test"]
+++

# Voice protocol

## Goal
Voice is a first-class input channel.

## Constraints
- Intent resolution belongs to core.
- Clients may send audio or transcripts.
- No client-specific semantics in the server.

## Done when
- TUI can start/stop voice capture.
- Transcript resolves into a normal intent.
- Voice implementation is client-independent.
```

Plan должен быть доступен разработчику, server core, model context, TUI и Emacs client.

## 9. Voice

Система является **voice-primary input, multimodal output**.

Голос особенно полезен для дешёвого управления состоянием:

```text
"Добавь schema только для чтения."
"Запусти те тесты."
"Исправь эти две ошибки и снова проверь типы."
```

Voice pipeline:

```text
audio
  ↓
STT
  ↓
project-aware normalization
  ↓
reference resolution
  ↓
discourse resolution
  ↓
intent
```

Система может говорить, но основной output остаётся визуальным. Голосовой output особенно уместен для коротких вопросов, ошибок и completion notifications.

## 10. Server-first architecture

Главный продукт — server/core. Клиенты заменяемы.

```text
TUI ──────┐
Emacs ────┼── Protocol ── Server ── Core
Other ────┘
```

> **Server owns semantics. Client owns interaction.**

## 11. Клиенты

### Stage 1 — TUI

Первый клиент — полноценное terminal application, а не набор CLI-команд.

Он одновременно reference implementation протокола, рабочий интерфейс и средство self-hosting разработки.

### Stage 2 — Emacs

Emacs client строится в духе Magit/SLIME: живое структурированное представление состояния, sections, быстрые действия над объектом под курсором, transient-like command surfaces, глубокая интеграция с Git и source buffers.

Emacs client не содержит business logic.

### Другие клиенты

VS Code, Neovim, desktop/Tauri, JetBrains и другие clients могут реализовываться другими авторами поверх protocol.

Core не получает client-specific abstractions.

## 12. Self-hosting / bootstrap

Система должна начать использовать саму себя как можно раньше.

Bootstrap point наступает, когда она умеет:

- открыть собственный Git repository;
- понять Cargo workspace;
- показать Git state;
- управлять explicit context;
- отправить bounded edit request;
- применить edit;
- checkpoint/commit/undo;
- обнаружить и запустить `check`, `test`, `clippy`, `fmt`;
- превратить failures в Evidence;
- продолжить после команды «исправь эти ошибки».

После этого дальнейшая разработка core должна по возможности выполняться через саму систему.

Repository проекта становится первым эталонным примером project organization.

## 13. Что проект сознательно не делает

Не является целью:

- создать полностью автономного software engineer;
- дать модели root-like доступ к среде;
- заменить Git;
- заменить build systems;
- заменить IDE;
- поддерживать все editor ecosystems самим;
- поддерживать arbitrary model-controlled shell;
- создавать multi-agent swarms;
- строить cloud execution platform;
- автоматически выполнять неизвестный repository code;
- минимизировать число действий пользователя любой ценой.

Главная цель:

> **Сделать действия разработчика очень дешёвыми, а действия модели — хорошо ограниченными и проверяемыми.**

## 14. Краткая формула

```text
Git
    → reality

Project files
    → capabilities

Project model
    → deterministic understanding

Plans
    → intended scope

Context
    → model knowledge

Permissions
    → model power

Voice
    → low-friction human control

LLM
    → semantic transformation
```

Итог:

> **Интеллектуальная Git-native среда разработки, которая усиливает контроль разработчика и использует LLM только там, где генеративное семантическое reasoning действительно полезно.**
