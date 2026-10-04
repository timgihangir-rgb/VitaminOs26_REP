#!/usr/bin/env python3
"""Создаёт образ диска VitaminOS26 с ФС VITAFS (фаза 3).

Расклад 8-МиБ образа (константы зеркалят src/vitafs.rs, блок = 4096 Б):
  блок 0      : суперблок "VITAFS1\\0" + поля u32 LE
                (VERSION=8, BLOCK_SIZE=12, TOTAL_BLOCKS=16, TOTAL_INODES=20,
                 FREE_INODES=24, JOURNAL_START=28, JOURNAL_BLOCKS=32,
                 INODE_TABLE_START=36, DATA_START=40, FLAGS=44, ROOT_INODE=48)
  блоки 1..65 : журнал (пока нули)
  блоки 65..67: битмап блоков (бит p -> блок DATA_START+p)
  блоки 67..69: битмап инодов (биты 0..=2 заняты, корень = инод 2)
  блоки 69..101: таблица инодов, 1024 записи по 128 Б:
                 u16 itype | u16 perms | u32 size | u32 links |
                 u32 atime | u32 mtime | u32 device_id |
                 direct[12] @24..72 | indirect1@72 | indirect2@76 | indirect3@80
                 uid @84..86 (владелец; резерв 86..128)
  блоки 101.. : данные. Дирент = 64 Б: u32 ino | u8 len | имя (58 Б).

Использование: mkfs.py <img> <programs.list>
где programs.list — строки "имя<TAB>путь-к-elf".
"""
import hashlib
import os
import struct
import sys

SECTOR = 512
BLOCK = 4096
IMG_SIZE = 8 * 1024 * 1024
TOTAL_BLOCKS = IMG_SIZE // BLOCK  # 2048

MAGIC = b"VITAFS1\x00"
JOURNAL_START = 1
JOURNAL_BLOCKS = 64
BLOCK_BITMAP_START = 65
BLOCK_BITMAP_BLOCKS = 2
INODE_BITMAP_START = 67
INODE_TABLE_START = 69
DATA_START = INODE_TABLE_START + 32  # 101
INODE_SIZE = 128
INODES_PER_BLOCK = BLOCK // INODE_SIZE  # 32
TOTAL_INODES = INODES_PER_BLOCK * 32  # 1024
ROOT_INODE = 2
DIRECT_BLOCKS = 12
PTRS_PER_BLOCK = BLOCK // 4  # 1024
KERNEL_NAME = "vitamin_os26.elf"
FIRST_UID = 1000

TYPE_FILE = 1
TYPE_DIR = 2

# TEMP(3.2): clock/crashy исключены - латентная порча контекста при спавне
# задач под вытеснением (см. bigtodo, блокер zero-ctx). Вернуть после фикса.
RC_CONF = "# VitaminOS26 init services\nticker builtin respawn\n"

now = 0


class FsBuilder:
    """Прямой формировщик образа VITAFS: иноды и блоки выдаёт по порядку."""

    def __init__(self, total_blocks):
        self.total_blocks = total_blocks
        self.img = bytearray(total_blocks * BLOCK)
        self.next_inode = ROOT_INODE + 1  # 0 и 1 зарезервированы, 2 - корень
        self.next_data = 0  # смещение от DATA_START
        # Резервные биты инодного битмапа 0..=2 - как в format_image ядра.
        self.inode_bm = bytearray(BLOCK)
        for bit in range(ROOT_INODE + 1):
            self.inode_bm[bit // 8] |= 1 << (bit % 8)
        self.put_block(INODE_BITMAP_START, self.inode_bm)

    def put_block(self, block_no, data=b""):
        off = block_no * BLOCK
        self.img[off:off + len(data)] = data

    def alloc_inode(self, itype, size, links=1, direct=None, indirect1=0,
                    ino=None, perms=None, uid=0):
        if ino is None:
            ino = self.next_inode
            self.next_inode += 1
        if ino >= TOTAL_INODES:
            raise SystemExit("иноды закончились")
        if perms is None:
            perms = 0o755 if itype == TYPE_DIR else 0o644
        raw = struct.pack(
            "<HHIIIII", itype, perms, size, links, now, now, 0)
        d = list(direct or [])
        d += [0] * (DIRECT_BLOCKS - len(d))
        for i, v in enumerate(d[:DIRECT_BLOCKS]):
            raw += struct.pack("<I", v)
        raw += struct.pack("<III", indirect1, 0, 0)
        # Байты 84..86 - владелец (см. DiskInode::uid в src/vitafs.rs).
        raw += struct.pack("<H", uid)
        raw += b"\x00" * (INODE_SIZE - len(raw))
        assert len(raw) == INODE_SIZE
        tb_off = (INODE_TABLE_START * BLOCK) + ino * INODE_SIZE
        self.img[tb_off:tb_off + INODE_SIZE] = raw
        bm = INODE_BITMAP_START * BLOCK + ino // 8
        self.img[bm] |= 1 << (ino % 8)
        self.inode_bm[ino // 8] |= 1 << (ino % 8)
        return ino

    def alloc_data(self, data):
        """Кладёт блок данных, помечает в битмапе, возвращает его номер."""
        p = self.next_data
        max_p = self.total_blocks - DATA_START
        if p >= max_p:
            raise SystemExit("данные не помещаются на диск")
        self.next_data += 1
        blk = bytearray(BLOCK)
        blk[:len(data)] = data
        self.put_block(DATA_START + p, blk)
        bm_off = BLOCK_BITMAP_START * BLOCK + p // 8
        self.img[bm_off] |= 1 << (p % 8)
        return DATA_START + p

    def write_file(self, name, content, ino=None):
        direct, indirect1, indirect_tbl = [], 0, []
        n_full = len(content) // BLOCK
        for bi in range(n_full):
            b = self.alloc_data(content[bi * BLOCK:(bi + 1) * BLOCK])
            if len(direct) < DIRECT_BLOCKS:
                direct.append(b)
            else:
                indirect_tbl.append(b)
        tail = content[n_full * BLOCK:]
        if tail:
            b = self.alloc_data(tail)
            if len(direct) < DIRECT_BLOCKS:
                direct.append(b)
            else:
                indirect_tbl.append(b)
        if indirect_tbl:
            tbl = b"".join(struct.pack("<I", b) for b in indirect_tbl)
            tbl += b"\x00" * (BLOCK - len(tbl))
            indirect1 = self.alloc_data(tbl)  # таблица указателей - тоже блок
            if len(indirect_tbl) > PTRS_PER_BLOCK:
                raise SystemExit(f"{name}: больше {DIRECT_BLOCKS + PTRS_PER_BLOCK} блоков")
        return self.alloc_inode(TYPE_FILE, len(content), direct=direct,
                                indirect1=indirect1, ino=ino)

    def write_dir(self, entries, ino=None, perms=None, uid=0):
        """entries: [(name, ino)] -> каталог с дирентами по 64 Б."""
        slots_per_block = BLOCK // 64
        blocks = []
        cur = bytearray(BLOCK)
        used = 0
        for ename, eino in entries:
            nb = ename.encode()
            if not (1 <= len(nb) <= 58):
                raise SystemExit(f"плохое имя: {ename!r}")
            if used == slots_per_block:
                blocks.append(bytes(cur))
                cur = bytearray(BLOCK)
                used = 0
            o = used * 64
            cur[o:o + 4] = struct.pack("<I", eino)
            cur[o + 4] = len(nb)
            cur[o + 5:o + 5 + len(nb)] = nb
            used += 1
        blocks.append(bytes(cur))
        direct = [self.alloc_data(b) for b in blocks]
        size = (len(blocks) - 1) * slots_per_block * 64 + used * 64
        return self.alloc_inode(TYPE_DIR, size, direct=direct, ino=ino,
                                perms=perms, uid=uid)


def build_image(img_path, programs, size=IMG_SIZE, kernel=None, installer=False,
                users=None):
    """users: [(name, password)] — учётные записи, зашитые прямо в образ.

    Нужны, чтобы собрать «предустановленный» образ: без них система при
    загрузке не спрашивает пароль (см. login.rs) и работает от root, и
    проверять вход и права не на чем."""
    global now
    import time
    now = int(time.time()) & 0xFFFFFFFF
    total_blocks = size // BLOCK
    if total_blocks * BLOCK != size:
        raise SystemExit("размер образа должен быть кратен 4096")
    max_blocks = DATA_START + BLOCK_BITMAP_BLOCKS * BLOCK * 8
    if total_blocks > max_blocks:
        raise SystemExit(f"образ слишком велик для фиксированных битмапов "
                         f"(максимум {max_blocks * BLOCK} байт)")
    fsb = FsBuilder(total_blocks)

    # Нумерация фиксирована: корень обязан быть инодом 2.
    n = len(programs)
    ino_bin, ino_elf_first = 3, 4
    ino_etc, ino_passwd, ino_shadow = 4 + n, 5 + n, 6 + n
    ino_hostname, ino_rcconf = 7 + n, 8 + n
    ino_home, ino_tmp, ino_var, ino_log = 9 + n, 10 + n, 11 + n, 12 + n
    nxt = 13 + n
    ino_boot = ino_kernel = ino_marker = None
    if kernel:
        ino_boot, ino_kernel = nxt, nxt + 1
        nxt += 2
    if installer:
        ino_marker = nxt
        nxt += 1

    bin_entries = []
    for k, (name, path) in enumerate(programs):
        with open(path, "rb") as f:
            data = f.read()
        if not data.startswith(b"\x7fELF"):
            raise SystemExit(f"{path}: не ELF-файл")
        bin_entries.append((name, fsb.write_file(name, data,
                                                 ino=ino_elf_first + k)))

    # Учётные записи: /etc/passwd и /etc/shadow плюс домашний каталог с
    # владельцем. Формат ровно тот, что читает user.rs: sha256(salt+пароль).
    passwd_lines, shadow_lines, home_entries = [], [], []
    fsb.next_inode = nxt
    for i, (uname, upass) in enumerate(users or []):
        uid = FIRST_UID + i
        salt = os.urandom(16).hex()
        digest = hashlib.sha256((salt + upass).encode()).hexdigest()
        passwd_lines.append(f"{uname}:{uid}:/home/{uname}\n")
        shadow_lines.append(f"{uname}:{salt}:{digest}\n")
        home_entries.append((uname, fsb.alloc_inode(TYPE_DIR, 0, uid=uid)))

    fsb.write_file("passwd", "".join(passwd_lines).encode(), ino=ino_passwd)
    fsb.write_file("shadow", "".join(shadow_lines).encode(), ino=ino_shadow)
    fsb.write_file("hostname", b"VitaminOS26\n", ino=ino_hostname)
    fsb.write_file("rc.conf", RC_CONF.encode(), ino=ino_rcconf)
    fsb.write_dir([("log", ino_log)], ino=ino_var)
    fsb.write_dir([], ino=ino_log)

    etc_entries = [("passwd", ino_passwd), ("shadow", ino_shadow),
                   ("hostname", ino_hostname), ("rc.conf", ino_rcconf)]
    if installer:
        fsb.write_file("installer", b"1\n", ino=ino_marker)
        etc_entries.append(("installer", ino_marker))
    root_entries = [("bin", ino_bin), ("etc", ino_etc), ("home", ino_home),
                    ("tmp", ino_tmp), ("var", ino_var)]
    if kernel:
        with open(kernel, "rb") as f:
            kdata = f.read()
        if not kdata.startswith(b"\x7fELF"):
            raise SystemExit(f"{kernel}: не ELF-файл")
        fsb.write_file(KERNEL_NAME, kdata, ino=ino_kernel)
        fsb.write_dir([(KERNEL_NAME, ino_kernel)], ino=ino_boot)
        root_entries.append(("boot", ino_boot))

    fsb.write_dir(bin_entries, ino=ino_bin)
    fsb.write_dir(etc_entries, ino=ino_etc)
    fsb.write_dir(home_entries, ino=ino_home)
    # /tmp доступен всем на запись (как /tmp в настоящих системах): иначе
    # пользователю негде складывать временные файлы.
    fsb.write_dir([], ino=ino_tmp, perms=0o777)
    fsb.write_dir(root_entries, ino=ROOT_INODE)

    # Суперблок - зеркально Superblock::for_image + encode.
    sb = bytearray(BLOCK)
    sb[0:8] = MAGIC
    fields = {
        8: 1,                    # VERSION
        12: BLOCK,               # BLOCK_SIZE
        16: total_blocks,        # TOTAL_BLOCKS
        20: TOTAL_INODES,        # TOTAL_INODES
        24: TOTAL_INODES - sum(bin(b).count("1") for b in fsb.inode_bm),
        # FREE_INODES - по факту заселённости битмапа
        28: JOURNAL_START,
        32: JOURNAL_BLOCKS,
        36: INODE_TABLE_START,
        40: DATA_START,
        44: 0,                   # FLAGS
        48: ROOT_INODE,
    }
    for off, val in fields.items():
        sb[off:off + 4] = struct.pack("<I", val)
    fsb.put_block(0, bytes(sb))

    with open(img_path, "wb") as f:
        f.write(fsb.img)
    used_blocks = DATA_START + fsb.next_data
    print(f"Disk image: {img_path} ({total_blocks} blocks, used up to block "
          f"{used_blocks}; {fsb.next_inode - 1} inodes; "
          f"{len(programs)} program(s))")


def main():
    import argparse
    p = argparse.ArgumentParser(description="Собрать образ VITAFS")
    p.add_argument("img")
    p.add_argument("programs_list")
    p.add_argument("--size", type=lambda s: int(s, 0), default=IMG_SIZE,
                   help="размер образа в байтах (по умолчанию 8 МиБ)")
    p.add_argument("--kernel", default=None,
                   help="ELF ядра -> /boot/" + KERNEL_NAME)
    p.add_argument("--installer", action="store_true",
                   help="добавить маркер /etc/installer")
    p.add_argument("--user", action="append", default=[], metavar="NAME:PASS",
                   help="зашить учётную запись (uid %d+); можно указать "
                        "несколько раз" % FIRST_UID)
    args = p.parse_args()

    users = []
    for spec in args.user:
        name, _, password = spec.partition(":")
        if not name or not password:
            raise SystemExit("--user ожидает NAME:PASSWORD")
        users.append((name, password))

    programs = []
    with open(args.programs_list) as f:
        for line in f:
            line = line.rstrip("\n")
            if not line:
                continue
            name, path = line.split("\t")
            programs.append((name, path))

    build_image(args.img, programs, size=args.size, kernel=args.kernel,
                installer=args.installer, users=users)


if __name__ == "__main__":
    main()
