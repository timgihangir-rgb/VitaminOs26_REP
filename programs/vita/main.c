#define MAX_LINES 500
#define MAX_FILE 4096
#define TEXT_ROWS 28
#define COLS 80

#define DIRTY      (*(volatile unsigned int *)0x6000)
#define FILE_SIZE  (*(volatile unsigned int *)0x6004)
#define MAX_SIZE   (*(volatile unsigned int *)0x6008)
#define FILENAME   ((volatile char *)0x600C)
#define FILEDATA   ((volatile char *)0x608C)

#define KBHIT_PTR   0x7828
#define KBREAD_PTR  0x7830

typedef int (*kbhit_fn)(void);
typedef unsigned int (*kbread_fn)(void);

#define abi_kbhit()  ((*(volatile kbhit_fn *)KBHIT_PTR)())
#define abi_kbread() ((*(volatile kbread_fn *)KBREAD_PTR)())

static int nlines;
static int line_off[MAX_LINES];
static int line_len[MAX_LINES];
static int cur_line, cur_col;
static int scroll;
static int modified;
static int ctrl_pressed;
static int shift_pressed;

static void outb(unsigned short port, unsigned char val);
static void set_cursor(int row, int col);
static void clear_screen(void);
static void draw_char(int row, int col, unsigned char ch, unsigned char attr);
static void draw_str(int row, int col, const char *s, unsigned char attr);
static void draw_str_n(int row, int col, const char *s, int n, unsigned char attr);
static void fill_row(int row, unsigned char attr);
static void rebuild(void);
static void delete_at(int pos);
static void ensure_visible(void);
static unsigned char kb_read(void);
static int kb_hit(void);

/* Shift-раскладка: цифры -> символы, буквы -> заглавные (как в ядре). */
static char shift_char(char c) {
    switch (c) {
        case '1': return '!'; case '2': return '@'; case '3': return '#';
        case '4': return '$'; case '5': return '%'; case '6': return '^';
        case '7': return '&'; case '8': return '*'; case '9': return '(';
        case '0': return ')'; case '-': return '_'; case '=': return '+';
        case ',': return '<'; case '.': return '>'; case '/': return '?';
        case '[': return '{'; case ']': return '}'; case ';': return ':';
        case '\'': return '"'; case '`': return '~'; case '\\': return '|';
        default:
            if (c >= 'a' && c <= 'z')
                return (char)(c - 32);
            return c;
    }
}

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    (void)argc;
    (void)argv;
    (void)vga_offset;
    int i;

    rebuild();

    cur_line = 0;
    cur_col = 0;
    scroll = 0;
    modified = 0;
    ctrl_pressed = 0;
    shift_pressed = 0;

    clear_screen();

    for (;;) {
        fill_row(0, 0x30);
        draw_str(0, 0, " Vita: ", 0x30);
        {
            int col = 7;
            const volatile char *fn = FILENAME;
            while (*fn && col < COLS - 20) {
                draw_char(0, col, *fn, 0x30);
                col++; fn++;
            }
            if (modified) draw_str(0, col, " [Modified]", 0x30);
        }
        {
            int tmp = cur_line + 1;
            char rev[10];
            int ri = 0;
            if (tmp == 0) rev[ri++] = '0';
            while (tmp > 0) { rev[ri++] = '0' + (tmp % 10); tmp /= 10; }
            int lx = COLS - 16;
            draw_str(0, lx, "Ln ", 0x30);
            for (i = 0; i < ri; i++)
                draw_char(0, lx + 3 + i, rev[ri - 1 - i], 0x30);
            draw_char(0, lx + 3 + ri, ':', 0x30);
            tmp = cur_col + 1;
            ri = 0;
            if (tmp == 0) rev[ri++] = '0';
            while (tmp > 0) { rev[ri++] = '0' + (tmp % 10); tmp /= 10; }
            for (i = 0; i < ri; i++)
                draw_char(0, lx + 4 + ri + i, rev[ri - 1 - i], 0x30);
        }

        for (int r = 0; r < TEXT_ROWS; r++) {
            int file_idx = scroll + r;
            if (file_idx < nlines) {
                int off = line_off[file_idx];
                int len = line_len[file_idx];
                if (len > COLS) len = COLS;
                draw_str_n(r + 1, 0, (const char *)(FILEDATA + off), len, 0x0F);
                for (int c = len; c < COLS; c++) draw_char(r + 1, c, ' ', 0x0F);
            } else {
                fill_row(r + 1, 0x0F);
            }
        }

        fill_row(29, 0x07);
        draw_str(29, 0, " Ctrl+Q Quit  Ctrl+S Save  Tab=4 AutoIndent ", 0x0E);

        set_cursor(cur_line - scroll + 1, cur_col < COLS ? cur_col : COLS - 1);

        unsigned char sc;
        for (;;) {
            while (!kb_hit()) {}
            sc = kb_read();

            if (sc == 0xE0) {
                while (!kb_hit()) {}
                sc = kb_read();
                if (sc & 0x80) continue;
                switch (sc) {
                    case 0x48: /* Up */
                        if (cur_line > 0) cur_line--;
                        break;
                    case 0x50: /* Down */
                        if (cur_line + 1 < nlines) cur_line++;
                        break;
                    case 0x4B: /* Left */
                        if (cur_col > 0) { cur_col--; }
                        else if (cur_line > 0) { cur_line--; cur_col = line_len[cur_line]; }
                        break;
                    case 0x4D: /* Right */
                        if (cur_col < line_len[cur_line]) { cur_col++; }
                        else if (cur_line + 1 < nlines) { cur_line++; cur_col = 0; }
                        break;
                    case 0x47: /* Home */ cur_col = 0; break;
                    case 0x4F: /* End */ cur_col = line_len[cur_line]; break;
                    case 0x49: /* PgUp */
                        if (cur_line >= TEXT_ROWS) cur_line -= TEXT_ROWS;
                        else cur_line = 0;
                        break;
                    case 0x51: /* PgDn */
                        cur_line += TEXT_ROWS;
                        if (cur_line >= nlines) cur_line = nlines - 1;
                        break;
                    case 0x53: /* Delete */
                        if (cur_col < line_len[cur_line]) {
                            delete_at(line_off[cur_line] + cur_col);
                            modified = 1;
                            rebuild();
                        } else if (cur_line + 1 < nlines) {
                            int pos = line_off[cur_line] + line_len[cur_line];
                            if (pos < FILE_SIZE && FILEDATA[pos] == '\n') {
                                delete_at(pos);
                                modified = 1;
                                rebuild();
                            }
                        }
                        break;
                }
                ensure_visible();
                break;
            }

            switch (sc) {
                case 0x2A: case 0x36: shift_pressed = 1; continue;
                case 0xAA: case 0xB6: shift_pressed = 0; continue;
                case 0x1D: ctrl_pressed = 1; continue;
                case 0x9D: ctrl_pressed = 0; continue;
            }

            if (sc & 0x80) continue;

            if (ctrl_pressed) {
                ctrl_pressed = 0;
                char c = 0;
                switch (sc) {
                    case 0x1F: c = 's'; break;
                    case 0x10: c = 'q'; break;
                }
        if (c == 's') {
            DIRTY = 1;
            modified = 0;
            fill_row(29, 0x07);
            draw_str(29, 0, " Saved!                     ", 0x0A);
            continue;
        }
                if (c == 'q') {
                    if (modified) {
                        fill_row(29, 0x07);
                        draw_str(29, 0, " Save modified buffer? (y/n) ", 0x0E);
                        for (;;) {
                            while (!kb_hit()) {}
                            unsigned char ans = kb_read();
                            if (ans & 0x80) continue;
                    if (ans == 0x15) { DIRTY = 1; break; }
                    if (ans == 0x31) { break; }
                        }
                    }
                    clear_screen();
                    return;
                }
                continue;
            }

            switch (sc) {
                case 0x1C: /* Enter: разрыв строки с авто-отступом */
                    if (FILE_SIZE >= MAX_SIZE) break;
                    {
                        char *ln = FILEDATA + line_off[cur_line];
                        int indent = 0;
                        while (indent < line_len[cur_line] && ln[indent] == ' ')
                            indent++;
                        /* Умный отступ: строка заканчивается открывающим блоком
                         * ({ или [) — новая строка глубже на 4. */
                        int extra = 0;
                        int last = line_len[cur_line];
                        while (last > 0 && (ln[last - 1] == ' ' || ln[last - 1] == '\t'))
                            last--;
                        if (last > 0 && (ln[last - 1] == '{' || ln[last - 1] == '['))
                            extra = 4;
                        int d = 1 + indent + extra; /* '\n' + пробелы */
                        if (FILE_SIZE + d > MAX_SIZE) break;
                        int pos = line_off[cur_line] + cur_col;
                        for (int k = FILE_SIZE; k > pos; k--)
                            FILEDATA[k + d - 1] = FILEDATA[k - 1];
                        FILEDATA[pos] = '\n';
                        for (int k = 0; k < indent + extra; k++)
                            FILEDATA[pos + 1 + k] = ' ';
                        FILE_SIZE += d;
                        modified = 1;
                        rebuild();
                        cur_line++;
                        cur_col = indent + extra;
                    }
                    break;
                case 0x0E: /* Backspace */
                    if (cur_col > 0) {
                        delete_at(line_off[cur_line] + cur_col - 1);
                        cur_col--;
                        modified = 1;
                        rebuild();
                    } else if (cur_line > 0) {
                        int pos = line_off[cur_line] - 1;
                        if (pos >= 0 && FILEDATA[pos] == '\n') {
                            delete_at(pos);
                            cur_col = line_len[cur_line - 1];
                            cur_line--;
                            modified = 1;
                            rebuild();
                        }
                    }
                    break;
                case 0x0F: /* Tab: до следующей позиции табуляции (шаг 4) */
                    {
                        int d = 4 - (cur_col % 4);
                        if (FILE_SIZE + d > MAX_SIZE) break;
                        int pos = line_off[cur_line] + cur_col;
                        for (int k = FILE_SIZE; k > pos; k--)
                            FILEDATA[k + d - 1] = FILEDATA[k - 1];
                        for (int t = 0; t < d; t++)
                            FILEDATA[pos + t] = ' ';
                        FILE_SIZE += d;
                        cur_col += d;
                        modified = 1;
                        rebuild();
                    }
                    break;
                default: {
                    char ch = 0;
                    switch (sc) {
                        case 0x02: ch = '1'; break; case 0x03: ch = '2'; break;
                        case 0x04: ch = '3'; break; case 0x05: ch = '4'; break;
                        case 0x06: ch = '5'; break; case 0x07: ch = '6'; break;
                        case 0x08: ch = '7'; break; case 0x09: ch = '8'; break;
                        case 0x0A: ch = '9'; break; case 0x0B: ch = '0'; break;
                        case 0x0C: ch = '-'; break; case 0x0D: ch = '='; break;
                        case 0x10: ch = 'q'; break; case 0x11: ch = 'w'; break;
                        case 0x12: ch = 'e'; break; case 0x13: ch = 'r'; break;
                        case 0x14: ch = 't'; break; case 0x15: ch = 'y'; break;
                        case 0x16: ch = 'u'; break; case 0x17: ch = 'i'; break;
                        case 0x18: ch = 'o'; break; case 0x19: ch = 'p'; break;
                        case 0x1A: ch = '['; break; case 0x1B: ch = ']'; break;
                        case 0x1E: ch = 'a'; break; case 0x1F: ch = 's'; break;
                        case 0x20: ch = 'd'; break; case 0x21: ch = 'f'; break;
                        case 0x22: ch = 'g'; break; case 0x23: ch = 'h'; break;
                        case 0x24: ch = 'j'; break; case 0x25: ch = 'k'; break;
                        case 0x26: ch = 'l'; break; case 0x27: ch = ';'; break;
                        case 0x28: ch = '\''; break; case 0x29: ch = '`'; break;
                        case 0x2B: ch = '\\'; break;
                        case 0x2C: ch = 'z'; break; case 0x2D: ch = 'x'; break;
                        case 0x2E: ch = 'c'; break; case 0x2F: ch = 'v'; break;
                        case 0x30: ch = 'b'; break; case 0x31: ch = 'n'; break;
                        case 0x32: ch = 'm'; break; case 0x33: ch = ','; break;
                        case 0x34: ch = '.'; break; case 0x35: ch = '/'; break;
                        case 0x39: ch = ' '; break;
                    }
                    if (shift_pressed)
                        ch = shift_char(ch);
                    if (ch && FILE_SIZE < MAX_SIZE) {
                        int pos = line_off[cur_line] + cur_col;
                        for (i = FILE_SIZE; i > pos; i--) FILEDATA[i] = FILEDATA[i - 1];
                        FILEDATA[pos] = ch;
                        FILE_SIZE++;
                        cur_col++;
                        if (ch == '}') {
                            /* Выравниваем } по родителю: отступ строки минус 4. */
                            char *ln = FILEDATA + line_off[cur_line];
                            int ind = 0;
                            while (ind < line_len[cur_line] && ln[ind] == ' ') ind++;
                            int targ = ind >= 4 ? ind - 4 : 0;
                            cur_col = targ;
                        }
                        modified = 1;
                        rebuild();
                    }
                }
            }
            ensure_visible();
            break;
        }
    }
}

static void rebuild(void) {
    int pos = 0;
    nlines = 0;
    while (pos < FILE_SIZE && nlines < MAX_LINES) {
        line_off[nlines] = pos;
        int start = pos;
        while (pos < FILE_SIZE && FILEDATA[pos] != '\n') pos++;
        line_len[nlines] = pos - start;
        nlines++;
        if (pos < FILE_SIZE && FILEDATA[pos] == '\n') pos++;
    }
    if (nlines == 0) {
        line_off[0] = 0;
        line_len[0] = 0;
        nlines = 1;
    }
    if (FILE_SIZE > 0 && FILEDATA[FILE_SIZE - 1] == '\n' && nlines < MAX_LINES) {
        line_off[nlines] = FILE_SIZE;
        line_len[nlines] = 0;
        nlines++;
    }
}

static void delete_at(int pos) {
    if (pos < 0 || pos >= FILE_SIZE) return;
    for (int i = pos; i < FILE_SIZE - 1; i++)
        FILEDATA[i] = FILEDATA[i + 1];
    FILE_SIZE--;
}

static void ensure_visible(void) {
    if (cur_line < 0) cur_line = 0;
    if (cur_line >= nlines) cur_line = nlines - 1;
    if (cur_line < scroll)
        scroll = cur_line;
    else if (cur_line >= scroll + TEXT_ROWS)
        scroll = cur_line - TEXT_ROWS + 1;
    int max_col = line_len[cur_line];
    if (cur_col > max_col) cur_col = max_col;
    if (cur_col < 0) cur_col = 0;
}

static void outb(unsigned short port, unsigned char val) {
    __asm__ volatile("outb %0, %1" : : "a"(val), "Nd"(port));
}

static void set_cursor(int row, int col) {
    int pos = row * 80 + col;
    outb(0x3D4, 0x0F);
    outb(0x3D5, pos & 0xFF);
    outb(0x3D4, 0x0E);
    outb(0x3D5, (pos >> 8) & 0xFF);
}

static void clear_screen(void) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    for (int i = 0; i < 80 * 30 * 2; i++)
        vga[i] = (i & 1) ? 0x0F : ' ';
}

static void draw_char(int row, int col, unsigned char ch, unsigned char attr) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    int idx = (row * 80 + col) * 2;
    vga[idx] = ch;
    vga[idx + 1] = attr;
}

static void draw_str(int row, int col, const char *s, unsigned char attr) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    int idx = (row * 80 + col) * 2;
    while (*s) {
        vga[idx] = *s++;
        vga[idx + 1] = attr;
        idx += 2;
    }
}

static void draw_str_n(int row, int col, const char *s, int n, unsigned char attr) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    int idx = (row * 80 + col) * 2;
    for (int i = 0; i < n; i++) {
        unsigned char c = (unsigned char)s[i];
        if (c < 32 || c == 127) c = '.';
        vga[idx] = c;
        vga[idx + 1] = attr;
        idx += 2;
    }
}

static void fill_row(int row, unsigned char attr) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    int idx = row * 80 * 2;
    for (int c = 0; c < 80; c++) {
        vga[idx] = ' ';
        vga[idx + 1] = attr;
        idx += 2;
    }
}

static int kb_hit(void) {
    return abi_kbhit();
}

static unsigned char kb_read(void) {
    return (unsigned char)abi_kbread();
}
