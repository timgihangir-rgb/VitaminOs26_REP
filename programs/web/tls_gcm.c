/* AES-128 (FIPS-197) и режим GCM — только шифрование.
 *
 * GHASH сделан двумя способами:
 *   ghash_mul()   — побитовый, эталон для самотеста;
 *   ghash_block() — табличный (16 элементов по 4 бита), рабочий: побитовый
 *                   вариант на TCG даёт ~300 операций на байт и на странице в
 *                   100 КБ уходил бы в десятки секунд.
 * Оба обязаны давать один результат — это проверяется tools/tls_selftest.c.
 */

#include "tls_crypto.h"

/* ─── AES-128 ────────────────────────────────────────────────────────────── */

static const uint8_t SBOX[256] = {
    0x63,0x7c,0x77,0x7b,0xf2,0x6b,0x6f,0xc5,0x30,0x01,0x67,0x2b,0xfe,0xd7,0xab,0x76,
    0xca,0x82,0xc9,0x7d,0xfa,0x59,0x47,0xf0,0xad,0xd4,0xa2,0xaf,0x9c,0xa4,0x72,0xc0,
    0xb7,0xfd,0x93,0x26,0x36,0x3f,0xf7,0xcc,0x34,0xa5,0xe5,0xf1,0x71,0xd8,0x31,0x15,
    0x04,0xc7,0x23,0xc3,0x18,0x96,0x05,0x9a,0x07,0x12,0x80,0xe2,0xeb,0x27,0xb2,0x75,
    0x09,0x83,0x2c,0x1a,0x1b,0x6e,0x5a,0xa0,0x52,0x3b,0xd6,0xb3,0x29,0xe3,0x2f,0x84,
    0x53,0xd1,0x00,0xed,0x20,0xfc,0xb1,0x5b,0x6a,0xcb,0xbe,0x39,0x4a,0x4c,0x58,0xcf,
    0xd0,0xef,0xaa,0xfb,0x43,0x4d,0x33,0x85,0x45,0xf9,0x02,0x7f,0x50,0x3c,0x9f,0xa8,
    0x51,0xa3,0x40,0x8f,0x92,0x9d,0x38,0xf5,0xbc,0xb6,0xda,0x21,0x10,0xff,0xf3,0xd2,
    0xcd,0x0c,0x13,0xec,0x5f,0x97,0x44,0x17,0xc4,0xa7,0x7e,0x3d,0x64,0x5d,0x19,0x73,
    0x60,0x81,0x4f,0xdc,0x22,0x2a,0x90,0x88,0x46,0xee,0xb8,0x14,0xde,0x5e,0x0b,0xdb,
    0xe0,0x32,0x3a,0x0a,0x49,0x06,0x24,0x5c,0xc2,0xd3,0xac,0x62,0x91,0x95,0xe4,0x79,
    0xe7,0xc8,0x37,0x6d,0x8d,0xd5,0x4e,0xa9,0x6c,0x56,0xf4,0xea,0x65,0x7a,0xae,0x08,
    0xba,0x78,0x25,0x2e,0x1c,0xa6,0xb4,0xc6,0xe8,0xdd,0x74,0x1f,0x4b,0xbd,0x8b,0x8a,
    0x70,0x3e,0xb5,0x66,0x48,0x03,0xf6,0x0e,0x61,0x35,0x57,0xb9,0x86,0xc1,0x1d,0x9e,
    0xe1,0xf8,0x98,0x11,0x69,0xd9,0x8e,0x94,0x9b,0x1e,0x87,0xe9,0xce,0x55,0x28,0xdf,
    0x8c,0xa1,0x89,0x0d,0xbf,0xe6,0x42,0x68,0x41,0x99,0x2d,0x0f,0xb0,0x54,0xbb,0x16
};

static uint8_t xtime(uint8_t x) {
    return (uint8_t)((x << 1) ^ ((x & 0x80) ? 0x1b : 0x00));
}

void aes128_setkey(aes128_key *k, const uint8_t key[16]) {
    memcpy(k->rk, key, 16);
    uint8_t rcon = 1;
    for (int i = 4; i < AES128_ROUND_KEYS * 4; i++) {
        uint8_t t[4];
        memcpy(t, k->rk + (i - 1) * 4, 4);
        if (i % 4 == 0) {
            uint8_t tmp = t[0];
            t[0] = (uint8_t)(SBOX[t[1]] ^ rcon);
            t[1] = SBOX[t[2]];
            t[2] = SBOX[t[3]];
            t[3] = SBOX[tmp];
            rcon = xtime(rcon);
        }
        for (int j = 0; j < 4; j++)
            k->rk[i * 4 + j] = (uint8_t)(k->rk[(i - 4) * 4 + j] ^ t[j]);
    }
}

/* Состояние хранится колонками: s[r + 4*c] — строка r, колонка c. */
static uint8_t gmul2(uint8_t a) {
    return (uint8_t)((a << 1) ^ ((a & 0x80) ? 0x1b : 0x00));
}

void aes128_encrypt_block(const aes128_key *k, const uint8_t in[16],
                          uint8_t out[16]) {
    const uint8_t *rk = k->rk;
    uint8_t s[16];

    memcpy(s, in, 16);
    for (int i = 0; i < 16; i++)
        s[i] ^= rk[i];

    for (int round = 1; round < AES128_ROUND_KEYS; round++) {
        /* SubBytes */
        for (int i = 0; i < 16; i++)
            s[i] = SBOX[s[i]];

        /* ShiftRows: строка r сдвигается влево на r. */
        uint8_t t[16];
        for (int c = 0; c < 4; c++)
            for (int r = 0; r < 4; r++)
                t[r + 4 * c] = s[r + 4 * ((c + r) & 3)];

        /* MixColumns (в последнем раунде её нет) */
        if (round != AES128_ROUND_KEYS - 1) {
            for (int c = 0; c < 4; c++) {
                uint8_t a0 = t[0 + 4 * c], a1 = t[1 + 4 * c];
                uint8_t a2 = t[2 + 4 * c], a3 = t[3 + 4 * c];
                s[0 + 4 * c] = (uint8_t)(gmul2(a0) ^ (gmul2(a1) ^ a1) ^ a2 ^ a3);
                s[1 + 4 * c] = (uint8_t)(a0 ^ gmul2(a1) ^ (gmul2(a2) ^ a2) ^ a3);
                s[2 + 4 * c] = (uint8_t)(a0 ^ a1 ^ gmul2(a2) ^ (gmul2(a3) ^ a3));
                s[3 + 4 * c] = (uint8_t)((gmul2(a0) ^ a0) ^ a1 ^ a2 ^ gmul2(a3));
            }
        } else {
            memcpy(s, t, 16);
        }

        /* AddRoundKey */
        const uint8_t *rkp = rk + round * 16;
        for (int i = 0; i < 16; i++)
            s[i] ^= rkp[i];
    }
    memcpy(out, s, 16);
}

/* ─── GCM ────────────────────────────────────────────────────────────────── */

/* Полином GHASH: x^128 + x^7 + x^2 + x + 1. Сдвиг на 1 бит влево, если
 * вылетевший бит равен 1, добавляет 0xE1 в старший байт. */
static void ghash_mul(uint8_t x[AES_BLOCK], const uint8_t y[AES_BLOCK]) {
    uint8_t z[AES_BLOCK];
    memset(z, 0, sizeof(z));
    for (int i = 0; i < 128; i++) {
        if ((y[i >> 3] >> (7 - (i & 7))) & 1)
            for (int b = 0; b < AES_BLOCK; b++)
                z[b] ^= x[b];
        int lsb = x[15] & 1;
        for (int b = 15; b > 0; b--)
            x[b] = (uint8_t)((x[b] >> 1) | ((x[b - 1] & 1) << 7));
        x[0] >>= 1;
        if (lsb)
            x[0] ^= 0xe1;
    }
    memcpy(x, z, AES_BLOCK);
}

/* Умножение аккумулятора на y^4 в соглашении спецификации GCM: строка
 * сдвигается вправо на 4 бита, а четыре вылетевших бита дают редукцию.
 * Бит j выходит наружу на j шагов позже первого, поэтому его вклад в 0xE1
 * успевает сдвинуться вправо на 3-j бит. */
static uint64_t gcm_r4[16];

static void gcm_r4_init(void) {
    for (int v = 0; v < 16; v++) {
        uint64_t r = 0;
        for (int j = 0; j < 4; j++)
            if (v & (1 << j))
                r ^= (0xE1ULL << 56) >> (3 - j);
        gcm_r4[v] = r;
    }
}

/* M[v] = v·H, где ниббл стоит в старших битах первого байта: старший бит
 * ниббла получает степень y^0, младший — y^3. */
static void gcm_table_init(gcm_ctx *c, const uint8_t h[AES_BLOCK]) {
    gcm_r4_init();
    for (int i = 0; i < 16; i++) {
        uint8_t v[AES_BLOCK];
        memset(v, 0, sizeof(v));
        v[0] = (uint8_t)(i << 4);
        ghash_mul(v, h);
        c->mt[i] = be64_get(v);
        c->ml[i] = be64_get(v + 8);
    }
    memcpy(c->hbytes, h, AES_BLOCK);
}

/* c->buf = c->buf · H. Схема Хорнера: нибблы идут от конца строки к
 * началу, на каждом шаге R = R·y^4 ⊕ M[N]. */
static void ghash_block(gcm_ctx *c) {
    uint64_t rh = 0, rl = 0;
    for (int j = 0; j < 32; j++) {
        /* Ниббл k покрывает позиции 4k..4k+3, то есть байт k>>1;
         * k чётный — старший ниббл этого байта. */
        uint32_t k = (uint32_t)(31 - j);
        uint32_t b = c->buf[k >> 1];
        uint32_t nib = (k & 1) == 0 ? (b >> 4) : (b & 0xF);

        uint32_t drop = (uint32_t)(rl & 0xF);    /* уходит при сдвиге вправо */
        rl = (rl >> 4) | (rh << 60);
        rh = (rh >> 4) ^ gcm_r4[drop];

        rh ^= c->mt[nib];
        rl ^= c->ml[nib];
    }
    be64_put(c->buf, rh);
    be64_put(c->buf + 8, rl);
}

/* x = x · H побитово — эталон для проверки табличного варианта. */
void gcm_ghash_ref(const gcm_ctx *c, uint8_t x[AES_BLOCK]) {
    ghash_mul(x, c->hbytes);
}

void gcm_init(gcm_ctx *c, const uint8_t key[16], const uint8_t nonce[12]) {
    uint8_t zero[AES_BLOCK];
    memset(zero, 0, sizeof(zero));
    aes128_setkey(&c->aes, key);
    aes128_encrypt_block(&c->aes, zero, c->hbytes); /* H = AES_K(0) */
    gcm_table_init(c, c->hbytes);
    /* J0 = IV || 0^31 || 1 — для 12-байтного nonce (в TLS 1.3 всегда так). */
    memcpy(c->j0, nonce, 12);
    c->j0[12] = 0;
    c->j0[13] = 0;
    c->j0[14] = 0;
    c->j0[15] = 1;
    /* CTR начинается с J0+1: сам J0 занят под E_K(J0) в теге. */
    memcpy(c->ctr, c->j0, sizeof(c->ctr));
    be32_put(c->ctr + 12, be32_get(c->ctr + 12) + 1);
    c->aadlen = 0;
    c->ctlen = 0;
    c->buflen = 0;
    c->phase = 0;
    memset(c->buf, 0, sizeof(c->buf));
}

static void gcm_ctr_xor(gcm_ctx *c, const uint8_t *in, uint8_t *out, size_t len) {
    uint8_t ks[AES_BLOCK];
    for (size_t off = 0; off < len; off += AES_BLOCK) {
        aes128_encrypt_block(&c->aes, c->ctr, ks);
        be32_put(c->ctr + 12, be32_get(c->ctr + 12) + 1);
        size_t take = len - off;
        if (take > AES_BLOCK)
            take = AES_BLOCK;
        for (size_t i = 0; i < take; i++)
            out[off + i] = (uint8_t)(in[off + i] ^ ks[i]);
    }
}

/* Данные копятся в c->buf поверх накопленного аккумулятора, поэтому
 * складываем их XOR-ом: ghash_block() считает (acc ⊕ block)·H. */
static void gcm_ghash_data(gcm_ctx *c, const uint8_t *data, size_t len) {
    while (len) {
        size_t take = AES_BLOCK - c->buflen;
        if (take > len)
            take = len;
        for (size_t i = 0; i < take; i++)
            c->buf[c->buflen + i] ^= data[i];
        c->buflen += take;
        data += take;
        len -= take;
        if (c->buflen == AES_BLOCK) {
            ghash_block(c);
            c->buflen = 0;
        }
    }
}

/* Завершить неполный блок zero-padding'ом: недостающие байты блока уже
 * неявно лежат в c->buf (там аккумулятор, а данных за buflen не писали),
 * поэтому достаточно выполнить домножение. */
static void ghash_flush(gcm_ctx *c) {
    if (c->buflen) {
        ghash_block(c);
        c->buflen = 0;
    }
}

void gcm_aad_update(gcm_ctx *c, const void *data, size_t len) {
    c->aadlen += len;
    gcm_ghash_data(c, (const uint8_t *)data, len);
}

void gcm_encrypt_update(gcm_ctx *c, const void *pt, size_t len, uint8_t *out) {
    if (c->phase == 0) {
        ghash_flush(c); /* неполный блок AAD дополняем нулями */
        c->phase = 1;
    }
    c->ctlen += len;
    gcm_ctr_xor(c, (const uint8_t *)pt, out, len);
    gcm_ghash_data(c, out, len);
}

void gcm_decrypt_update(gcm_ctx *c, const void *ct, size_t len, uint8_t *out) {
    if (c->phase == 0) {
        ghash_flush(c);
        c->phase = 1;
    }
    c->ctlen += len;
    gcm_ghash_data(c, (const uint8_t *)ct, len);
    gcm_ctr_xor(c, (const uint8_t *)ct, out, len);
}

static void gcm_tag(gcm_ctx *c, uint8_t tag[GCM_TAG]) {
    uint8_t len[AES_BLOCK];
    ghash_flush(c); /* неполный блок шифротекста дополняем нулями */
    be64_put(len, c->aadlen * 8);
    be64_put(len + 8, c->ctlen * 8);
    gcm_ghash_data(c, len, AES_BLOCK);
    uint8_t s[AES_BLOCK];
    aes128_encrypt_block(&c->aes, c->j0, s);
    for (int i = 0; i < GCM_TAG; i++)
        tag[i] = (uint8_t)(c->buf[i] ^ s[i]);
}

/* _final по const ctx: считаем тег на копии — состояние не портим. */
void gcm_encrypt_final(const gcm_ctx *c, uint8_t tag[GCM_TAG]) {
    gcm_ctx tmp;
    memcpy(&tmp, c, sizeof(tmp));
    gcm_tag(&tmp, tag);
}

int gcm_decrypt_final(const gcm_ctx *c, const uint8_t tag[GCM_TAG]) {
    uint8_t want[GCM_TAG];
    gcm_encrypt_final(c, want);
    uint8_t diff = 0;
    for (int i = 0; i < GCM_TAG; i++)
        diff |= (uint8_t)(want[i] ^ tag[i]);
    return diff == 0 ? 0 : -1;
}

void aes128_gcm_encrypt(const uint8_t key[16], const uint8_t nonce[12],
                        const uint8_t *aad, size_t aadlen, const uint8_t *pt,
                        size_t ptlen, uint8_t *ct, uint8_t tag[GCM_TAG]) {
    gcm_ctx c;
    gcm_init(&c, key, nonce);
    gcm_aad_update(&c, aad, aadlen);
    gcm_encrypt_update(&c, pt, ptlen, ct);
    gcm_encrypt_final(&c, tag);
}

int aes128_gcm_decrypt(const uint8_t key[16], const uint8_t nonce[12],
                       const uint8_t *aad, size_t aadlen, const uint8_t *ct,
                       size_t ctlen, const uint8_t tag[GCM_TAG], uint8_t *pt) {
    gcm_ctx c;
    gcm_init(&c, key, nonce);
    gcm_aad_update(&c, aad, aadlen);
    gcm_decrypt_update(&c, ct, ctlen, pt);
    return gcm_decrypt_final(&c, tag);
}