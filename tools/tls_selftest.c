/* Хостовый самотест крипто-примитивов TLS 1.3.
 *
 * Собирается и запускается на хосте (scripts/tls_selftest.sh), чтобы ловить
 * ошибки в SHA-256/AES/GCM/X25519/HKDF за секунды, а не за прогон в QEMU.
 * В гостевую сборку не попадает.
 *
 * Векторы: FIPS 180-4 (SHA-256), RFC 4231 (HMAC), RFC 5869 (HKDF),
 * FIPS-197 (AES-128), NIST GCM, RFC 7748 (X25519).
 */

#include <stdio.h>
#include <string.h>

#include "../programs/web/tls_crypto.h"

static int failures;

static void hexdump(const char *tag, const uint8_t *p, size_t n) {
    printf("  %-10s", tag);
    for (size_t i = 0; i < n; i++)
        printf("%02x", p[i]);
    printf("\n");
}

static int check(const char *name, const uint8_t *got, const char *want_hex,
                 size_t n) {
    char buf[256];
    size_t pos = 0;
    for (size_t i = 0; i < n && pos + 2 < sizeof(buf); i++)
        pos += (size_t)snprintf(buf + pos, sizeof(buf) - pos, "%02x", got[i]);
    buf[pos] = 0;
    if (strcmp(buf, want_hex) == 0) {
        printf("ok   %s\n", name);
        return 0;
    }
    printf("FAIL %s\n  got  %s\n  want %s\n", name, buf, want_hex);
    failures++;
    return 1;
}

static void fromhex(const char *hex, uint8_t *out) {
    size_t n = strlen(hex) / 2;
    for (size_t i = 0; i < n; i++) {
        unsigned v;
        sscanf(hex + i * 2, "%2x", &v);
        out[i] = (uint8_t)v;
    }
}

/* ─── SHA-256 ────────────────────────────────────────────────────────────── */

static void test_sha256(void) {
    uint8_t d[32];

    sha256("", 0, d);
    check("sha256(\"\")", d,
          "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", 32);

    sha256("abc", 3, d);
    check("sha256(abc)", d,
          "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad", 32);

    const char *m = "abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
    sha256(m, strlen(m), d);
    check("sha256(448-bit)", d,
          "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1", 32);

    /* Тот же вектор, но по кускам — проверяем буферизацию update(). */
    sha256_ctx c;
    sha256_init(&c);
    for (size_t i = 0; i < strlen(m); i += 7) {
        size_t take = strlen(m) - i;
        if (take > 7)
            take = 7;
        sha256_update(&c, m + i, take);
    }
    sha256_final(&c, d);
    check("sha256(chunked)", d,
          "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1", 32);

    /* Миллион 'a' — много блоков. */
    sha256_init(&c);
    uint8_t a[1000];
    memset(a, 'a', sizeof(a));
    for (int i = 0; i < 1000; i++)
        sha256_update(&c, a, sizeof(a));
    sha256_final(&c, d);
    check("sha256(1e6 x 'a')", d,
          "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0", 32);
}

/* ─── HMAC-SHA256 ────────────────────────────────────────────────────────── */

static void test_hmac(void) {
    uint8_t d[32], key[131], data[64];
    memset(key, 0x0b, 20);
    hmac256(key, 20, "Hi There", 8, d);
    check("hmac RFC4231 #1", d,
          "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7", 32);

    hmac256((const uint8_t *)"Jefe", 4, "what do ya want for nothing?", 28, d);
    check("hmac RFC4231 #2", d,
          "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843", 32);

    /* RFC 4231 #6: ключ длиннее блока — сначала хешируется. */
    memset(key, 0xaa, 131);
    hmac256(key, 131,
            "Test Using Larger Than Block-Size Key - Hash Key First", 54, d);
    check("hmac RFC4231 #6", d,
          "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54", 32);

    memset(key, 0xaa, 131);
    memset(data, 0xdd, 50);
    hmac256(key, 131, data, 50, d);
    check("hmac key>block, 50xdd", d,
          "124c7d2385aa1743aaad12204e3464f06305fd1a6d291250fa564dceffab0c8a", 32);

    /* Ключ короче блока — ветка с дополнением нулями. */
    hmac256((const uint8_t *)"k", 1, "x", 1, d);
    check("hmac short key", d,
          "c38edc8815c8489f64738978f44008f8596345545f0baa68ef6fcf5c53e57189", 32);
}

/* ─── HKDF ───────────────────────────────────────────────────────────────── */

static void test_hkdf(void) {
    uint8_t ikm[22], salt[13], info[10], out[42], prk[32];
    memset(ikm, 0x0b, sizeof(ikm));
    for (int i = 0; i < 13; i++)
        salt[i] = (uint8_t)i;
    for (int i = 0; i < 10; i++)
        info[i] = (uint8_t)(0xf0 + i);

    hkdf_extract(salt, sizeof(salt), ikm, sizeof(ikm), prk);
    check("hkdf PRK (RFC5869 A.1)", prk,
          "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5", 32);

    hkdf_expand(prk, info, sizeof(info), out, sizeof(out));
    check("hkdf OKM (RFC5869 A.1)", out,
          "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf"
          "34007208d5b887185865", 42);

    /* A.3: salt и info пустые (salt = HashLen нулей), IKM = 0x0b * 22. */
    uint8_t zero[32];
    memset(zero, 0, sizeof(zero));
    hkdf_extract(NULL, 0, ikm, 22, prk);
    check("hkdf PRK (A.3)", prk,
          "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04", 32);
    hkdf_expand(prk, NULL, 0, out, sizeof(out));
    check("hkdf OKM (A.3)", out,
          "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d"
          "9d201395faa4b61a96c8", 42);
}

/* Метки TLS 1.3: кодирование HkdfLabel ("tls13 " + label). */
static void test_hkdf_label(void) {
    uint8_t secret[32], out[32];
    for (int i = 0; i < 32; i++)
        secret[i] = (uint8_t)i;

    hkdf_expand_label(secret, "key", NULL, 0, out, 16);
    check("expand_label key/16", out, "9c9783cf77ea32d44f369da41f19f3cc", 16);

    hkdf_expand_label(secret, "iv", NULL, 0, out, 12);
    check("expand_label iv/12", out, "2f41c846a431a163814bcd71", 12);

    hkdf_expand_label(secret, "finished", NULL, 0, out, 32);
    check("expand_label finished", out,
          "38bfb0a834fc61265acc278446da8b66db085dbf77c75210a53deb87a4cc7d0e", 32);

    uint8_t msg[3] = {'a', 'b', 'c'}, h[32];
    sha256(msg, 3, h);
    hkdf_expand_label(secret, "c hs traffic", h, 32, out, 32);
    check("expand_label c hs traffic", out,
          "c55cc98053c7e91ec8bd87e2f5771a05a83cb88a0057cace016d583064091216", 32);
}

/* ─── AES-128 ────────────────────────────────────────────────────────────── */

static void test_aes(void) {
    uint8_t key[16], pt[16], ct[16];
    aes128_key k;
    for (int i = 0; i < 16; i++) {
        key[i] = (uint8_t)i;
        pt[i] = (uint8_t)(i * 0x11);
    }
    aes128_setkey(&k, key);
    aes128_encrypt_block(&k, pt, ct);
    check("aes128 (FIPS-197)", ct, "69c4e0d86a7b0430d8cdb78070b4c55a", 16);

    /* Известный ключ/текст NIST SP 800-38A, F.1.1 (AES-128, single block). */
    fromhex("2b7e151628aed2a6abf7158809cf4f3c", key);
    fromhex("6bc1bee22e409f96e93d7e117393172a", pt);
    aes128_setkey(&k, key);
    aes128_encrypt_block(&k, pt, ct);
    check("aes128 (38A F.1.1)", ct, "3ad77bb40d7a3660a89ecaf32466ef97", 16);

    /* SP 800-38A F.1.2: второй блок той же серии. */
    fromhex("ae2d8a571e03ac9c9eb76fac45af8e51", pt);
    aes128_encrypt_block(&k, pt, ct);
    check("aes128 (38A F.1.2)", ct, "f5d3d58503b9699de785895a96fdbaaf", 16);
}

/* ─── GCM ────────────────────────────────────────────────────────────────── */

static void test_gcm(void) {
    uint8_t key[16], nonce[12], ct[64], tag[16], pt[64];

    /* NIST GCM test case 1: пустые AAD и plaintext. */
    memset(key, 0, sizeof(key));
    memset(nonce, 0, sizeof(nonce));
    aes128_gcm_encrypt(key, nonce, NULL, 0, NULL, 0, ct, tag);
    check("gcm case 1 tag", tag, "58e2fccefa7e3061367f1d57a4e7455a", 16);

    /* NIST GCM test case 2: 16 байт, AAD нет. */
    memset(pt, 0, 16);
    aes128_gcm_encrypt(key, nonce, NULL, 0, pt, 16, ct, tag);
    check("gcm case 2 ct", ct, "0388dace60b6a392f328c2b971b2fe78", 16);
    check("gcm case 2 tag", tag, "ab6e47d42cec13bdf53a67b21257bddf", 16);

    /* NIST GCM test case 3: plaintext 64 байта, AAD нет. */
    fromhex("feffe9928665731c6d6a8f9467308308", key);
    fromhex("cafebabefacedbaddecaf888", nonce);
    fromhex("d9313225f88406e5a55909c5aff5269a86a7a9531534f7da2e4c303d8a318a721"
            "c3c0c95956809532fcf0e2449a6b525b16aedf5aa0de657ba637b391aafd255",
            pt);
    aes128_gcm_encrypt(key, nonce, NULL, 0, pt, 64, ct, tag);
    check("gcm case 3 ct", ct,
          "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e"
          "21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985",
          64);
    check("gcm case 3 tag", tag, "4d5c2af327cd64a62cf35abd2ba6fab4", 16);

    /* NIST GCM test case 4: AAD 20 байт (неполный блок) + plaintext 60 байт. */
    uint8_t aad[20];
    fromhex("feedfacedeadbeeffeedfacedeadbeefabaddad2", aad);
    aes128_gcm_encrypt(key, nonce, aad, sizeof(aad), pt, 60, ct, tag);
    check("gcm case 4 ct", ct,
          "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e"
          "21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091", 60);
    check("gcm case 4 tag", tag, "5bc94fbc3221a5db94fae95ae7121a47", 16);

    /* Расшифровка обязана дать то же самое и отвергнуть испорченный тег. */
    uint8_t back[64];
    if (aes128_gcm_decrypt(key, nonce, aad, sizeof(aad), ct, 60, tag, back) != 0 ||
        memcmp(back, pt, 60) != 0) {
        printf("FAIL gcm decrypt roundtrip\n");
        failures++;
    } else {
        printf("ok   gcm decrypt roundtrip\n");
    }
    tag[0] ^= 1;
    if (aes128_gcm_decrypt(key, nonce, aad, sizeof(aad), ct, 60, tag, back) == 0) {
        printf("FAIL gcm bad tag accepted\n");
        failures++;
    } else {
        printf("ok   gcm bad tag rejected\n");
    }

    /* Невыровненные длины: 3 байта AAD и 17 байт данных. */
    uint8_t aad3[3], pt17[17];
    fromhex("abcdef", aad3);
    for (int i = 0; i < 17; i++)
        pt17[i] = (uint8_t)(i * 11 + 3);
    aes128_gcm_encrypt(key, nonce, aad3, 3, pt17, 17, ct, tag);
    uint8_t back17[17];
    if (aes128_gcm_decrypt(key, nonce, aad3, 3, ct, 17, tag, back17) != 0 ||
        memcmp(back17, pt17, 17) != 0) {
        printf("FAIL gcm odd lengths roundtrip\n");
        failures++;
    } else {
        printf("ok   gcm odd lengths roundtrip\n");
    }

    /* Табличный GHASH обязан совпадать с побитовым на всех 16 нибблах. */
    gcm_ctx c;
    gcm_init(&c, key, nonce);
    int bad = 0;
    for (int i = 0; i < 16; i++) {
        uint8_t v[AES_BLOCK];
        memset(v, 0, sizeof(v));
        v[0] = (uint8_t)(i << 4);
        gcm_ghash_ref(&c, v);
        if (be64_get(v) != c.mt[i] || be64_get(v + 8) != c.ml[i]) {
            printf("FAIL ghash table M[%d]\n", i);
            hexdump("ref", v, 16);
            bad = 1;
        }
    }
    if (bad)
        failures++;
    else
        printf("ok   ghash table == bitwise (M[0..15])\n");

    /* Случайные блоки: накопление через gcm_aad_update должно совпасть с
     * побитовым эталоном по всему AAD. */
    gcm_ctx d1, d2;
    gcm_init(&d1, key, nonce);
    gcm_init(&d2, key, nonce);
    uint8_t rnd[96];
    for (size_t i = 0; i < sizeof(rnd); i++)
        rnd[i] = (uint8_t)(i * 37 + 11);
    /* d1 — табличный, d2 — вручную через эталонный GHASH. Длина кратна
     * 16, иначе d1 оставит неполный хвост невыгруженным. */
    gcm_aad_update(&d1, rnd, sizeof(rnd));
    for (size_t off = 0; off < sizeof(rnd); off += 16) {
        uint8_t blk[AES_BLOCK];
        size_t take = sizeof(rnd) - off < 16 ? sizeof(rnd) - off : 16;
        memset(blk, 0, sizeof(blk));
        memcpy(blk, rnd + off, take);
        for (int i = 0; i < AES_BLOCK; i++)
            blk[i] ^= d2.buf[i];
        gcm_ghash_ref(&d2, blk);
        memcpy(d2.buf, blk, AES_BLOCK);
    }
    if (memcmp(d1.buf, d2.buf, 16) != 0) {
        printf("FAIL ghash block accumulator\n");
        hexdump("table", d1.buf, 16);
        hexdump("ref", d2.buf, 16);
        failures++;
    } else {
        printf("ok   ghash block accumulator == bitwise\n");
    }
}

/* ─── X25519 ─────────────────────────────────────────────────────────────── */

static void test_x25519(void) {
    uint8_t s[32], u[32], out[32], want[32];
    fromhex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4", s);
    fromhex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c", u);
    x25519_base(out, s, u);
    check("x25519 (RFC7748 5.2)", out,
          "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552", 32);

    /* RFC 7748 6.1: Диффи-Хеллман Алисы и Боба. */
    uint8_t apriv[32], bpriv[32], apub[32], bpub[32], shared[2][32];
    fromhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a",
            apriv);
    fromhex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb",
            bpriv);
    x25519_public(apub, apriv);
    check("x25519 alice pub", apub,
          "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a", 32);
    x25519_public(bpub, bpriv);
    check("x25519 bob pub", bpub,
          "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f", 32);
    if (x25519_shared(shared[0], apriv, bpub) != 0) {
        printf("FAIL x25519 shared a*b\n");
        failures++;
    }
    x25519_shared(shared[1], bpriv, apub);
    check("x25519 shared (RFC7748 6.1)", shared[0],
          "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742", 32);
    if (memcmp(shared[0], shared[1], 32) != 0) {
        printf("FAIL x25519 shared mismatch\n");
        failures++;
    } else {
        printf("ok   x25519 shared agree\n");
    }
    (void)want;

    /* Точка малого порядка (нулевая) должна отвергаться. */
    uint8_t zero32[32];
    memset(zero32, 0, 32);
    if (x25519_shared(out, apriv, zero32) == 0) {
        printf("FAIL x25519 accepted zero point\n");
        failures++;
    } else {
        printf("ok   x25519 rejects zero point\n");
    }
}

int main(void) {
    printf("=== TLS 1.3 primitives selftest ===\n");
    test_sha256();
    test_hmac();
    test_hkdf();
    test_hkdf_label();
    test_aes();
    test_gcm();
    test_x25519();
    if (failures) {
        printf("=== %d FAILURE(S) ===\n", failures);
        return 1;
    }
    printf("=== ALL PASS ===\n");
    return 0;
}
