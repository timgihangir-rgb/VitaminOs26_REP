/* TLS 1.3 клиент — реализация (см. tls_client.h).
 *
 * Поддерживается только TLS_AES_128_GCM_SHA256 и x25519. Сертификат не
 * проверяется. ChangeCipherSpec игнорируется (совместимость с middlebox).
 * NewSessionTicket (пост-хендшейковый handshake) пропускается.
 */

#include "tls_client.h"

#define REC_HANDSHAKE 22
#define REC_CCS       20
#define REC_ALERT     21
#define REC_APPDATA   23

/* ─── мелкие утилиты (freestanding: без libc-строк) ──────────────────────── */

static size_t s_len(const char *s) {
    size_t n = 0;
    while (s[n])
        n++;
    return n;
}

static void s_copy(char *d, const char *s, size_t n) {
    size_t i = 0;
    for (; i + 1 < n && s[i]; i++)
        d[i] = s[i];
    d[i] = 0;
}

static void put_u24(uint8_t *p, uint32_t v) {
    p[0] = (uint8_t)(v >> 16);
    p[1] = (uint8_t)(v >> 8);
    p[2] = (uint8_t)v;
}

static void set_err(tls_conn *c, const char *msg) {
    s_copy(c->err, msg, sizeof(c->err));
}

const char *tls_error(const tls_conn *c) { return c->err; }

static void tls_random(tls_conn *c, uint8_t *out, size_t len) {
    c->tr.random(c->tr.ctx, out, len);
}

/* ─── транспорт ──────────────────────────────────────────────────────────── */

static int send_all(tls_conn *c, const uint8_t *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        int n = c->tr.send(c->tr.ctx, buf + off, len - off);
        if (n <= 0)
            return -1;
        off += (size_t)n;
    }
    return 0;
}

/* 1 — успех, 0 — EOF, -1 — ошибка. */
static int recv_exact(tls_conn *c, uint8_t *buf, size_t len) {
    size_t off = 0;
    while (off < len) {
        int n = c->tr.recv(c->tr.ctx, buf + off, len - off);
        if (n == 0)
            return 0;
        if (n < 0)
            return -1;
        off += (size_t)n;
    }
    return 1;
}

/* ─── запись/чтение TLS-записей ─────────────────────────────────────────── */

static int send_record(tls_conn *c, uint8_t type, const uint8_t *data,
                       size_t len) {
    uint8_t hdr[5];
    hdr[0] = type;
    hdr[1] = 0x03;
    hdr[2] = 0x03;
    be16_put(hdr + 3, (uint16_t)len);
    if (send_all(c, hdr, 5) < 0)
        return -1;
    if (len && send_all(c, data, len) < 0)
        return -1;
    return 0;
}

/* Собирает nonce = iv XOR seq (seq — в последних 8 байтах, big-endian). */
static void make_nonce(uint8_t nonce[12], const uint8_t iv[12], uint64_t seq) {
    memcpy(nonce, iv, 12);
    for (int i = 0; i < 8; i++)
        nonce[11 - i] ^= (uint8_t)(seq >> (8 * i));
}

static int send_enc_record(tls_conn *c, uint8_t inner_type,
                           const uint8_t *data, size_t len,
                           const uint8_t key[16], const uint8_t iv[12],
                           uint64_t *seq) {
    size_t n = len + 1; /* + внутренний тип контента */
    memcpy(c->plain, data, len);
    c->plain[len] = inner_type;

    uint8_t hdr[5];
    hdr[0] = REC_APPDATA;
    hdr[1] = 0x03;
    hdr[2] = 0x03;
    be16_put(hdr + 3, (uint16_t)(n + GCM_TAG));

    uint8_t nonce[12], tag[GCM_TAG];
    make_nonce(nonce, iv, *seq);
    aes128_gcm_encrypt(key, nonce, hdr, 5, c->plain, n, c->plain, tag);
    (*seq)++;

    if (send_all(c, hdr, 5) < 0)
        return -1;
    if (send_all(c, c->plain, n) < 0)
        return -1;
    if (send_all(c, tag, GCM_TAG) < 0)
        return -1;
    return 0;
}

/* Читает одну запись, пропуская CCS. >0 — длина payload (в c->rec),
 * 0 — EOF, -1 — ошибка. Тип записи — в *type, заголовок — в c->rechdr. */
static int read_record(tls_conn *c, int *type) {
    for (;;) {
        uint8_t hdr[5];
        int r = recv_exact(c, hdr, 5);
        if (r <= 0)
            return r;
        uint8_t t = hdr[0];
        size_t l = be16_get(hdr + 3);
        if (l > sizeof(c->rec))
            return -1;
        if (l) {
            r = recv_exact(c, c->rec, l);
            if (r <= 0)
                return r;
        }
        if (t == REC_CCS)
            continue;
        memcpy(c->rechdr, hdr, 5);
        *type = t;
        return (int)l;
    }
}

/* Расшифровывает c->rec (len байт) серверным ключом. Возвращает внутренний
 * тип контента (>=0), -1 — ошибка. Результат — в c->plain. */
static int decrypt_record(tls_conn *c, size_t len) {
    if (len < GCM_TAG)
        return -1;
    size_t ctlen = len - GCM_TAG;
    uint8_t nonce[12], tag[GCM_TAG];
    make_nonce(nonce, c->s_iv, c->s_seq);
    c->s_seq++;
    memcpy(tag, c->rec + ctlen, GCM_TAG);
    if (aes128_gcm_decrypt(c->s_key, nonce, c->rechdr, 5, c->rec, ctlen, tag,
                           c->plain) != 0)
        return -1;
    /* Снять padding и внутренний тип. */
    while (ctlen > 0 && c->plain[ctlen - 1] == 0)
        ctlen--;
    if (ctlen == 0)
        return -1;
    uint8_t inner = c->plain[ctlen - 1];
    c->plain_len = ctlen - 1;
    c->plain_off = 0;
    return inner;
}

/* ─── ключевой план (RFC 8446, 7.1) ─────────────────────────────────────── */

static void derive_key_iv(const uint8_t *traffic, uint8_t key[16],
                          uint8_t iv[12]) {
    hkdf_expand_label(traffic, "key", NULL, 0, key, 16);
    hkdf_expand_label(traffic, "iv", NULL, 0, iv, 12);
}

static void transcript_hash(tls_conn *c, uint8_t out[SHA256_DIGEST]) {
    sha256_ctx t = c->transcript;
    sha256_final(&t, out);
}

static void finished_mac(const uint8_t *traffic, const uint8_t th[SHA256_DIGEST],
                         uint8_t out[SHA256_DIGEST]) {
    uint8_t fk[SHA256_DIGEST];
    hkdf_expand_label(traffic, "finished", NULL, 0, fk, SHA256_DIGEST);
    hmac256(fk, SHA256_DIGEST, th, SHA256_DIGEST, out);
}

/* ─── ClientHello ───────────────────────────────────────────────────────── */

static size_t build_client_hello(tls_conn *c, const uint8_t pub[32],
                                 const char *sni) {
    uint8_t *p = c->hs;
    size_t n = 0;
    p[n++] = 1; /* client_hello */
    size_t len_off = n;
    n += 3;
    be16_put(p + n, 0x0303); /* legacy_version */
    n += 2;
    tls_random(c, p + n, 32); /* random */
    n += 32;
    tls_random(c, p + n + 1, 32); /* legacy_session_id (32 байта) */
    p[n] = 32;
    n += 33;
    be16_put(p + n, 2); /* cipher_suites */
    n += 2;
    be16_put(p + n, 0x1301); /* TLS_AES_128_GCM_SHA256 */
    n += 2;
    p[n++] = 1; /* compression_methods */
    p[n++] = 0;

    size_t ext_off = n;
    n += 2;
    size_t ext_start = n;

    /* server_name (0x0000) */
    {
        size_t sl = s_len(sni);
        be16_put(p + n, 0x0000);
        n += 2;
        be16_put(p + n, (uint16_t)(1 + 2 + sl + 2)); /* list_len + name_type
                                                      * + name_len + name */
        n += 2;
        be16_put(p + n, (uint16_t)(1 + 2 + sl));
        n += 2;
        p[n++] = 0; /* host_name */
        be16_put(p + n, (uint16_t)sl);
        n += 2;
        memcpy(p + n, sni, sl);
        n += sl;
    }

    /* supported_groups (0x000a): x25519 */
    be16_put(p + n, 0x000a);
    n += 2;
    be16_put(p + n, 4);
    n += 2;
    be16_put(p + n, 2);
    n += 2;
    be16_put(p + n, 0x001d);
    n += 2;

    /* signature_algorithms (0x000d) */
    {
        static const uint16_t sa[] = {
            0x0403, 0x0503, 0x0603, /* ecdsa p256/p384/p521 */
            0x0804, 0x0805, 0x0806, /* rsa_pss_rsae */
            0x0401, 0x0501, 0x0601, /* rsa_pkcs1 */
            0x0807, 0x0808          /* ed25519/ed448 */
        };
        size_t cnt = sizeof(sa) / sizeof(sa[0]);
        be16_put(p + n, 0x000d);
        n += 2;
        be16_put(p + n, (uint16_t)(2 + cnt * 2));
        n += 2;
        be16_put(p + n, (uint16_t)(cnt * 2));
        n += 2;
        for (size_t i = 0; i < cnt; i++) {
            be16_put(p + n, sa[i]);
            n += 2;
        }
    }

    /* ALPN (0x0010): http/1.1 */
    {
        static const char alpn[] = "http/1.1";
        size_t al = sizeof(alpn) - 1;
        be16_put(p + n, 0x0010);
        n += 2;
        be16_put(p + n, (uint16_t)(2 + 1 + al));
        n += 2;
        be16_put(p + n, (uint16_t)(1 + al));
        n += 2;
        p[n++] = (uint8_t)al;
        memcpy(p + n, alpn, al);
        n += al;
    }

    /* supported_versions (0x002b): TLS 1.3 */
    be16_put(p + n, 0x002b);
    n += 2;
    be16_put(p + n, 3);
    n += 2;
    p[n++] = 2;
    be16_put(p + n, 0x0304);
    n += 2;

    /* key_share (0x0033): x25519 */
    be16_put(p + n, 0x0033);
    n += 2;
    be16_put(p + n, 38);
    n += 2;
    be16_put(p + n, 36); /* client_shares length */
    n += 2;
    be16_put(p + n, 0x001d);
    n += 2;
    be16_put(p + n, 32);
    n += 2;
    memcpy(p + n, pub, 32);
    n += 32;

    be16_put(p + ext_off, (uint16_t)(n - ext_start));
    put_u24(p + len_off, (uint32_t)(n - (len_off + 3)));
    return n;
}

/* ─── разбор ServerHello и вывод handshake-ключей ───────────────────────── */

static int parse_server_hello(tls_conn *c, const uint8_t *b, size_t len) {
    if (len < 2 + 32 + 1)
        return -1;
    size_t off = 2 + 32;
    uint8_t sid_len = b[off++];
    if (off + sid_len + 2 + 1 + 2 > len)
        return -1;
    off += sid_len;
    uint16_t cs = be16_get(b + off);
    off += 2;
    if (cs != 0x1301) {
        set_err(c, "server picked unsupported cipher");
        return -1;
    }
    off += 1; /* legacy_compression_method */
    uint16_t extlen = be16_get(b + off);
    off += 2;
    size_t end = off + extlen;
    if (end > len)
        return -1;

    const uint8_t *srv_pub = NULL;
    while (off + 4 <= end) {
        uint16_t et = be16_get(b + off);
        uint16_t el = be16_get(b + off + 2);
        off += 4;
        if (off + el > end)
            return -1;
        if (et == 0x002b) { /* supported_versions */
            if (el != 2 || be16_get(b + off) != 0x0304) {
                set_err(c, "server is not TLS 1.3");
                return -1;
            }
        } else if (et == 0x0033) { /* key_share */
            if (el < 4) {
                set_err(c, "bad key_share");
                return -1;
            }
            uint16_t grp = be16_get(b + off);
            uint16_t klen = be16_get(b + off + 2);
            if (grp != 0x001d || klen != 32 || el < 4 + klen) {
                set_err(c, "server key_share is not x25519");
                return -1;
            }
            srv_pub = b + off + 4;
        }
        off += el;
    }
    if (!srv_pub) {
        set_err(c, "no key_share in ServerHello");
        return -1;
    }

    uint8_t shared[32];
    if (x25519_shared(shared, c->priv, srv_pub) != 0) {
        set_err(c, "x25519 shared secret is zero");
        return -1;
    }

    uint8_t zeros[SHA256_DIGEST], sha_empty[SHA256_DIGEST];
    memset(zeros, 0, sizeof(zeros));
    sha256(NULL, 0, sha_empty);

    uint8_t early[SHA256_DIGEST], derived[SHA256_DIGEST];
    uint8_t th[SHA256_DIGEST];
    hkdf_extract(zeros, sizeof(zeros), zeros, sizeof(zeros), early);
    tls_derive_secret(early, "derived", sha_empty, derived);
    hkdf_extract(derived, sizeof(derived), shared, sizeof(shared), c->hs_secret);

    transcript_hash(c, th); /* Hash(ClientHello..ServerHello) */
    tls_derive_secret(c->hs_secret, "c hs traffic", th, c->c_hs_traffic);
    tls_derive_secret(c->hs_secret, "s hs traffic", th, c->s_hs_traffic);
    derive_key_iv(c->s_hs_traffic, c->s_key, c->s_iv);
    derive_key_iv(c->c_hs_traffic, c->c_key, c->c_iv);
    c->c_seq = 0;
    c->s_seq = 0;
    return 0;
}

/* ─── хендшейк ──────────────────────────────────────────────────────────── */

/* Следующее handshake-сообщение целиком в c->hs? 1 — да, 0 — нет. */
static int hs_next(tls_conn *c, uint8_t *type, size_t *blen) {
    if (c->hslen < 4)
        return 0;
    size_t l = ((size_t)c->hs[1] << 16) | ((size_t)c->hs[2] << 8) | c->hs[3];
    if (c->hslen < 4 + l)
        return 0;
    *type = c->hs[0];
    *blen = l;
    return 1;
}

static void hs_consume(tls_conn *c, size_t n) {
    for (size_t i = 0; i + n < c->hslen; i++)
        c->hs[i] = c->hs[i + n];
    c->hslen -= n;
}

static int hs_append(tls_conn *c, const uint8_t *data, size_t len) {
    if (c->hslen + len > sizeof(c->hs))
        return -1;
    memcpy(c->hs + c->hslen, data, len);
    c->hslen += len;
    return 0;
}

/* Завершает рукопожатие: проверяет серверный Finished, шлёт клиентский,
 * переключает ключи на application traffic. 0 — успех. */
static int finish_handshake(tls_conn *c, const uint8_t *fin_body,
                            size_t fin_len) {
    uint8_t zeros[SHA256_DIGEST], sha_empty[SHA256_DIGEST];
    memset(zeros, 0, sizeof(zeros));
    sha256(NULL, 0, sha_empty);

    uint8_t th[SHA256_DIGEST], expect[SHA256_DIGEST];
    transcript_hash(c, th); /* до добавления серверного Finished */
    finished_mac(c->s_hs_traffic, th, expect);
    if (fin_len != SHA256_DIGEST || memcmp(expect, fin_body, SHA256_DIGEST) != 0) {
        set_err(c, "server Finished mismatch");
        return -1;
    }

    /* Добавляем серверный Finished в транскрипт. */
    uint8_t msg[4 + SHA256_DIGEST];
    msg[0] = 20;
    put_u24(msg + 1, SHA256_DIGEST);
    memcpy(msg + 4, fin_body, SHA256_DIGEST);
    sha256_update(&c->transcript, msg, sizeof(msg));

    /* Master secret и application traffic secrets (CH..server Finished). */
    uint8_t derived2[SHA256_DIGEST], master[SHA256_DIGEST];
    uint8_t c_ap[SHA256_DIGEST], s_ap[SHA256_DIGEST];
    tls_derive_secret(c->hs_secret, "derived", sha_empty, derived2);
    hkdf_extract(derived2, sizeof(derived2), zeros, sizeof(zeros), master);
    transcript_hash(c, th); /* CH..server Finished */
    tls_derive_secret(master, "c ap traffic", th, c_ap);
    tls_derive_secret(master, "s ap traffic", th, s_ap);

    /* Client Finished (под handshake-ключами клиента). */
    uint8_t cfin[SHA256_DIGEST];
    finished_mac(c->c_hs_traffic, th, cfin);
    uint8_t cmsg[4 + SHA256_DIGEST];
    cmsg[0] = 20;
    put_u24(cmsg + 1, SHA256_DIGEST);
    memcpy(cmsg + 4, cfin, SHA256_DIGEST);

    uint8_t ccs = 1;
    if (send_record(c, REC_CCS, &ccs, 1) < 0) {
        set_err(c, "send CCS failed");
        return -1;
    }
    if (send_enc_record(c, REC_HANDSHAKE, cmsg, sizeof(cmsg), c->c_key,
                        c->c_iv, &c->c_seq) < 0) {
        set_err(c, "send Finished failed");
        return -1;
    }

    /* Переключаемся на application-ключи. */
    derive_key_iv(c_ap, c->c_key, c->c_iv);
    derive_key_iv(s_ap, c->s_key, c->s_iv);
    c->c_seq = 0;
    c->s_seq = 0;
    /* send_enc_record использовал c->plain как скретч — обнуляем буфер,
     * иначе tls_read вернёт остатки handshake как данные приложения. */
    c->plain_len = 0;
    c->plain_off = 0;
    c->done = 1;
    return 0;
}

int tls_handshake(tls_conn *c, const char *sni) {
    sha256_init(&c->transcript);
    c->hslen = 0;
    c->plain_len = 0;
    c->plain_off = 0;
    c->c_seq = 0;
    c->s_seq = 0;
    c->done = 0;
    c->err[0] = 0;

    uint8_t pub[32];
    tls_random(c, c->priv, 32);
    x25519_public(pub, c->priv);

    size_t chlen = build_client_hello(c, pub, sni);
    sha256_update(&c->transcript, c->hs, chlen);
    if (send_record(c, REC_HANDSHAKE, c->hs, chlen) < 0) {
        set_err(c, "send ClientHello failed");
        return -1;
    }
    c->hslen = 0; /* c->hs снова буфер сборки */

    int have_sh = 0;

    for (;;) {
        int type;
        int n = read_record(c, &type);
        if (n <= 0) {
            set_err(c, "connection closed during handshake");
            return -1;
        }
        if (type == REC_HANDSHAKE) {
            if (hs_append(c, c->rec, (size_t)n) < 0) {
                set_err(c, "handshake message too big");
                return -1;
            }
        } else if (type == REC_APPDATA) {
            if (!have_sh) {
                set_err(c, "encrypted record before ServerHello");
                return -1;
            }
            int inner = decrypt_record(c, (size_t)n);
            if (inner < 0) {
                set_err(c, "record decrypt failed");
                return -1;
            }
            if (inner == REC_HANDSHAKE) {
                if (hs_append(c, c->plain, c->plain_len) < 0) {
                    set_err(c, "handshake message too big");
                    return -1;
                }
            } else if (inner == REC_ALERT) {
                set_err(c, "server sent alert");
                return -1;
            }
        } else if (type == REC_ALERT) {
            set_err(c, "server sent plaintext alert");
            return -1;
        }

        uint8_t mt;
        size_t blen;
        while (hs_next(c, &mt, &blen)) {
            const uint8_t *body = c->hs + 4;
            size_t full = 4 + blen;
            if (mt == 2) { /* ServerHello */
                /* Сначала в транскрипт: parse_server_hello выводит ключи из
                 * Hash(CH||SH). */
                sha256_update(&c->transcript, c->hs, full);
                if (parse_server_hello(c, body, blen) < 0)
                    return -1;
                have_sh = 1;
            } else if (mt == 20) { /* Finished */
                /* finish_handshake сам добавляет сообщение в транскрипт. */
                if (finish_handshake(c, body, blen) < 0)
                    return -1;
            } else {
                /* EncryptedExtensions / Certificate / CertificateVerify /
                 * прочее: не проверяем, но включаем в транскрипт. */
                sha256_update(&c->transcript, c->hs, full);
            }
            hs_consume(c, full);
        }
        if (c->done)
            return 0;
    }
}

/* ─── приложение ────────────────────────────────────────────────────────── */

int tls_write(tls_conn *c, const uint8_t *data, size_t len) {
    while (len) {
        size_t take = len > 16384 ? 16384 : len;
        if (send_enc_record(c, REC_APPDATA, data, take, c->c_key, c->c_iv,
                            &c->c_seq) < 0) {
            set_err(c, "app send failed");
            return -1;
        }
        data += take;
        len -= take;
    }
    return 0;
}

int tls_read(tls_conn *c, uint8_t *buf, size_t max) {
    for (;;) {
        if (c->plain_off < c->plain_len) {
            size_t avail = c->plain_len - c->plain_off;
            if (avail > max)
                avail = max;
            memcpy(buf, c->plain + c->plain_off, avail);
            c->plain_off += avail;
            return (int)avail;
        }
        int type;
        int n = read_record(c, &type);
        if (n <= 0)
            return n;
        if (type == REC_ALERT)
            return 0; /* close_notify */
        if (type != REC_APPDATA)
            continue;
        int inner = decrypt_record(c, (size_t)n);
        if (inner < 0) {
            set_err(c, "app record decrypt failed");
            return -1;
        }
        if (inner == REC_APPDATA)
            continue; /* цикл вернёт данные */
        /* Не данные приложения: сбросить буфер, иначе следующий виток
         * вернёт расшифрованный handshake (например NewSessionTicket). */
        c->plain_len = 0;
        c->plain_off = 0;
        if (inner == REC_ALERT)
            return 0;
        /* Новый сеансный тикет/прочий handshake — пропускаем. */
    }
}

void tls_conn_init(tls_conn *c, tls_transport tr) {
    memset(c, 0, sizeof(*c));
    c->tr = tr;
}
