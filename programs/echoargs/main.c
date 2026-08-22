/* echoargs: печатает argc и все argv — проверка передачи аргументов
 * ELF-загрузчиком (rdi=argc, rsi=&argv[0], rdx=vga_offset).
 * Вывод идёт в VGA с позиции vga_offset; в конце выставляет EXIT_ROW/COL,
 * чтобы промпт шелла не перерисовался поверх вывода. */

#define EXIT_ROW_P 0x708C
#define EXIT_COL_P 0x7090

static void put_char(unsigned int *offset, char ch, char attr) {
    volatile char *vga = (volatile char *)0xB8000;
    if (ch == '\n') {
        *offset = (*offset / 160 + 1) * 160;
        return;
    }
    vga[*offset] = ch;
    vga[*offset + 1] = attr;
    *offset += 2;
}

static void put_str(unsigned int *offset, const char *s, char attr) {
    while (*s)
        put_char(offset, *s++, attr);
}

static void put_u64(unsigned int *offset, unsigned long v, char attr) {
    char buf[24];
    int i = 0;
    if (v == 0)
        buf[i++] = '0';
    while (v > 0) {
        buf[i++] = '0' + (char)(v % 10);
        v /= 10;
    }
    while (i > 0)
        put_char(offset, buf[--i], attr);
}

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    unsigned int off = vga_offset;

    put_str(&off, "argc=", 0x0B);
    put_u64(&off, argc, 0x0F);
    put_char(&off, '\n', 0x0F);

    for (unsigned long k = 0; k < argc; k++) {
        if (argv[k] == 0)
            break;
        put_str(&off, "arg: ", 0x0B);
        const char *s = argv[k];
        while (*s)
            put_char(&off, *s++, 0x0F);
        put_char(&off, '\n', 0x0F);
    }

    /* Курсор за концом вывода: промпт рисуется ниже, не затирая строки. */
    *(volatile int *)EXIT_ROW_P = (int)(off / 160);
    *(volatile int *)EXIT_COL_P = (int)((off % 160) / 2);
}
