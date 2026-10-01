#!/bin/bash
# Хостовый прогон HTML->текст рендера браузера.
#
#   tools/render_test.sh <body.html>   — распечатать строки и ссылки
#
# Компилирует main.c как часть теста (переименовывая _start в web_guest_start),
# кладёт тело в rxbuf и вызывает build_page.

set -e
cd "$(dirname "$0")/.."

OUT=target/render_test
mkdir -p target

gcc -O0 -g -std=gnu11 -Wall -Wextra -Wno-unused-parameter \
    -Wno-unused-function -Wno-unused-variable \
    -o "$OUT" \
    tools/render_test.c \
    programs/web/tls_client.c \
    programs/web/tls_crypto.c \
    programs/web/tls_gcm.c \
    programs/web/tls_x25519.c \
    -Iprograms/web

exec "$OUT" "$@"
