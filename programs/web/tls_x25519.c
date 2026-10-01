/* X25519 (RFC 7748) — обмен ключами ECDHE для TLS 1.3.
 *
 * Поле: 5 лимбов по 51 биту, произведения считаются в unsigned __int128
 * (x86_64 это умеет и во freestanding). Проверяется на хосте векторами
 * RFC 7748 в tools/tls_selftest.c.
 */

#include "tls_crypto.h"

typedef uint64_t fe[5];

#define M51 0x7ffffffffffffULL /* 2^51 - 1 */

static void fe_zero(fe h) {
    for (int i = 0; i < 5; i++)
        h[i] = 0;
}

static void fe_one(fe h) {
    fe_zero(h);
    h[0] = 1;
}

static void fe_copy(fe h, const fe f) {
    for (int i = 0; i < 5; i++)
        h[i] = f[i];
}

static void fe_add(fe h, const fe f, const fe g) {
    for (int i = 0; i < 5; i++)
        h[i] = f[i] + g[i];
}

/* h = f - g с добавлением 2p, чтобы не уйти в отрицательные числа. */
static void fe_sub(fe h, const fe f, const fe g) {
    h[0] = f[0] + 0xfffffffffffdaULL - g[0];
    h[1] = f[1] + 0xffffffffffffeULL - g[1];
    h[2] = f[2] + 0xffffffffffffeULL - g[2];
    h[3] = f[3] + 0xffffffffffffeULL - g[3];
    h[4] = f[4] + 0xffffffffffffeULL - g[4];
}

static void fe_carry(fe h) {
    uint64_t c;
    c = h[0] >> 51; h[0] &= M51; h[1] += c;
    c = h[1] >> 51; h[1] &= M51; h[2] += c;
    c = h[2] >> 51; h[2] &= M51; h[3] += c;
    c = h[3] >> 51; h[3] &= M51; h[4] += c;
    c = h[4] >> 51; h[4] &= M51; h[0] += c * 19;
    c = h[0] >> 51; h[0] &= M51; h[1] += c;
}

static void fe_mul(fe h, const fe f, const fe g) {
    typedef unsigned __int128 u128;
    uint64_t f0 = f[0], f1 = f[1], f2 = f[2], f3 = f[3], f4 = f[4];
    uint64_t g0 = g[0], g1 = g[1], g2 = g[2], g3 = g[3], g4 = g[4];
    uint64_t g1_19 = 19 * g1, g2_19 = 19 * g2, g3_19 = 19 * g3, g4_19 = 19 * g4;
    u128 r0, r1, r2, r3, r4;
    uint64_t c;

    r0 = (u128)f0 * g0 + (u128)f1 * g4_19 + (u128)f2 * g3_19 +
         (u128)f3 * g2_19 + (u128)f4 * g1_19;
    r1 = (u128)f0 * g1 + (u128)f1 * g0 + (u128)f2 * g4_19 +
         (u128)f3 * g3_19 + (u128)f4 * g2_19;
    r2 = (u128)f0 * g2 + (u128)f1 * g1 + (u128)f2 * g0 +
         (u128)f3 * g4_19 + (u128)f4 * g3_19;
    r3 = (u128)f0 * g3 + (u128)f1 * g2 + (u128)f2 * g1 +
         (u128)f3 * g0 + (u128)f4 * g4_19;
    r4 = (u128)f0 * g4 + (u128)f1 * g3 + (u128)f2 * g2 +
         (u128)f3 * g1 + (u128)f4 * g0;

    c = (uint64_t)(r0 >> 51); h[0] = (uint64_t)r0 & M51; r1 += c;
    c = (uint64_t)(r1 >> 51); h[1] = (uint64_t)r1 & M51; r2 += c;
    c = (uint64_t)(r2 >> 51); h[2] = (uint64_t)r2 & M51; r3 += c;
    c = (uint64_t)(r3 >> 51); h[3] = (uint64_t)r3 & M51; r4 += c;
    c = (uint64_t)(r4 >> 51); h[4] = (uint64_t)r4 & M51; h[0] += c * 19;

    c = h[0] >> 51; h[0] &= M51; h[1] += c;
    c = h[1] >> 51; h[1] &= M51; h[2] += c;
    c = h[2] >> 51; h[2] &= M51; h[3] += c;
    c = h[3] >> 51; h[3] &= M51; h[4] += c;
    c = h[4] >> 51; h[4] &= M51; h[0] += c * 19;
}

static void fe_sq(fe h, const fe f) {
    fe_mul(h, f, f);
}

static void fe_mul121666(fe h, const fe f) {
    typedef unsigned __int128 u128;
    uint64_t c;
    /* f[i] < 2^52, произведение < 2^69: держим его в u128 и лишь затем
     * снимаем перенос — иначе приведение к uint64_t до сдвига теряет
     * старшие 5 бит. */
    u128 t0 = (u128)f[0] * 121666;
    u128 t1 = (u128)f[1] * 121666;
    u128 t2 = (u128)f[2] * 121666;
    u128 t3 = (u128)f[3] * 121666;
    u128 t4 = (u128)f[4] * 121666;
    c = (uint64_t)(t0 >> 51); h[0] = (uint64_t)t0 & M51; t1 += c;
    c = (uint64_t)(t1 >> 51); h[1] = (uint64_t)t1 & M51; t2 += c;
    c = (uint64_t)(t2 >> 51); h[2] = (uint64_t)t2 & M51; t3 += c;
    c = (uint64_t)(t3 >> 51); h[3] = (uint64_t)t3 & M51; t4 += c;
    c = (uint64_t)(t4 >> 51); h[4] = (uint64_t)t4 & M51; h[0] += c * 19;
}

static void fe_frombytes(fe h, const uint8_t s[32]) {
    uint64_t w[4];
    for (int i = 0; i < 4; i++) {
        uint64_t v = 0;
        for (int j = 7; j >= 0; j--)
            v = (v << 8) | s[i * 8 + j];
        w[i] = v;
    }
    h[0] = w[0] & M51;
    h[1] = ((w[0] >> 51) | (w[1] << 13)) & M51;
    h[2] = ((w[1] >> 38) | (w[2] << 26)) & M51;
    h[3] = ((w[2] >> 25) | (w[3] << 39)) & M51;
    h[4] = (w[3] >> 12) & M51; /* бит 255 отброшен — он всегда 1 в U */
}

static void fe_tobytes(uint8_t s[32], const fe f) {
    uint64_t t[5], q, c;
    fe_copy(t, f);

    /* Сначала зануляем бит 255: пока t >= 2^255, это эквивалентно t - 2^255. */
    c = t[0] >> 51; t[0] &= M51; t[1] += c;
    c = t[1] >> 51; t[1] &= M51; t[2] += c;
    c = t[2] >> 51; t[2] &= M51; t[3] += c;
    c = t[3] >> 51; t[3] &= M51; t[4] += c;
    c = t[4] >> 51; t[4] &= M51; t[0] += c * 19;

    /* Теперь q = 1, если t >= p (p = 2^255-19), иначе 0; добавляем 19*q,
     * то есть либо оставляем t, либо заменяем t - p. */
    q = (t[0] + 19) >> 51;
    q = (t[1] + q) >> 51;
    q = (t[2] + q) >> 51;
    q = (t[3] + q) >> 51;
    q = (t[4] + q) >> 51;
    t[0] += 19 * q;

    c = t[0] >> 51; t[0] &= M51; t[1] += c;
    c = t[1] >> 51; t[1] &= M51; t[2] += c;
    c = t[2] >> 51; t[2] &= M51; t[3] += c;
    c = t[3] >> 51; t[3] &= M51; t[4] += c;
    t[4] &= M51;

    uint64_t w[4];
    w[0] = t[0] | (t[1] << 51);
    w[1] = (t[1] >> 13) | (t[2] << 38);
    w[2] = (t[2] >> 26) | (t[3] << 25);
    w[3] = (t[3] >> 39) | (t[4] << 12);
    for (int i = 0; i < 4; i++)
        for (int j = 0; j < 8; j++)
            s[i * 8 + j] = (uint8_t)(w[i] >> (8 * j));
}

static void fe_cswap(fe a, fe b, uint64_t swap) {
    uint64_t mask = 0 - swap;
    for (int i = 0; i < 5; i++) {
        uint64_t x = mask & (a[i] ^ b[i]);
        a[i] ^= x;
        b[i] ^= x;
    }
}

/* r = f^(p-2) = f^-1 по малой теореме Ферма. Показатель p-2 = 2^255-21
 * задан явно (little-endian) и обходится обычным square-and-multiply:
 * цепочка из кубических возведений короче, но её легко перепутать, а
 * ошибка здесь молча ломает весь обмен ключами. */
static void fe_invert(fe r, const fe f) {
    static const uint8_t e[32] = {
        0xeb, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff
    };
    fe g;
    /* r может совпадать с f (в лестнице так и есть), поэтому вход надо
     * скопировать до fe_one(r). */
    fe_copy(g, f);
    fe_one(r);
    for (int i = 254; i >= 0; i--) {
        fe_sq(r, r);
        if ((e[i >> 3] >> (i & 7)) & 1)
            fe_mul(r, r, g);
    }
}

/* Лестница Монтгомери из RFC 7748 (псевдокод 5), развёрнутая в цикл. */
static void x25519_scalarmult(uint8_t out[32], const uint8_t scalar[32],
                              const uint8_t point[32]) {
    uint8_t k[32];
    fe x1, x2, z2, x3, z3;
    fe a, aa, b, bb, e, c, d, da, cb, t1, t2;
    uint32_t swap = 0;
    int pos;

    memcpy(k, scalar, 32);
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;

    fe_frombytes(x1, point);
    fe_one(x2);
    fe_zero(z2);
    fe_copy(x3, x1);
    fe_one(z3);

    for (pos = 254; pos >= 0; --pos) {
        uint32_t bit = (uint32_t)((k[pos >> 3] >> (pos & 7)) & 1);
        swap ^= bit;
        fe_cswap(x2, x3, swap);
        fe_cswap(z2, z3, swap);
        swap = bit;

        fe_add(a, x2, z2);
        fe_sq(aa, a);
        fe_sub(b, x2, z2);
        fe_sq(bb, b);
        fe_sub(e, aa, bb);
        fe_add(c, x3, z3);
        fe_sub(d, x3, z3);
        fe_mul(da, d, a);
        fe_mul(cb, c, b);

        fe_add(t1, da, cb);
        fe_sq(x3, t1);
        fe_sub(t1, da, cb);
        fe_sq(t1, t1);
        fe_mul(z3, x1, t1);

        fe_mul(x2, aa, bb);
        /* z2 = E·(BB + 121666·E). Через a24=121665 это E·(AA + 121665·E):
         * E·(BB+121666E) = E·(AA - E + 121666E) = E·(AA + 121665E). */
        fe_mul121666(t1, e);
        fe_add(t2, bb, t1);
        fe_mul(z2, e, t2);
    }
    fe_cswap(x2, x3, swap);
    fe_cswap(z2, z3, swap);

    fe_invert(z2, z2);
    fe_mul(x2, x2, z2);
    fe_tobytes(out, x2);
}

void x25519_base(uint8_t out[32], const uint8_t scalar[32],
                 const uint8_t point[32]) {
    x25519_scalarmult(out, scalar, point);
}

int x25519_shared(uint8_t out[32], const uint8_t scalar[32],
                  const uint8_t point[32]) {
    int nonzero = 0;
    x25519_scalarmult(out, scalar, point);
    /* RFC 7748: результат не должен быть нулевым (точка малого порядка). */
    for (int i = 0; i < 32; i++)
        nonzero |= out[i];
    return nonzero ? 0 : -1;
}

void x25519_public(uint8_t out[32], const uint8_t scalar[32]) {
    static const uint8_t base[32] = {9};
    x25519_scalarmult(out, scalar, base);
}
