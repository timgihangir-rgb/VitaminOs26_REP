/* programs/common/vitauser.h — общие мелочи для программ, работающих с
 * учётными записями (sudo, adduser, removeuser).
 *
 * Общей libc в системе нет, поэтому и вывод в VGA, и чтение клавиатуры, и
 * вызовы системных функций приходится писать руками. Здесь они собраны один
 * раз; функции и таблицы static, поэтому каждый бинарник тянет только нужное.
 *
 * Заголовок подключается как "../common/vitauser.h": сборщик программ
 * компилирует все *.c внутри каталога программы, а этот файл лежит рядом с
 * ними, а не внутри, и в бинарник сам по себе не попадает.
 *
 * ВАЖНО: `_start` обязан быть первой функцией в main.c - ядро выполняет
 * бинарник с самого первого байта. */

#ifndef VITAUSER_H
#define VITAUSER_H

/* ─── Адреса трамплинов (см. src/progabi.rs) ─────────────────────────── */
#define TICKS_PTR   0x7800
#define KBHIT_PTR   0x7828
#define KBREAD_PTR  0x7830
#define IOCTL_PTR   0x7838
#define EXIT_ROW_P  0x708C
#define EXIT_COL_P  0x7090
#define EXIT_CODE_P 0x7098
#define AUTH_PTR    0x7868
#define GETUID_PTR  0x7870
#define WHOAMI_PTR  0x7878
#define USERADD_PTR 0x7880
#define USERDEL_PTR 0x7888
#define EXEC_PTR    0x7890
#define WAIT_PTR    0x7898

typedef unsigned long long (*fn_ticks)(void);
typedef int (*fn_kbhit)(void);
typedef int (*fn_kbread)(void);
typedef int (*fn_ioctl)(int, unsigned long);
typedef int (*fn_auth)(const char *);
typedef int (*fn_getuid)(void);
typedef int (*fn_whoami)(char *, unsigned int);
typedef int (*fn_useradd)(const char *, const char *);
typedef int (*fn_userdel)(const char *);
typedef int (*fn_exec)(const char *, unsigned int, const char **);
typedef int (*fn_wait)(unsigned int);

#define abi_ticks()   ((*(volatile fn_ticks *)TICKS_PTR)())
#define abi_kbhit()   ((*(volatile fn_kbhit *)KBHIT_PTR)())
#define abi_kbread()  ((*(volatile fn_kbread *)KBREAD_PTR)())
#define abi_ioctl(c, a) ((*(volatile fn_ioctl *)IOCTL_PTR)((c), (a)))
#define abi_auth(p)   ((*(volatile fn_auth *)AUTH_PTR)(p))
#define abi_getuid()  ((*(volatile fn_getuid *)GETUID_PTR)())
#define abi_whoami(b, m) ((*(volatile fn_whoami *)WHOAMI_PTR)((b), (m)))
#define abi_useradd(n, p) ((*(volatile fn_useradd *)USERADD_PTR)((n), (p)))
#define abi_userdel(n) ((*(volatile fn_userdel *)USERDEL_PTR)(n))
#define abi_exec(p, c, a) ((*(volatile fn_exec *)EXEC_PTR)((p), (c), (a)))
#define abi_wait(p)   ((*(volatile fn_wait *)WAIT_PTR)(p))

/* ─── Вывод в VGA ────────────────────────────────────────────────────────
 *
 * Программа пишет в свой кусок текстового экрана начиная с vga_offset,
 * который ядро передаёт в rdx. В конце обязательно сообщаем шеллу, где
 * остановились (EXIT_ROW/EXIT_COL), иначе промпт нарисуется поверх вывода. */

static unsigned int vu_row;
static unsigned int vu_col;

static void vu_init(unsigned int vga_offset) {
    vu_row = vga_offset / 160;
    vu_col = (vga_offset % 160) / 2;
}

static void vu_put(char ch, char attr) {
    volatile char *vga = (volatile char *)0xB8000;
    if (ch == '\r') {
        vu_col = 0;
        return;
    }
    if (ch == '\n') {
        if (vu_col != 0)
            vu_row++;
        vu_col = 0;
    } else if (vu_col >= 80) {
        vu_row++;
        vu_col = 0;
    }
    if (vu_row >= 30)
        vu_row = 29;
    unsigned int off = (vu_row * 80 + vu_col) * 2;
    vga[off] = ch;
    vga[off + 1] = attr;
    vu_col++;
}

static void vu_puts(const char *s, char attr) {
    while (*s)
        vu_put(*s++, attr);
}

/* Атрибуты: чтобы подсказки и ошибки читались, а не сливались. */
#define ATTR_WHITE 0x0F
#define ATTR_CYAN  0x0B
#define ATTR_RED   0x0C
#define ATTR_GREEN 0x0A

/* Сообщаем шеллу позицию и выходим: возврат из _start уходит на hlt-стаб,
 * который ядро кладёт на верхушку user-стека (см. scheduler::spawn_user). */
#define VU_EXIT()                                                            \
    do {                                                                     \
        volatile int *r = (volatile int *)EXIT_ROW_P;                        \
        volatile int *c = (volatile int *)EXIT_COL_P;                        \
        *r = (int)vu_row;                                                    \
        *c = (int)vu_col;                                                    \
        return;                                                              \
    } while (0)

/* Выход с кодом возврата: шелл читает его после завершения программы
 * (exec::last_exit_code). */
#define VU_EXIT_CODE(code)                                                  \
    do {                                                                     \
        volatile int *e = (volatile int *)EXIT_CODE_P;                       \
        *e = (code);                                                         \
        VU_EXIT();                                                           \
    } while (0)

/* ─── Ввод с клавиатуры ─────────────────────────────────────────────────
 *
 * Ядро отдаёт сканкоды набора 1 и ASCII не переводит. Раскладка одна (US),
 * поэтому из модификаторов нужен только Shift - ровно как и в
 * keyboard::read_key в ядре. */

static const char vu_keymap[256] = {
    [0x02] = '1',  [0x03] = '2',  [0x04] = '3',  [0x05] = '4',
    [0x06] = '5',  [0x07] = '6',  [0x08] = '7',  [0x09] = '8',
    [0x0A] = '9',  [0x0B] = '0',  [0x0C] = '-',  [0x0D] = '=',
    [0x10] = 'q',  [0x11] = 'w',  [0x12] = 'e',  [0x13] = 'r',
    [0x14] = 't',  [0x15] = 'y',  [0x16] = 'u',  [0x17] = 'i',
    [0x18] = 'o',  [0x19] = 'p',  [0x1A] = '[',  [0x1B] = ']',
    [0x1E] = 'a',  [0x1F] = 's',  [0x20] = 'd',  [0x21] = 'f',
    [0x22] = 'g',  [0x23] = 'h',  [0x24] = 'j',  [0x25] = 'k',
    [0x26] = 'l',  [0x27] = ';',  [0x28] = '\'', [0x29] = '`',
    [0x2B] = '\\', [0x2C] = 'z',  [0x2D] = 'x',  [0x2E] = 'c',
    [0x2F] = 'v',  [0x30] = 'b',  [0x31] = 'n',  [0x32] = 'm',
    [0x33] = ',',  [0x34] = '.',  [0x35] = '/',  [0x39] = ' ',
};

/* Что эти же клавиши дают с Shift. Буквы верхним регистром получаются из
 * keymap, здесь только символы. */
static const char vu_shiftmap[256] = {
    [0x02] = '!',  [0x03] = '@',  [0x04] = '#',  [0x05] = '$',
    [0x06] = '%',  [0x07] = '^',  [0x08] = '&',  [0x09] = '*',
    [0x0A] = '(',  [0x0B] = ')',  [0x0C] = '_',  [0x0D] = '+',
    [0x1A] = '{',  [0x1B] = '}',  [0x27] = ':',  [0x28] = '"',
    [0x29] = '~',  [0x2B] = '|',  [0x33] = '<',  [0x34] = '>',
    [0x35] = '?',
};

/* ASCII по сканкоду; 0 - клавиша не даёт символа. */
static char vu_ascii(unsigned int sc, int shift) {
    if (sc >= 256)
        return 0;
    char c = vu_keymap[sc];
    if (!c)
        return 0;
    if (shift) {
        char s = vu_shiftmap[sc];
        if (s)
            return s;
        if (c >= 'a' && c <= 'z')
            return (char)(c - 'a' + 'A');
    }
    return c;
}

/* Читает строку до Enter. При echo == 0 символы не печатаются (пароль),
 * но Backspace работает. Возвращает длину строки. */
static int vu_read_line(char *buf, int max, int echo) {
    int n = 0;
    int shift = 0;
    for (;;) {
        /* Ждём клавишу; без hlt задача крутилась бы вхолостую. */
        while (!abi_kbhit())
            abi_ticks();
        unsigned int sc = (unsigned int)abi_kbread();
        if (sc & 0x80) {
            if (sc == 0xAA) /* отпускание Shift */
                shift = 0;
            continue;
        }
        if (sc == 0x2A) { /* нажатие Shift */
            shift = 1;
            continue;
        }
        if (sc == 0x1C) { /* Enter */
            if (echo)
                vu_put('\n', ATTR_WHITE);
            buf[n] = '\0';
            return n;
        }
        if (sc == 0x0E) { /* Backspace */
            if (n > 0) {
                n--;
                if (echo) {
                    vu_put('\b', ATTR_WHITE);
                    vu_put(' ', ATTR_WHITE);
                    vu_put('\b', ATTR_WHITE);
                }
            }
            continue;
        }
        if (n >= max - 1)
            continue;
        char c = vu_ascii(sc, shift);
        if (!c)
            continue;
        buf[n++] = c;
        if (echo)
            vu_put(c, ATTR_WHITE);
    }
}

/* Сравнение строк: 0 - равны. */
static int vu_streq(const char *a, const char *b) {
    while (*a && *a == *b) {
        a++;
        b++;
    }
    return *a == *b;
}

#endif /* VITAUSER_H */