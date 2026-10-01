#!/bin/bash
# Хостовый прогон TLS 1.3 клиента браузера.
#
#   tools/tls_client_test.sh local [port]          — против локального openssl s_server
#   tools/tls_client_test.sh <host> <port> <path> [sni]  — против реального сайта
#
# Собирает programs/web/tls_client.c + крипто-примитивы обычным gcc и
# запускает tools/tls_client_test.c.

set -e
cd "$(dirname "$0")/.."

OUT=target/tls_client_test
mkdir -p target

gcc -O2 -std=gnu11 -Wall -Wextra -Wno-unused-parameter -o "$OUT" \
    tools/tls_client_test.c \
    programs/web/tls_client.c \
    programs/web/tls_crypto.c \
    programs/web/tls_gcm.c \
    programs/web/tls_x25519.c \
    -Iprograms/web

if [ "$1" = "local" ]; then
    D=target/tlstest
    mkdir -p "$D"
    if [ ! -f "$D/cert.pem" ]; then
        openssl req -x509 -newkey rsa:2048 -keyout "$D/key.pem" -out "$D/cert.pem" \
            -days 2 -nodes -subj "/CN=localhost" 2>/dev/null
    fi
    PORT=${2:-8443}
    openssl s_server -accept "$PORT" -cert "$D/cert.pem" -key "$D/key.pem" \
        -tls1_3 -alpn http/1.1 -www -quiet >/dev/null 2>"$D/s_server.log" &
    SRV=$!
    sleep 0.5
    set +e
    "$OUT" 127.0.0.1 "$PORT" / localhost
    RC=$?
    set -e
    kill "$SRV" 2>/dev/null || true
    wait "$SRV" 2>/dev/null || true
    exit $RC
fi

exec "$OUT" "$@"
