//! src/part.rs - таблица разделов MBR.
//!
//! Читает разделы установленного диска (чтобы ядро нашло VITAFS в разделе,
//! а не с нулевого блока) и умеет собрать 512-байтный MBR: байты stage1,
//! таблица по смещению 0x1BE и подпись 0x55AA.
//!
//! Здесь работают абсолютные секторные адреса
//! (`blockdev::read_sectors_pub`/`write_sectors_pub`); смещение ФС из
//! `blockdev` не участвует, иначе таблицу разделов было бы не найти.

use crate::blockdev::{self, SECTORS_PER_BLOCK, SECTOR_SIZE};

pub const TABLE_OFF: usize = 0x1BE;
pub const ENTRY_LEN: usize = 16;
pub const ENTRY_COUNT: usize = 4;
pub const SIGNATURE_OFF: usize = 0x1FE;
/// Тип раздела VITAFS (совпадает с причудой mkbootdisk.py/layout.inc).
pub const TYPE_VITAFS: u8 = 0x83;

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub bootable: bool,
    pub ptype: u8,
    pub start_lba: u32,
    pub sectors: u32,
}

impl Entry {
    pub const fn empty() -> Self {
        Entry {
            bootable: false,
            ptype: 0,
            start_lba: 0,
            sectors: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ptype == 0
    }

    pub fn encode(&self) -> [u8; ENTRY_LEN] {
        let mut e = [0u8; ENTRY_LEN];
        e[0] = if self.bootable { 0x80 } else { 0x00 };
        e[1..4].copy_from_slice(&chs_bytes(self.start_lba));
        e[4] = self.ptype;
        let last = self.start_lba.saturating_add(self.sectors).saturating_sub(1);
        e[5..8].copy_from_slice(&chs_bytes(last));
        e[8..12].copy_from_slice(&self.start_lba.to_le_bytes());
        e[12..16].copy_from_slice(&self.sectors.to_le_bytes());
        e
    }

    fn decode(e: &[u8]) -> Entry {
        Entry {
            bootable: e[0] == 0x80,
            ptype: e[4],
            start_lba: u32::from_le_bytes([e[8], e[9], e[10], e[11]]),
            sectors: u32::from_le_bytes([e[12], e[13], e[14], e[15]]),
        }
    }
}

/// LBA -> упакованные CHS (головка, сектор|старшие биты цилиндра, младший
/// цилиндр). Старые BIOS читают CHS, поэтому поле заполняем честно, хотя
/// чтение всё равно идёт по LBA.
fn chs_bytes(lba: u32) -> [u8; 3] {
    const HEADS: u32 = 255;
    const SPT: u32 = 63;
    let mut cyl = lba / (HEADS * SPT);
    let (head, sec) = if cyl > 1023 {
        cyl = 1023;
        (254u32, 63u32)
    } else {
        let tmp = lba % (HEADS * SPT);
        (tmp / SPT, tmp % SPT + 1)
    };
    [
        head as u8,
        ((sec & 0x3F) | ((cyl >> 2) & 0xC0)) as u8,
        (cyl & 0xFF) as u8,
    ]
}

/// Читает сектор 0 (MBR/stage1).
pub fn read_sector0() -> Option<[u8; SECTOR_SIZE]> {
    let mut buf = [0u8; SECTOR_SIZE];
    if blockdev::read_sectors_pub(0, 1, &mut buf).is_err() {
        return None;
    }
    Some(buf)
}

/// Есть ли подпись загрузочного сектора.
pub fn has_signature(sec: &[u8]) -> bool {
    sec.len() >= SECTOR_SIZE
        && sec[SIGNATURE_OFF] == 0x55
        && sec[SIGNATURE_OFF + 1] == 0xAA
}

/// Порядок ли разметка на диске (подпись MBR).
pub fn mbr_present() -> bool {
    match read_sector0() {
        Some(sec) => has_signature(&sec),
        None => false,
    }
}

/// Разбор четырёх записей таблицы разделов.
pub fn parse(sec: &[u8]) -> [Entry; ENTRY_COUNT] {
    let mut out = [Entry::empty(); ENTRY_COUNT];
    if sec.len() < SECTOR_SIZE {
        return out;
    }
    for (i, slot) in out.iter_mut().enumerate() {
        let off = TABLE_OFF + i * ENTRY_LEN;
        *slot = Entry::decode(&sec[off..off + ENTRY_LEN]);
    }
    out
}

/// Базовый блок VITAFS (в 4-КиБ блоках) из первого непустого раздела,
/// выровненного на границу блока. None - MBR нет или разделы не годятся.
pub fn vitafs_base_blocks() -> Option<u64> {
    let sec = read_sector0()?;
    if !has_signature(&sec) {
        return None;
    }
    for e in parse(&sec) {
        if e.is_empty() {
            continue;
        }
        let start = e.start_lba as u64;
        if start % SECTORS_PER_BLOCK as u64 == 0 {
            return Some(start / SECTORS_PER_BLOCK as u64);
        }
    }
    None
}

/// Собирает полный 512-байтный MBR: байты stage1, затем таблица поверх
/// хвоста (stage1 сам его обнуляет) и подпись.
pub fn build_mbr(stage1: &[u8], entries: &[Entry; ENTRY_COUNT]) -> [u8; SECTOR_SIZE] {
    let mut mbr = [0u8; SECTOR_SIZE];
    let n = core::cmp::min(stage1.len(), SECTOR_SIZE);
    mbr[..n].copy_from_slice(&stage1[..n]);
    for (i, e) in entries.iter().enumerate() {
        let off = TABLE_OFF + i * ENTRY_LEN;
        mbr[off..off + ENTRY_LEN].copy_from_slice(&e.encode());
    }
    mbr[SIGNATURE_OFF] = 0x55;
    mbr[SIGNATURE_OFF + 1] = 0xAA;
    mbr
}

/// Накладывает таблицу на уже записанный сектор 0.
pub fn write_table(entries: &[Entry; ENTRY_COUNT]) -> Result<(), ()> {
    let mut sec = read_sector0().ok_or(())?;
    for (i, e) in entries.iter().enumerate() {
        let off = TABLE_OFF + i * ENTRY_LEN;
        sec[off..off + ENTRY_LEN].copy_from_slice(&e.encode());
    }
    sec[SIGNATURE_OFF] = 0x55;
    sec[SIGNATURE_OFF + 1] = 0xAA;
    blockdev::write_sectors_pub(0, 1, &sec)
}
