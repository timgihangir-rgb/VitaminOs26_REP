#!/usr/bin/env python3
"""Читает образ VITAFS VitaminOS26 и печатает дерево каталогов + статистику.

Константы зеркалят src/vitafs.rs / tools/mkfs.py (блок = 4096 Б).
Использование: dumpfs.py <img>
"""
import struct
import sys

BLOCK = 4096
MAGIC = b"VITAFS1\x00"
INODE_BITMAP_START = 67
INODE_TABLE_START = 69
DATA_START = 101
INODE_SIZE = 128
INODES_PER_BLOCK = BLOCK // INODE_SIZE
DIRENT_SIZE = 64
TYPE_FILE = 1
TYPE_DIR = 2
TYPE_CHARDEV = 3
TYPE_SYMLINK = 4

def load(img_path):
    with open(img_path, "rb") as f:
        return bytearray(f.read())

def read_inode(img, ino):
    off = INODE_TABLE_START * BLOCK + ino * INODE_SIZE
    raw = bytes(img[off:off + INODE_SIZE])
    itype, perms, size, links, atime, mtime, dev = struct.unpack("<HHIIIII", raw[:24])
    direct = list(struct.unpack("<12I", raw[24:72]))
    indirect1, indirect2, indirect3 = struct.unpack("<III", raw[72:84])
    return {
        "itype": itype, "perms": perms, "size": size, "links": links,
        "direct": direct, "indirect1": indirect1,
    }

def read_dir(img, ino):
    """Возвращает [(name, child_ino)], размер=size инода."""
    node = read_inode(img, ino)
    nblocks = (node["size"] + BLOCK - 1) // BLOCK
    out = []
    for bi in range(nblocks):
        bno = node["direct"][bi] if bi < 12 else None
        if bno is None or bno == 0:
            continue
        blk = bytes(img[bno * BLOCK:(bno + 1) * BLOCK])
        for s in range(BLOCK // DIRENT_SIZE):
            o = s * DIRENT_SIZE
            cinо, ln = struct.unpack_from("<IB", blk, o)
            if ln == 0 or ln > 58:
                continue
            name = blk[o + 5:o + 5 + ln].decode("utf-8", "replace")
            out.append((name, cinо))
    return out

def walk(img, ino, prefix, depth=0, out=None):
    if out is None:
        out = []
    node = read_inode(img, ino)
    tname = {TYPE_FILE: "file", TYPE_DIR: "dir", TYPE_CHARDEV: "chr",
             TYPE_SYMLINK: "sym"}.get(node["itype"], f"t{node['itype']}")
    out.append(f"{prefix} [{tname} ino={ino} size={node['size']}]")
    if node["itype"] == TYPE_DIR:
        for name, c in read_dir(img, ino):
            walk(img, c, prefix + "/" + name, depth + 1, out)
    return out

def main():
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    img = load(sys.argv[1])
    sb = img[0:BLOCK]
    print("magic:", sb[0:8])
    if sb[0:8] != MAGIC:
        sys.exit("error: not a VITAFS image")
    total_blocks = struct.unpack_from("<I", sb, 16)[0]
    free_inodes = struct.unpack_from("<I", sb, 24)[0]
    # занятые биты битмапа блоков и инодов напрямую
    used_b = sum(bin(b).count("1") for b in img[65 * BLOCK:67 * BLOCK])
    used_i = sum(bin(b).count("1") for b in img[67 * BLOCK:69 * BLOCK])
    # иноды, реально присутствующие в таблице (itype != 0)
    table_used = 0
    for ino in range(32 * 32):
        off = INODE_TABLE_START * BLOCK + ino * INODE_SIZE
        itype = struct.unpack_from("<H", img, off)[0]
        if itype != 0:
            table_used += 1
    print(f"total_blocks={total_blocks} free_inodes={free_inodes}")
    print(f"bitmap: blocks used={used_b} inodes used={used_i}")
    print(f"inode table: {table_used} non-zero entries"
          f" {'OK' if table_used + 2 == used_i else 'MISMATCH!'}")
    print()
    for line in walk(img, 2, ""):
        print(line)
    print()
    print(f"data blocks used (per bitmap)={used_b}")

if __name__ == "__main__":
    main()