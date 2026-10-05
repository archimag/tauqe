# Recipe по умолчанию: запускает dev-окружение
default: dev

# Собрать сервер и запустить TUI для разработки
dev *args="":
    cargo build -p workbench-server
    cargo run -p workbench-tui -- {{args}}

# Быстрая проверка всех пакетов воркспейса
check:
    cargo check --workspace --all-targets

# Запуск тестов во всех пакетах
test *args="":
    cargo test --workspace {{args}}

# Запуск clippy с подсветкой ошибок и предупреждений
clippy:
    cargo clippy --workspace --all-targets -- -D warnings

# Автоматическое форматирование исходного кода
fmt:
    cargo fmt --all

# Проверка форматирования без изменения файлов
fmt-check:
    cargo fmt --all -- --check

# Сборка всех пакетов воркспейса в debug-режиме
build:
    cargo build --workspace

# Сборка релизных бинарников workbench-server и workbench-tui
release:
    cargo build --release -p workbench-server -p workbench-tui

# Очистка артефактов сборки
clean:
    cargo clean
