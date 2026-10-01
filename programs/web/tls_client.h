/* TLS 1.3 клиент для браузера web (RFC 8446).
 *
 * Минимальный профиль: одна TLS_AES_128_GCM_SHA256 (0x1301), одна группа
 * x25519 (0x001d), ALPN «http/1.1». Сертификат НЕ проверяется (аналог
 * curl -k): Certificate/CertificateVerify пропускаются, но участвуют в
 * транскрипте. Транспорт абстрагирован, чтобы один и тот же код гонялся
 * на хосте через сокеты и в госте через ABI net_*.
 *
 * Память статическая: tls_conn ~50 КБ, вызывающий размещает его сам
 * (в госте — статически, стек не тратится на записи по 16 КБ).
 */
#ifndef TLS_CLIENT_H
#define TLS_CLIENT_H

#include <stddef.h>
#include <stdint.h>
#include "tls_crypto.h"

/* Максимальный размер полезной нагрузки одной записи (2^14 + запас). */
#define TLS_REC_MAX 16640

typedef struct {
    /* Отправить len байт. Возвращает len при успехе, <0 — ошибка. */
    int (*send)(void *ctx, const uint8_t *buf, size_t len);
    /* Прочитать до max байт. >0 — сколько, 0 — EOF, <0 — ошибка.
     * Блокирует, пока не появится хотя бы один байт. */
    int (*recv)(void *ctx, uint8_t *buf, size_t max);
    /* Заполнить out случайными байтами. */
    void (*random)(void *ctx, uint8_t *out, size_t len);
    void *ctx;
} tls_transport;

typedef struct {
    tls_transport tr;

    uint8_t priv[32];  /* эфемерный приватный ключ x25519 */

    uint8_t c_key[16], c_iv[12]; /* ключи записи клиента (текущие) */
    uint8_t s_key[16], s_iv[12]; /* ключи записи сервера (текущие) */
    uint64_t c_seq, s_seq;

    uint8_t c_hs_traffic[SHA256_DIGEST]; /* client/server handshake traffic */
    uint8_t s_hs_traffic[SHA256_DIGEST];
    uint8_t hs_secret[SHA256_DIGEST];    /* Handshake Secret (для master) */

    sha256_ctx transcript;

    uint8_t rec[TLS_REC_MAX];   /* полезная нагрузка читаемой записи */
    uint8_t rechdr[5];          /* заголовок читаемой записи (AAD) */
    uint8_t plain[TLS_REC_MAX]; /* расшифрованный внутренний plaintext */
    size_t plain_len, plain_off;

    uint8_t hs[TLS_REC_MAX]; /* сборка handshake-сообщений; до отправки CH
                              * используется как буфер под ClientHello */
    size_t hslen;

    int done;
    char err[96];
} tls_conn;

void tls_conn_init(tls_conn *c, tls_transport tr);

/* Полное рукопожатие с указанным SNI. 0 — успех, <0 — ошибка (c->err). */
int tls_handshake(tls_conn *c, const char *sni);

/* Отправить данные приложения. 0 — успех, <0 — ошибка. */
int tls_write(tls_conn *c, const uint8_t *data, size_t len);

/* Прочитать расшифрованные данные приложения: >0 — байт, 0 — EOF/close, <0 — ошибка. */
int tls_read(tls_conn *c, uint8_t *buf, size_t max);

/* Текст последней ошибки (для отладочного вывода). */
const char *tls_error(const tls_conn *c);

#endif /* TLS_CLIENT_H */
