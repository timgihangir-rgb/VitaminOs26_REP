static unsigned char inb(unsigned short port);
static void outb(unsigned short port, unsigned char val);
static void clear_screen(void);
static void draw_char(int row, int col, unsigned char ch, unsigned char attr);
static void draw_str(int row, int col, const char *s, unsigned char attr);
static int kb_hit(void);
static unsigned char kb_read(void);
static unsigned int pit_now(void);
static void draw_score(int score);
static int next_food(int *fx, int *fy, const int *sx, const int *sy, int len,
                     int rows, int cols);
static unsigned int rand_state;

#define STEP_MS 180

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    (void)argc;
    (void)argv;
    rand_state = vga_offset + 12345;

    clear_screen();

    /* Малое поле в левой части экрана 80x30: стены на строках 0/21 и
     * колонках 0/61, игровая область - строки 1..20 x колонки 1..60.
     * Свободное место справа (колонки 63..79) отведено под счёт очков,
     * внизу (строки 22..29) - сообщение об окончании игры. */
    int rows = 20;
    int cols = 60;
    int top = 1;
    int left = 1;
    int bottom = top + rows - 1;
    int right = left + cols - 1;

    for (int c = left; c <= right; c++) {
        draw_char(top - 1, c, '#', 0x0B);
        draw_char(bottom + 1, c, '#', 0x0B);
    }
    for (int r = top; r <= bottom; r++) {
        draw_char(r, left - 1, '#', 0x0B);
        draw_char(r, right + 1, '#', 0x0B);
    }

    int sx[500], sy[500];
    int slen = 3;
    int dir = 0;
    int next_dir = dir;

    sx[0] = cols / 2; sy[0] = rows / 2;
    sx[1] = sx[0] - 1; sy[1] = sy[0];
    sx[2] = sx[1] - 1; sy[2] = sy[1];

    int fx = -1, fy = -1;
    next_food(&fx, &fy, sx, sy, slen, rows, cols);

    int score = 0;
    int game_over = 0;

    for (int i = 0; i < slen; i++)
        draw_char(sy[i] + top, sx[i] + left, i == 0 ? '@' : 'o', 0x0A);

    draw_score(0);

    /* Движение по таймеру PIT (ядро поставило его на 100 Гц, делитель 11932):
     * каждый шаг ждём STEP_MS реального времени, а не крутим сырой busy-loop,
     * скорость которого зависит от CPU/QEMU. */
    unsigned int prev_pit = pit_now();
    unsigned int elapsed_ms = 0;

    for (;;) {
        unsigned int c = pit_now();
        if ((int)c > (int)prev_pit)
            elapsed_ms += 10;   /* PIT-счётчик обернулся = прошло 10 мс */
        prev_pit = c;

        if (elapsed_ms >= STEP_MS) {
            elapsed_ms = 0;

            int old_tail_x = sx[slen - 1];
            int old_tail_y = sy[slen - 1];

            dir = next_dir;

            int nx = sx[0], ny = sy[0];
            switch (dir) {
                case 0: nx++; break;
                case 1: ny++; break;
                case 2: nx--; break;
                case 3: ny--; break;
            }

            if (nx < 0 || nx >= cols || ny < 0 || ny >= rows) {
                game_over = 1;
                break;
            }

            int hitself = 0;
            for (int i = 0; i < slen; i++) {
                if (sx[i] == nx && sy[i] == ny) { hitself = 1; break; }
            }
            if (hitself) { game_over = 1; break; }

            int ate = (nx == fx && ny == fy);

            for (int i = slen - 1; i > 0; i--) {
                sx[i] = sx[i - 1];
                sy[i] = sy[i - 1];
            }
            sx[0] = nx; sy[0] = ny;

            if (ate) {
                score++;
                draw_score(score);
                int tail2_x = sx[slen - 2];
                int tail2_y = sy[slen - 2];
                int tdx = old_tail_x - tail2_x;
                int tdy = old_tail_y - tail2_y;
                slen += 2;
                sx[slen - 2] = old_tail_x;
                sy[slen - 2] = old_tail_y;
                sx[slen - 1] = old_tail_x + tdx;
                sy[slen - 1] = old_tail_y + tdy;
                fx = -1; fy = -1;
                if (!next_food(&fx, &fy, sx, sy, slen, rows, cols)) {
                    game_over = 1;
                    break;
                }
            }

            for (int i = 0; i < slen; i++)
                draw_char(sy[i] + top, sx[i] + left,
                          i == 0 ? '@' : 'o', 0x0A);

            if (!ate)
                draw_char(old_tail_y + top, old_tail_x + left, ' ', 0x0F);

            if (fx >= 0 && fy >= 0)
                draw_char(fy + top, fx + left, '*', 0x0C);
        }

        if (kb_hit()) {
            unsigned char sc = kb_read();
            if (sc == 0x10) break;
            if (sc == 0x11 && dir != 1) next_dir = 3;
            if (sc == 0x1F && dir != 3) next_dir = 1;
            if (sc == 0x1E && dir != 0) next_dir = 2;
            if (sc == 0x20 && dir != 2) next_dir = 0;
        }
    }

    if (game_over)
        draw_str(23, 0, "Game Over!", 0x0C);

    char score_str[30];
    int si = 0;
    int tmp = score;
    char rev[10];
    int ri = 0;
    if (tmp == 0) rev[ri++] = '0';
    while (tmp > 0) { rev[ri++] = '0' + (tmp % 10); tmp /= 10; }
    while (ri > 0) score_str[si++] = rev[--ri];
    score_str[si] = '\0';

    draw_str(23, 12, "Score: ", 0x0F);
    draw_str(23, 19, score_str, 0x0F);
    draw_str(23, 26, "Press Q to quit", 0x07);

    while (1) {
        if (kb_hit() && kb_read() == 0x10) break;
    }

    clear_screen();
}

static unsigned char inb(unsigned short port) {
    unsigned char result;
    __asm__ volatile("inb %1, %0" : "=a"(result) : "Nd"(port));
    return result;
}

static void outb(unsigned short port, unsigned char val) {
    __asm__ volatile("outb %0, %1" :: "a"(val), "Nd"(port));
}

/* Читает текущее значение счётчика PIT, канал 0 (LATCH + LSB/MSB).
 * Ядро ставит PIT на 100 Гц (делитель 11932, режим 3), т.е. счётчик идёт
 * 11932..0 и перезагружается; каждое "обёртывание" - 10 мс. Чтение через
 * LATCH не сбрасывает таймер, поэтому оно безопасно для ядра. */
static unsigned int pit_now(void) {
    unsigned char lsb, msb;
    outb(0x43, 0x00);
    lsb = inb(0x40);
    msb = inb(0x40);
    return (unsigned int)((msb << 8) | lsb);
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

/* Живой счёт очков в свободном месте справа от поля (строка 1, колонки 63..).
 * Пишется фиксированной шириной, чтобы затирать предыдущее значение. */
static void draw_score(int score) {
    char buf[16];
    int i = 0;
    buf[i++] = 'S'; buf[i++] = 'c'; buf[i++] = 'o'; buf[i++] = 'r';
    buf[i++] = 'e'; buf[i++] = ':'; buf[i++] = ' ';
    int tmp = score;
    char rev[10];
    int ri = 0;
    if (tmp == 0) rev[ri++] = '0';
    while (tmp > 0) { rev[ri++] = '0' + (tmp % 10); tmp /= 10; }
    while (ri > 0) buf[i++] = rev[--ri];
    while (i < 15) buf[i++] = ' ';
    buf[15] = '\0';
    draw_str(1, 63, buf, 0x0F);
}

/* Ввод через ABI ядра (int 0x80): SYS_KBHIT=6 / SYS_KBREAD=7. Трамплины
 * ставит progabi по фиксированным адресам пользовательского стека-страницы.
 * Читать порты 0x60/0x64 напрямую нельзя: их же обслуживает IRQ1-обработчик
 * ядра, и он забирает сканкоды первым - игра остаётся без управления, а
 * накопленное в буфере ядра вываливается в шелл после выхода. */
#define KBHIT_PTR   0x7828
#define KBREAD_PTR  0x7830

typedef int (*kbhit_fn)(void);
typedef unsigned int (*kbread_fn)(void);

static int kb_hit(void) {
    return ((*(volatile kbhit_fn *)KBHIT_PTR)());
}

static unsigned char kb_read(void) {
    return (unsigned char)((*(volatile kbread_fn *)KBREAD_PTR)());
}

static int next_food(int *fx, int *fy, const int *sx, const int *sy, int len,
                      int rows, int cols) {
    int free = 0;
    for (int r = 0; r < rows * cols; r++) {
        int x = r % cols;
        int y = r / cols;
        int occupied = 0;
        for (int i = 0; i < len; i++) {
            if (sx[i] == x && sy[i] == y) { occupied = 1; break; }
        }
        if (!occupied) free++;
    }
    if (free == 0) return 0;

    rand_state = rand_state * 1103515245 + 12345;
    int target = rand_state % free;

    int n = 0;
    for (int r = 0; r < rows * cols; r++) {
        int x = r % cols;
        int y = r / cols;
        int occupied = 0;
        for (int i = 0; i < len; i++) {
            if (sx[i] == x && sy[i] == y) { occupied = 1; break; }
        }
        if (!occupied) {
            if (n == target) { *fx = x; *fy = y; return 1; }
            n++;
        }
    }
    return 0;
}
