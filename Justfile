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
book-build:
    mdbook build book

# Serve mdBook documentation with live reload
book-serve *args="":
    mdbook serve book {{args}}
