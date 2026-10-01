/* Крипто-примитивы TLS 1.3 — реализация. См. tls_crypto.h.
 *
 * Считаем в предположении little-endian x86_64: это позволяет AES и SHA
 * грузить байты прямо из буфера, но всё, что попадает в сеть, сериализуется
 * явно через be*_put/be*_get.
 */

#include "tls_crypto.h"

/* ─── memcpy/memset/memcmp ───────────────────────────────────────────────── */
/* gcc в freestanding генерирует вызовы этих функций даже при -nostdlib,
 * поэтому определения нужны свои. */

void *memcpy(void *d, const void *s, size_t n) {
    unsigned char *dp = (unsigned char *)d;
    const unsigned char *sp = (const unsigned char *)s;
    while (n >= 8) {
        *(uint64_t *)dp = *(const uint64_t *)sp;
        dp += 8;
        sp += 8;
        n -= 8;
    }
    while (n--)
        *dp++ = *sp++;
    return d;
}

void *memset(void *d, int c, size_t n) {
    unsigned char *dp = (unsigned char *)d;
    uint64_t v = (uint8_t)c;
    v |= v << 8;
    v |= v << 16;
    v |= v << 32;
    while (n >= 8) {
        *(uint64_t *)dp = v;
        dp += 8;
        n -= 8;
    }
    while (n--)
        *dp++ = (uint8_t)c;
    return d;
}

int memcmp(const void *a, const void *b, size_t n) {
    const unsigned char *x = (const unsigned char *)a;
    const unsigned char *y = (const unsigned char *)b;
    for (size_t i = 0; i < n; i++)
        if (x[i] != y[i])
            return x[i] < y[i] ? -1 : 1;
    return 0;
}

/* ─── SHA-256 (FIPS 180-4) ───────────────────────────────────────────────── */

static const uint32_t K256[64] = {
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
    0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
    0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
    0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2
};

static inline uint32_t ror32(uint32_t x, int n) {
    return (x >> n) | (x << (32 - n));
}

static void sha256_block(sha256_ctx *c, const uint8_t *p) {
    uint32_t w[64];
    for (int i = 0; i < 16; i++)
        w[i] = be32_get(p + i * 4);
    for (int i = 16; i < 64; i++) {
        uint32_t s0 = ror32(w[i - 15], 7) ^ ror32(w[i - 15], 18) ^ (w[i - 15] >> 3);
        uint32_t s1 = ror32(w[i - 2], 17) ^ ror32(w[i - 2], 19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16] + s0 + w[i - 7] + s1;
    }
    uint32_t a = c->h[0], b = c->h[1], cc = c->h[2], d = c->h[3];
    uint32_t e = c->h[4], f = c->h[5], g = c->h[6], h = c->h[7];
    for (int i = 0; i < 64; i++) {
        uint32_t S1 = ror32(e, 6) ^ ror32(e, 11) ^ ror32(e, 25);
        uint32_t ch = (e & f) ^ ((~e) & g);
        uint32_t t1 = h + S1 + ch + K256[i] + w[i];
        uint32_t S0 = ror32(a, 2) ^ ror32(a, 13) ^ ror32(a, 22);
        uint32_t maj = (a & b) ^ (a & cc) ^ (b & cc);
        uint32_t t2 = S0 + maj;
        h = g; g = f; f = e; e = d + t1;
        d = cc; cc = b; b = a; a = t1 + t2;
    }
    c->h[0] += a; c->h[1] += b; c->h[2] += cc; c->h[3] += d;
    c->h[4] += e; c->h[5] += f; c->h[6] += g; c->h[7] += h;
}

void sha256_init(sha256_ctx *c) {
    c->h[0] = 0x6a09e667; c->h[1] = 0xbb67ae85;
    c->h[2] = 0x3c6ef372; c->h[3] = 0xa54ff53a;
    c->h[4] = 0x510e527f; c->h[5] = 0x9b05688c;
    c->h[6] = 0x1f83d9ab; c->h[7] = 0x5be0cd19;
    c->nbytes = 0;
    c->buflen = 0;
}

void sha256_update(sha256_ctx *c, const void *data, size_t len) {
    const uint8_t *p = (const uint8_t *)data;
    c->nbytes += len;
    if (c->buflen) {
        size_t need = SHA256_BLOCK - c->buflen;
        if (len < need) {
            memcpy(c->buf + c->buflen, p, len);
            c->buflen += len;
            return;
        }
        memcpy(c->buf + c->buflen, p, need);
        sha256_block(c, c->buf);
        p += need;
        len -= need;
        c->buflen = 0;
    }
    while (len >= SHA256_BLOCK) {
        sha256_block(c, p);
        p += SHA256_BLOCK;
        len -= SHA256_BLOCK;
    }
    if (len) {
        memcpy(c->buf, p, len);
        c->buflen = len;
    }
}

void sha256_final(sha256_ctx *c, uint8_t out[SHA256_DIGEST]) {
    uint64_t bits = c->nbytes * 8;
    uint8_t pad = 0x80;
    sha256_update(c, &pad, 1);
    uint8_t zero = 0;
    while (c->buflen != 56)
        sha256_update(c, &zero, 1);
    uint8_t len[8];
    be64_put(len, bits);
    sha256_update(c, len, 8);
    for (int i = 0; i < 8; i++)
        be32_put(out + i * 4, c->h[i]);
}

void sha256(const void *data, size_t len, uint8_t out[SHA256_DIGEST]) {
    sha256_ctx c;
    sha256_init(&c);
    sha256_update(&c, data, len);
    sha256_final(&c, out);
}

/* ─── HMAC-SHA256 (RFC 4231) ─────────────────────────────────────────────── */

void hmac256_init(hmac256_ctx *c, const uint8_t *key, size_t keylen) {
    uint8_t k[SHA256_BLOCK];
    memset(k, 0, sizeof(k));
    if (keylen > SHA256_BLOCK)
        sha256(key, keylen, k);
    else
        memcpy(k, key, keylen);

    uint8_t ipad[SHA256_BLOCK];
    for (int i = 0; i < SHA256_BLOCK; i++) {
        ipad[i] = (uint8_t)(k[i] ^ 0x36);
        c->opad[i] = (uint8_t)(k[i] ^ 0x5c);
    }
    sha256_init(&c->inner);
    sha256_update(&c->inner, ipad, SHA256_BLOCK);
}

void hmac256_update(hmac256_ctx *c, const void *data, size_t len) {
    sha256_update(&c->inner, data, len);
}

void hmac256_final(hmac256_ctx *c, uint8_t out[SHA256_DIGEST]) {
    uint8_t ih[SHA256_DIGEST];
    sha256_final(&c->inner, ih);
    sha256_ctx o;
    sha256_init(&o);
    sha256_update(&o, c->opad, SHA256_BLOCK);
    sha256_update(&o, ih, SHA256_DIGEST);
    sha256_final(&o, out);
}

void hmac256(const uint8_t *key, size_t keylen, const void *data, size_t len,
             uint8_t out[SHA256_DIGEST]) {
    hmac256_ctx c;
    hmac256_init(&c, key, keylen);
    hmac256_update(&c, data, len);
    hmac256_final(&c, out);
}

/* ─── HKDF (RFC 5869) и метки TLS 1.3 (RFC 8446, 7.1) ───────────────────── */

void hkdf_extract(const uint8_t *salt, size_t saltlen, const uint8_t *ikm,
                  size_t ikmlen, uint8_t prk[SHA256_DIGEST]) {
    uint8_t zero[SHA256_DIGEST];
    if (!salt) {
        memset(zero, 0, sizeof(zero));
        salt = zero;
        saltlen = sizeof(zero);
    }
    hmac256(salt, saltlen, ikm, ikmlen, prk);
}

void hkdf_expand(const uint8_t *prk, const uint8_t *info, size_t infolen,
                 uint8_t *out, size_t outlen) {
    uint8_t t[SHA256_DIGEST];
    size_t tlen = 0, done = 0;
    uint8_t counter = 1;
    while (done < outlen) {
        hmac256_ctx c;
        hmac256_init(&c, prk, SHA256_DIGEST);
        if (tlen)
            hmac256_update(&c, t, tlen);
        hmac256_update(&c, info, infolen);
        hmac256_update(&c, &counter, 1);
        hmac256_final(&c, t);
        tlen = SHA256_DIGEST;
        size_t take = outlen - done;
        if (take > SHA256_DIGEST)
            take = SHA256_DIGEST;
        memcpy(out + done, t, take);
        done += take;
        counter++;
    }
}

/* HkdfLabel: uint16 length; opaque label<7..255> = "tls13 "+Label;
 *             opaque context<0..255> = Context */
/* Длина строки без libc: strlen в freestanding не гарантирован. */
static size_t cstr_len(const char *s) {
    size_t n = 0;
    while (s[n])
        n++;
    return n;
}

void hkdf_expand_label(const uint8_t *secret, const char *label,
                       const uint8_t *ctx, size_t ctxlen, uint8_t *out,
                       size_t outlen) {
    /* 2 (len) + 1 + 6 ("tls13 ") + label + 1 + ctx */
    uint8_t info[2 + 1 + 6 + 32 + 1 + 32];
    size_t llen = cstr_len(label);
    size_t n = 0;
    if (llen > 32 || ctxlen > 32 || outlen > 0xFFFF)
        return;
    be16_put(info + n, (uint16_t)outlen);
    n += 2;
    info[n++] = (uint8_t)(6 + llen);
    memcpy(info + n, "tls13 ", 6);
    n += 6;
    memcpy(info + n, label, llen);
    n += llen;
    info[n++] = (uint8_t)ctxlen;
    if (ctxlen) {
        memcpy(info + n, ctx, ctxlen);
        n += ctxlen;
    }
    hkdf_expand(secret, info, n, out, outlen);
}

void tls_derive_secret(const uint8_t *secret, const char *label,
                       const uint8_t *msg_hash, uint8_t out[SHA256_DIGEST]) {
    hkdf_expand_label(secret, label, msg_hash, SHA256_DIGEST, out,
                      SHA256_DIGEST);
}