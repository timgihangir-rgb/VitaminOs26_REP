/* removeuser: удаляет учётную запись.
 *
 *   sudo removeuser <имя>
 *
 * Домашний каталог остаётся на месте: под ним могут лежать данные, которые
 * ещё нужны, и удалять их молча нельзя. Убрать каталог - осознанное действие
 * администратора (`rm -rf /home/<имя>` от root).
 *
 * Пароль убираемой записи не спрашивается: право и так есть только у root,
 * а пароль чужой учётки администратору ничего не даёт. */

#include "../common/vitauser.h"

void _start(unsigned long argc, char **argv, unsigned int vga_offset) {
    vu_init(vga_offset);

    if (abi_getuid() != 0) {
        vu_puts("removeuser: needs administrator rights; run as:\n", ATTR_RED);
        vu_puts("  sudo removeuser <name>\n", ATTR_WHITE);
        VU_EXIT();
    }

    if (argc != 2) {
        vu_puts("usage: sudo removeuser <name>\n", ATTR_WHITE);
        VU_EXIT();
    }

    const char *name = argv[1];

    if (abi_userdel(name) == 0) {
        vu_puts("user ", ATTR_GREEN);
        vu_puts(name, ATTR_WHITE);
        vu_puts(" removed\n", ATTR_WHITE);
        vu_puts("note: /home/", ATTR_CYAN);
        vu_puts(name, ATTR_CYAN);
        vu_puts(" was left in place\n", ATTR_CYAN);
    } else {
        vu_puts("removeuser: no such user, or it cannot be removed\n",
                ATTR_RED);
    }
    VU_EXIT();
}