/* Web: lynx-подобный мини-браузер — HTTP GET через TCP-сокет ядра, рендер
 * текста и ссылок прямо в VGA.
 *
 *   run web                          — открыть start_url из /etc/web.conf
 *   run web <host> [port] [path]     — схема http, порт 80, путь /
 *   run web 10.0.2.2 8080 /test.html
 *   run web -c /tmp/my.conf <url>    — другой конфиг (см. /etc/web.conf)
 *
 * Настройки — в файле /etc/web.conf (`ключ = значение`, см. раздел «Конфиг»
 * ниже): домашняя страница, ширина текста, User-Agent/Accept, таймауты,
 * размер загрузки, глубина истории, нумерация ссылок, строка состояния,
 * печать заголовков ответа, цвет и клавиши. Значения по умолчанию повторяют
 * прежние константы, поэтому отсутствие файла ничего не меняет.
 *
 * Сеть — ABI-трамплины net_* (0x7840..0x7860): connect/send/recv/close и
 * resolve (DNS через UDP в ядре). Сисколы НЕ блокируют CPU: ядро продвигает
 * TCP-состояние за каждый вызов и возвращает -2 (EAGAIN), пока ждёт сети;
 * программа крутит циклы `sleep(ticks+2)` — как и положено cooperative-задаче.
 *
 * Рендер как в lynx: режем HTTP-заголовки, распаковываем chunked, стрипуем
 * теги, декодируем сущности/UTF-8. Блочные теги дают структуру — заголовки и
 * абзацы с отступом и пустой строкой, <ul>/<ol> — буллиты и нумерацию,
 * <pre> сохраняет пробелы, <blockquote> — вложенный отступ. Текст
 * оборачивается по ширине, ссылки подписываются [1], [2]… прямо в строке.
 * Номера выдаются на видимую часть страницы, как в lynx.
 *
 * Клавиши: стрелки ↑↓ — скролл, PgUp/PgDn/Home/End — экран, ←/→ и TAB —
 * предыдущая/следующая ссылка, Enter или «номер.» — открыть ссылку,
 * g — «го» (ввод адреса), h — назад по истории, q — выход.
 * Буквы настраиваются (key_quit/key_back/key_goto).
 *
 * Коды результатов net_*: 0 = ok, -1 = EOF (FIN получен, всё прочитано),
 * -2 = EAGAIN (ещё в процессе / данных нет), -3 = ошибка/RST.
 *
 * HTTPS: для схемы https (порт 443) поверх TCP поднимается TLS 1.3
 * (tls_client.c). Сертификат не проверяется — аналог curl -k. Настройки для
 * этого нет и быть не должно: опции «проверять» не существует.
 */

#include "tls_client.h"
#include "../common/vgasize.h"

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

/* Чтение файла — тот же сискол, что у fetch: без него не прочитать конфиг. */
#define VFS_READ_P 0x7818
typedef int (*fn_vread)(const char *, void *, int);
#define abi_vread(p, b, n) ((*(volatile fn_vread *)VFS_READ_P)(p, b, n))

typedef unsigned long long (*fn_ticks)(void);
typedef int (*fn_sleep)(unsigned long long);
typedef int (*fn_connect)(const char *, unsigned int);
typedef int (*fn_send)(int, const void *, unsigned int);
typedef int (*fn_recv)(int, void *, unsigned int);
typedef int (*fn_close)(int);
typedef int (*fn_state)(int);
typedef int (*fn_resolve)(const char *, unsigned char *, int);
typedef int (*fn_kbhit)(void);
typedef int (*fn_kbread)(void);

#define abi_ticks()        ((*(volatile fn_ticks *)TICKS_P)())
#define abi_sleep(t)       ((*(volatile fn_sleep *)SLEEP_P)(t))
#define abi_connect(h, p)  ((*(volatile fn_connect *)NET_CONNECT_P)(h, p))
#define abi_state(s)      ((*(volatile fn_state *)NET_STATE_P)(s))
#define abi_send(b, l)     ((*(volatile fn_send *)NET_SEND_P)(cursock, b, l))
#define abi_recv(b, m)     ((*(volatile fn_recv *)NET_RECV_P)(cursock, b, m))
#define abi_close()        ((*(volatile fn_close *)NET_CLOSE_P)(cursock))
#define abi_resolve(h, o, m) ((*(volatile fn_resolve *)NET_RESOLVE_P)(h, o, m))
#define NET_STATE_P   0x78A8

/* Текущий сокет — устанавливается в http_get при успешном connect. */
static int cursock = -1;
#define abi_kbhit()        ((*(volatile fn_kbhit *)KBHIT_P)())
#define abi_kbread()       ((*(volatile fn_kbread *)KBREAD_P)())

/* Сетку берём у ядра (programs/common/vgasize.h): на 1280x800 она 80x25,
 * на 1920x1080 — 120x33. Раньше тут стояло жёстко 80x30, и браузер писал
 * мимо экрана. */
#define TEXT_W_MAX 194 /* максимум сетки (200 колонок) минус 6 на « [NN]» */
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

static char lines[MAX_LINES][TEXT_W_MAX + 1];
static int nlines;

struct Link {
    int line;     /* строка документа, где стоит ссылка */
    int col_mark; /* колонка, куда вставить «[N]»; -1 — ссылка ещё открыта */
    char href[HREF_MAX];
};
static struct Link links[MAX_LINKS];
static int nlinks;
static int open_link = -1; /* индекс ссылки между <a> и </a> */

static int vis_num[MAX_LINKS]; /* ссылка -> видимый номер 1.., 0 = не видна */
static int max_vis;
static int cur_link = -1; /* выбранная (курсорная) ссылка */

static char hist_url[MAX_HIST][URL_MAX];
static int nhist;

/* ─── VGA-примитивы ──────────────────────────────────────────────────────── */

static void put_ch(int row, int colc, char ch, unsigned char attr) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    if (row < 0 || (unsigned)row >= g_screen_h || colc < 0 ||
        (unsigned)colc >= g_screen_w)
        return;
    int off = row * (int)(g_screen_w * 2) + colc * 2;
    vga[off] = (unsigned char)ch;
    vga[off + 1] = attr;
}

static int put_str(int row, int colc, const char *s, unsigned char attr) {
    while (*s && (unsigned)colc < g_screen_w) {
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
    for (unsigned c = 0; c < g_screen_w; c++)
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

/* ─── Конфиг ────────────────────────────────────────────────────────────────
 *
 * Файл /etc/web.conf, формат `ключ = значение`, `#` и `;` — комментарии:
 *
 *   start_url       = http://10.0.2.2:8080/  что открывать без аргументов
 *   width           = 74                     ширина текста, 40..ширина сетки-6
 *   user_agent      = VitaminOS-web/1.0      заголовок User-Agent
 *   accept          = text/html              заголовок Accept
 *   accept_encoding =                        пусто = заголовок не шлём
 *   default_port    = 80                     порт без схемы в адресе
 *   timeout_connect = 800                    тики PIT (100 Гц) = 8 с
 *   timeout_send    = 800
 *   timeout_recv    = 4000                   40 с
 *   max_download    = 0                      байт; 0 = сколько влезет
 *   max_redirects   = 5
 *   history         = 12                     глубина «назад», 0..12
 *   link_numbers    = on                     подписи [N] у ссылок
 *   status_bar      = on                     строка состояния внизу
 *   show_url        = on                     URL курсорной ссылки в строке
 *   show_headers    = off                    печатать заголовки ответа
 *   color           = on                     off = монохром
 *   key_quit        = q                      клавиша выхода
 *   key_back        = h
 *   key_goto        = g
 *
 * Значения по умолчанию — ровно те, что раньше были константами, поэтому
 * отсутствие файла ничего не меняет. `web -c PATH` читает другой файл.
 *
 * accept_encoding по умолчанию пуст не по недосмотру: браузер не умеет
 * gzip/deflate, а объявить кодировку и не распаковать — получить битый текст.
 */

/* Ширина текста: TEXT_W_MAX — потолок буфера строк (место под « [NN]»
 * справа), text_w — текущая, по конфигу и сетке.
 *
 * Ширина считается от сетки, а не от исторических 74: браузер, который
 * знает только про 80x30, на 120x33 отдавал бы четверть экрана пустотой
 * справа, а на 80x25 вылезал бы за правый край. */
static int text_w = 74;

static int default_text_w(void) {
    int w = (int)g_screen_w - 6; /* « [NN]» справа */
    if (w > TEXT_W_MAX)
        w = TEXT_W_MAX;
    if (w < 40)
        w = 40;
    return w;
}

struct WebCfg {
    char start_url[URL_MAX];
    char user_agent[96];
    char accept[64];
    char accept_encoding[48];
    int width;
    int default_port;
    int timeout_connect;
    int timeout_send;
    int timeout_recv;
    int max_download;
    int max_redirects;
    int history;
    int link_numbers;
    int status_bar;
    int show_url;
    int show_headers;
    int color;
    char key_quit;
    char key_back;
    char key_goto;
};

static struct WebCfg cfg;

static void cfg_defaults(void) {
    cfg.start_url[0] = 0;
    str_copy(cfg.user_agent, "VitaminOS-web/1.0", sizeof(cfg.user_agent));
    str_copy(cfg.accept, "text/html", sizeof(cfg.accept));
    cfg.accept_encoding[0] = 0;
    cfg.width = default_text_w();
    cfg.default_port = 80;
    cfg.timeout_connect = 800;
    cfg.timeout_send = 800;
    cfg.timeout_recv = 4000;
    cfg.max_download = 0;
    cfg.max_redirects = 5;
    cfg.history = MAX_HIST;
    cfg.link_numbers = 1;
    cfg.status_bar = 1;
    cfg.show_url = 1;
    cfg.show_headers = 0;
    cfg.color = 1;
    cfg.key_quit = 'q';
    cfg.key_back = 'h';
    cfg.key_goto = 'g';
}

/* Сравнение без учёта регистра: имена ключей в конфиге пишут как удобнее. */
static int str_ieq(const char *a, const char *b) {
    int i = 0;
    for (; a[i] && b[i]; i++) {
        int x = a[i], y = b[i];
        if (x >= 'A' && x <= 'Z')
            x += 32;
        if (y >= 'A' && y <= 'Z')
            y += 32;
        if (x != y)
            return 0;
    }
    return a[i] == 0 && b[i] == 0;
}

static int is_space_c(int c) {
    return c == ' ' || c == '\t' || c == '\r' || c == '\n';
}

static int is_digit(int c) { return c >= '0' && c <= '9'; }

/* on/off/yes/no/1/0 — всё, что человек напишет в конфиге. */
static int cfg_bool(const char *v, int dflt) {
    if (!v[0])
        return dflt;
    if (str_ieq(v, "on") || str_ieq(v, "yes") || str_ieq(v, "1") ||
        str_ieq(v, "true"))
        return 1;
    if (str_ieq(v, "off") || str_ieq(v, "no") || str_ieq(v, "0") ||
        str_ieq(v, "false"))
        return 0;
    return dflt;
}

/* Число с необязательным знаком; мусор -> dflt (а не 0: молчаливый ноль в
 * timeout_recv означал бы мгновенный отказ от страницы). */
static int cfg_int(const char *v, int lo, int hi, int dflt) {
    while (is_space_c(*v))
        v++;
    if (!is_digit(*v))
        return dflt;
    long n = 0;
    while (is_digit(*v)) {
        n = n * 10 + (*v - '0');
        if (n > 1000000000L)
            n = 1000000000L;
        v++;
    }
    if (n < lo)
        return lo;
    if (n > hi)
        return hi;
    return (int)n;
}

/* Один символ значения: для key_quit/key_back/key_goto. */
static char cfg_char(const char *v, char dflt) {
    while (is_space_c(*v))
        v++;
    if (!v[0] || v[1])
        return dflt; /* пусто или больше одного символа — оставляем как было */
    return v[0];
}

/* Разбор одной строки `ключ = значение`. */
static void cfg_apply(const char *key, const char *val) {
    if (str_ieq(key, "start_url"))
        str_copy(cfg.start_url, val, sizeof(cfg.start_url));
    else if (str_ieq(key, "width"))
        cfg.width = cfg_int(val, 40, TEXT_W_MAX, cfg.width);
    else if (str_ieq(key, "user_agent"))
        str_copy(cfg.user_agent, val, sizeof(cfg.user_agent));
    else if (str_ieq(key, "accept"))
        str_copy(cfg.accept, val, sizeof(cfg.accept));
    else if (str_ieq(key, "accept_encoding"))
        str_copy(cfg.accept_encoding, val, sizeof(cfg.accept_encoding));
    else if (str_ieq(key, "default_port"))
        cfg.default_port = cfg_int(val, 1, 65535, cfg.default_port);
    else if (str_ieq(key, "timeout_connect"))
        cfg.timeout_connect = cfg_int(val, 20, 60000, cfg.timeout_connect);
    else if (str_ieq(key, "timeout_send"))
        cfg.timeout_send = cfg_int(val, 20, 60000, cfg.timeout_send);
    else if (str_ieq(key, "timeout_recv"))
        cfg.timeout_recv = cfg_int(val, 20, 60000, cfg.timeout_recv);
    else if (str_ieq(key, "max_download"))
        cfg.max_download = cfg_int(val, 0, RX_CAP, cfg.max_download);
    else if (str_ieq(key, "max_redirects"))
        cfg.max_redirects = cfg_int(val, 1, 16, cfg.max_redirects);
    else if (str_ieq(key, "history"))
        cfg.history = cfg_int(val, 0, MAX_HIST, cfg.history);
    else if (str_ieq(key, "link_numbers"))
        cfg.link_numbers = cfg_bool(val, cfg.link_numbers);
    else if (str_ieq(key, "status_bar"))
        cfg.status_bar = cfg_bool(val, cfg.status_bar);
    else if (str_ieq(key, "show_url"))
        cfg.show_url = cfg_bool(val, cfg.show_url);
    else if (str_ieq(key, "show_headers"))
        cfg.show_headers = cfg_bool(val, cfg.show_headers);
    else if (str_ieq(key, "color"))
        cfg.color = cfg_bool(val, cfg.color);
    else if (str_ieq(key, "key_quit"))
        cfg.key_quit = cfg_char(val, cfg.key_quit);
    else if (str_ieq(key, "key_back"))
        cfg.key_back = cfg_char(val, cfg.key_back);
    else if (str_ieq(key, "key_goto"))
        cfg.key_goto = cfg_char(val, cfg.key_goto);
    /* Неизвестный ключ молча пропускаем: конфиг переживает добавление
     * параметров в новую версию web, лишняя строка не должна ломать запуск. */
}

/* Читает и разбирает конфиг. Отсутствие файла — не ошибка. */
static void load_config(const char *path) {
    static char buf[4096];
    static char pbuf[64];
    int pn = str_len(path);
    if (pn <= 0 || pn >= (int)sizeof(pbuf))
        return;
    for (int i = 0; i <= pn; i++)
        pbuf[i] = path[i];
    int n = abi_vread(pbuf, buf, (int)sizeof(buf) - 1);
    if (n <= 0)
        return;
    buf[n] = 0;

    int i = 0;
    while (i < n) {
        int start = i;
        while (i < n && buf[i] != '\n')
            i++;
        int end = i;
        if (i < n)
            i++;
        /* Обрезаем \r и хвостовые пробелы, ведущие — пропускаем. */
        while (end > start && is_space_c(buf[end - 1]))
            end--;
        int s = start;
        while (s < end && is_space_c(buf[s]))
            s++;
        if (s >= end || buf[s] == '#' || buf[s] == ';')
            continue;
        /* Ищем '=' в строке целиком: в значении он тоже допустим. */
        int eq = -1;
        for (int k = s; k < end; k++)
            if (buf[k] == '=') {
                eq = k;
                break;
            }
        if (eq < 0)
            continue;
        int ke = eq;
        while (ke > s && is_space_c(buf[ke - 1]))
            ke--;
        int vs = eq + 1;
        while (vs < end && is_space_c(buf[vs]))
            vs++;
        buf[ke] = 0;
        buf[end] = 0;
        cfg_apply(buf + s, buf + vs);
    }
}

/* Цветовой атрибут с учётом color=off: весь интерфейс становится серым. */
static unsigned char att(unsigned char a) {
    return cfg.color ? a : 0x07;
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
    /* Порт по умолчанию берём из конфига: если у сервера он не 80, не надо
     * каждый раз дописывать «:8080» в адрес. Для https он всё равно 443. */
    int def_port = cfg.default_port;
    host[0] = 0;
    path[0] = 0;
    *port_out = cfg.default_port;
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

/* Схема URL, если она есть: «http» или «https». Иначе — «http».
 *
 * Схему нельзя терять при пересборке URL: «https://host:8443/» без неё
 * превратился бы в «http://host:8443/», и браузер пошёл бы на 8443
 * обычным HTTP вместо TLS. */
static void url_scheme(const char *u, char *out, int outsz) {
    int i = 0;
    str_copy(out, "http", (int)sizeof("https"));
    if (!u)
        return;
    for (; u[i] && u[i] != ':' && i < (int)sizeof("https") - 1; i++) {
        if (u[i] == '/' || u[i] == ' ' || u[i] == '\t')
            return; /* схемы нет */
    }
    if (u[i] != ':' || u[i + 1] != '/' || u[i + 2] != '/')
        return;
    /* Известны только два — никаких выдуманных протоколов. */
    if ((i == 4 && lower(u[0]) == 'h' && lower(u[1]) == 't' && lower(u[2]) == 't' &&
         lower(u[3]) == 'p') ||
        (i == 5 && lower(u[0]) == 'h' && lower(u[1]) == 't' && lower(u[2]) == 't' &&
         lower(u[3]) == 'p' && lower(u[4]) == 's')) {
        for (int k = 0; k <= i && k < outsz - 1; k++)
            out[k] = lower(u[k]);
        out[i] = 0;
    }
}

/* Разрешает href относительно текущего URL (cur_url) в абсолютный. */
static void make_url(char *out, int outsz, const char *host, int port,
                     const char *path);
static void make_url_sch(char *out, int outsz, const char *scheme,
                        const char *host, int port, const char *path);

static void resolve_href(const char *href, char *out, int outsz) {
    char base[HOST_MAX];
    char bpath[PATH_MAX];
    char sch[8];
    int bport;
    url_scheme(cur_url, sch, sizeof(sch));
    if (parse_url(cur_url, base, sizeof(base), bpath, sizeof(bpath), &bport) < 0) {
        str_copy(out, href, outsz);
        return;
    }
    /* Якорь. */
    if (href[0] == '#') {
        make_url_sch(out, outsz, sch, base, bport, bpath);
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
        const char *p = "://";
        int n = 0;
        for (const char *q = sch; *q && n < outsz - 1; q++)
            out[n++] = *q;
        for (; *p && n < outsz - 1; )
            out[n++] = *p++;
        for (p = href + 2; *p && n < outsz - 1; )
            out[n++] = *p++;
        out[n] = 0;
        return;
    }
    /* Абсолютный путь — префиксуем хостом базового URL. */
    if (href[0] == '/') {
        make_url_sch(out, outsz, sch, base, bport, href);
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
    make_url_sch(out, outsz, sch, base, bport, norm);
}

static void make_url(char *out, int outsz, const char *host, int port,
                     const char *path) {
    make_url_sch(out, outsz, "http", host, port, path);
}

static void make_url_sch(char *out, int outsz, const char *scheme,
                        const char *host, int port, const char *path) {
    int n = 0;
    int https = 0;
    if (scheme && str_len(scheme) == 5 && lower(scheme[0]) == 'h' &&
        lower(scheme[1]) == 't' && lower(scheme[2]) == 't' &&
        lower(scheme[3]) == 'p' && lower(scheme[4]) == 's')
        https = 1;
    const char *p = scheme;
    for (; *p && n < outsz - 1; )
        out[n++] = *p++;
    for (p = "://"; *p && n < outsz - 1; )
        out[n++] = *p++;
    for (p = host; *p && n < outsz - 1; )
        out[n++] = *p++;
    /* Порт по умолчанию для схемы не печатаем. */
    if (port != 0 && port != (https ? 443 : 80)) {
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
    unsigned long long deadline = abi_ticks() + cfg.timeout_send;
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
    unsigned long long deadline = abi_ticks() + cfg.timeout_recv;
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
    unsigned long long deadline = abi_ticks() + cfg.timeout_connect;
    while (1) {
        int sid = abi_connect(ip, (unsigned int)port);
        if (sid < 0) {
            str_copy(errmsg, "connection refused/failed", errsz);
            return 0;
        }
        cursock = sid;
        int sc = abi_state(cursock);
        if (sc == 3) /* Established */ break;
        if (sc == 6 || sc < 0) {
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

    char req[1024];
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
    /* Порт в Host пишем, когда он не дефолтный для СХЕМЫ (не для конфига):
     * сервер отличает virtual host по «host:port», и молчаливый 8080 здесь
     * приводит к 404 на половине сайтов. */
    if (port != (tls_mode ? 443 : 80)) {
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
    /* Заголовки из конфига. accept_encoding пустой — не шлём вовсе: сам
     * браузер gzip не распаковывает, объявить кодировку = отдать битый текст. */
    m = "\r\nUser-Agent: ";
    while (*m)
        req[rr++] = *m++;
    for (const char *u = cfg.user_agent; *u && rr < (int)sizeof(req) - 2; )
        req[rr++] = *u++;
    m = "\r\nAccept: ";
    while (*m)
        req[rr++] = *m++;
    for (const char *u = cfg.accept; *u && rr < (int)sizeof(req) - 2; )
        req[rr++] = *u++;
    if (cfg.accept_encoding[0]) {
        m = "\r\nAccept-Encoding: ";
        while (*m)
            req[rr++] = *m++;
        for (const char *u = cfg.accept_encoding;
             *u && rr < (int)sizeof(req) - 2; )
            req[rr++] = *u++;
    }
    /* HTTP/1.0 + Connection: close: без закрытия сервер держит соединение,
     * а ждать конца потока мы не умеем (таймаут recv - это отказ). */
    m = "\r\nConnection: close\r\n\r\n";
    while (*m && rr < (int)sizeof(req))
        req[rr++] = *m++;
    if (rr >= (int)sizeof(req)) {
        str_copy(errmsg, "request too long", errsz);
        abi_close();
        return 0;
    }

    /* Сколько байт принимать: конфиг может ограничить (max_download). */
    int cap = cfg.max_download > 0 ? cfg.max_download : RX_CAP;

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
        while (rx_total < cap) {
            int n = tls_read(&tls, (uint8_t *)(rxbuf + rx_total),
                             (size_t)(cap - rx_total));
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
        deadline = abi_ticks() + cfg.timeout_recv;
        while (1) {
            int n = abi_recv(rxbuf + rx_total, (unsigned int)(cap - rx_total));
            if (n > 0) {
                rx_total += n;
                if (rx_total >= cap)
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
static int col;  /* текущая колонка на строке cur */
static int cur;  /* текущая строка документа */
static int indent;      /* левое поле переносимого текста */
static int want_blank;  /* нужна пустая строка перед следующим блоком */
static int pre_mode;    /* внутри <pre>: пробелы и переводы строк сохраняются */
static unsigned char line_used[MAX_LINES]; /* на строке есть текст */
static int indent_stack[16];
static int indent_sp;

static void clear_line(int k) {
    for (int i = 0; i <= text_w; i++)
        lines[k][i] = 0;
    line_used[k] = 0;
}

/* Заполнить левое поле на текущей (пустой) строке. */
static void apply_indent(void) {
    for (int i = 0; i < indent && i < text_w; i++)
        lines[cur][i] = ' ';
    col = indent;
    lines[cur][col] = 0;
}

static void new_line(void) {
    if (cur < MAX_LINES - 1) {
        cur++;
        clear_line(cur);
        apply_indent();
    }
}

/* Сменить отступ; если строка ещё пуста — перерисовать её поле. */
static void set_indent(int v) {
    if (v < 0)
        v = 0;
    if (v > text_w)
        v = text_w;
    indent = v;
    if (cur < MAX_LINES && !line_used[cur]) {
        clear_line(cur);
        apply_indent();
    }
}

static void push_indent(int add) {
    if (indent_sp < 16)
        indent_stack[indent_sp++] = indent;
    set_indent(indent + add);
}

static void pop_indent(void) {
    if (indent_sp > 0)
        set_indent(indent_stack[--indent_sp]);
}

static void flush_word(void) {
    if (wl == 0)
        return;
    if (cur >= MAX_LINES) {
        wl = 0;
        return;
    }
    if (line_used[cur] && col + 1 + wl > text_w)
        new_line();
    else if (line_used[cur])
        lines[cur][col++] = ' ';
    for (int i = 0; i < wl && col < text_w; i++)
        lines[cur][col++] = word[i];
    lines[cur][col] = 0; /* без терминатора строка читается до хвоста */
    line_used[cur] = 1;
    if (col >= text_w)
        new_line();
    wl = 0;
}

/* Записать один символ прямо в строку (pre, буллиты, маркеры). */
static void line_put(char c) {
    if (cur >= MAX_LINES)
        return;
    if (col >= text_w) {
        if (cur >= MAX_LINES - 1)
            return; /* последняя строка заполнена — писать больше некуда */
        new_line();
        if (col >= text_w)
            return;
    }
    lines[cur][col++] = c;
    lines[cur][col] = 0;
    line_used[cur] = 1;
}

static void line_puts(const char *s) {
    while (*s)
        line_put(*s++);
}

static void line_put_num(int v) {
    char b[12];
    int n = 0;
    if (v <= 0) {
        line_put('0');
        return;
    }
    while (v > 0 && n < 11) {
        b[n++] = (char)('0' + v % 10);
        v /= 10;
    }
    while (n > 0)
        line_put(b[--n]);
}

static void push_ch(unsigned char c) {
    if (pre_mode) {
        line_put((char)c);
        return;
    }
    if (c >= 0x20 && c < 0x7F && wl < 79)
        word[wl++] = (char)c;
}

/* Перейти на новую строку, если текущая не пуста (без пустой вставки). */
static void hard_break(void) {
    flush_word();
    if (line_used[cur])
        new_line();
}

/* Запросить пустую строку-разделитель перед следующим блоком. */
static void request_blank(void) { want_blank = 1; }

/* Начать новый блок: закончить строку и, если запрошено, вставить пустую. */
static void block_start(void) {
    flush_word();
    if (line_used[cur])
        new_line();
    if (want_blank && cur > 0)
        new_line();
    want_blank = 0;
}

static void finish_link(void) {
    if (open_link < 0)
        return;
    /* Ставим «[N]» сразу после текста ссылки, не разрывая слово: следующие
     * символы (точка и т.п.) остаются в том же слове и не получают пробел. */
    int start = line_used[cur] ? col + 1 : col;
    if (cur >= MAX_LINES || (line_used[cur] && col + 1 + wl > text_w)) {
        flush_word();
        start = col;
    } else {
        start += wl;
    }
    if (start < 0)
        start = 0;
    if (start > text_w)
        start = text_w;
    links[open_link].line = cur;
    links[open_link].col_mark = start;
    open_link = -1;
}

static int add_link(const char *href) {
    if (nlinks >= MAX_LINKS || !href || !href[0])
        return -1;
    char abs[URL_MAX];
    resolve_href(href, abs, sizeof(abs));
    links[nlinks].line = cur;
    links[nlinks].col_mark = -1;
    str_copy(links[nlinks].href, abs, HREF_MAX);
    nlinks++;
    return nlinks - 1;
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

static int is_heading(const char *name, int nl) {
    return tag_is(name, nl, "h1") || tag_is(name, nl, "h2") ||
           tag_is(name, nl, "h3") || tag_is(name, nl, "h4") ||
           tag_is(name, nl, "h5") || tag_is(name, nl, "h6");
}

/* Контейнерные блоки: вызывают разрыв строки, но не добавляют пустую. */
static int is_container_tag(const char *name, int nl) {
    static const char *tags[] = { "div",  "section", "header", "footer",
                                  "article", "nav", "main",  "aside",
                                  "form", "figure", "figcaption", "center" };
    for (unsigned i = 0; i < sizeof(tags) / sizeof(tags[0]); i++)
        if (tag_is(name, nl, tags[i]))
            return 1;
    return 0;
}

/* Стек списков для нумерации <ol>. */
struct ListCtx {
    int ordered;
    int counter;
};
static struct ListCtx lstk[8];
static int ldepth;

/* Разбор сущности &name; / &#N; / &#xN;. Возвращает 1 и кладёт ASCII в out
 * (outn символов), consumed — сколько байт от '&' израсходовано. */
static int decode_entity(const unsigned char *b, int i, int n, char *out,
                         int *outn, int *consumed) {
    *outn = 0;
    if (i + 1 >= n || b[i] != '&')
        return 0;
    int j = i + 1;
    if (b[j] == '#') {
        j++;
        int hex = 0;
        if (j < n && (b[j] == 'x' || b[j] == 'X')) {
            hex = 1;
            j++;
        }
        int base = hex ? 16 : 10;
        long v = 0;
        int any = 0;
        while (j < n) {
            int d = -1;
            if (b[j] >= '0' && b[j] <= '9')
                d = b[j] - '0';
            else if (hex && b[j] >= 'a' && b[j] <= 'f')
                d = b[j] - 'a' + 10;
            else if (hex && b[j] >= 'A' && b[j] <= 'F')
                d = b[j] - 'A' + 10;
            if (d < 0 || d >= base)
                break;
            v = v * base + d;
            if (v > 0x10FFFF)
                v = 0x10FFFF;
            any = 1;
            j++;
        }
        if (!any || j >= n || b[j] != ';')
            return 0;
        *consumed = (j + 1) - i;
        if (v == 0xA0)
            out[(*outn)++] = ' ';
        else if (v == 0x2018 || v == 0x2019)
            out[(*outn)++] = '\'';
        else if (v == 0x201C || v == 0x201D)
            out[(*outn)++] = '"';
        else if (v == 0x2013 || v == 0x2014)
            out[(*outn)++] = '-';
        else if (v == 0x2026) {
            out[(*outn)++] = '.';
            out[(*outn)++] = '.';
            out[(*outn)++] = '.';
        } else if (v == 0x00B7)
            out[(*outn)++] = '*';
        else if (v >= 0x20 && v < 0x7F)
            out[(*outn)++] = (char)v;
        else
            out[(*outn)++] = ' '; /* неизобразимое — пробел вместо склейки */
        return 1;
    }
    static const struct {
        const char *name;
        const char *rep;
    } ents[] = {
        { "nbsp", " " },   { "amp", "&" },    { "lt", "<" },   { "gt", ">" },
        { "quot", "\"" },  { "apos", "'" },   { "copy", "(c)" }, { "reg", "(r)" },
        { "hellip", "..." }, { "mdash", "-" }, { "ndash", "-" }, { "lsquo", "'" },
        { "rsquo", "'" },  { "ldquo", "\"" }, { "rdquo", "\"" }, { "middot", "*" },
        { "bull", "*" },   { "sect", "S" },   { "para", "P" },  { "times", "x" },
    };
    int start = j;
    while (j < n && j - start < 12 && b[j] != ';' && b[j] != '&' && b[j] != ' ' &&
           b[j] != '\n')
        j++;
    if (j >= n || b[j] != ';')
        return 0;
    int len = j - start;
    for (unsigned e = 0; e < sizeof(ents) / sizeof(ents[0]); e++) {
        if ((int)str_len(ents[e].name) != len)
            continue;
        int ok = 1;
        for (int q = 0; q < len; q++) {
            char a = (char)b[start + q];
            if (a >= 'A' && a <= 'Z')
                a = (char)(a + 32);
            if (a != ents[e].name[q]) {
                ok = 0;
                break;
            }
        }
        if (ok) {
            str_copy(out, ents[e].rep, 8);
            *outn = str_len(out);
            *consumed = (j + 1) - i;
            return 1;
        }
    }
    return 0;
}

/* Текст заголовков ответа (show_headers = on). Заполняется в fetch() из
 * rxbuf до того, как декодер chunked перезапишет его с начала. */
static char hdr_text[2048];

static void build_page(int body_start) {
    wl = 0;
    cur = 0;
    indent = 3; /* как lynx: текст абзаца с отступом */
    want_blank = 0;
    pre_mode = 0;
    indent_sp = 0;
    ldepth = 0;
    open_link = -1;
    /* Буферы строк переиспользуются между страницами — чистим, иначе на
     * экране остаётся хвост предыдущего документа. */
    for (int r = 0; r < MAX_LINES; r++)
        clear_line(r);
    col = 0;
    apply_indent();
    nlinks = 0;

    /* show_headers: печатаем строку статуса и заголовки как обычный текст
     * документа — они участвуют в нумерации строк и скролле, как в lynx. */
    if (cfg.show_headers && hdr_text[0]) {
        int hn = 0;
        for (const char *p = hdr_text; *p && hn < 60; ) {
            const char *e = p;
            while (*e && *e != '\n')
                e++;
            for (const char *q = p; q < e; q++)
                push_ch((unsigned char)*q);
            flush_word();
            if (col)
                hard_break();
            hn++;
            p = *e ? e + 1 : e;
        }
        request_blank();
    }

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
                const char *skip_needle = NULL;
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
                    skip_needle = "</script";
                else if (!closing && tag_is(name, nl, "style"))
                    skip_needle = "</style";
                else if (!closing && tag_is(name, nl, "title"))
                    skip_needle = "</title"; /* lynx не печатает <title> в теле */
                else if (tag_is(name, nl, "br")) {
                    finish_link();
                    flush_word();
                    new_line(); /* явный разрыв: <br><br> даёт пустую строку */
                } else if (tag_is(name, nl, "hr")) {
                    finish_link();
                    block_start();
                    for (int k = indent; k < text_w; k++)
                        line_put('-');
                    hard_break();
                    request_blank();
                } else if (!closing && tag_is(name, nl, "img")) {
                    if (attr_value(tag, taglen, "alt", href_buf,
                                   sizeof(href_buf)) && href_buf[0]) {
                        finish_link();
                        flush_word();
                        line_put('[');
                        line_puts(href_buf);
                        line_put(']');
                    }
                } else if (tag_is(name, nl, "a")) {
                    finish_link(); /* закрывает предыдущую/текущую ссылку */
                    if (!closing && attr_value(tag, taglen, "href", href_buf,
                                               sizeof(href_buf))) {
                        flush_word();
                        open_link = add_link(href_buf);
                    }
                } else if (tag_is(name, nl, "p")) {
                    finish_link();
                    if (closing) {
                        hard_break();
                        request_blank();
                    } else {
                        block_start();
                        request_blank(); /* пустая строка после абзаца */
                    }
                } else if (is_heading(name, nl)) {
                    finish_link();
                    if (closing) {
                        hard_break();
                        pop_indent();
                        request_blank();
                    } else {
                        block_start();
                        push_indent(-indent); /* заголовок без отступа */
                        request_blank();
                    }
                } else if (tag_is(name, nl, "ul") || tag_is(name, nl, "ol")) {
                    finish_link();
                    if (closing) {
                        hard_break();
                        if (ldepth > 0)
                            ldepth--;
                        pop_indent();
                        request_blank();
                    } else {
                        if (ldepth == 0)
                            block_start();
                        else
                            hard_break();
                        if (ldepth < 8) {
                            lstk[ldepth].ordered = tag_is(name, nl, "ol");
                            lstk[ldepth].counter = 0;
                            ldepth++;
                        }
                        push_indent(2);
                    }
                } else if (tag_is(name, nl, "li")) {
                    finish_link();
                    if (closing) {
                        hard_break();
                    } else {
                        hard_break();
                        want_blank = 0;
                        int ord = (ldepth > 0) ? lstk[ldepth - 1].ordered : 0;
                        if (ord && ldepth > 0) {
                            lstk[ldepth - 1].counter++;
                            line_put_num(lstk[ldepth - 1].counter);
                            line_puts(".");
                        } else {
                            line_puts("*");
                        }
                    }
                } else if (tag_is(name, nl, "blockquote")) {
                    finish_link();
                    if (closing) {
                        hard_break();
                        pop_indent();
                        request_blank();
                    } else {
                        block_start();
                        push_indent(3);
                    }
                } else if (tag_is(name, nl, "pre")) {
                    finish_link();
                    if (closing) {
                        hard_break();
                        pre_mode = 0;
                        pop_indent();
                        request_blank();
                    } else {
                        block_start();
                        pre_mode = 1;
                        push_indent(3);
                    }
                } else if (tag_is(name, nl, "table")) {
                    finish_link();
                    if (closing) {
                        hard_break();
                        request_blank();
                    } else {
                        block_start();
                    }
                } else if (tag_is(name, nl, "tr")) {
                    finish_link();
                    hard_break();
                } else if (tag_is(name, nl, "td") || tag_is(name, nl, "th")) {
                    if (closing) {
                        flush_word();
                    } else {
                        flush_word();
                        if (line_used[cur])
                            line_puts("  ");
                    }
                } else if (tag_is(name, nl, "dl")) {
                    finish_link();
                    if (closing)
                        request_blank();
                    else
                        block_start();
                } else if (tag_is(name, nl, "dd")) {
                    finish_link();
                    if (closing) {
                        hard_break();
                        pop_indent();
                    } else {
                        block_start();
                        push_indent(3);
                    }
                } else if (tag_is(name, nl, "dt")) {
                    finish_link();
                    if (closing)
                        hard_break();
                    else
                        block_start();
                } else if (is_container_tag(name, nl)) {
                    finish_link();
                    if (closing)
                        hard_break();
                    else
                        block_start();
                }
                if (skip_needle) {
                    int nl2 = str_len(skip_needle);
                    while (i < rx_total) {
                        if (rxbuf[i] == '<' && i + nl2 <= rx_total) {
                            int ok = 1;
                            for (int k = 0; k < nl2; k++)
                                if ((char)rxbuf[i + k] != skip_needle[k]) {
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
            char ent[8];
            int en = 0, used = 0;
            if (decode_entity(rxbuf, i, rx_total, ent, &en, &used)) {
                for (int k = 0; k < en; k++)
                    push_ch((unsigned char)ent[k]);
                i += used;
                continue;
            }
            push_ch('&');
            i++;
            continue;
        }

        if (c == ' ' || c == '\t' || c == '\r' || c == '\n') {
            if (pre_mode) {
                if (c == '\n' || c == '\r')
                    hard_break();
                else
                    line_put(' ');
            } else {
                flush_word();
            }
            i++;
            continue;
        }
        if (c < 0x20) { /* прочий мусор (NUL и т.п.) */
            i++;
            continue;
        }
        if (c >= 0x80) { /* UTF-8: общие знаки -> ASCII, остальное -> пробел */
            int cp = -1, len = 1;
            if ((c & 0xE0) == 0xC0 && i + 1 < rx_total) {
                cp = ((c & 0x1F) << 6) | (rxbuf[i + 1] & 0x3F);
                len = 2;
            } else if ((c & 0xF0) == 0xE0 && i + 2 < rx_total) {
                cp = ((c & 0x0F) << 12) | ((rxbuf[i + 1] & 0x3F) << 6) |
                     (rxbuf[i + 2] & 0x3F);
                len = 3;
            } else if ((c & 0xF8) == 0xF0 && i + 3 < rx_total) {
                cp = ((c & 0x07) << 18) | ((rxbuf[i + 1] & 0x3F) << 12) |
                     ((rxbuf[i + 2] & 0x3F) << 6) | (rxbuf[i + 3] & 0x3F);
                len = 4;
            }
            if (cp == 0x2018 || cp == 0x2019)
                push_ch('\'');
            else if (cp == 0x201C || cp == 0x201D)
                push_ch('"');
            else if (cp == 0x2013 || cp == 0x2014)
                push_ch('-');
            else if (cp == 0x2026) {
                push_ch('.');
                push_ch('.');
                push_ch('.');
            } else
                push_ch(' ');
            i += len;
            continue;
        }
        push_ch(c);
        i++;
    }
    flush_word();
    finish_link();
    nlines = cur + 1;
    if (nlines > MAX_LINES)
        nlines = MAX_LINES;
    /* Пустые строки по краям не нужны. */
    while (nlines > 1 && !line_used[nlines - 1])
        nlines--;
}

/* ─── Экран ──────────────────────────────────────────────────────────────── */

/* Высота документа: строка состояния (если включена) ею не делится. */
static int view_h(void) {
    return cfg.status_bar ? (int)g_screen_h - 1 : (int)g_screen_h;
}

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
    int attr = att(0x0F);
    if (cur_link >= 0 && cur_link < nlinks && links[cur_link].line == doc_line)
        attr = att(0x1F); /* строка с курсорной ссылкой подсвечена */
    int colc = 0;
    fill_row(row, attr); /* строка затирается целиком — иначе виден хвост */
    /* Идём по символам строки, вставляя «[N]» в записанные колонки. */
    for (int pos = 0; pos <= text_w && colc < (int)g_screen_w; pos++) {
        for (int i = 0; cfg.link_numbers && i < nlinks; i++) {
            if (links[i].line != doc_line || !vis_num[i] ||
                links[i].col_mark != pos)
                continue;
            int v = vis_num[i];
            int mark = att((i == cur_link) ? 0x1E : 0x0E);
            if (colc < (int)g_screen_w)
                put_ch(row, colc++, '[', mark);
            if (v >= 100 && colc < (int)g_screen_w)
                put_ch(row, colc++, (char)('0' + (v / 100) % 10), mark);
            if (v >= 10 && colc < (int)g_screen_w)
                put_ch(row, colc++, (char)('0' + (v / 10) % 10), mark);
            if (colc < (int)g_screen_w)
                put_ch(row, colc++, (char)('0' + v % 10), mark);
            if (colc < (int)g_screen_w)
                put_ch(row, colc++, ']', mark);
        }
        if (pos >= text_w)
            break;
        if (!s[pos])
            break;
        if (colc < (int)g_screen_w)
            put_ch(row, colc++, s[pos], attr);
    }
}

static void draw_screen(int top) {
    renumber(top);
    for (int r = 0; r < view_h(); r++) {
        int doc = top + r;
        if (doc < nlines)
            draw_line(r, doc);
        else
            fill_row(r, att(0x0F));
    }
}

static void draw_status(int top, const char *msg) {
    int row = (int)g_screen_h - 1;
    /* Выключена в конфиге: последняя строка принадлежит документу (view_h()
     * == g_screen_h), затирать её нельзя — иначе «Ln 1/40» съедает последнюю
     * строку текста при каждой перерисовке. */
    if (!cfg.status_bar)
        return;
    unsigned char bar = att(0x0B);
    fill_row(row, bar);
    int c = 0;
    c = put_str(row, c, "Ln ", bar);
    c = put_num(row, c, top + 1, bar);
    c = put_str(row, c, "/", bar);
    c = put_num(row, c, nlines, bar);
    if (max_vis > 0 && cfg.link_numbers) {
        c = put_str(row, c, "  Lnk ", bar);
        if (cur_link >= 0 && cur_link < nlinks)
            c = put_num(row, c, vis_num[cur_link], bar);
        else
            c = put_num(row, c, 0, bar);
        c = put_str(row, c, "/", bar);
        c = put_num(row, c, max_vis, bar);
    }
    if (msg && msg[0]) {
        put_str(row, (int)g_screen_w - str_len(msg) - 1, msg, att(0x1F));
    } else if (cfg.show_url && cur_link >= 0 && cur_link < nlinks) {
        const char *u = links[cur_link].href;
        int maxw = 44;
        int uc = (int)g_screen_w - str_len(u) - 1;
        if (uc < 24)
            uc = 24;
        if (str_len(u) > maxw)
            u += str_len(u) - maxw; /* хвост URL важнее начала */
        put_str(row, uc, u, att(0x0E));
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
    int row = (int)g_screen_h - 1;
    fill_row(row, att(0x1F));
    int c = 0;
    c = put_str(row, c, prompt, att(0x1F));
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
                put_ch(row, c, ' ', att(0x1F));
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
            put_ch(row, c++, ch, att(0x1F));
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
    for (int hops = 0; hops < cfg.max_redirects; hops++) {
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
        /* Копируем заголовки, пока они ещё в rxbuf: дальше декодер chunked
         * сдвинет тело в начало и сотрёт их. */
        hdr_text[0] = 0;
        if (cfg.show_headers) {
            int hn = 0;
            for (int q = 0; q < body && hn < (int)sizeof(hdr_text) - 2; q++) {
                /* CRLF -> LF: иначе каждая строка получит ведущий CR. */
                hdr_text[hn++] = (rxbuf[q] == '\r') ? '\n' : (char)rxbuf[q];
            }
            hdr_text[hn] = 0;
        }
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

/* Скролл вверх на n строк: под промптом места для справки не хватает, а
 * писать за нижним краем нельзя. */
static void scroll_up(int n) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    for (int r = 0; r < (int)g_screen_h - n; r++)
        for (int c = 0; c < (int)(g_screen_w * 2); c++)
            vga[r * (int)(g_screen_w * 2) + c] =
                vga[(r + n) * (int)(g_screen_w * 2) + c];
    for (int r = (int)g_screen_h - n; r < (int)g_screen_h; r++)
        fill_row(r, att(0x0F));
}

/* Справка по ключам конфига. Печатается с текущей строки и при нехватке
 * места подтягивает экран вверх; промпт потом встаёт под последней строкой. */
static void print_help(unsigned int vga_offset) {
    static const char *const hl[] = {
        "usage: web [-c PATH] [host [port [path]] | url]",
        "  -c, --config PATH   конфиг (по умолчанию /etc/web.conf)",
        "  -h, --help          эта справка",
        "",
        "ключи /etc/web.conf (ключ = значение, # и ; — комментарии):",
        "  start_url       что открывать без аргументов",
        "  width           ширина текста, 40..74",
        "  user_agent      заголовок User-Agent",
        "  accept          заголовок Accept",
        "  accept_encoding Accept-Encoding; пусто = не слать",
        "  default_port    порт, если в адресе схемы нет",
        "  timeout_connect / timeout_send / timeout_recv  тики PIT, 100 Гц",
        "  max_download    предел загрузки, байт; 0 = без предела",
        "  max_redirects   сколько редиректов подряд",
        "  history         глубина «назад», 0..12",
        "  link_numbers    подписи [N] у ссылок",
        "  status_bar      строка состояния внизу",
        "  show_url        URL курсорной ссылки в строке состояния",
        "  show_headers    печатать заголовки ответа",
        "  color           off = монохром",
        "  key_quit / key_back / key_goto  выход / назад / ввод адреса",
        "",
        "клавиши: стрелки скроллят, PgUp/PgDn/Home/End - экран,",
        "  <-/-> и TAB - ссылка, Enter или номер - открыть,",
        "  key_goto - адрес, key_back - история, key_quit - выход",
    };
    int r = (int)(vga_offset / (g_screen_w * 2));
    if (r < 0 || r > (int)g_screen_h - 1)
        r = 0;
    for (int i = 0; i < (int)(sizeof(hl) / sizeof(hl[0])); i++) {
        if (r > (int)g_screen_h - 1) {
            scroll_up(1);
            r = (int)g_screen_h - 1;
        }
        fill_row(r, att(0x0F));
        put_str(r, 0, hl[i], att(0x0F));
        r++;
    }
    /* Последняя строка справки может лечь на самый низ экрана, и тогда r
     * равен g_screen_h - строке за экраном нет. Шелл берёт это число как
     * позицию промпта и зажимает у себя, но полагаться на это не стоит:
     * приписываем явно. */
    *(volatile int *)EXIT_ROW_P = r < (int)g_screen_h ? r : (int)g_screen_h - 1;
    *(volatile int *)EXIT_COL_P = 0;
}

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    char url[URL_MAX];
    char status[128];

    vga_read_grid();
    cfg_defaults();

    /* Опции читаем первыми: `web -c /tmp/my.conf example.com`. */
    const char *cfgpath = "/etc/web.conf";
    unsigned long ai = 1;
    while (ai < argc) {
        const char *a = argv[ai];
        if (a[0] == '-' && a[1]) {
            if (str_ieq(a, "-c") || str_ieq(a, "--config")) {
                if (ai + 1 < argc)
                    cfgpath = argv[++ai];
                ai++;
                continue;
            }
            if (str_ieq(a, "-h") || str_ieq(a, "--help")) {
                print_help(vga_offset);
                return;
            }
        }
        break;
    }
    load_config(cfgpath);
    text_w = cfg.width;

    /* Дальше argv — адрес: [host [port [path]]] или полный URL. */
    argc -= ai - 1;
    argv += ai - 1;
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
        make_url(url, sizeof(url), argv[1], any ? port : cfg.default_port,
                 argv[3]);
    } else if (argc >= 2) {
        char host[HOST_MAX], path[PATH_MAX], sch[8];
        int port;
        if (parse_url(argv[1], host, sizeof(host), path, sizeof(path), &port) < 0) {
            print_help(vga_offset);
            return;
        }
        /* Схему из аргумента сохраняем: «https://h:8443/» иначе стал бы
         * обычным HTTP на 8443. */
        url_scheme(argv[1], sch, sizeof(sch));
        make_url_sch(url, sizeof(url), sch, host, port, path);
    } else if (cfg.start_url[0]) {
        /* Без аргументов открываем домашнюю страницу из конфига. */
        str_copy(url, cfg.start_url, sizeof(url));
    } else {
        print_help(vga_offset);
        return;
    }

    for (int r = 0; r < (int)g_screen_h; r++)
        fill_row(r, att(0x0F));

    int top = 0;
    str_copy(cur_url, url, sizeof(cur_url));
    put_str(0, 0, "Loading ", att(0x0B));
    put_str(0, 8, cur_url, att(0x0B));
    str_copy(status, "", sizeof(status));

    if (!fetch(url, status, sizeof(status))) {
        fill_row(0, att(0x0F));
        put_str(0, 0, "web: ", att(0x0F));
        put_str(0, 5, status, att(0x0F));
        draw_status(0, "press any key");
        wait_key_any();
        *(volatile int *)EXIT_ROW_P = (int)g_screen_h - 1;
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
        /* Буквы сравниваем как символы, а не как скан-коды: key_quit/
         * key_back/key_goto в конфиге заданы буквами. */
        char kc = sc_to_char(sc, shift_down);
        int is_cfg_key = (kc == cfg.key_quit || kc == cfg.key_back ||
                          kc == cfg.key_goto);

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
            /* Подсказка «#N» живёт на строке состояния. Если её выключили,
             * места для подсказки нет — цифры просто копятся, курсорная
             * ссылка и так подсвечена. */
            if (cfg.status_bar) {
                fill_row((int)g_screen_h - 1, att(0x1F));
                put_str((int)g_screen_h - 1, 0, nb, att(0x1F));
            }
            continue;
        }

        /* Enter или '.' — открыть ссылку. */
        if (sc == SC_ENTER || (kc == '.' && !is_cfg_key)) {
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
                if (nhist < cfg.history)
                    str_copy(hist_url[nhist++], cur_url, URL_MAX);
                char next[URL_MAX];
                str_copy(next, links[target].href, URL_MAX);
                str_copy(cur_url, next, sizeof(cur_url));
                str_copy(url, next, sizeof(url));
                for (int r = 0; r < (int)g_screen_h; r++)
                    fill_row(r, att(0x0F));
                put_str(0, 0, "Loading ", att(0x0B));
                put_str(0, 8, cur_url, att(0x0B));
                if (!fetch(url, status, sizeof(status))) {
                    fill_row(0, att(0x0F));
                    put_str(0, 0, "web: ", att(0x0F));
                    put_str(0, 5, status, att(0x0F));
                    draw_status(0, "press any key");
                    wait_key_any();
                    /* возвращаемся на прошлую страницу. */
                    if (nhist > 0) {
                        nhist--;
                        str_copy(cur_url, hist_url[nhist], sizeof(cur_url));
                        str_copy(url, cur_url, sizeof(url));
                        for (int r = 0; r < (int)g_screen_h; r++)
                            fill_row(r, att(0x0F));
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

        /* key_quit (по умолчанию q) — выход. */
        if (kc == cfg.key_quit)
            break;

        /* key_back (по умолчанию h) — назад. */
        if (kc == cfg.key_back && nhist > 0) {
            nhist--;
            str_copy(cur_url, hist_url[nhist], sizeof(cur_url));
            str_copy(url, cur_url, sizeof(url));
            for (int r = 0; r < (int)g_screen_h; r++)
                fill_row(r, att(0x0F));
            put_str(0, 0, "Loading ", att(0x0B));
            put_str(0, 8, cur_url, att(0x0B));
            if (!fetch(url, status, sizeof(status))) {
                fill_row(0, att(0x0F));
                put_str(0, 0, "web: ", att(0x0F));
                put_str(0, 5, status, att(0x0F));
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

        /* key_goto (по умолчанию g) — «го»: ввод адреса. */
        if (kc == cfg.key_goto) {
            char buf[URL_MAX];
            if (prompt_line("Go to: ", buf, sizeof(buf))) {
                char h2[HOST_MAX], p2[PATH_MAX], s2[8];
                int p2n;
                if (parse_url(buf, h2, sizeof(h2), p2, sizeof(p2), &p2n) == 0) {
                    if (nhist < cfg.history)
                        str_copy(hist_url[nhist++], cur_url, URL_MAX);
                    char next[URL_MAX];
                    url_scheme(buf, s2, sizeof(s2));
                    make_url_sch(next, sizeof(next), s2, h2, p2n, p2);
                    str_copy(cur_url, next, sizeof(cur_url));
                    str_copy(url, next, sizeof(url));
                    for (int r = 0; r < (int)g_screen_h; r++)
                        fill_row(r, att(0x0F));
                    put_str(0, 0, "Loading ", att(0x0B));
                    put_str(0, 8, cur_url, att(0x0B));
                    if (!fetch(url, status, sizeof(status))) {
                        fill_row(0, att(0x0F));
                        put_str(0, 0, "web: ", att(0x0F));
                        put_str(0, 5, status, att(0x0F));
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

    *(volatile int *)EXIT_ROW_P = (int)g_screen_h - 1;
    *(volatile int *)EXIT_COL_P = 0;
}
