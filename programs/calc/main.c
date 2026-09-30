/* Calc: калькулятор выражений с приоритетами (+ - * / %) и скобками.
 *
 *    run calc 2 + 3 * 4        ->  = 14
 *    run calc (2 + 3) * 4      ->  = 20
 *    run calc 12 / 4 - 1       ->  = 2
 *
 * Парсер — рекурсивный спуск (expr -> term -> factor). Все операнды целые
 * 64-битные; деление на ноль и мусор дают "calc: bad expression".
 * Вывод — прямо в VGA с позиции vga_offset (как clock), курсор в EXIT_ROW.
 */

#define EXIT_ROW_P  0x708C
#define EXIT_COL_P  0x7090

typedef long long num_t;

static const char *p;
static int err;

static void skip_ws(void) {
    while (*p == ' ' || *p == '\t')
        p++;
}

static num_t expr(void);
static num_t term(void);
static num_t factor(void);

static num_t expr(void) {
    num_t v = term();
    for (;;) {
        skip_ws();
        if (*p == '+') { p++; v += term(); }
        else if (*p == '-') { p++; v -= term(); }
        else break;
    }
    return v;
}

static num_t term(void) {
    num_t v = factor();
    for (;;) {
        skip_ws();
        if (*p == '*') { p++; v *= factor(); }
        else if (*p == '/') {
            p++;
            num_t d = factor();
            if (d == 0) { err = 1; return 0; }
            v /= d;
        } else if (*p == '%') {
            p++;
            num_t d = factor();
            if (d == 0) { err = 1; return 0; }
            v %= d;
        } else break;
    }
    return v;
}

static num_t factor(void) {
    skip_ws();
    if (*p == '(') {
        p++;
        num_t v = expr();
        skip_ws();
        if (*p == ')')
            p++;
        else
            err = 1;
        return v;
    }
    if (*p == '-') { p++; return -factor(); }
    if (*p == '+') { p++; return factor(); }
    if (*p >= '0' && *p <= '9') {
        num_t v = 0;
        while (*p >= '0' && *p <= '9') {
            v = v * 10 + (*p - '0');
            p++;
        }
        return v;
    }
    err = 1;
    return 0;
}

/* "= <число>" или "calc: bad expression" — в VGA с позиции vga_offset. */
static void print_result(unsigned int vga_offset, const char *msg, int n) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    int off = (int)vga_offset;
    for (int i = 0; i < n && off + 2 * i < 4800; i++) {
        vga[off + 2 * i] = (unsigned char)msg[i];
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

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    char text[256];
    int n = 0;
    for (unsigned long i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (!a)
            break;
        if (n > 0 && text[n - 1] != ' ')
            text[n++] = ' ';
        while (*a && n < 254)
            text[n++] = (char)*a++;
    }
    text[n] = 0;

    p = text;
    err = 0;
    num_t v = expr();
    skip_ws();
    if (err || *p != 0) {
        print_result(vga_offset, "calc: bad expression", 20);
        return;
    }

    /* Формируем "= <значение>". */
    char out[32];
    int o = 0;
    out[o++] = '=';
    out[o++] = ' ';
    if (v < 0) {
        out[o++] = '-';
        v = -v;
    }
    char tmp[24];
    int t = 0;
    if (v == 0) {
        tmp[t++] = '0';
    }
    while (v > 0) {
        tmp[t++] = (char)('0' + v % 10);
        v /= 10;
    }
    while (t > 0)
        out[o++] = tmp[--t];
    out[o] = 0;

    print_result(vga_offset, out, o);
}