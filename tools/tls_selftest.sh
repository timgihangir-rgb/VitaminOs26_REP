#!/bin/bash
# Самотест крипто-примитивов TLS 1.3 на хосте.
#
#   tools/tls_selftest.sh
#
# Компилирует programs/web/tls_*.c вместе с tools/tls_selftest.c обычным gcc
# (без -nostdlib) и прогоняет векторы FIPS-197, RFC 4231, RFC 5869,
# NIST GCM и RFC 7748. Нужен, чтобы не ловить ошибки крипто за прогон QEMU.

set -e
cd "$(dirname "$0")/.."

OUT=target/tls_selftest
mkdir -p target

gcc -O2 -std=gnu11 -Wall -Wextra -Wno-unused-parameter -o "$OUT" \
    tools/tls_selftest.c \
    programs/web/tls_crypto.c \
    programs/web/tls_gcm.c \
    programs/web/tls_x25519.c \
    -Iprograms/web

"$OUT"
