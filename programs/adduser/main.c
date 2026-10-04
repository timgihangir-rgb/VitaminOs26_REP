/* adduser: создаёт учётную запись и домашний каталог.
 *
 *   sudo adduser <имя>
 *
 * Проверяет пароль в ядре (системный вызов useradd) и сразу создаёт
 * /home/<имя> с владельцем нового пользователя, чтобы он мог писать туда без
 * sudo. Само создание доступно только root, поэтому программу надо запускать
 * через sudo.
 *
 * Домашний каталог удалённой учётки не трогает - это осознанное решение
 * администратора: `rm -rf /home/<имя>` вручную. */

#include "../common/vitauser.h"

#define PASS_MAX 65

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    vu_init(vga_offset);

    if (abi_getuid() != 0) {
        vu_puts("adduser: needs administrator rights; run as:\n", ATTR_RED);
        vu_puts("  sudo adduser <name>\n", ATTR_WHITE);
        VU_EXIT();
    }

    if (argc != 2) {
        vu_puts("usage: sudo adduser <name>\n", ATTR_WHITE);
        vu_puts("name: latin letters, digits, dash and underscore\n", ATTR_WHITE);
        VU_EXIT();
    }

    const char *name = argv[1];

    char pass[PASS_MAX];
    char again[PASS_MAX];
    vu_puts("New password for ", ATTR_CYAN);
    vu_puts(name, ATTR_WHITE);
    vu_puts(": ", ATTR_WHITE);
    vu_read_line(pass, sizeof pass, 0);
    vu_puts("Retype password: ", ATTR_WHITE);
    vu_read_line(again, sizeof again, 0);

    if (!vu_streq(pass, again)) {
        vu_puts("adduser: passwords do not match\n", ATTR_RED);
        for (unsigned i = 0; i < sizeof pass; i++) {
            pass[i] = '\0';
            again[i] = '\0';
        }
        VU_EXIT();
    }

    int r = abi_useradd(name, pass);
    for (unsigned i = 0; i < sizeof pass; i++) {
        pass[i] = '\0';
        again[i] = '\0';
    }

    if (r == 0) {
        vu_puts("user ", ATTR_GREEN);
        vu_puts(name, ATTR_WHITE);
        vu_puts(" created, home directory /home/", ATTR_WHITE);
        vu_puts(name, ATTR_WHITE);
        vu_puts("\n", ATTR_WHITE);
    } else if (r == -3) {
        vu_puts("adduser: unusable name (letters, digits, dash, underscore)\n",
                ATTR_RED);
    } else {
        vu_puts("adduser: cannot create user (it exists already?)\n", ATTR_RED);
    }
    VU_EXIT();
}