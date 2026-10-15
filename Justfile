# Запуск dev-окружения по умолчанию
default: dev

# Собрать сервер и запустить TUI для разработки
dev *args="":
    cargo build -p tauqe-server
    cargo run -p tauqe-tui -- {{args}}

# Установка релизных версий tauqe-server и tauqe в систему (~/.cargo/bin)
# (cargo install по умолчанию всегда собирает бинарники в --release режиме)
install:
    cargo install --path crates/server --locked
    cargo install --path crates/tui --locked
    @echo "Tauqe успешно установлен в release-режиме! Доступна команда 'tauqe'."

# Интерактивный просмотр и анализ журнала сервера через lnav
logs *args="":
    lnav -I .lnav {{args}} .tauqe/server.log

# Живой просмотр журнала сервера в lnav (с автопереходом в конец файла и отслеживанием)
logs-live:
    lnav -I .lnav -c ':goto 100%' .tauqe/server.log

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
