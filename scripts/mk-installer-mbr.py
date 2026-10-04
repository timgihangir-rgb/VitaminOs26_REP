#!/usr/bin/env python3
"""Собирает MBR установочного диска: stage1 + таблица разделов + 0x55AA.

Запись раздела побайтово повторяет src/part.rs (`Entry::encode` и
`chs_bytes`): один и тот же формат пишет установщик, когда ставит систему на
целевой диск. sfdisk здесь не годится - он переписывает MBR целиком и
затирает код загрузчика, а код stage1 обязан лежать в первых 446 байтах.
"""
import sys

SECTOR = 512
BOOT_CODE_END = 446          # дальше таблица разделов
TABLE_OFF = 0x1BE
TABLE_END = 0x1FE
TYPE_VITAFS = 0x83
ENTRY_LEN = 16

stage1_path, out_path, start_lba, sectors = (
    sys.argv[1], sys.argv[2], int(sys.argv[3]), int(sys.argv[4]))

boot = bytearray(open(stage1_path, "rb").read())
if len(boot) != SECTOR:
    sys.exit("stage1 должен быть %d байт, получилось %d" % (SECTOR, len(boot)))

mbr = bytearray(SECTOR)
mbr[:BOOT_CODE_END] = boot[:BOOT_CODE_END]   # хвост stage1 не нужен


def chs(lba: int) -> bytes:
    """LBA -> упакованные CHS, ровно как chs_bytes() в src/part.rs."""
    HEADS, SPT = 255, 63
    cyl = lba // (HEADS * SPT)
    if cyl > 1023:
        cyl, head, sec = 1023, 254, 63
    else:
        tmp = lba % (HEADS * SPT)
        head, sec = tmp // SPT, tmp % SPT + 1
    return bytes([head & 0xFF, (sec & 0x3F) | ((cyl >> 2) & 0xC0), cyl & 0xFF])


entry = bytearray(ENTRY_LEN)
entry[0] = 0x80                                  # bootable
entry[1:4] = chs(start_lba)
entry[4] = TYPE_VITAFS
entry[5:8] = chs(start_lba + sectors - 1)
entry[8:12] = start_lba.to_bytes(4, "little")
entry[12:16] = sectors.to_bytes(4, "little")

table = bytearray(TABLE_END - TABLE_OFF)
table[:ENTRY_LEN] = entry
mbr[TABLE_OFF:TABLE_END] = table
mbr[TABLE_END:TABLE_END + 2] = b"\x55\xaa"

assert len(mbr) == SECTOR, "MBR вышел %d байт" % len(mbr)
open(out_path, "wb").write(mbr)