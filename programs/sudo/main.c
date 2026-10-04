/* sudo: запускает команду от имени администратора.
 *
 *   sudo <программа> [аргументы...]
 *
 * Спрашивает пароль ТЕКУЩЕГО пользователя и, если он верный, поднимает права
 * задачи до root (системный вызов auth делает это в ядре: пароль проверяет
 * ядро, а не программа). После этого команда запускается сама системным
 * вызовом exec и выполняется уже с правами root, а sudo ждёт её завершения.
 *
 * Отдельного "setuid" в системе намеренно нет: любая программа могла бы
 * тогда повысить себя до root без пароля.
 *
 * Три неудачные попытки - и отказ. Как и в настоящем sudo, пароль показывается
 * звёздочками, чтобы было видно, что символы вводятся. */

#include "../common/vitauser.h"

#define ATTEMPTS 3
#define PASS_MAX 65

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    vu_init(vga_offset);

    /* Показываем пользователя от чьего имени работаем: одна учётная запись
     * может быть администратором, а под sudo - все. */
    char user[32];
    if (abi_whoami(user, sizeof user) <= 0)
        user[0] = '?';
    user[sizeof user - 1] = '\0';

    char pass[PASS_MAX];
    int ok = 0;
    for (int attempt = 0; attempt < ATTEMPTS; attempt++) {
        vu_puts("[sudo] password for ", ATTR_CYAN);
        vu_puts(user, ATTR_WHITE);
        vu_puts(": ", ATTR_WHITE);
        vu_read_line(pass, sizeof pass, 0);
        if (abi_auth(pass) == 0) {
            ok = 1;
            break;
        }
        /* Ошибок намеренно никаких: не подсказываем, что именно не так -
         * ни «пользователя нет», ни «пароль неверен». */
        vu_puts("Sorry, try again.\n", ATTR_RED);
    }

    /* Стираем пароль из памяти: он больше не нужен, а в дампе задачи ему
     * не место. */
    for (unsigned i = 0; i < sizeof pass; i++)
        pass[i] = '\0';

    if (!ok) {
        vu_puts("sudo: 3 incorrect password attempts\n", ATTR_RED);
        VU_EXIT_CODE(1);
    }

    /* sudo -v: только спросить пароль, ничего не запускать. Так проверку
     * пароля делает шелл перед встроенной командой: /bin/sudo - единственная
     * программа, которой это положено, а встроенных команд в /bin нет. */
    if (argc == 2 && argv[1][0] == '-' && argv[1][1] == 'v' && argv[1][2] == '\0') {
        VU_EXIT_CODE(0);
    }

    if (argc < 2) {
        vu_puts("usage: sudo <program> [arguments...]\n", ATTR_WHITE);
        VU_EXIT_CODE(1);
    }

    /* Команда и её аргументы копируем в свой буфер: указатели на строки из
     * argv ядро прочитает уже после возврата отсюда, а исходные строки живут
     * на user-стеке шелла и в любой момент могут быть затёрты. */
    static char store[8][128];
    static const char *args[9];
    int n = 0;
    for (unsigned long i = 1; i < argc && n < 8; i++) {
        unsigned long j = 0;
        while (argv[i][j] && j < sizeof store[0] - 1) {
            store[n][j] = argv[i][j];
            j++;
        }
        store[n][j] = '\0';
        args[n] = store[n];
        n++;
    }

    /* Права подняты: exec запускает команду от uid 0 (см. sys_exec). */
    int pid = abi_exec(store[0], (unsigned int)n, args);
    if (pid < 0) {
        vu_puts("sudo: cannot run ", ATTR_RED);
        vu_puts(store[0], ATTR_WHITE);
        vu_puts("\n", ATTR_WHITE);
        VU_EXIT();
    }

    abi_wait((unsigned int)pid);
    VU_EXIT();
}