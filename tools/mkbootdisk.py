#!/usr/bin/env python3
"""tools/mkbootdisk.py - собирает тестовый диск для собственного загрузчика.

Раскладка повторяет установленную систему (src/bootldr/layout.inc):
  LBA 0            stage1 (512 Б) + таблица разделов + 0x55AA
  LBA 1..16        stage2
  LBA 256..        сырой ELF-образ ядра
  LBA 32768..      раздел 1: VITAFS

Раздел нужен только чтобы разметка была настоящей: stage2 ядро грузит
не по таблице разделов, а по фиксированному LBA. Но без раздела нельзя
проверить, что загрузка не ломается о реальный MBR.
"""
import argparse
import os
import struct
import subprocess
import sys

SECTOR = 512
STAGE1_LBA = 0
STAGE2_LBA = 1
STAGE2_SECTORS = 16
KERNEL_LBA = 256
PART1_LBA = 32768
PART1_SECTORS = 4096  # 2 МиБ - для проверки хватает
PART_TYPE = 0x83      # Linux
BOOTABLE = 0x80


def find_out_dir():
    """Каталог OUT_DIR из свежей сборки: там лежат stage1.bin/stage2.bin."""
    candidates = []
    for root, dirs, files in os.walk("target"):
        if "stage1.bin" in files and "stage2.bin" in files:
            candidates.append(root)
    if not candidates:
        sys.exit("stage1.bin не найден - сначала cargo build")
    return max(candidates, key=os.path.getmtime)


def chs(lba):
    """LBA -> три байта CHS в раскладке msdos (её же читает BIOS).

        byte 0 = старшие 2 бита цилиндра | головка
        byte 1 = сектор | (младшие 2 бита цилиндра << 6)
        byte 2 = младший байт цилиндра
    """
    heads, spt = 255, 63
    cyl = lba // (heads * spt)
    head = (lba // spt) % heads
    sec = lba % spt + 1
    if cyl > 1023:
        cyl, head, sec = 1023, 254, 63
    b0 = ((cyl >> 2) & 0xC0) | (head & 0xFF)
    b1 = (sec & 0x3F) | ((cyl & 0x03) << 6)
    b2 = cyl & 0xFF
    return bytes([b0, b1, b2])


def partition_entry(start, count, ptype=PART_TYPE, bootable=False):
    """Запись таблицы разделов MBR: ровно 16 байт.

    Порядок полей фиксирован, спутать нельзя - запись молча прочитается как
    мусор и BIOS не увидит раздела:

        0x00  1  флаг загрузки (0x80)
        0x01  3  CHS начала
        0x04  1  тип раздела
        0x05  3  CHS конца
        0x08  4  LBA начала
        0x0C  4  число секторов
    """
    if count == 0:
        return bytes(16)
    e = bytes([BOOTABLE if bootable else 0])
    e += chs(start)
    e += bytes([ptype & 0xFF])
    e += chs(start + count - 1)
    e += struct.pack("<I", start)
    e += struct.pack("<I", count)
    assert len(e) == 16, len(e)
    return e


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("output", help="файл образа")
    ap.add_argument("--kernel", default="target/x86_64-unknown-none/debug/vitamin_os26")
    ap.add_argument("--fs", help="VITAFS-образ, положить в раздел 1")
    ap.add_argument("--no-fs", action="store_true", help="раздел оставить пустым")
    args = ap.parse_args()

    out = find_out_dir()
    stage1 = open(os.path.join(out, "stage1.bin"), "rb").read()
    stage2 = open(os.path.join(out, "stage2.bin"), "rb").read()
    kernel = open(args.kernel, "rb").read()

    assert len(stage1) == 512, "stage1 должен быть 512 байт"
    assert len(stage2) <= STAGE2_SECTORS * SECTOR, "stage2 не влезает в 16 секторов"
    print("stage1 %d Б, stage2 %d Б, ядро %d Б (%d секторов)"
          % (len(stage1), len(stage2), len(kernel), len(kernel) // SECTOR + 1))

    kern_sectors = (len(kernel) + SECTOR - 1) // SECTOR
    end_kernel = KERNEL_LBA + kern_sectors
    if end_kernel > PART1_LBA:
        sys.exit("ядро не влезает в промежуток до раздела: конец LBA %d, раздел с LBA %d"
                 % (end_kernel, PART1_LBA))

    total = PART1_LBA + PART1_SECTORS
    img = bytearray(total * SECTOR)

    # LBA 0: stage1 + таблица разделов + сигнатура. Всё это делит один сектор:
    # stage1 занимает 0x000..0x1EF, таблица 0x1BE..0x1FD (64 байта, четыре
    # записи по 16), сигнатура 0x1FE. Порядок именно такой: сначала stage1,
    # потом разметка поверх, потому что stage1 эти байты затирает нулями.
    mbr = bytearray(stage1)
    if len(mbr) != SECTOR:
        sys.exit("stage1 должен быть ровно %d байт, а не %d" % (SECTOR, len(mbr)))
    table = partition_entry(PART1_LBA, PART1_SECTORS, bootable=True)
    table += bytes(16 * 3)          # три пустые записи
    assert len(table) == 64, len(table)
    mbr[0x1BE:0x1FE] = table
    mbr[0x1FE:0x200] = b"\x55\xAA"
    assert len(mbr) == SECTOR, len(mbr)
    img[0:SECTOR] = mbr

    img[STAGE2_LBA * SECTOR:STAGE2_LBA * SECTOR + len(stage2)] = stage2
    img[KERNEL_LBA * SECTOR:KERNEL_LBA * SECTOR + len(kernel)] = kernel

    if args.fs and not args.no_fs:
        fs = open(args.fs, "rb").read()
        if len(fs) > PART1_SECTORS * SECTOR:
            fs = fs[:PART1_SECTORS * SECTOR]
        img[PART1_LBA * SECTOR:PART1_LBA * SECTOR + len(fs)] = fs
        print("раздел 1: VITAFS %d Б" % len(fs))

    with open(args.output, "wb") as f:
        f.write(img)
    print("готово: %s (%d МиБ)" % (args.output, total * SECTOR // 1024 // 1024))


if __name__ == "__main__":
    main()