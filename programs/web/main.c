/* Web: lynx-подобный мини-браузер — HTTP GET через TCP-сокет ядра, рендер
 * текста и ссылок прямо в VGA.
 *
 *   run web <host> [port] [path]
 *   run web example.com            — схема http, порт 80, путь /
 *   run web 10.0.2.2 8080 /test.html
 *
 * Сеть — ABI-трамплины net_* (0x7840..0x7860): connect/send/recv/close и
 * resolve (DNS через UDP в ядре). Сисколы НЕ блокируют CPU: ядро продвигает
 * TCP-состояние за каждый вызов и возвращает -2 (EAGAIN), пока ждёт сети;
 * программа крутит циклы `sleep(ticks+2)` — как и положено cooperative-задаче.
 *
 * Рендер как в lynx: режем HTTP-заголовки, распаковываем chunked, стрипуем
 * теги, декодируем сущности, оборачиваем по ширине и подписываем ссылки
 * как [1], [2]… Номера выдаются на видимую часть страницы, как в lynx.
 *
 * Клавиши: стрелки ↑↓ — скролл, PgUp/PgDn/Home/End — экран, ←/→ и TAB —
 * предыдущая/следующая ссылка, Enter или «номер.» — открыть ссылку,
 * g — «го» (ввод адреса), h — назад по истории, q — выход.
 *
 * Коды результатов net_*: 0 = ok, -1 = EOF (FIN получен, всё прочитано),
 * -2 = EAGAIN (ещё в процессе / данных нет), -3 = ошибка/RST.
 *
 * HTTPS: для схемы https (порт 443) поверх TCP поднимается TLS 1.3
 * (tls_client.c). Сертификат не проверяется — аналог curl -k.
 */

#include "tls_client.h"

#define NET_CONNECT_P 0x7840
#define NET_SEND_P    0x7848
#define NET_RECV_P    0x7850
#define NET_CLOSE_P   0x7858
#define NET_RESOLVE_P 0x7860
#define TICKS_P       0x7800
#define SLEEP_P       0x7820
#define KBHIT_P       0x7828
#define KBREAD_P      0x7830
#define EXIT_ROW_P    0x708C
#define EXIT_COL_P    0x7090

#define NET_EAGAIN (-2)
#define NET_EOF    (-1)
#define NET_ERR    (-3)
#define NULL       ((void *)0)

typedef unsigned long long (*fn_ticks)(void);
typedef int (*fn_sleep)(unsigned long long);
typedef int (*fn_connect)(const char *, unsigned int);
typedef int (*fn_send)(const void *, unsigned int);
typedef int (*fn_recv)(void *, unsigned int);
typedef int (*fn_close)(void);
typedef int (*fn_resolve)(const char *, unsigned char *, int);
typedef int (*fn_kbhit)(void);
typedef int (*fn_kbread)(void);

#define abi_ticks()        ((*(volatile fn_ticks *)TICKS_P)())
#define abi_sleep(t)       ((*(volatile fn_sleep *)SLEEP_P)(t))
#define abi_connect(h, p)  ((*(volatile fn_connect *)NET_CONNECT_P)(h, p))
#define abi_send(b, l)     ((*(volatile fn_send *)NET_SEND_P)(b, l))
#define abi_recv(b, m)     ((*(volatile fn_recv *)NET_RECV_P)(b, m))
#define abi_close()        ((*(volatile fn_close *)NET_CLOSE_P)())
#define abi_resolve(h, o, m) ((*(volatile fn_resolve *)NET_RESOLVE_P)(h, o, m))
#define abi_kbhit()        ((*(volatile fn_kbhit *)KBHIT_P)())
#define abi_kbread()       ((*(volatile fn_kbread *)KBREAD_P)())

#define SCREEN_W 80
#define SCREEN_H 30
#define TEXT_W   74 /* ширина текста; справа остаётся место под « [NN]» */
#define RX_CAP   98304 /* совпадает с TCP_RX_CAP в ядре: страница целиком */
#define MAX_LINES 800
#define MAX_LINKS 96
#define MAX_HIST  12
#define HOST_MAX  128
#define PATH_MAX  256
#define URL_MAX   384
#define HREF_MAX  384

/* ─── Состояние ──────────────────────────────────────────────────────────── */

static unsigned char rxbuf[RX_CAP];
static int rx_total;

static char lines[MAX_LINES][TEXT_W + 1];
static int nlines;

struct Link {
    int line; /* строка документа, где стоит ссылка */
    char href[HREF_MAX];
};
static struct Link links[MAX_LINKS];
static int nlinks;

static int vis_num[MAX_LINKS]; /* ссылка -> видимый номер 1.., 0 = не видна */
static int max_vis;
static int cur_link = -1; /* выбранная (курсорная) ссылка */

static char hist_url[MAX_HIST][URL_MAX];
static int nhist;

/* ─── VGA-примитивы ──────────────────────────────────────────────────────── */

static void put_ch(int row, int colc, char ch, unsigned char attr) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    if (row < 0 || row > SCREEN_H - 1 || colc < 0 || colc > SCREEN_W - 1)
        return;
    int off = row * 160 + colc * 2;
    vga[off] = (unsigned char)ch;
    vga[off + 1] = attr;
}

static int put_str(int row, int colc, const char *s, unsigned char attr) {
    while (*s && colc < SCREEN_W) {
        put_ch(row, colc++, *s++, attr);
    }
    return colc;
}

static int put_num(int row, int colc, int v, unsigned char attr) {
    char buf[12];
    int n = 0;
    if (v < 0) {
        put_ch(row, colc++, '-', attr);
        v = -v;
    }
    if (v == 0) {
        put_ch(row, colc++, '0', attr);
        return colc;
    }
    while (v > 0) {
        buf[n++] = (char)('0' + v % 10);
        v /= 10;
    }
    while (n > 0)
        put_ch(row, colc++, buf[--n], attr);
    return colc;
}

static void fill_row(int row, unsigned char attr) {
    for (int c = 0; c < SCREEN_W; c++)
        put_ch(row, c, ' ', attr);
}

static int str_len(const char *s) {
    int n = 0;
    while (s[n])
        n++;
    return n;
}

static void str_copy(char *dst, const char *src, int max) {
    int i = 0;
    while (src[i] && i < max - 1) {
        dst[i] = src[i];
        i++;
    }
    dst[i] = 0;
}

/* Текущий URL — база для разрешения относительных href. */
static char cur_url[URL_MAX];

/* ─── Клавиатура ─────────────────────────────────────────────────────────── */

#define SC_LSHIFT 0x2A
#define SC_RSHIFT 0x36
#define SC_ENTER  0x1C
#define SC_ESC    0x01
#define SC_BKSP   0x0E
#define SC_TAB    0x0F
#define SC_SPACE  0x39
#define SC_UP     0x48
#define SC_DOWN   0x50
#define SC_LEFT   0x4B
#define SC_RIGHT  0x4D
#define SC_PGUP   0x49
#define SC_PGDN   0x51
#define SC_HOME   0x47
#define SC_END    0x4F

static int shift_down;

/* Таблицы набора 1 (US), индекс = сканкод - 0x02, до 0x39 (пробел) включительно.
 * Раскладка выровнена по группам клавиатуры; NUL на месте модификаторов
 * (0x2A lshift, 0x36 rshift), служебных (enter/tab/bksp) и keypad. */
static char sc_to_char(int sc, int shift) {
    static const char lo[] = "1234567890-=" "\b\t"        /* 0x02..0x0F */
                            "qwertyuiop[]" "\r" "\0"      /* 0x10..0x1D */
                            "asdfghjkl;'\x60" "\0" "\\"    /* 0x1E..0x2B */
                            "zxcvbnm,./"                   /* 0x2C..0x35 */
                            "\0\0\0 ";                     /* 0x36..0x39 */
    static const char up[] = "!@#$%^&*()_+" "\0\0"        /* 0x02..0x0F */
                            "QWERTYUIOP{}" "\0" "\0"      /* 0x10..0x1D */
                            "ASDFGHJKL:\"" "~" "\0" "|"   /* 0x1E..0x2B */
                            "ZXCVBNM<>?"                   /* 0x2C..0x35 */
                            "\0\0\0 ";                     /* 0x36..0x39 */
    if (sc < 0x02 || sc > 0x39)
        return 0;
    int idx = sc - 0x02;
    if (idx >= (int)sizeof(lo) || idx >= (int)sizeof(up))
        return 0;
    return shift ? up[idx] : lo[idx];
}

/* Единственный способ получить make-код. Break-коды (0x80) — не нажатия, но
 * для Shift их нужно прочитать, иначе состояние «залипнет» до конца сессии. */
static int wait_key(void) {
    while (1) {
        if (abi_kbhit()) {
            int sc = abi_kbread();
            if (sc == (SC_LSHIFT | 0x80) || sc == (SC_RSHIFT | 0x80)) {
                shift_down = 0;
                continue;
            }
            if (sc & 0x80)
                continue; /* break прочей клавиши — игнор */
            if (sc == SC_LSHIFT || sc == SC_RSHIFT) {
                shift_down = 1;
                continue;
            }
            return sc;
        }
        abi_sleep(abi_ticks() + 2);
    }
}

static void wait_key_any(void) {
    (void)wait_key();
}

/* ─── URL ────────────────────────────────────────────────────────────────── */

static int is_digit(int c) { return c >= '0' && c <= '9'; }

static int lower(int c) { return (c >= 'A' && c <= 'Z') ? c + 32 : c; }

static int scheme_is(const char *s, int len, const char *want) {
    int i = 0;
    for (; i < len && want[i]; i++)
        if (lower(s[i]) != want[i])
            return 0;
    return i == len && want[len] == 0;
}

/* Разбирает URL на host/port/path. Возвращает 0 или -1.
 *
 * Схема распознаётся по «://», а не по «буквы-до-двоеточия»: иначе адрес без
 * схемы («10.0.2.2:8080/», «example.com/») не отличить от «scheme:path».
 */
static int parse_url(const char *in, char *host, int hostsz, char *path,
                     int pathsz, int *port_out) {
    int i = 0, pn = 0;
    int def_port = 80;
    host[0] = 0;
    path[0] = 0;
    *port_out = 80;
    if (!in || !in[0])
        return -1;

    /* Ищем «://» — только он делает префикс схемой. */
    int sep = -1;
    for (int k = 0; in[k]; k++) {
        if (in[k] == ':' && in[k + 1] == '/' && in[k + 2] == '/') {
            sep = k;
            break;
        }
        if (in[k] == '/' || in[k] == ' ' || in[k] == '\t')
            break; /* до схемы не дотянулись: адрес начинается с хоста */
    }
    if (sep > 0) {
        if (scheme_is(in, sep, "https"))
            def_port = 443; /* TLS 1.3 поднимается в http_get() */
        i = sep + 3;
    }

    int hn = 0;
    while (in[i] && in[i] != '/' && in[i] != '?' && in[i] != '#' &&
           in[i] != ':' && hn < hostsz - 1)
        host[hn++] = in[i++];
    host[hn] = 0;
    if (hn == 0)
        return -1;
    if (in[i] == ':') {
        i++;
        int pv = 0, any = 0;
        while (is_digit(in[i])) {
            pv = pv * 10 + (in[i] - '0');
            if (pv > 65535)
                pv = 65535;
            i++;
            any = 1;
        }
        *port_out = any ? pv : def_port;
        if (!any && in[i] != 0 && in[i] != '/' && in[i] != '?' && in[i] != '#')
            return -1; /* мусор после двоеточия */
    } else {
        *port_out = def_port;
    }
    if (in[i] == 0) {
        path[pn++] = '/';
    } else {
        while (in[i] && pn < pathsz - 1)
            path[pn++] = in[i++];
    }
    path[pn] = 0;
    return 0;
}

/* Разрешает href относительно текущего URL (cur_url) в абсолютный. */
static void make_url(char *out, int outsz, const char *host, int port,
                     const char *path);

static void resolve_href(const char *href, char *out, int outsz) {
    char base[HOST_MAX];
    char bpath[PATH_MAX];
    int bport;
    if (parse_url(cur_url, base, sizeof(base), bpath, sizeof(bpath), &bport) < 0) {
        str_copy(out, href, outsz);
        return;
    }
    /* Якорь. */
    if (href[0] == '#') {
        make_url(out, outsz, base, bport, bpath);
        return;
    }
    /* Схема распознаётся по "://" — как в parse_url. */
    for (int k = 0; k < 9 && href[k]; k++) {
        if (href[k] == ':' && href[k + 1] == '/' && href[k + 2] == '/') {
            str_copy(out, href, outsz);
            return;
        }
    }
    /* //host/path -> протокол текущий. */
    if (href[0] == '/' && href[1] == '/') {
        const char *p = "http://";
        int n = 0;
        while (*p && n < outsz - 1)
            out[n++] = *p++;
        for (p = href + 2; *p && n < outsz - 1; )
            out[n++] = *p++;
        out[n] = 0;
        return;
    }
    /* Абсолютный путь — префиксуем хостом базового URL. */
    if (href[0] == '/') {
        make_url(out, outsz, base, bport, href);
        return;
    }
    /* Относительный: каталог базового пути + href. */
    char tmp[URL_MAX];
    int n = 0, last = -1;
    for (int k = 0; bpath[k]; k++)
        if (bpath[k] == '/')
            last = k;
    for (int k = 0; k <= last && n < URL_MAX - 1; k++)
        tmp[n++] = bpath[k];
    for (int k = 0; href[k] && n < URL_MAX - 1; k++)
        tmp[n++] = href[k];
    tmp[n] = 0;

    /* Схлопываем «.», «..» и «//»; результат всегда с ведущим «/». */
    char norm[URL_MAX];
    int m = 0;
    norm[m++] = '/';
    int k = 0;
    while (k < n) {
        if (tmp[k] == '/') {
            k++;
            continue;
        }
        int sk = k;
        while (k < n && tmp[k] != '/')
            k++;
        int sl = k - sk;
        if (sl == 1 && tmp[sk] == '.') {
            /* «.» — ничего */
        } else if (sl == 2 && tmp[sk] == '.' && tmp[sk + 1] == '.') {
            if (m > 1) {
                m--; /* сойти с «/» */
                while (m > 1 && norm[m - 1] != '/')
                    m--;
            }
        } else {
            if (m > 1 && m < URL_MAX - 1)
                norm[m++] = '/';
            for (int q = sk; q < k && m < URL_MAX - 1; q++)
                norm[m++] = tmp[q];
        }
    }
    if (n > 0 && tmp[n - 1] == '/' && m < URL_MAX - 1)
        norm[m++] = '/'; /* каталог: сохраняем завершающий «/» */
    norm[m < URL_MAX ? m : URL_MAX - 1] = 0;
    make_url(out, outsz, base, bport, norm);
}

static void make_url(char *out, int outsz, const char *host, int port,
                     const char *path) {
    int n = 0;
    const char *p = "http://";
    while (*p && n < outsz - 1)
        out[n++] = *p++;
    for (p = host; *p && n < outsz - 1; )
        out[n++] = *p++;
    if (port != 80) {
        if (n < outsz - 1)
            out[n++] = ':';
        char pb[8];
        int pn = 0, v = port;
        if (v == 0)
            pb[pn++] = '0';
        while (v > 0) {
            pb[pn++] = (char)('0' + v % 10);
            v /= 10;
        }
        while (pn > 0 && n < outsz - 1)
            out[n++] = pb[--pn];
    }
    for (p = path; *p && n < outsz - 1; )
        out[n++] = *p++;
    out[n] = 0;
}

/* ─── DNS ────────────────────────────────────────────────────────────────── */

static int resolve_to_ip(const char *host, char *ip_out, int outsz) {
    unsigned char ips[8 * 4];
    int n = abi_resolve(host, ips, 8);
    if (n <= 0) {
        ip_out[0] = 0;
        return -1;
    }
    int p = 0;
    for (int i = 0; i < 4 && p < outsz - 5; i++) {
        if (i)
            ip_out[p++] = '.';
        int v = ips[i];
        if (v >= 100)
            ip_out[p++] = (char)('0' + v / 100);
        if (v >= 10)
            ip_out[p++] = (char)('0' + (v / 10) % 10);
        ip_out[p++] = (char)('0' + v % 10);
    }
    ip_out[p] = 0;
    return 0;
}

/* ─── HTTP ───────────────────────────────────────────────────────────────── */

/* Копирует [from..to) из rxbuf в out с учётом переноса CRLF->LF. */
static int copy_body(int from, int to, char *out, int outsz) {
    int o = 0;
    for (int i = from; i < to && o < outsz - 1; i++) {
        if (rxbuf[i] == '\r')
            continue;
        out[o++] = (char)rxbuf[i];
    }
    out[o] = 0;
    return o;
}

/* Ищет заголовок (без учёта регистра) в первых len байтах, начиная с from. */
static int find_header(int from, int len, const char *name, char *out,
                       int outsz) {
    int nl = str_len(name);
    for (int i = from; i + nl < len; i++) {
        /* Конец блока заголовков — пустая строка («\r\n\r\n» или «\n\n»).
         * Одиночный «\r\n» концом не является: это лишь конец строки. */
        if (rxbuf[i] == '\r' && rxbuf[i + 1] == '\n' && rxbuf[i + 2] == '\r' &&
            rxbuf[i + 3] == '\n')
            break;
        if (rxbuf[i] == '\n' && rxbuf[i + 1] == '\n')
            break;
        int ok = 1;
        for (int k = 0; k < nl; k++) {
            char a = (char)rxbuf[i + k];
            char b = name[k];
            if (a >= 'A' && a <= 'Z')
                a = (char)(a + 32);
            if (a != b) {
                ok = 0;
                break;
            }
        }
        if (!ok)
            continue;
        int j = i + nl;
        while (j < len && (rxbuf[j] == ' ' || rxbuf[j] == ':'))
            j++;
        int o = 0;
        while (j < len && rxbuf[j] != '\r' && rxbuf[j] != '\n' && o < outsz - 1)
            out[o++] = (char)rxbuf[j++];
        out[o] = 0;
        return 1;
    }
    out[0] = 0;
    return 0;
}

/* Один обмен: connect -> GET -> recv до EOF. Возвращает 1 или 0. */
/* ─── TLS-транспорт над ABI net_* ───────────────────────────────────────── */

static int net_send_all(const void *buf, unsigned int len) {
    unsigned int off = 0;
    unsigned long long deadline = abi_ticks() + 800;
    while (off < len) {
        int n = abi_send((const char *)buf + off, len - off);
        if (n > 0) {
            off += (unsigned int)n;
            continue;
        }
        if (n == NET_ERR || n == NET_EOF)
            return -1;
        if (abi_ticks() > deadline)
            return -1;
        abi_sleep(abi_ticks() + 2);
    }
    return (int)off;
}

static int tls_tr_send(void *ctx, const uint8_t *buf, size_t len) {
    (void)ctx;
    return net_send_all(buf, (unsigned int)len);
}

/* >0 — байт, 0 — EOF/таймаут (сервер закрыл или молчит), -1 — сбой. */
static int tls_tr_recv(void *ctx, uint8_t *buf, size_t max) {
    (void)ctx;
    unsigned long long deadline = abi_ticks() + 4000;
    for (;;) {
        int n = abi_recv(buf, (unsigned int)max);
        if (n > 0)
            return n;
        if (n == NET_EOF)
            return 0;
        if (n == NET_ERR)
            return -1;
        if (abi_ticks() > deadline)
            return 0;
        abi_sleep(abi_ticks() + 2);
    }
}

/* PRNG на SHA-256 (freestanding): seed = ticks + адрес стека. Для игрушечной
 * ОС достаточно; проверка сертификатов всё равно отключена. */
static void tls_tr_random(void *ctx, uint8_t *out, size_t len) {
    (void)ctx;
    static uint8_t seed[SHA256_DIGEST];
    static unsigned long long ctr;
    static int init;
    if (!init) {
        sha256_ctx sc;
        unsigned long long t = abi_ticks();
        unsigned long long a = (unsigned long long)(uintptr_t)out;
        sha256_init(&sc);
        sha256_update(&sc, &t, sizeof(t));
        sha256_update(&sc, &a, sizeof(a));
        sha256_final(&sc, seed);
        init = 1;
    }
    while (len) {
        uint8_t h[SHA256_DIGEST];
        sha256_ctx sc;
        sha256_init(&sc);
        sha256_update(&sc, seed, sizeof(seed));
        sha256_update(&sc, &ctr, sizeof(ctr));
        sha256_final(&sc, h);
        ctr++;
        size_t take = len < sizeof(h) ? len : sizeof(h);
        for (size_t i = 0; i < take; i++)
            out[i] = h[i];
        sha256(seed, sizeof(seed), seed);
        out += take;
        len -= take;
    }
}

static tls_conn tls;

static void tls_errmsg(char *errmsg, int errsz, const char *prefix) {
    char b[128];
    int n = 0;
    while (prefix[n] && n < (int)sizeof(b) - 1) {
        b[n] = prefix[n];
        n++;
    }
    const char *e = tls_error(&tls);
    int k = 0;
    while (e[k] && n < (int)sizeof(b) - 1)
        b[n++] = e[k++];
    b[n] = 0;
    str_copy(errmsg, b, errsz);
}

static int http_get(const char *host, int port, const char *path,
                    int tls_mode, char *errmsg, int errsz) {
    char ip[64];
    if (resolve_to_ip(host, ip, sizeof(ip)) < 0) {
        str_copy(errmsg, "cannot resolve host", errsz);
        return 0;
    }
    int st;
    unsigned long long deadline = abi_ticks() + 800;
    while (1) {
        st = abi_connect(ip, (unsigned int)port);
        if (st == 0)
            break;
        if (st < NET_EAGAIN) {
            str_copy(errmsg, "connection refused/failed", errsz);
            return 0;
        }
        if (abi_ticks() > deadline) {
            str_copy(errmsg, "connect timeout", errsz);
            return 0;
        }
        abi_sleep(abi_ticks() + 2);
    }

    if (tls_mode) {
        tls_transport tr;
        tr.send = tls_tr_send;
        tr.recv = tls_tr_recv;
        tr.random = tls_tr_random;
        tr.ctx = NULL;
        tls_conn_init(&tls, tr);
        if (tls_handshake(&tls, host) < 0) {
            tls_errmsg(errmsg, errsz, "tls: ");
            abi_close();
            return 0;
        }
    }

    char req[768];
    int rr = 0;
    const char *m = "GET ";
    while (*m)
        req[rr++] = *m++;
    for (const char *pp = path; *pp; )
        req[rr++] = *pp++;
    m = " HTTP/1.0\r\nHost: ";
    while (*m)
        req[rr++] = *m++;
    for (const char *hh = host; *hh; )
        req[rr++] = *hh++;
    if (port != 80) {
        req[rr++] = ':';
        char pb[8];
        int pn = 0, v = port;
        if (v == 0)
            pb[pn++] = '0';
        while (v > 0) {
            pb[pn++] = (char)('0' + v % 10);
            v /= 10;
        }
        while (pn > 0)
            req[rr++] = pb[--pn];
    }
    m = "\r\nUser-Agent: VitaminOS-web/1.0\r\nAccept: text/html\r\n"
        "Connection: close\r\n\r\n";
    while (*m)
        req[rr++] = *m++;

    if (tls_mode) {
        if (tls_write(&tls, (const uint8_t *)req, (size_t)rr) < 0) {
            tls_errmsg(errmsg, errsz, "tls write: ");
            abi_close();
            return 0;
        }
    } else if (net_send_all(req, (unsigned int)rr) < 0) {
        str_copy(errmsg, "send failed", errsz);
        return 0;
    }

    rx_total = 0;
    if (tls_mode) {
        while (rx_total < RX_CAP) {
            int n = tls_read(&tls, (uint8_t *)(rxbuf + rx_total),
                             (size_t)(RX_CAP - rx_total));
            if (n > 0) {
                rx_total += n;
                continue;
            }
            if (n == 0)
                break; /* EOF/close_notify/таймаут — берём что пришло */
            tls_errmsg(errmsg, errsz, "tls read: ");
            abi_close();
            return 0;
        }
    } else {
        deadline = abi_ticks() + 4000;
        while (1) {
            int n = abi_recv(rxbuf + rx_total, RX_CAP - rx_total);
            if (n > 0) {
                rx_total += n;
                if (rx_total >= RX_CAP)
                    break;
                continue;
            }
            if (n == NET_EOF)
                break;
            if (n == NET_ERR) {
                str_copy(errmsg, "connection reset", errsz);
                return 0;
            }
            if (abi_ticks() > deadline) {
                /* Часть сайтов не закрывают поток — принимаем что есть. */
                if (rx_total > 0)
                    break;
                str_copy(errmsg, "receive timeout", errsz);
                return 0;
            }
            abi_sleep(abi_ticks() + 2);
        }
    }
    abi_close();
    return 1;
}

/* Разбирает ответ: заголовки -> body_start; распаковывает chunked.
 * Возвращает длину тела (в rxbuf начиная с body_start). */
static int parse_response(char *status, int statsz, char *location,
                          int locsz) {
    int hdr_end = -1;
    for (int i = 0; i + 3 < rx_total; i++) {
        if (rxbuf[i] == '\r' && rxbuf[i + 1] == '\n' &&
            rxbuf[i + 2] == '\r' && rxbuf[i + 3] == '\n') {
            hdr_end = i + 4;
            break;
        }
        if (rxbuf[i] == '\n' && rxbuf[i + 1] == '\n') {
            hdr_end = i + 2;
            break;
        }
    }
    if (hdr_end < 0) {
        str_copy(status, "bad response", statsz);
        return 0;
    }
    copy_body(0, hdr_end < 64 ? hdr_end : 64, status, statsz);
    for (int i = 0; status[i]; i++)
        if (status[i] == '\n' || status[i] == '\r')
            status[i] = 0;
    find_header(0, hdr_end, "location", location, locsz);

    char te[64];
    int chunked = find_header(0, hdr_end, "transfer-encoding", te, sizeof(te));
    if (!chunked)
        return hdr_end;

    int in_chunk = 0;
    for (int i = 0; i < 64; i++) {
        char a = te[i];
        if (a >= 'A' && a <= 'Z')
            a = (char)(a + 32);
        if (a == 'c' && te[i + 1] == 'h' && te[i + 2] == 'u' &&
            te[i + 3] == 'n' && te[i + 4] == 'k' && te[i + 5] == 'e' &&
            te[i + 6] == 'd') {
            in_chunk = 1;
            break;
        }
    }
    if (!in_chunk)
        return hdr_end;

    /* Распаковка на месте (dst <= src всегда). */
    int src = hdr_end, dst = hdr_end;
    while (src < rx_total) {
        int size = 0, any = 0, k = src;
        while (k < rx_total && rxbuf[k] != '\r' && rxbuf[k] != '\n') {
            char c = (char)rxbuf[k];
            int v;
            if (c >= '0' && c <= '9')
                v = c - '0';
            else if ((c | 0x20) >= 'a' && (c | 0x20) <= 'f')
                v = (c | 0x20) - 'a' + 10;
            else {
                any = 0;
                break;
            }
            size = size * 16 + v;
            any = 1;
            k++;
        }
        if (!any || size <= 0)
            break;
        /* Перевод строки после размера чанка. */
        while (k < rx_total && rxbuf[k] != '\n')
            k++;
        k++;
        /* Тело чанка — с k, а не с src (иначе в текст попадёт «28»). */
        for (int j = 0; j < size && k < rx_total; j++)
            rxbuf[dst++] = rxbuf[k++];
        if (k < rx_total && rxbuf[k] == '\r')
            k++;
        if (k < rx_total && rxbuf[k] == '\n')
            k++;
        src = k;
    }
    rx_total = dst;
    return hdr_end;
}

/* ─── HTML -> строки и ссылки ────────────────────────────────────────────── */

static char word[160];
static int wl;
static int col;
static int cur;

static void new_line(void) {
    if (cur < MAX_LINES - 1) {
        cur++;
        lines[cur][0] = 0;
        col = 0;
    }
}

static void flush_word(void) {
    if (wl == 0)
        return;
    if (cur >= MAX_LINES)
        return;
    if (col > 0 && col + 1 + wl > TEXT_W)
        new_line();
    else if (col > 0)
        lines[cur][col++] = ' ';
    for (int i = 0; i < wl && col < TEXT_W; i++)
        lines[cur][col++] = word[i];
    lines[cur][col] = 0; /* без терминатора строка читается до хвоста */
    if (col >= TEXT_W)
        new_line();
    wl = 0;
}

static void push_ch(unsigned char c) {
    if (c >= 0x20 && c < 0x7F && wl < 79)
        word[wl++] = (char)c;
}

static void block_end(void) {
    flush_word();
    if (col > 0)
        new_line();
}

static void add_link(const char *href) {
    if (nlinks >= MAX_LINKS || !href || !href[0])
        return;
    char abs[URL_MAX];
    resolve_href(href, abs, sizeof(abs));
    links[nlinks].line = cur;
    str_copy(links[nlinks].href, abs, HREF_MAX);
    nlinks++;
}

/* Значение атрибута name внутри тега tag[0..taglen). */
static int attr_value(const unsigned char *tag, int taglen, const char *name,
                      char *out, int outsz) {
    int nl = str_len(name);
    for (int i = 0; i + nl < taglen; i++) {
        int ok = 1;
        for (int k = 0; k < nl; k++) {
            char a = (char)tag[i + k];
            if (a >= 'A' && a <= 'Z')
                a = (char)(a + 32);
            if (a != name[k]) {
                ok = 0;
                break;
            }
        }
        if (!ok)
            continue;
        int j = i + nl;
        while (j < taglen && (tag[j] == ' ' || tag[j] == '=' || tag[j] == '\t'))
            j++;
        if (j >= taglen)
            break;
        if (tag[j] == '"' || tag[j] == '\'') {
            char q = (char)tag[j];
            j++;
            int n = 0;
            while (j < taglen && tag[j] != q && n < outsz - 1)
                out[n++] = (char)tag[j++];
            out[n] = 0;
            return n;
        }
        int n = 0;
        while (j < taglen && tag[j] != ' ' && tag[j] != '>' && n < outsz - 1)
            out[n++] = (char)tag[j++];
        out[n] = 0;
        return n;
    }
    out[0] = 0;
    return 0;
}

static int tag_is(const char *name, int nl, const char *want) {
    int wl_ = str_len(want);
    if (wl_ != nl)
        return 0;
    for (int i = 0; i < nl; i++) {
        char a = name[i];
        if (a >= 'A' && a <= 'Z')
            a = (char)(a + 32);
        if (a != want[i])
            return 0;
    }
    return 1;
}

static int is_block_tag(const char *name, int nl) {
    static const char *tags[] = { "p",  "div", "h1", "h2",      "h3",
                                  "h4", "h5",  "h6", "ul",      "ol",
                                  "li", "tr",  "td", "th",      "blockquote",
                                  "pre", "hr", "table", "section", "header",
                                  "footer", "article", "nav", "dl", "dt", "dd" };
    for (unsigned i = 0; i < sizeof(tags) / sizeof(tags[0]); i++)
        if (tag_is(name, nl, tags[i]))
            return 1;
    return 0;
}

static void build_page(int body_start) {
    wl = 0;
    col = 0;
    cur = 0;
    lines[0][0] = 0;
    /* Буферы строк переиспользуются между страницами — чистим, иначе на
     * экране остаётся хвост предыдущего документа. */
    for (int r = 1; r < MAX_LINES; r++)
        lines[r][0] = 0;
    nlinks = 0;

    int i = body_start;
    char href_buf[URL_MAX];

    while (i < rx_total && cur < MAX_LINES) {
        unsigned char c = rxbuf[i];

        if (c == '<') {
            /* Пропускаем комментарии. */
            if (i + 3 < rx_total && rxbuf[i + 1] == '!' && rxbuf[i + 2] == '-' &&
                rxbuf[i + 3] == '-') {
                i += 4;
                while (i + 2 < rx_total &&
                       !(rxbuf[i] == '-' && rxbuf[i + 1] == '-' && rxbuf[i + 2] == '>'))
                    i++;
                i += 3;
                continue;
            }
            int j = i + 1;
            int q = 0;
            while (j < rx_total) {
                unsigned char d = rxbuf[j];
                if (q) {
                    if (d == q)
                        q = 0;
                } else if (d == '"' || d == '\'') {
                    q = d;
                } else if (d == '>') {
                    break;
                }
                j++;
            }
            int end = (j < rx_total) ? j : rx_total;
            int taglen = end - i - 1;
            if (taglen > 0) {
                const unsigned char *tag = rxbuf + i + 1;
                int skip_to_close = -1;
                int closing = 0;
                int s = 0;
                if (tag[0] == '/') {
                    closing = 1;
                    s = 1;
                }
                char name[16];
                int nl = 0;
                while (s + nl < taglen && nl < 15 && tag[s + nl] != ' ' &&
                       tag[s + nl] != '/' && tag[s + nl] != '>') {
                    name[nl] = (char)tag[s + nl];
                    nl++;
                }
                name[nl] = 0;

                if (!closing && tag_is(name, nl, "script"))
                    skip_to_close = 1;
                else if (!closing && tag_is(name, nl, "style"))
                    skip_to_close = 2;
                else if (!closing && tag_is(name, nl, "br")) {
                    block_end();
                } else if (!closing && tag_is(name, nl, "hr")) {
                    block_end();
                } else if (!closing && tag_is(name, nl, "a")) {
                    if (attr_value(tag, taglen, "href", href_buf,
                                   sizeof(href_buf))) {
                        flush_word();
                        add_link(href_buf);
                    }
                } else if (is_block_tag(name, nl)) {
                    block_end();
                }
                if (skip_to_close > 0) {
                    const char *needle = (skip_to_close == 1) ? "</script" : "</style";
                    int nl2 = str_len(needle);
                    while (i < rx_total) {
                        if (rxbuf[i] == '<' && i + nl2 <= rx_total) {
                            int ok = 1;
                            for (int k = 0; k < nl2; k++)
                                if ((char)rxbuf[i + k] != needle[k]) {
                                    ok = 0;
                                    break;
                                }
                            if (ok)
                                break;
                        }
                        i++;
                    }
                    /* i стоит на '<' закрывающего тега: следующая итерация
                     * разберёт его как обычный тег (иначе '<' теряется). */
                    continue;
                }
            }
            i = (end < rx_total ? end + 1 : rx_total);
            continue;
        }

        if (c == '&') {
            if (i + 3 < rx_total && rxbuf[i + 1] == 'l' && rxbuf[i + 2] == 't' &&
                rxbuf[i + 3] == ';') {
                flush_word();
                push_ch('<');
                i += 4;
                continue;
            }
            if (i + 3 < rx_total && rxbuf[i + 1] == 'g' && rxbuf[i + 2] == 't' &&
                rxbuf[i + 3] == ';') {
                flush_word();
                push_ch('>');
                i += 4;
                continue;
            }
            if (i + 4 < rx_total && rxbuf[i + 1] == 'a' && rxbuf[i + 2] == 'm' &&
                rxbuf[i + 3] == 'p' && rxbuf[i + 4] == ';') {
                flush_word();
                push_ch('&');
                i += 5;
                continue;
            }
            if (i + 5 < rx_total && rxbuf[i + 1] == 'q' && rxbuf[i + 2] == 'u' &&
                rxbuf[i + 3] == 'o' && rxbuf[i + 4] == 't' && rxbuf[i + 5] == ';') {
                flush_word();
                push_ch('"');
                i += 6;
                continue;
            }
            if (i + 2 < rx_total && rxbuf[i + 1] == '#') {
                int j = i + 2, num = 0, is_hex = 0;
                if (j < rx_total && (rxbuf[j] == 'x' || rxbuf[j] == 'X')) {
                    is_hex = 1;
                    j++;
                }
                int any = 0;
                while (j < rx_total && rxbuf[j] >= '0' && rxbuf[j] <= '9') {
                    num = num * 10 + (rxbuf[j] - '0');
                    any = 1;
                    j++;
                }
                if (!is_hex && any && j < rx_total && rxbuf[j] == ';' &&
                    num > 0 && num < 0x7F) {
                    flush_word();
                    push_ch((unsigned char)num);
                    i = j + 1;
                    continue;
                }
                push_ch('&');
                i++;
                continue;
            }
            push_ch('&');
            i++;
            continue;
        }

        if (c == ' ' || c == '\t' || c == '\r' || c == '\n') {
            flush_word();
            i++;
            continue;
        }
        if (c < 0x20) { /* прочий мусор (NUL и т.п.) */
            i++;
            continue;
        }
        push_ch(c);
        i++;
    }
    flush_word();
    nlines = cur + 1;
    if (nlines > MAX_LINES)
        nlines = MAX_LINES;
    /* Пустые строки по краям не нужны. */
    while (nlines > 1 && lines[nlines - 1][0] == 0)
        nlines--;
}

/* ─── Экран ──────────────────────────────────────────────────────────────── */

static int view_h(void) { return SCREEN_H - 1; }

static void renumber(int top) {
    for (int i = 0; i < nlinks; i++)
        vis_num[i] = 0;
    max_vis = 0;
    for (int i = 0; i < nlinks; i++) {
        if (links[i].line >= top && links[i].line < top + view_h()) {
            max_vis++;
            vis_num[i] = max_vis;
        }
    }
    if (cur_link >= 0 && cur_link < nlinks && !vis_num[cur_link])
        cur_link = -1;
}

static void draw_line(int row, int doc_line) {
    const char *s = (doc_line >= 0 && doc_line < nlines) ? lines[doc_line] : "";
    int attr = 0x0F;
    if (cur_link >= 0 && cur_link < nlinks && links[cur_link].line == doc_line)
        attr = 0x1F; /* строка с курсорной ссылкой подсвечена */
    int colc = 0;
    fill_row(row, 0x0F); /* строка затирается целиком — иначе виден хвост */
    while (*s && colc < TEXT_W)
        put_ch(row, colc++, *s++, attr);
    for (int i = 0; i < nlinks; i++) {
        if (links[i].line != doc_line || !vis_num[i])
            continue;
        int v = vis_num[i];
        int mark = (i == cur_link) ? 0x1E : 0x0E;
        put_ch(row, colc++, '[', mark);
        if (v >= 10)
            put_ch(row, colc++, (char)('0' + (v / 10) % 10), mark);
        put_ch(row, colc++, (char)('0' + v % 10), mark);
        put_ch(row, colc++, ']', mark);
    }
}

static void draw_screen(int top) {
    renumber(top);
    for (int r = 0; r < view_h(); r++) {
        int doc = top + r;
        if (doc < nlines)
            draw_line(r, doc);
        else
            fill_row(r, 0x0F);
    }
}

static void draw_status(int top, const char *msg) {
    int row = SCREEN_H - 1;
    fill_row(row, 0x0B);
    int c = 0;
    c = put_str(row, c, "Ln ", 0x0B);
    c = put_num(row, c, top + 1, 0x0B);
    c = put_str(row, c, "/", 0x0B);
    c = put_num(row, c, nlines, 0x0B);
    if (max_vis > 0) {
        c = put_str(row, c, "  Lnk ", 0x0B);
        if (cur_link >= 0 && cur_link < nlinks)
            c = put_num(row, c, vis_num[cur_link], 0x0B);
        else
            c = put_num(row, c, 0, 0x0B);
        c = put_str(row, c, "/", 0x0B);
        c = put_num(row, c, max_vis, 0x0B);
    }
    if (msg && msg[0]) {
        put_str(row, SCREEN_W - str_len(msg) - 1, msg, 0x1F);
    } else if (cur_link >= 0 && cur_link < nlinks) {
        const char *u = links[cur_link].href;
        int maxw = 44;
        int uc = SCREEN_W - str_len(u) - 1;
        if (uc < 24)
            uc = 24;
        if (str_len(u) > maxw)
            u += str_len(u) - maxw; /* хвост URL важнее начала */
        put_str(row, uc, u, 0x0E);
    }
}

/* Прокрутка так, чтобы курсорная ссылка была в кадре. */
static void ensure_link_visible(int top) {
    int l = links[cur_link].line;
    if (l < top)
        top = l;
    else if (l >= top + view_h())
        top = l - view_h() + 1;
    if (top > nlines - view_h())
        top = nlines - view_h();
    if (top < 0)
        top = 0;
    draw_screen(top);
}

static int next_link(int top, int dir) {
    if (nlinks == 0)
        return -1;
    int cur_num = (cur_link >= 0) ? vis_num[cur_link] : 0;
    int best = -1;
    for (int i = 0; i < nlinks; i++) {
        if (!vis_num[i])
            continue;
        if (dir > 0 && vis_num[i] > cur_num) {
            best = i;
            break;
        }
        if (dir < 0 && vis_num[i] < cur_num)
            best = i;
    }
    if (best < 0)
        best = (dir > 0) ? 0 : nlinks - 1;
    cur_link = best;
    ensure_link_visible(top);
    return best;
}

/* ─── Ввод строки (для goto) ─────────────────────────────────────────────── */

static int prompt_line(const char *prompt, char *out, int outsz) {
    int row = SCREEN_H - 1;
    fill_row(row, 0x1F);
    int c = 0;
    c = put_str(row, c, prompt, 0x1F);
    int n = 0;
    out[0] = 0;
    while (1) {
        int sc = wait_key();
        if (sc == SC_ENTER) {
            out[n] = 0;
            return n > 0;
        }
        if (sc == SC_ESC)
            return 0;
        if (sc == SC_BKSP) {
            if (n > 0) {
                n--;
                c--;
                put_ch(row, c, ' ', 0x1F);
            }
            continue;
        }
        int is_shift = 0;
        char ch = sc_to_char(sc, shift_down);
        if (!ch) {
            /* Цифры/буквы с shift: sc_to_char уже учитывает shift_down. */
            continue;
        }
        is_shift = shift_down;
        (void)is_shift;
        if (n < outsz - 2) {
            out[n++] = ch;
            put_ch(row, c++, ch, 0x1F);
        }
    }
}

/* ─── Загрузка страницы (с редиректами) ──────────────────────────────────── */

/* URL начинается с http-схемы https? */
static int is_https_url(const char *u) {
    static const char p[] = "https://";
    for (int i = 0; p[i]; i++)
        if (lower((unsigned char)u[i]) != p[i])
            return 0;
    return 1;
}

static int fetch(char *url, char *status, int statsz) {
    char host[HOST_MAX], path[PATH_MAX];
    int port;
    for (int hops = 0; hops < 5; hops++) {
        if (parse_url(url, host, sizeof(host), path, sizeof(path), &port) < 0) {
            str_copy(status, "bad url", statsz);
            return 0;
        }
        /* TLS нужен для схемы https и/или порта 443. */
        int tls_mode = is_https_url(url) || port == 443;
        /* Базовый URL для относительных ссылок и редиректов — текущий хоп. */
        str_copy(cur_url, url, sizeof(cur_url));

        char errmsg[96];
        str_copy(errmsg, "connect failed", sizeof(errmsg));
        if (!http_get(host, port, path, tls_mode, errmsg, sizeof(errmsg))) {
            str_copy(status, errmsg, statsz);
            return 0;
        }
        char st[80], loc[URL_MAX];
        int body = parse_response(st, sizeof(st), loc, sizeof(loc));
        str_copy(status, st, statsz);
        /* 3xx с Location -> следующий хоп. */
        if (st[9] == '3' && loc[0]) {
            char next[URL_MAX];
            resolve_href(loc, next, sizeof(next));
            str_copy(url, next, URL_MAX);
            continue;
        }
        build_page(body);
        return 1;
    }
    str_copy(status, "too many redirects", statsz);
    return 0;
}

/* ─── main ───────────────────────────────────────────────────────────────── */

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    char url[URL_MAX];
    char status[128];

    /* Аргументы: [host [port [path]]] — поддерживаем и полный URL. */
    url[0] = 0;
    if (argc >= 4) {
        int port = 0, any = 0;
        for (const char *p = argv[2]; *p; p++) {
            if (*p < '0' || *p > '9')
                break;
            port = port * 10 + (*p - '0');
            if (port > 65535)
                port = 65535;
            any = 1;
        }
        make_url(url, sizeof(url), argv[1], any ? port : 80, argv[3]);
    } else if (argc >= 2) {
        char host[HOST_MAX], path[PATH_MAX];
        int port;
        if (parse_url(argv[1], host, sizeof(host), path, sizeof(path), &port) < 0) {
            char msg[64];
            str_copy(msg, "usage: web <host> [port] [path]", sizeof(msg));
            int r = (int)(vga_offset / 160);
            if (r < 0 || r > SCREEN_H - 2)
                r = 0;
            put_str(r, 0, msg, 0x0F);
            *(volatile int *)EXIT_ROW_P = r + 1;
            *(volatile int *)EXIT_COL_P = 40;
            return;
        }
        make_url(url, sizeof(url), host, port, path);
    } else {
        int r = (int)(vga_offset / 160);
        if (r < 0 || r > SCREEN_H - 2)
            r = 0;
        put_str(r, 0, "usage: web <host> [port] [path]", 0x0F);
        *(volatile int *)EXIT_ROW_P = r + 1;
        *(volatile int *)EXIT_COL_P = 40;
        return;
    }

    for (int r = 0; r < SCREEN_H; r++)
        fill_row(r, 0x0F);

    int top = 0;
    str_copy(cur_url, url, sizeof(cur_url));
    put_str(0, 0, "Loading ", 0x0B);
    put_str(0, 8, cur_url, 0x0B);
    str_copy(status, "", sizeof(status));

    if (!fetch(url, status, sizeof(status))) {
        fill_row(0, 0x0F);
        put_str(0, 0, "web: ", 0x0F);
        put_str(0, 5, status, 0x0F);
        draw_status(0, "press any key");
        wait_key_any();
        *(volatile int *)EXIT_ROW_P = SCREEN_H - 1;
        *(volatile int *)EXIT_COL_P = 0;
        return;
    }

    draw_screen(0);
    draw_status(0, NULL);

    int numbuf = 0; /* накопленный номер ссылки */

    while (1) {
        int sc = wait_key();
        char msg[64];
        msg[0] = 0;

        /* Цифра — накапливаем номер ссылки. Скан-коды 0x02..0x0B = «1»..«0». */
        if (sc >= 0x02 && sc <= 0x0B) {
            int d = (sc == 0x0B) ? 0 : sc - 0x01;
            numbuf = numbuf * 10 + d;
            if (numbuf > 999)
                numbuf = d;
            str_copy(msg, "", sizeof(msg));
            draw_status(top, msg);
            char nb[8];
            int p = 0;
            nb[p++] = '#';
            int v = numbuf;
            char dg[4];
            int dn = 0;
            if (v == 0)
                dg[dn++] = '0';
            while (v > 0) {
                dg[dn++] = (char)('0' + v % 10);
                v /= 10;
            }
            while (dn > 0)
                nb[p++] = dg[--dn];
            nb[p] = 0;
            fill_row(SCREEN_H - 1, 0x1F);
            put_str(SCREEN_H - 1, 0, nb, 0x1F);
            continue;
        }

        /* Enter или '.' — открыть ссылку. */
        if (sc == SC_ENTER || sc == 0x34) {
            int target = -1;
            if (numbuf > 0) {
                for (int i = 0; i < nlinks; i++)
                    if (vis_num[i] == numbuf) {
                        target = i;
                        break;
                    }
            } else if (cur_link >= 0 && vis_num[cur_link]) {
                target = cur_link;
            } else if (max_vis > 0) {
                target = 0;
                for (int i = 0; i < nlinks; i++)
                    if (vis_num[i] == 1) {
                        target = i;
                        break;
                    }
            }
            numbuf = 0;
            if (target >= 0) {
                if (nhist < MAX_HIST)
                    str_copy(hist_url[nhist++], cur_url, URL_MAX);
                char next[URL_MAX];
                str_copy(next, links[target].href, URL_MAX);
                str_copy(cur_url, next, sizeof(cur_url));
                str_copy(url, next, sizeof(url));
                for (int r = 0; r < SCREEN_H; r++)
                    fill_row(r, 0x0F);
                put_str(0, 0, "Loading ", 0x0B);
                put_str(0, 8, cur_url, 0x0B);
                if (!fetch(url, status, sizeof(status))) {
                    fill_row(0, 0x0F);
                    put_str(0, 0, "web: ", 0x0F);
                    put_str(0, 5, status, 0x0F);
                    draw_status(0, "press any key");
                    wait_key_any();
                    /* возвращаемся на прошлую страницу. */
                    if (nhist > 0) {
                        nhist--;
                        str_copy(cur_url, hist_url[nhist], sizeof(cur_url));
                        str_copy(url, cur_url, sizeof(url));
                        for (int r = 0; r < SCREEN_H; r++)
                            fill_row(r, 0x0F);
                        if (!fetch(url, status, sizeof(status))) {
                            draw_status(0, "load failed");
                            wait_key_any();
                            break;
                        }
                    } else {
                        break;
                    }
                }
                top = 0;
                cur_link = -1;
                draw_screen(0);
                draw_status(0, NULL);
                continue;
            }
            draw_status(top, NULL);
            continue;
        }

        /* q — выход. */
        if (sc == 0x10)
            break;

        /* h — назад. */
        if (sc == 0x23 && nhist > 0) {
            nhist--;
            str_copy(cur_url, hist_url[nhist], sizeof(cur_url));
            str_copy(url, cur_url, sizeof(url));
            for (int r = 0; r < SCREEN_H; r++)
                fill_row(r, 0x0F);
            put_str(0, 0, "Loading ", 0x0B);
            put_str(0, 8, cur_url, 0x0B);
            if (!fetch(url, status, sizeof(status))) {
                fill_row(0, 0x0F);
                put_str(0, 0, "web: ", 0x0F);
                put_str(0, 5, status, 0x0F);
                draw_status(0, "press any key");
                wait_key_any();
                break;
            }
            top = 0;
            cur_link = -1;
            draw_screen(0);
            draw_status(0, NULL);
            continue;
        }

        /* g — «го»: ввод адреса. */
        if (sc == 0x22) {
            char buf[URL_MAX];
            if (prompt_line("Go to: ", buf, sizeof(buf))) {
                char h2[HOST_MAX], p2[PATH_MAX];
                int p2n;
                if (parse_url(buf, h2, sizeof(h2), p2, sizeof(p2), &p2n) == 0) {
                    if (nhist < MAX_HIST)
                        str_copy(hist_url[nhist++], cur_url, URL_MAX);
                    char next[URL_MAX];
                    make_url(next, sizeof(next), h2, p2n, p2);
                    str_copy(cur_url, next, sizeof(cur_url));
                    str_copy(url, next, sizeof(url));
                    for (int r = 0; r < SCREEN_H; r++)
                        fill_row(r, 0x0F);
                    put_str(0, 0, "Loading ", 0x0B);
                    put_str(0, 8, cur_url, 0x0B);
                    if (!fetch(url, status, sizeof(status))) {
                        fill_row(0, 0x0F);
                        put_str(0, 0, "web: ", 0x0F);
                        put_str(0, 5, status, 0x0F);
                        draw_status(0, "press any key");
                        wait_key_any();
                        break;
                    }
                    top = 0;
                    cur_link = -1;
                    draw_screen(0);
                    draw_status(0, NULL);
                    continue;
                }
            }
            draw_screen(top);
            draw_status(top, NULL);
            continue;
        }

        switch (sc) {
        case SC_UP:
            if (top > 0)
                top--;
            draw_screen(top);
            break;
        case SC_DOWN:
            if (top + view_h() < nlines)
                top++;
            draw_screen(top);
            break;
        case SC_PGUP:
            top -= view_h();
            if (top < 0)
                top = 0;
            draw_screen(top);
            break;
        case SC_PGDN:
            top += view_h();
            if (top > nlines - view_h())
                top = nlines - view_h();
            if (top < 0)
                top = 0;
            draw_screen(top);
            break;
        case SC_HOME:
            top = 0;
            draw_screen(0);
            break;
        case SC_END:
            top = nlines - view_h();
            if (top < 0)
                top = 0;
            draw_screen(top);
            break;
        case SC_TAB:
        case SC_RIGHT:
            if (nlinks > 0 && cur_link < 0) {
                for (int i = 0; i < nlinks; i++)
                    if (vis_num[i] == 1) {
                        cur_link = i;
                        break;
                    }
                if (cur_link >= 0)
                    ensure_link_visible(top);
            } else {
                next_link(top, 1);
            }
            break;
        case SC_LEFT:
            if (cur_link < 0) {
                for (int i = 0; i < nlinks; i++)
                    if (vis_num[i] == 1) {
                        cur_link = i;
                        break;
                    }
                if (cur_link >= 0)
                    ensure_link_visible(top);
            } else {
                next_link(top, -1);
            }
            break;
        case SC_SPACE:
            if (top + view_h() < nlines) {
                top++;
                draw_screen(top);
            }
            break;
        default:
            break;
        }
        draw_status(top, NULL);
    }

    *(volatile int *)EXIT_ROW_P = SCREEN_H - 1;
    *(volatile int *)EXIT_COL_P = 0;
}
