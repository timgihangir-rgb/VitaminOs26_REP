/* Крипто-примитивы для TLS 1.3 в браузере web.
 *
 * Только то, что нужно TLS_AES_128_GCM_SHA256:
 *   SHA-256 (+ HMAC, HKDF-Extract/Expand, HKDF-Expand-Label),
 *   AES-128 (только шифрование) + GCM,
 *   X25519 (обмен ключами).
 *
 * Рассчитано на freestanding: без libc, без __attribute__((section)).
 * Проверяется на хосте через tools/tls_selftest.c (векторы RFC 6234,
 * RFC 4231, RFC 5869, FIPS-197, NIST GCM, RFC 7748).
 */
#ifndef TLS_CRYPTO_H
#define TLS_CRYPTO_H

#include <stddef.h>
#include <stdint.h>

/* ─── Байтовые помощники (freestanding: gcc сам генерирует вызовы memcpy) ── */
void *memcpy(void *d, const void *s, size_t n);
void *memset(void *d, int c, size_t n);
int memcmp(const void *a, const void *b, size_t n);

static inline uint16_t be16_get(const uint8_t *p) {
    return (uint16_t)((p[0] << 8) | p[1]);
}
static inline uint32_t be32_get(const uint8_t *p) {
    return ((uint32_t)p[0] << 24) | ((uint32_t)p[1] << 16) |
           ((uint32_t)p[2] << 8) | (uint32_t)p[3];
}
static inline uint64_t be64_get(const uint8_t *p) {
    return ((uint64_t)be32_get(p) << 32) | be32_get(p + 4);
}
static inline void be16_put(uint8_t *p, uint16_t v) {
    p[0] = (uint8_t)(v >> 8);
    p[1] = (uint8_t)v;
}
static inline void be32_put(uint8_t *p, uint32_t v) {
    p[0] = (uint8_t)(v >> 24);
    p[1] = (uint8_t)(v >> 16);
    p[2] = (uint8_t)(v >> 8);
    p[3] = (uint8_t)v;
}
static inline void be64_put(uint8_t *p, uint64_t v) {
    be32_put(p, (uint32_t)(v >> 32));
    be32_put(p + 4, (uint32_t)v);
}

/* ─── SHA-256 ────────────────────────────────────────────────────────────── */
#define SHA256_DIGEST 32
#define SHA256_BLOCK 64

typedef struct {
    uint32_t h[8];
    uint64_t nbytes;
    uint8_t buf[SHA256_BLOCK];
    size_t buflen;
} sha256_ctx;

void sha256_init(sha256_ctx *c);
void sha256_update(sha256_ctx *c, const void *data, size_t len);
void sha256_final(sha256_ctx *c, uint8_t out[SHA256_DIGEST]);
void sha256(const void *data, size_t len, uint8_t out[SHA256_DIGEST]);

/* ─── HMAC-SHA256 ────────────────────────────────────────────────────────── */
#define HMAC256_BLOCK 64
typedef struct {
    sha256_ctx inner;
    uint8_t opad[SHA256_BLOCK];
} hmac256_ctx;

void hmac256_init(hmac256_ctx *c, const uint8_t *key, size_t keylen);
void hmac256_update(hmac256_ctx *c, const void *data, size_t len);
void hmac256_final(hmac256_ctx *c, uint8_t out[SHA256_DIGEST]);
void hmac256(const uint8_t *key, size_t keylen, const void *data, size_t len,
             uint8_t out[SHA256_DIGEST]);

/* ─── HKDF (RFC 5869) ────────────────────────────────────────────────────── */
void hkdf_extract(const uint8_t *salt, size_t saltlen, const uint8_t *ikm,
                  size_t ikmlen, uint8_t prk[SHA256_DIGEST]);
void hkdf_expand(const uint8_t *prk, const uint8_t *info, size_t infolen,
                 uint8_t *out, size_t outlen);
/* TLS 1.3: HKDF-Expand-Label с префиксом "tls13 " (RFC 8446, 7.1). */
void hkdf_expand_label(const uint8_t *secret, const char *label,
                       const uint8_t *ctx, size_t ctxlen, uint8_t *out,
                       size_t outlen);
/* Derive-Secret(secret, label, messages) — хеш транскрипта передаётся msg. */
void tls_derive_secret(const uint8_t *secret, const char *label,
                       const uint8_t *msg_hash, uint8_t out[SHA256_DIGEST]);

/* ─── AES-128 + GCM ──────────────────────────────────────────────────────── */
#define AES_BLOCK 16
#define AES128_ROUND_KEYS 11

typedef struct {
    uint8_t rk[AES128_ROUND_KEYS * AES_BLOCK]; /* 11 раундовых ключей */
} aes128_key;

void aes128_setkey(aes128_key *k, const uint8_t key[16]);
void aes128_encrypt_block(const aes128_key *k, const uint8_t in[16],
                          uint8_t out[16]);

#define GCM_TAG 16
typedef struct {
    aes128_key aes;
    uint64_t mt[16];  /* GHASH: M[n] = n·H (старшие 8 байт) */
    uint64_t ml[16];  /* GHASH: M[n] (младшие 8 байт) */
    uint8_t hbytes[AES_BLOCK];
    uint8_t j0[AES_BLOCK];  /* исходный J0 — им подписывается тег */
    uint8_t ctr[AES_BLOCK]; /* счётчик CTR, стартует с J0 + 1 */
    uint64_t aadlen;
    uint64_t ctlen;
    uint8_t buf[AES_BLOCK]; /* накопленные блоки AAD/шифротекста */
    size_t buflen;
    uint8_t phase; /* 0 — накапливается AAD, 1 — шифротекст */
} gcm_ctx;

/* Ключ = 16 байт AES; nonce 12 байт (как требует TLS 1.3). */
void gcm_init(gcm_ctx *c, const uint8_t key[16], const uint8_t nonce[12]);
/* Добавить AAD (до вывода шифротекста) либо шифротекст — после. */
void gcm_aad_update(gcm_ctx *c, const void *data, size_t len);
void gcm_encrypt_update(gcm_ctx *c, const void *pt, size_t len, uint8_t *out);
void gcm_decrypt_update(gcm_ctx *c, const void *ct, size_t len, uint8_t *out);
void gcm_encrypt_final(const gcm_ctx *c, uint8_t tag[GCM_TAG]);
/* out может совпадать с in. Возвращает 0, если тег не сошёлся. */
int gcm_decrypt_final(const gcm_ctx *c, const uint8_t tag[GCM_TAG]);

/* Только для самотеста: эталонный побитовый GHASH (табличный обязан совпасть). */
void gcm_ghash_ref(const gcm_ctx *c, uint8_t x[AES_BLOCK]);

/* Односторонние вызовы: pt/ct могут совпадать; aad — дополнительные данные. */
void aes128_gcm_encrypt(const uint8_t key[16], const uint8_t nonce[12],
                        const uint8_t *aad, size_t aadlen, const uint8_t *pt,
                        size_t ptlen, uint8_t *ct, uint8_t tag[GCM_TAG]);
int aes128_gcm_decrypt(const uint8_t key[16], const uint8_t nonce[12],
                       const uint8_t *aad, size_t aadlen, const uint8_t *ct,
                       size_t ctlen, const uint8_t tag[GCM_TAG], uint8_t *pt);

/* ─── X25519 (RFC 7748) ──────────────────────────────────────────────────── */
void x25519_base(uint8_t out[32], const uint8_t scalar[32], const uint8_t point[32]);
/* scalarmult + проверка, что общий секрет не нулевой. 0 = ок. */
int x25519_shared(uint8_t out[32], const uint8_t scalar[32], const uint8_t point[32]);
void x25519_public(uint8_t out[32], const uint8_t scalar[32]);

#endif /* TLS_CRYPTO_H */