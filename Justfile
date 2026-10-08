# Recipe по умолчанию: запускает dev-окружение
default: dev

# Собрать сервер и запустить TUI для разработки
dev *args="":
    cargo build -p tauqe-server
    cargo run -p tauqe-tui -- {{args}}

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

# Сборка релизных бинарников tauqe-server и tauqe-tui
release:
    cargo build --release -p tauqe-server -p tauqe-tui

# Установка tauqe-server и tauqe в систему (~/.cargo/bin)
install:
    cargo install --path crates/server --locked
    cargo install --path crates/tui --locked
    @echo "Tauqe успешно установлен! Доступна команда 'tauqe'."

# Очистка артефактов сборки
clean:
    cargo clean

# Build mdBook documentation
docs-build:
    mdbook build docs

# Serve mdBook documentation with live reload
docs-serve *args="":
    mdbook serve docs {{args}}

# Подсчет строк кода проекта (включая тесты) с помощью tokei
loc *args="crates":
    tokei {{args}}

# Подсчет строк кода проекта без тестов (вырезка тестов утилитами find и sed)
loc-no-tests:
    #!/usr/bin/env bash
    set -euo pipefail
    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT
    cp -r crates "$tmp/"
    find "$tmp" -type d -name "tests" -exec rm -rf {} +
    find "$tmp" -type f -name "*_test.rs" -delete
    find "$tmp" -type f -name "*.rs" -exec sed -i '/^[[:space:]]*#\[cfg(test)\]/,$d' {} +
    (cd "$tmp" && tokei crates)
