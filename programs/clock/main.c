/* Clock: полностью независимая программа, показывающая текущие дату и время.
 *
 * Установка часов через конфиг /etc/timezone: пишем в него текущее время
 * (HH:MM:SS) — clock применяет его как установку и дальше отсчитывает время
 * от него. Смещение между записанным временем и CMOS RTC хранится в
 * /var/clock_set, поэтому каждый запуск clock выдаёт текущее время, идущее
 * от установленного значения. Конфиг создаётся самой программой при первом
 * запуске (в него записывается текущее время RTC).
 *
 * Режимы (различаются по vga_offset, который ядро передаёт в _start):
 *   - фоновая задача (init-служба `clock bin` из /etc/rc.conf либо `bg clock`,
 *     vga_offset == 0): использует многозадачность — раз в 100 тиков просыпается
 *     по таймеру и дописывает "clock N YYYY-MM-DD HH:MM:SS" в /tmp/clock.log;
 *   - команда (`clock` / `run clock`, vga_offset > 0): один раз печатает время
 *     в VGA с позиции vga_offset и кладёт курсор в EXIT_ROW/EXIT_COL.
 *
 * ВНИМАНИЕ: `_start` обязан быть первой функцией в файле — ядро выполняет
 * бинарник с его самого первого байта (как и во всех остальных /bin-программах).
 */

#define TICKS_PTR   0x7800
#define HLT_PTR     0x7808
#define VFS_WRITE_P 0x7810
#define VFS_READ_P  0x7818
#define SLEEP_PTR   0x7820
#define EXIT_ROW_P  0x708C
#define EXIT_COL_P  0x7090

#define CMOS_INDEX  0x70
#define CMOS_DATA   0x71

#define RTC_SECONDS 0x00
#define RTC_MINUTES 0x02
#define RTC_HOURS   0x04
#define RTC_DAY     0x07
#define RTC_MONTH   0x08
#define RTC_YEAR    0x09
#define RTC_REGA    0x0A
#define RTC_REGB    0x0B

#define UIP_FLAG    0x80
#define BIN_MODE    0x04
#define H24_MODE    0x02

#define TZ_CONFIG   "/etc/timezone"
#define SET_STATE   "/var/clock_set"
#define SECS_PER_DAY 86400

typedef unsigned long long (*ticks_fn)(void);
typedef void (*hlt_fn)(void);
typedef int (*write_fn)(const char *, const void *, unsigned int);
typedef int (*read_fn)(const char *, void *, unsigned int);
typedef void (*sleep_fn)(unsigned long long);

#define abi_ticks()      ((*(volatile ticks_fn *)TICKS_PTR)())
#define abi_hlt()        ((*(volatile hlt_fn *)HLT_PTR)())
#define abi_write(p, d, l) ((*(volatile write_fn *)VFS_WRITE_P)(p, d, l))
#define abi_read(p, b, l)  ((*(volatile read_fn *)VFS_READ_P)(p, b, l))
#define abi_sleep(t)     ((*(volatile sleep_fn *)SLEEP_PTR)(t))

static int u64_to_str(char *out, unsigned long long v);
static int fmt2(char *out, unsigned int v);
static int ensure_config(void);
static int format_time(char *out);
static void daemon_mode(void);
static void command_mode(unsigned int vga_offset);
static long rtc_now_sec(void);
static long read_cfg_time(void);
static void fmt_time_of_day(char *out, long total);
static int read_state(long *prev, long *delta);
static void save_state(long prev, long delta);

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    (void)argc;
    (void)argv;
    if (vga_offset == 0)
        daemon_mode();
    else
        command_mode(vga_offset);
}

/* --- CMOS RTC --- */

static unsigned char cmos_read(unsigned char reg) {
    __asm__ volatile("outb %0, %1" : : "a"(reg), "Nd"(CMOS_INDEX));
    unsigned char v;
    __asm__ volatile("inb %1, %0" : "=a"(v) : "Nd"(CMOS_DATA));
    return v;
}

static int bcd_to_bin(int v) {
    return (v & 0x0F) + (v >> 4) * 10;
}

/* Текущее время суток по RTC в секундах от полуночи (с учётом BCD и 12/24h). */
static long rtc_now_sec(void) {
    int spins = 0;
    while (cmos_read(RTC_REGA) & UIP_FLAG) {
        if (++spins > 1000000)
            break;
    }
    unsigned char regb = cmos_read(RTC_REGB);
    int bin = (regb & BIN_MODE) != 0;
    int h24 = (regb & H24_MODE) != 0;

    unsigned char sec = cmos_read(RTC_SECONDS);
    unsigned char min = cmos_read(RTC_MINUTES);
    unsigned char hr  = cmos_read(RTC_HOURS);

    if (!bin) {
        sec = (unsigned char)bcd_to_bin(sec);
        min = (unsigned char)bcd_to_bin(min);
        hr  = (unsigned char)bcd_to_bin(hr);
    }
    if (!h24) {
        int pm = (hr & 0x80) != 0;
        hr &= 0x7F;
        if (pm && hr != 12)
            hr += 12;
        if (!pm && hr == 12)
            hr = 0;
    }
    return (long)hr * 3600 + (long)min * 60 + (long)sec;
}

/* --- Конфиг времени --- */

/* Разбирает "HH:MM:SS" (минуты и секунды можно опустить). Возвращает секунды
 * от полуночи или -1, если времени в строке нет. */
static long parse_time(const char *s) {
    while (*s == ' ' || *s == '\t')
        s++;
    if (s[0] < '0' || s[0] > '9' || s[1] < '0' || s[1] > '9')
        return -1;
    int hh = (s[0] - '0') * 10 + (s[1] - '0');
    const char *p = s + 2;
    int mm = 0, ss = 0;
    if (*p == ':') {
        p++;
        if (p[0] >= '0' && p[0] <= '9' && p[1] >= '0' && p[1] <= '9') {
            mm = (p[0] - '0') * 10 + (p[1] - '0');
            p += 2;
            if (*p == ':') {
                p++;
                if (p[0] >= '0' && p[0] <= '9' && p[1] >= '0' && p[1] <= '9')
                    ss = (p[0] - '0') * 10 + (p[1] - '0');
            }
        }
    }
    return (long)hh * 3600 + (long)mm * 60 + (long)ss;
}

/* Читает /etc/timezone (комментарии # пропускаются), возвращает время в секундах. */
static long read_cfg_time(void) {
    char buf[128];
    int len = abi_read(TZ_CONFIG, buf, sizeof(buf) - 1);
    if (len <= 0)
        return -1;
    buf[len] = 0;
    for (const char *p = buf; *p; p++) {
        if (*p == '#') {
            while (*p && *p != '\n')
                p++;
            continue;
        }
        if (*p == '\n' || *p == ' ' || *p == '\t')
            continue;
        return parse_time(p);
    }
    return -1;
}

/* "HH:MM:SS" из секунд от полуночи (с нормализацией). */
static void fmt_time_of_day(char *out, long total) {
    total %= SECS_PER_DAY;
    if (total < 0)
        total += SECS_PER_DAY;
    long hh = total / 3600;
    long mm = (total % 3600) / 60;
    long ss = total % 60;
    out[0] = (char)('0' + hh / 10);
    out[1] = (char)('0' + hh % 10);
    out[2] = ':';
    out[3] = (char)('0' + mm / 10);
    out[4] = (char)('0' + mm % 10);
    out[5] = ':';
    out[6] = (char)('0' + ss / 10);
    out[7] = (char)('0' + ss % 10);
    out[8] = 0;
}

/* Создаёт /etc/timezone при первом запуске (если его ещё нет): текущее время RTC. */
static int ensure_config(void) {
    char buf[128];
    int len = abi_read(TZ_CONFIG, buf, sizeof(buf) - 1);
    if (len >= 0)
        return len;
    char time[16];
    fmt_time_of_day(time, rtc_now_sec());
    const char *hdr = "# VitaminOS26 clock time. Write current time here as HH:MM:SS to set the clock.\n";
    char cfg[192];
    int n = 0;
    for (int i = 0; hdr[i]; i++)
        cfg[n++] = hdr[i];
    for (int i = 0; time[i]; i++)
        cfg[n++] = time[i];
    cfg[n++] = '\n';
    if (abi_write(TZ_CONFIG, cfg, n) != 0)
        return 0;
    return n;
}

/* --- Состояние установки (/var/clock_set): записанное время и смещение к RTC --- */

/* Формат: "<cfg_time_sec> <delta_sec>\n". Читает, возвращает 1 при успехе. */
static int read_state(long *prev, long *delta) {
    char buf[64];
    int len = abi_read(SET_STATE, buf, sizeof(buf) - 1);
    if (len <= 0)
        return 0;
    buf[len] = 0;
    const char *p = buf;
    long a = 0;
    while (*p >= '0' && *p <= '9') {
        a = a * 10 + (*p - '0');
        p++;
    }
    while (*p == ' ' || *p == '\t' || *p == '\n')
        p++;
    int neg = 0;
    if (*p == '-') {
        neg = 1;
        p++;
    }
    long b = 0;
    while (*p >= '0' && *p <= '9') {
        b = b * 10 + (*p - '0');
        p++;
    }
    if (neg)
        b = -b;
    *prev = a;
    *delta = b;
    return 1;
}

static void save_state(long prev, long delta) {
    char buf[64];
    int n = 0;
    char tmp[32];
    int m = u64_to_str(tmp, (unsigned long long)prev);
    for (int i = 0; i < m; i++)
        buf[n++] = tmp[i];
    buf[n++] = ' ';
    if (delta < 0) {
        buf[n++] = '-';
        delta = -delta;
    }
    m = u64_to_str(tmp, (unsigned long long)delta);
    for (int i = 0; i < m; i++)
        buf[n++] = tmp[i];
    buf[n++] = '\n';
    abi_write(SET_STATE, buf, n);
}

/* --- Форматирование времени --- */

static int u64_to_str(char *out, unsigned long long v) {
    char tmp[32];
    int n = 0;
    if (v == 0) {
        out[0] = '0';
        out[1] = 0;
        return 1;
    }
    while (v) {
        tmp[n++] = (char)('0' + v % 10);
        v /= 10;
    }
    for (int i = 0; i < n; i++)
        out[i] = tmp[n - 1 - i];
    out[n] = 0;
    return n;
}

static int fmt2(char *out, unsigned int v) {
    out[0] = (char)('0' + v / 10);
    out[1] = (char)('0' + v % 10);
    return 2;
}

/* Текущее время с учётом установки из /etc/timezone: "YYYY-MM-DD HH:MM:SS". */
static int format_time(char *out) {
    ensure_config();
    long delta = 0;
    long cfg = read_cfg_time();
    if (cfg >= 0) {
        long prev;
        if (!read_state(&prev, &delta) || prev != cfg) {
            delta = cfg - rtc_now_sec();
            save_state(cfg, delta);
        }
    }

    int spins = 0;
    while (cmos_read(RTC_REGA) & UIP_FLAG) {
        if (++spins > 1000000)
            break;
    }
    unsigned char regb = cmos_read(RTC_REGB);
    int bin = (regb & BIN_MODE) != 0;
    int h24 = (regb & H24_MODE) != 0;

    unsigned char sec = cmos_read(RTC_SECONDS);
    unsigned char min = cmos_read(RTC_MINUTES);
    unsigned char hr  = cmos_read(RTC_HOURS);
    unsigned char day = cmos_read(RTC_DAY);
    unsigned char mon = cmos_read(RTC_MONTH);
    unsigned char yr  = cmos_read(RTC_YEAR);

    if (!bin) {
        sec = (unsigned char)bcd_to_bin(sec);
        min = (unsigned char)bcd_to_bin(min);
        hr  = (unsigned char)bcd_to_bin(hr);
        day = (unsigned char)bcd_to_bin(day);
        mon = (unsigned char)bcd_to_bin(mon);
        yr  = (unsigned char)bcd_to_bin(yr);
    }
    if (!h24) {
        int pm = (hr & 0x80) != 0;
        hr &= 0x7F;
        if (pm && hr != 12)
            hr += 12;
        if (!pm && hr == 12)
            hr = 0;
    }

    long total = (long)hr * 3600 + (long)min * 60 + (long)sec + delta;
    total %= SECS_PER_DAY;
    if (total < 0)
        total += SECS_PER_DAY;
    hr = (unsigned char)(total / 3600);
    min = (unsigned char)((total % 3600) / 60);
    sec = (unsigned char)(total % 60);

    int n = 0;
    n += u64_to_str(out + n, 2000 + yr);
    out[n++] = '-';
    n += fmt2(out + n, mon);
    out[n++] = '-';
    n += fmt2(out + n, day);
    out[n++] = ' ';
    n += fmt2(out + n, hr);
    out[n++] = ':';
    n += fmt2(out + n, min);
    out[n++] = ':';
    n += fmt2(out + n, sec);
    out[n] = 0;
    return n;
}

/* --- Вывод --- */

/* Дописывает line в конец файла path (читает старое содержимое и пишет всё). */
static void append_line(const char *path, const char *line) {
    char buf[2048];
    int len = 0;
    int old = abi_read(path, buf, 2047);
    if (old > 0)
        len = old;
    int i = 0;
    while (line[i] && len < 2047)
        buf[len++] = line[i++];
    abi_write(path, buf, len);
}

/* Фоновая задача: раз в 100 тиков (1 сек) пишет текущее время в /tmp/clock.log.
 * Спит блокирующим sleep-сисколом, поэтому не съедает кванты планировщика. */
static void daemon_mode(void) {
    ensure_config();
    unsigned long long entry = 0;
    unsigned long long target = abi_ticks();
    for (;;) {
        target += 100;
        abi_sleep(target);
        entry++;

        char time[32];
        int tn = format_time(time);

        char line[160];
        char *p = line;
        const char *prefix = "clock ";
        while (*prefix)
            *p++ = *prefix++;
        p += u64_to_str(p, entry);
        *p++ = ' ';
        for (int i = 0; i < tn; i++)
            *p++ = time[i];
        *p++ = '\n';
        *p = 0;
        append_line("/tmp/clock.log", line);
    }
}

/* Команда: один раз печатает время в VGA и кладёт курсор в EXIT_ROW/EXIT_COL. */
static void command_mode(unsigned int vga_offset) {
    ensure_config();
    char line[64];
    int n = format_time(line);

    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    int off = (int)vga_offset;
    for (int i = 0; i < n && off + 2 * i < 9600; i++) {
        vga[off + 2 * i] = (unsigned char)line[i];
        vga[off + 2 * i + 1] = 0x0F;
    }

    int start_row = (int)(vga_offset / 160);
    int start_col = (int)((vga_offset % 160) / 2);
    int end_col = start_col + n;
    if (end_col > 79)
        end_col = 79;
    *(volatile int *)EXIT_ROW_P = start_row;
    *(volatile int *)EXIT_COL_P = end_col;
}
