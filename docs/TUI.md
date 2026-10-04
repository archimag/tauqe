# TUI

## 1. Назначение

TUI — первый полноценный client системы.

Это не временный CLI и не набор subcommands.

TUI должен быть:

- reference implementation protocol;
- реальным рабочим интерфейсом;
- достаточно удобным для self-hosting разработки;
- визуально плотным;
- keyboard-first;
- voice-first по вводу intent;
- похожим по философии на Magit/SLIME, но работающим в terminal.

Он должен общаться с server **только через protocol**.

## 2. Основной UX принцип

TUI представляет проект как **живое структурированное состояние**, а не как чат и не как editor.

Главные объекты интерфейса:

- Repository/Git state;
- Active Task/Plan;
- Context;
- Changes;
- Runs;
- Diagnostics;
- Current model result;
- Voice/Text input.

Source editor не является центром UI.

## 3. Визуальная модель

Базовый экран:

```text
┌──────────────────────────────────────────────────────────────┐
│ Project: workbench   branch: main   HEAD: a17c92f   clean   │
├──────────────────────┬───────────────────────────────────────┤
│ CONTEXT              │ ACTIVE PLAN                           │
│                      │                                       │
│ Editable             │ voice-protocol                        │
│  protocol/src/...    │ status: active                        │
│  core/src/...        │                                       │
│                      │ CURRENT RESULT                         │
│ Read only            │                                       │
│  ARCHITECTURE.md     │ model response / explanation          │
│                      │                                       │
│ Evidence             │                                       │
│  typecheck #184      │                                       │
├──────────────────────┼───────────────────────────────────────┤
│ CHANGES              │ RUNS                                  │
│ protocol.rs +24 -7   │ check      PASS                       │
│ server.rs    +8 -2   │ test       FAIL 2                     │
│                      │ clippy     NOT RUN                    │
├──────────────────────┴───────────────────────────────────────┤
│ 🎤 "fix those two failures and rerun tests"                  │
└──────────────────────────────────────────────────────────────┘
```

Layout может меняться, но концептуальные sections должны сохраняться.

## 4. Не делать chat основным экраном

Conversation может существовать, но не должна занимать весь интерфейс.

Главный объект — project state.

Model responses могут появляться в Current Result, task history или temporary detail view.

## 5. Section-oriented UX

Как в Magit, screen состоит из collapsible sections.

Пример:

```text
Repository
Task
Plan
Context
  Editable
  Read-only
  Evidence
Changes
Runs
Diagnostics
History
```

Каждый section:

- можно раскрыть/свернуть;
- может иметь cursor selection;
- предоставляет contextual actions.

## 6. Object under cursor

Действия должны применяться к semantic object под курсором.

### File

```text
Enter     inspect
e         editable
r         read-only
x         remove from context
o         open externally
```

### Run

```text
Enter     show output
r         rerun
c         add to context
x         remove evidence
```

### Git change

```text
Enter     show diff
u         revert selected safe AI change
c         add diff to context
```

### Plan

```text
Enter     open
a         activate
g         run gates
```

## 7. Keybinding philosophy

Keybindings должны быть mnemonic и compositional.

Возможный top-level набор:

```text
g   Git
c   Context
r   Runs
p   Plans
a   Actions
m   Model
v   Voice
/   Search
?   Help
```

Не нужно копировать Emacs буквально. Главное — быстрый доступ без mouse-oriented navigation.

## 8. Transient-like command surfaces

После prefix key показывается временная панель доступных действий.

Например `c`:

```text
Context
  a   add
  x   remove
  e   editable
  r   read-only
  p   pin
  C   clear
```

`a`:

```text
Actions
  c   check
  t   test
  l   lint
  f   format
  b   build
```

Список строится из actual project actions, а не hardcoded assumptions.

## 9. Command palette

Дополнительно полезен fuzzy command palette.

Он работает поверх protocol capabilities и current state.

Пример:

```text
Run: check
Run: protocol tests
Add file to context
Activate plan
Show diff
Undo last AI change
Start voice
```

## 10. Voice UX

Voice — основной быстрый semantic input.

TUI должен поддерживать push-to-talk, если terminal/input stack позволяет надёжно определить key-up:

```text
press/hold V
  → voice/startCapture

release V
  → voice/stopCapture
```

Если key-up ненадёжен, используется toggle:

```text
V
  start

V / Enter / Esc
  stop
```

Server owns voice semantics.

## 11. Voice state

Status line показывает:

```text
● LISTENING
```

Partial transcript:

```text
🎤 запусти те...
```

Final:

```text
🎤 запусти те тесты
```

Resolved intent можно кратко показать:

```text
→ Run action: protocol-tests
```

Это делает semantic resolution прозрачным.

## 12. Text input

Всегда должен существовать text input.

Он нужен для:

- точных filenames;
- regex;
- длинных prompts;
- code fragments;
- environments, где voice неудобен.

Voice и text идут через общий intent subsystem.

## 13. Speech output

TUI может использовать system TTS, но это optional.

Default policy:

- questions → spoken;
- errors → spoken;
- completion → spoken;
- long explanations → screen only.

User preferences могут выбирать категории.

TTS является presentation policy клиента.

## 14. Repository header

Постоянно видимая информация:

```text
project
branch
HEAD short hash
clean/dirty
workspace trust
active task/plan
server connection
```

## 15. Context view

Показывать реальную структуру context.

```text
Context  18.4k

Editable
  crates/core/src/task.rs          5.8k
  crates/protocol/src/voice.rs     4.2k

Read-only
  ARCHITECTURE.md                  2.7k

Evidence
  check #184                       1.2k

Repo map                           1.5k
History                            3.0k
```

Нужно показывать estimated token size, но не превращать UI в token dashboard.

## 16. Context provenance

Detail view показывает:

```text
Added by: user
Reason: active plan scope
Access: read-only
Representation: full
Revision: 42
```

Это полезно для debugging и понимания prompt state.

## 17. Add context flow

`c a` открывает selector.

Sources:

```text
files
symbols
plans
runs
diagnostics
diffs
recent items
```

Search использует server-side project vocabulary/index.

## 18. Plans view

```text
Plans

* voice-protocol      ACTIVE
  project-model       draft
  emacs-client        planned
```

Active plan detail:

```text
Goal
Scope
Constraints
Gates
Done criteria
```

Actions:

```text
activate
show source
add to context
run gates
```

## 19. Runs view

```text
Runs

#184 check          FAIL    0.8s
#183 protocol-test  PASS    2.1s
#182 clippy         PASS    1.4s
```

Output expandable.

Diagnostics извлекаются server-side.

## 20. Run output

Не нужен full terminal emulator.

Нужен structured output viewer:

```text
stdout
stderr
diagnostics
exit code
duration
action
target
```

Interactive programs не являются primary use case.

## 21. Diagnostics

Diagnostics должны быть navigable.

```text
E crates/core/src/task.rs:91
  mismatched types

E crates/protocol/src/voice.rs:44
  missing field `source`
```

`Enter` открывает detail или external editor.

`c` добавляет diagnostic set в context.

## 22. Changes view

Changes показываются Git-native.

```text
Changes

M crates/protocol/src/voice.rs     +24 -7
M crates/server/src/session.rs     +8 -2
```

`Enter` открывает diff.

## 23. Diff view

Diff является primary review UI.

Нужны:

- unified diff;
- file navigation;
- hunk navigation;
- syntax color where possible;
- relation to operation/commit;
- ability to revert safe known AI operation.

MVP не обязан поддерживать interactive staging как Magit.

## 24. Operation history

Показывать semantic operations:

```text
#52  Edit voice protocol
     model: ...
     context rev: 42
     commit: a17c92f
     check: PASS
     test: FAIL

#51  Add Architecture read-only

#50  Run protocol tests
```

Это полезнее plain chat history.

## 25. Model result

Short model answers показываются inline.

Long explanation может открываться отдельным detail view.

Structured model result должен явно разделять:

```text
Answer
Requested context
Proposed edit
Applied edit
```

## 26. Model context request

Если модель просит additional context:

```text
AI requests:

+ crates/core/src/task.rs
  Need to inspect Task lifecycle.

[a] add read-only
[e] add editable
[x] reject
```

Default recommendation — read-only.

## 27. Ambiguity resolution

Voice/text ambiguity отображается компактно.

```text
Which auth file?

1 frontend/auth.ts
2 backend/auth.ts

Press 1/2 or answer by voice.
```

Clarification должна быть быстрой.

## 28. Project actions view

TUI не hardcodes shell.

Он показывает actions, пришедшие от server:

```text
Available actions

check
test
protocol-tests
clippy
fmt
build
```

Недоступные current task actions могут отображаться disabled с reason.

## 29. Никакого arbitrary agent shell в TUI

TUI не должен создавать впечатление, что model может выполнять terminal commands.

Если есть explicit user command facility, она визуально и семантически отделена:

```text
User command:
> cargo tree -d
```

Она маркируется как действие человека, не агента.

## 30. External editor integration

Настройка:

```text
editor command
```

Actions:

```text
open file
open file at line
open symbol
```

Для self-hosting можно использовать Emacs даже до появления полноценного Emacs client.

## 31. Search

Search modes:

```text
file
symbol
text
action
plan
run
```

Поиск source выполняется server-side.

TUI только отображает результаты.

## 32. Notifications

Не использовать шумный toast-like UX.

Notifications появляются как status line, event log, short spoken notification или highlighted section.

Примеры:

```text
Check passed.
Two tests failed.
Context changed.
File changed externally.
Edit rejected as stale.
```

## 33. Connection model

TUI может:

1. spawn local server;
2. connect to existing local server.

Для self-hosting удобен режим:

```text
tui → spawn server child → stdio protocol
```

Но architecture должна также поддерживать socket daemon.

## 34. Crash/reconnect

Если client падает, server session не обязана исчезать; state хранится server-side; client может reconnect.

Если server child process принадлежит TUI, policy может быть проще.

## 35. Startup flow

```text
start TUI
 ↓
initialize protocol
 ↓
open repository
 ↓
load ProjectModel
 ↓
show trust state
 ↓
load task/plan/context
 ↓
ready
```

Если workspace untrusted, executable actions disabled.

## 36. Self-hosting target

TUI считается достаточным для bootstrap, когда через него можно:

1. открыть repository проекта;
2. активировать plan;
3. добавить/убрать context;
4. отправить edit task;
5. review diff;
6. run `check`;
7. run tests;
8. добавить failure как evidence;
9. попросить fix;
10. undo;
11. продолжать цикл без Aider/Claude Code.

## 37. TUI technology

Для Rust естественный кандидат — `ratatui` или аналогичная библиотека.

Но TUI architecture не должна зависеть от конкретной rendering library.

Основные client layers:

```text
protocol client
state cache/view model
input mapping
layout
widgets/sections
voice controls
```

## 38. Local client state

TUI хранит только presentation state:

```text
selected section
cursor
expanded/collapsed
scroll position
temporary input
local preferences
```

Domain state остаётся на server.

## 39. Reference-client requirement

Если TUI для какой-то функции вынужден обходить protocol и напрямую обращаться к filesystem, Git или core, это считается архитектурным дефектом.

Правильное решение — улучшить protocol/core abstraction.

## 40. UI character

Целевая эстетика:

- плотная;
- быстрая;
- спокойная;
- keyboard-oriented;
- state-first;
- минимум декоративных элементов;
- максимальная информативность;
- progressive disclosure.

Цель — не «современный красивый terminal dashboard», а ощущение:

> **проект находится перед разработчиком как панель управления, которую можно буквально трогать.**

Это тот же класс ощущения, который дают Magit и SLIME: сложная система представлена как живое интерактивное состояние, а не как набор разрозненных команд.
