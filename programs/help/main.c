#define ROWS 30
#define COLS 80

#define PROGLIST ((volatile char *)0x608C)
#define EXIT_ROW (*(volatile int *)0x708C)
#define EXIT_COL (*(volatile int *)0x7090)

static void scroll(void);
static void wputc(unsigned char ch, unsigned char attr);
static void wputs(const char *s, unsigned char attr);
static void pad_to(int target, unsigned char attr);
static void clear_screen(void);

static const char COMMANDS[][8] = {
    "help", "pwd", "ls", "cd", "mkdir", "rmdir", "touch", "rm",
    "cat", "cp", "mv", "find", "echo", "run", "bg", "ps",
    "kill", "init", "clear", "version",
};

static const char DESCS[][40] = {
    "Show this help", "Print working directory", "List directory",
    "Change directory", "Create directory", "Remove empty directory",
    "Create empty file", "Remove file", "Display file content",
    "Copy file or directory", "Move or rename file/directory",
    "Search filesystem for a file by name", "Echo text / redirect to file",
    "Run a program from VFS", "Run a background task", "List running tasks",
    "Terminate a background task", "Manage init services", "Clear screen",
    "Show OS version",
};

#define NUM_COMMANDS ((int)(sizeof(COMMANDS) / sizeof(COMMANDS[0])))

#define MAX_PROGS 64
#define PROG_NAME_LEN 16

static int r;
static int c;
static char prog_names[MAX_PROGS][PROG_NAME_LEN];
static int prog_count;

void _start(unsigned int vga_offset) {
    (void)vga_offset;

    clear_screen();

    wputs("=================================\n", 0x0F);
    wputs("  VitaminOS26 - Help\n", 0x0B);
    wputs("=================================\n", 0x0F);

    wputs("Standard commands:\n", 0x0B);
    for (int i = 0; i < NUM_COMMANDS; i++) {
        wputs("  ", 0x0F);
        wputs(COMMANDS[i], 0x0F);
        pad_to(15, 0x0F);
        wputs(" - ", 0x0F);
        wputs(DESCS[i], 0x0F);
        wputc('\n', 0x0F);
    }

    const volatile char *p = PROGLIST;
    prog_count = 0;
    while (*p && prog_count < MAX_PROGS) {
        int len = 0;
        while (*p && *p != '\n' && len < PROG_NAME_LEN - 1)
            prog_names[prog_count][len++] = *p++;
        prog_names[prog_count][len] = 0;
        prog_count++;
        if (*p == '\n')
            p++;
    }

    wputs("Installed programs:\n", 0x0B);
    if (prog_count == 0) {
        wputs("  (none)\n", 0x0F);
    } else {
        for (int i = 0; i < prog_count; i++) {
            if (i % 2 == 0)
                wputs("  ", 0x0F);
            wputs(prog_names[i], 0x0F);
            int l = 0;
            while (prog_names[i][l])
                l++;
            while (l < PROG_NAME_LEN) {
                wputc(' ', 0x0F);
                l++;
            }
            if (i % 2 == 1)
                wputc('\n', 0x0F);
        }
    }

    EXIT_ROW = r;
    EXIT_COL = c;
}

static void scroll(void) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    for (int i = 0; i < (ROWS - 1) * COLS * 2; i++)
        vga[i] = vga[i + COLS * 2];
    for (int i = (ROWS - 1) * COLS * 2; i < ROWS * COLS * 2; i += 2) {
        vga[i] = ' ';
        vga[i + 1] = 0x0F;
    }
    r = ROWS - 1;
}

static void wputc(unsigned char ch, unsigned char attr) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    if (ch == '\n') {
        c = 0;
        if (r == ROWS - 1)
            scroll();
        else
            r++;
        return;
    }
    vga[(r * COLS + c) * 2] = ch;
    vga[(r * COLS + c) * 2 + 1] = attr;
    if (++c == COLS) {
        c = 0;
        if (r == ROWS - 1)
            scroll();
        else
            r++;
    }
}

static void wputs(const char *s, unsigned char attr) {
    while (*s)
        wputc(*s++, attr);
}

static void pad_to(int target, unsigned char attr) {
    while (c < target)
        wputc(' ', attr);
}

static void clear_screen(void) {
    volatile unsigned char *vga = (volatile unsigned char *)0xB8000;
    for (int i = 0; i < ROWS * COLS * 2; i++)
        vga[i] = (i & 1) ? 0x0F : ' ';
    r = 0;
    c = 0;
}
