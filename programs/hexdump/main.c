/* Hexdump: дамп файла в hex + ascii-колонке, прямо в VGA.
 *
 *    run hexdump /etc/hostname
 *
 * Читает файл сисколом vfs_read (ABI-трамплин 0x7818) в буфер 4096 байт,
 * печатает по 16 байт в строку: offset : hex ... |ascii|. Вывод ограничен
 * экраном (от строки vga_offset вниз); при нехватке места — "..." в конце.
 */

#define VFS_READ_P  0x7818
#define EXIT_ROW_P  0x708C
#define EXIT_COL_P  0x7090

typedef int (*read_fn)(const char *, void *, unsigned int);
#define abi_read(p, b, l) ((*(volatile read_fn *)VFS_READ_P)(p, b, l))

static unsigned char buf[4096];

static const char hexd[] = "0123456789abcdef";

/* Печатает строку дампа на row-й строке экрана, начиная с колонки 0.
 * Возвращает колонку конца (для EXIT_COL). */
static int draw_line(int row, int off, int n) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    int base = row * 160;
    int col = 0;

    /* offset: 8 hex-цифр (cyan) */
    for (int sh = 28; sh >= 0; sh -= 4) {
        vga[base + col * 2] = (unsigned char)hexd[(off >> sh) & 0xF];
        vga[base + col * 2 + 1] = 0x0B;
        col++;
    }
    vga[base + col * 2] = ' ';
    vga[base + col * 2 + 1] = 0x0F;
    col++;

    /* hex-байты (white) */
    for (int i = 0; i < 16; i++) {
        if (i == 8) {
            vga[base + col * 2] = ' ';
            vga[base + col * 2 + 1] = 0x0F;
            col++;
        }
        if (i < n) {
            vga[base + col * 2] = (unsigned char)hexd[buf[off + i] >> 4];
            vga[base + col * 2 + 1] = 0x0F;
            col++;
            vga[base + col * 2] = (unsigned char)hexd[buf[off + i] & 0xF];
            vga[base + col * 2 + 1] = 0x0F;
            col++;
        } else {
            vga[base + col * 2] = ' ';
            vga[base + col * 2 + 1] = 0x0F;
            col++;
            vga[base + col * 2] = ' ';
            vga[base + col * 2 + 1] = 0x0F;
            col++;
        }
    }
    vga[base + col * 2] = ' ';
    vga[base + col * 2 + 1] = 0x0F;
    col++;
    vga[base + col * 2] = '|';
    vga[base + col * 2 + 1] = 0x0F;
    col++;
    for (int i = 0; i < 16; i++) {
        if (i < n) {
            unsigned char c = buf[off + i];
            if (c < 0x20 || c >= 0x7F)
                c = '.';
            vga[base + col * 2] = (unsigned char)c;
            vga[base + col * 2 + 1] = 0x0A;
        } else {
            vga[base + col * 2] = ' ';
            vga[base + col * 2 + 1] = 0x0A;
        }
        col++;
    }
    vga[base + col * 2] = '|';
    vga[base + col * 2 + 1] = 0x0F;
    col++;
    return col;
}

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    const char *path = (argc >= 2) ? argv[1] : 0;
    if (!path) {
        path = "usage: hexdump <path>";
    }
    int len = abi_read(path, buf, sizeof(buf));
    if (len < 0) {
        volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
        int row = (int)(vga_offset / 160);
        int col = 0;
        const char *m = "hexdump: cannot read: ";
        while (*m && col < 79) {
            vga[row * 160 + col * 2] = (unsigned char)*m++;
            vga[row * 160 + col * 2 + 1] = 0x0F;
            col++;
        }
        while (*path && col < 79) {
            vga[row * 160 + col * 2] = (unsigned char)*path++;
            vga[row * 160 + col * 2 + 1] = 0x0F;
            col++;
        }
        *(volatile int *)EXIT_ROW_P = row;
        *(volatile int *)EXIT_COL_P = col;
        return;
    }

    int start_row = (int)(vga_offset / 160);
    int rows = 30 - start_row;
    int needed = (len + 15) / 16;
    int show = needed;
    int truncated = 0;
    if (show > rows) {
        show = rows - 1; /* последнюю строку оставляем под "..." */
        if (show < 0)
            show = 0;
        truncated = (needed > show);
    }

    int last_col = 0;
    int last_row = start_row;
    for (int r = 0; r < show; r++) {
        int off = r * 16;
        last_col = draw_line(start_row + r, off, off + 16 <= len ? 16 : len - off);
        last_row = start_row + r;
    }
    if (truncated) {
        volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
        int row = start_row + show;
        int col = 0;
        const char *m = "... (";
        while (*m && col < 79) {
            vga[row * 160 + col * 2] = (unsigned char)*m++;
            vga[row * 160 + col * 2 + 1] = 0x0B;
            col++;
        }
        char num[16];
        int n2 = 0;
        int left = len - show * 16;
        if (left < 0) left = 0;
        if (left == 0) num[n2++] = '0';
        while (left > 0) {
            num[n2++] = (char)('0' + left % 10);
            left /= 10;
        }
        for (int k = n2 - 1; k >= 0; k--) {
            if (col >= 79) break;
            vga[row * 160 + col * 2] = (unsigned char)num[k];
            vga[row * 160 + col * 2 + 1] = 0x0B;
            col++;
        }
        vga[row * 160 + col * 2] = ' ';
        vga[row * 160 + col * 2 + 1] = 0x0B;
        col++;
        vga[row * 160 + col * 2] = 'b';
        vga[row * 160 + col * 2 + 1] = 0x0B;
        col++;
        vga[row * 160 + col * 2] = 'y';
        vga[row * 160 + col * 2 + 1] = 0x0B;
        col++;
        vga[row * 160 + col * 2] = 't';
        vga[row * 160 + col * 2 + 1] = 0x0B;
        col++;
        vga[row * 160 + col * 2] = 'e';
        vga[row * 160 + col * 2 + 1] = 0x0B;
        col++;
        vga[row * 160 + col * 2] = 's';
        vga[row * 160 + col * 2 + 1] = 0x0B;
        col++;
        vga[row * 160 + col * 2] = ')';
        vga[row * 160 + col * 2 + 1] = 0x0B;
        col++;
        last_row = row;
        last_col = col;
    }

    *(volatile int *)EXIT_ROW_P = last_row;
    *(volatile int *)EXIT_COL_P = last_col;
}