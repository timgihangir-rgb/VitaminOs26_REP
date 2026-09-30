/* Banner: рисует строку крупными буквами (5x5 bitmap-шрифт) прямо в VGA.
 *
 *    run banner VITA
 *    run banner hello world
 *
 * Перенос: каждая буква 5 колонок + 1 зазор; помещается 13 букв в строку
 * (80 колонок), при большем количестве — перенос на следующий блок строк.
 * Под артом печатается исходная строка обычным текстом.
 */

#define EXIT_ROW_P  0x708C
#define EXIT_COL_P  0x7090

/* Шрифт 5x5: 5 рядов по 5 бит (бит 4 = левая колонка). */
static const unsigned char FONT[40][5] = {
    /* A */ {0x0E, 0x11, 0x11, 0x1F, 0x11},
    /* B */ {0x1E, 0x11, 0x1E, 0x11, 0x1E},
    /* C */ {0x0F, 0x10, 0x10, 0x10, 0x0F},
    /* D */ {0x1E, 0x11, 0x11, 0x11, 0x1E},
    /* E */ {0x1F, 0x10, 0x1E, 0x10, 0x1F},
    /* F */ {0x1F, 0x10, 0x1E, 0x10, 0x10},
    /* G */ {0x0F, 0x10, 0x17, 0x11, 0x0F},
    /* H */ {0x11, 0x11, 0x1F, 0x11, 0x11},
    /* I */ {0x1F, 0x04, 0x04, 0x04, 0x1F},
    /* J */ {0x0F, 0x02, 0x02, 0x12, 0x0C},
    /* K */ {0x11, 0x12, 0x1C, 0x12, 0x11},
    /* L */ {0x10, 0x10, 0x10, 0x10, 0x1F},
    /* M */ {0x11, 0x1B, 0x15, 0x11, 0x11},
    /* N */ {0x11, 0x19, 0x15, 0x13, 0x11},
    /* O */ {0x0E, 0x11, 0x11, 0x11, 0x0E},
    /* P */ {0x1E, 0x11, 0x1E, 0x10, 0x10},
    /* Q */ {0x0E, 0x11, 0x11, 0x12, 0x0D},
    /* R */ {0x1E, 0x11, 0x1E, 0x12, 0x11},
    /* S */ {0x0F, 0x10, 0x0E, 0x01, 0x1E},
    /* T */ {0x1F, 0x04, 0x04, 0x04, 0x04},
    /* U */ {0x11, 0x11, 0x11, 0x11, 0x0E},
    /* V */ {0x11, 0x11, 0x11, 0x0A, 0x04},
    /* W */ {0x11, 0x11, 0x15, 0x15, 0x0A},
    /* X */ {0x11, 0x0A, 0x04, 0x0A, 0x11},
    /* Y */ {0x11, 0x0A, 0x04, 0x04, 0x04},
    /* Z */ {0x1F, 0x02, 0x04, 0x08, 0x1F},
    /* 0 */ {0x0E, 0x13, 0x15, 0x19, 0x0E},
    /* 1 */ {0x04, 0x0C, 0x04, 0x04, 0x0E},
    /* 2 */ {0x0E, 0x11, 0x02, 0x04, 0x1F},
    /* 3 */ {0x1E, 0x01, 0x0E, 0x01, 0x1E},
    /* 4 */ {0x02, 0x06, 0x0A, 0x1F, 0x02},
    /* 5 */ {0x1F, 0x10, 0x1E, 0x01, 0x1E},
    /* 6 */ {0x0E, 0x10, 0x1E, 0x11, 0x0E},
    /* 7 */ {0x1F, 0x01, 0x02, 0x04, 0x08},
    /* 8 */ {0x0E, 0x11, 0x0E, 0x11, 0x0E},
    /* 9 */ {0x0E, 0x11, 0x0F, 0x01, 0x0E},
    /* - */ {0x00, 0x00, 0x1F, 0x00, 0x00},
    /* . */ {0x00, 0x00, 0x00, 0x04, 0x04},
    /* + */ {0x00, 0x04, 0x1F, 0x04, 0x00},
};

static int glyph_index(char c) {
    if (c >= 'A' && c <= 'Z') return (int)(c - 'A');
    if (c >= 'a' && c <= 'z') return (int)(c - 'a');
    if (c >= '0' && c <= '9') return 26 + (int)(c - '0');
    if (c == '-') return 36;
    if (c == '.') return 37;
    if (c == '+') return 38;
    return -1;
}

/* Рисует одну букву: левый верхний угол (row, col), цвет #. */
static void draw_glyph(int row, int col, int g, unsigned char attr) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    for (int r = 0; r < 5; r++) {
        unsigned char bits = FONT[g][r];
        for (int c = 0; c < 5; c++) {
            int on = (bits >> (4 - c)) & 1;
            if (row + r >= 30 || row + r < 0) continue;
            int x = col + c;
            if (x >= 80) continue;
            int off = ((row + r) * 80 + x) * 2;
            vga[off] = on ? '#' : ' ';
            vga[off + 1] = attr;
        }
    }
}

/* Печатает обычную строку (под артом). Возвращает колонку конца. */
static int draw_plain(int row, const char *s, unsigned char attr) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    int col = 0;
    while (*s && col < 79) {
        vga[row * 160 + col * 2] = (unsigned char)*s++;
        vga[row * 160 + col * 2 + 1] = attr;
        col++;
    }
    return col;
}

#define GLYPH_W 6 /* 5 колонок + зазор */
#define GLYPHS_PER_ROW 13 /* 13 * 6 = 78 <= 80 */

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    /* Собираем строку из argv[1..], разделяя аргументы пробелами. */
    char word[192];
    int n = 0;
    for (unsigned long i = 1; i < argc; i++) {
        const char *a = argv[i];
        if (!a) break;
        if (n > 0) word[n++] = ' ';
        while (*a && n < 190) word[n++] = (char)*a++;
    }
    if (n == 0) {
        word[0] = 'V'; word[1] = 'I'; word[2] = 'T'; word[3] = 'A';
        n = 4;
    }
    word[n] = 0;

    /* Разворачиваем в массив глифов. */
    int glyphs[192];
    int ng = 0;
    for (int i = 0; i < n; i++) {
        int g = glyph_index(word[i]);
        if (g >= 0 && ng < 192)
            glyphs[ng++] = g;
    }

    int start_row = (int)(vga_offset / 160);
    int block_rows = (ng + GLYPHS_PER_ROW - 1) / GLYPHS_PER_ROW;
    int need = block_rows * 5 + 2; /* арт + строка текста */
    int avail = 30 - start_row;
    if (need > avail) start_row = 30 - need;
    if (start_row < 0) start_row = 0;
    /* Крайний случай: очень длинная строка — рисуем сколько влезает. */
    avail = 30 - start_row;
    if (avail >= 7) {
        int fit = (avail - 2) / 5;
        if (block_rows > fit)
            block_rows = fit;
    } else {
        block_rows = 1;
    }

    for (int b = 0; b < block_rows; b++) {
        int row = start_row + b * 5;
        for (int k = 0; k < GLYPHS_PER_ROW; k++) {
            int gi = b * GLYPHS_PER_ROW + k;
            if (gi >= ng) break;
            draw_glyph(row, k * GLYPH_W, glyphs[gi], 0x0A); /* зелёный */
        }
    }

    int text_row = start_row + block_rows * 5;
    if (text_row >= 30) text_row = 29;
    int end_col = draw_plain(text_row, word, 0x0F);

    *(volatile int *)EXIT_ROW_P = text_row;
    *(volatile int *)EXIT_COL_P = end_col;
}