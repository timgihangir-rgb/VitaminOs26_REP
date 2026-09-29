// src/vitafs.rs
//
// VITAFS - настоящая файловая система VitaminOS26 (Фаза 3).
// Формат диска описан в docs/fs.md. Этот модуль - фундамент слоя:
// константы раскладки и суперблок.
//
// Суперблок занимает блок 0; валидность определяется magic "VITAFS1\0".

use crate::blockdev::BLOCK_SIZE;

pub const MAGIC: &[u8; 8] = b"VITAFS1\0";
pub const VERSION: u32 = 1;

// Раскладка диска (номера блоков, см. docs/fs.md).
pub const JOURNAL_START: u32 = 1;
pub const JOURNAL_BLOCKS: u32 = 64;
pub const BLOCK_BITMAP_START: u32 = 65;
pub const BLOCK_BITMAP_BLOCKS: u32 = 2;
pub const INODE_BITMAP_START: u32 = 67;
pub const INODE_BITMAP_BLOCKS: u32 = 2;
pub const INODE_TABLE_START: u32 = 69;
pub const INODE_TABLE_BLOCKS: u32 = 32;
pub const DATA_START: u32 = INODE_TABLE_START + INODE_TABLE_BLOCKS; // 101

// Лимит геометрии: битмапы данных фиксированы (BLOCK_BITMAP_BLOCKS), поэтому
// суперблок не может описывать диск больше этой ёмкости. format_image()
// клампит реальную ёмкость устройства этим пределом.
pub const MAX_DATA_BITMAP_BITS: u32 = BLOCK_BITMAP_BLOCKS * BLOCK_SIZE as u32 * 8;
pub const MAX_TOTAL_BLOCKS: u32 = DATA_START + MAX_DATA_BITMAP_BITS;

pub const INODE_SIZE: usize = 128;
pub const INODES_PER_BLOCK: u32 = (BLOCK_SIZE / INODE_SIZE) as u32; // 32
pub const TOTAL_INODES: u32 = INODES_PER_BLOCK * INODE_TABLE_BLOCKS; // 1024

pub const ROOT_INODE: u32 = 2;

// Используется журналом (3.2).
#[allow(dead_code)]
pub const FLAG_DIRTY: u32 = 1 << 0;

/// Смещения полей суперблока внутри блока 0.
mod off {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 8;
    pub const BLOCK_SIZE: usize = 12;
    pub const TOTAL_BLOCKS: usize = 16;
    pub const TOTAL_INODES: usize = 20;
    pub const FREE_INODES: usize = 24;
    pub const JOURNAL_START: usize = 28;
    pub const JOURNAL_BLOCKS: usize = 32;
    pub const INODE_TABLE_START: usize = 36;
    pub const DATA_START: usize = 40;
    pub const FLAGS: usize = 44;
    pub const ROOT_INODE: usize = 48;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Superblock {
    pub version: u32,
    pub block_size: u32,
    pub total_blocks: u32,
    pub total_inodes: u32,
    pub free_inodes: u32,
    pub journal_start: u32,
    pub journal_blocks: u32,
    pub inode_table_start: u32,
    pub data_start: u32,
    pub flags: u32,
    pub root_inode: u32,
}

impl Superblock {
    /// Канонический суперблок для образа заданного размера.
    pub fn for_image(bytes: u64) -> Superblock {
        let total_blocks = (bytes / BLOCK_SIZE as u64) as u32;
        Superblock {
            version: VERSION,
            block_size: BLOCK_SIZE as u32,
            total_blocks,
            total_inodes: TOTAL_INODES,
            free_inodes: TOTAL_INODES - (ROOT_INODE + 1), // заняты 0..=2
            journal_start: JOURNAL_START,
            journal_blocks: JOURNAL_BLOCKS,
            inode_table_start: INODE_TABLE_START,
            data_start: DATA_START,
            flags: 0,
            root_inode: ROOT_INODE,
        }
    }

    /// Разбор с валидацией. None = это не VITAFS или поля противоречивы.
    pub fn decode(buf: &[u8; BLOCK_SIZE]) -> Option<Superblock> {
        if &buf[off::MAGIC..off::MAGIC + 8] != MAGIC {
            return None;
        }
        let g32 = |o: usize| -> u32 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&buf[o..o + 4]);
            u32::from_le_bytes(b)
        };
        let sb = Superblock {
            version: g32(off::VERSION),
            block_size: g32(off::BLOCK_SIZE),
            total_blocks: g32(off::TOTAL_BLOCKS),
            total_inodes: g32(off::TOTAL_INODES),
            free_inodes: g32(off::FREE_INODES),
            journal_start: g32(off::JOURNAL_START),
            journal_blocks: g32(off::JOURNAL_BLOCKS),
            inode_table_start: g32(off::INODE_TABLE_START),
            data_start: g32(off::DATA_START),
            flags: g32(off::FLAGS),
            root_inode: g32(off::ROOT_INODE),
        };
        if !sb.validate() {
            return None;
        }
        Some(sb)
    }

    /// Сериализация в блок (остаток блока - нули).
    pub fn encode(&self, out: &mut [u8; BLOCK_SIZE]) {
        *out = [0u8; BLOCK_SIZE];
        out[off::MAGIC..off::MAGIC + 8].copy_from_slice(MAGIC);
        let mut put = |o: usize, v: u32| out[o..o + 4].copy_from_slice(&v.to_le_bytes());
        put(off::VERSION, self.version);
        put(off::BLOCK_SIZE, self.block_size);
        put(off::TOTAL_BLOCKS, self.total_blocks);
        put(off::TOTAL_INODES, self.total_inodes);
        put(off::FREE_INODES, self.free_inodes);
        put(off::JOURNAL_START, self.journal_start);
        put(off::JOURNAL_BLOCKS, self.journal_blocks);
        put(off::INODE_TABLE_START, self.inode_table_start);
        put(off::DATA_START, self.data_start);
        put(off::FLAGS, self.flags);
        put(off::ROOT_INODE, self.root_inode);
    }

    /// Согласованность геометрии: области не перекрываются и влезают в диск.
    pub fn validate(&self) -> bool {
        if self.version != VERSION || self.block_size != BLOCK_SIZE as u32 {
            return false;
        }
        if self.root_inode >= self.total_inodes || self.total_inodes % INODES_PER_BLOCK != 0 {
            return false;
        }
        let end_of_meta = self.inode_table_start + self.total_inodes / INODES_PER_BLOCK;
        if self.journal_start + self.journal_blocks > self.inode_table_start {
            return false;
        }
        if end_of_meta > self.data_start || self.data_start > self.total_blocks {
            return false;
        }
        if self.journal_start != JOURNAL_START {
            return false;
        }
        // Битмапы обязаны вмещать свои регионы в отведённые блоки.
        if bitmap_blocks_needed(self.total_blocks - self.data_start) > BLOCK_BITMAP_BLOCKS {
            return false;
        }
        if bitmap_blocks_needed(self.total_inodes) > INODE_BITMAP_BLOCKS {
            return false;
        }
        true
    }
}

/// Самопроверка: encode->decode roundtrip, отказ на чужом magic,
/// валидация геометрии для стандартного 8-МиБ образа.
pub fn selftest() -> bool {
    let mut buf = [0u8; BLOCK_SIZE];
    let sb = Superblock::for_image(8 * 1024 * 1024);
    if sb.total_blocks != 2048 {
        return false;
    }
    sb.encode(&mut buf);
    match Superblock::decode(&buf) {
        Some(back) if back == sb => {}
        _ => return false,
    }
    // Чужой magic должен отбрасываться.
    buf[0] = b'X';
    if Superblock::decode(&buf).is_some() {
        return false;
    }
    // Геометрически неверный вариант должен отбрасываться валидатором.
    let mut bad = sb;
    bad.data_start = bad.total_blocks + 1;
    if bad.validate() {
        return false;
    }
    bitmap_selftest_pure()
}

// ---------------------------------------------------------------------------
// Битмапы свободных блоков и inode (3.1.2).
//
// Чистая битовая логика отделена от дисковых обёрток: её можно тестировать
// без диска. Дисковые операции идут через bcache (write-back, выгоняет
// flush_all). Номера на диске абсолютные; в битмапе данных индекс = блок -
// data_start, в битмапе inode индекс = номер inode.
//
// Резерв inode: 0 - невалидный номер, 1 - служебный (журнал), 2 - корень.
// Аллокация начинается с 3. Форматирование выставляет биты 0..=2 заранее.

/// Число битовых подблоков-блоков для региона заданной ёмкости.
pub const fn bitmap_blocks_needed(bits: u32) -> u32 {
    (bits + BLOCK_SIZE as u32 * 8 - 1) / (BLOCK_SIZE as u32 * 8)
}

fn bit_get(data: &[u8], idx: usize) -> bool {
    data[idx >> 3] & (1 << (idx & 7)) != 0
}

fn bit_set(data: &mut [u8], idx: usize) {
    data[idx >> 3] |= 1 << (idx & 7);
}

fn bit_clear(data: &mut [u8], idx: usize) {
    data[idx >> 3] &= !(1 << (idx & 7));
}

/// Первый нулевой бит; None если всё занято.
fn first_zero(data: &[u8], limit_bits: usize) -> Option<usize> {
    let full_bytes = limit_bits / 8;
    for (i, b) in data.iter().take(full_bytes).enumerate() {
        if *b != 0xFF {
            for bit in 0..8 {
                if b & (1 << bit) == 0 {
                    return Some(i * 8 + bit);
                }
            }
        }
    }
    let rem = limit_bits % 8;
    if rem > 0 {
        let b = data[full_bytes];
        for bit in 0..rem {
            if b & (1 << bit) == 0 {
                return Some(full_bytes * 8 + bit);
            }
        }
    }
    None
}

fn count_zeros(data: &[u8], limit_bits: usize) -> u32 {
    let mut n = 0u32;
    for idx in 0..limit_bits {
        if !bit_get(data, idx) {
            n += 1;
        }
    }
    n
}

/// Ёмкость битмапа данных в битах (по одному биту на блок данных).
pub fn data_region_bits(sb: &Superblock) -> usize {
    sb.total_blocks.saturating_sub(sb.data_start) as usize
}

fn load_bitmap(region_start: u32, sub: u32, buf: &mut [u8; BLOCK_SIZE]) -> bool {
    fs_read_block((region_start + sub) as u64, buf)
}

fn save_bitmap(region_start: u32, sub: u32, buf: &[u8; BLOCK_SIZE]) -> bool {
    fs_write_block((region_start + sub) as u64, buf)
}

// --- WAL-интеграция (3.2) ---------------------------------------------------
//
// Все мутации метаданных и данных идут через fs_write_block: при активной
// транзакции блок попадает в staging журнала (и чтения видят его через
// fs_read_block), вне транзакции - прямо в bcache как раньше.

/// Читает блок ФС сквозь транзакцию: staging -> bcache.
fn fs_read_block(bno: u64, out: &mut [u8; BLOCK_SIZE]) -> bool {
    if crate::wal::read_through(bno, out) {
        return true;
    }
    crate::bcache::read(bno, out).is_ok()
}

/// Пишет блок ФС через транзакцию (staging), вне транзакции - в bcache.
fn fs_write_block(bno: u64, buf: &[u8; BLOCK_SIZE]) -> bool {
    if crate::wal::txn_active() && crate::wal::stage(bno, buf) {
        return true;
    }
    crate::bcache::write(bno, buf).is_ok()
}

/// Захват грязных inode из icache в WAL-staging (вызывается из wal::commit_all
/// при уже закрытой глубине транзакции, поэтому stage вызывается напрямую).
pub fn capture_dirty_inodes_into_wal() -> bool {
    // Статические буферы: экономим ~8 КиБ стека в глубокой цепочке
    // commit (однопроцессорное ядро, реентерабельности нет).
    static mut RAW: [u8; INODE_SIZE] = [0; INODE_SIZE];
    static mut BUF: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let sb = current_sb();
        for i in 0..INODE_CACHE_SLOTS {
            let (ino, raw): (u32, &mut [u8; INODE_SIZE]) = {
                let e = &ITABLE[i];
                if e.state != IState::Dirty {
                    continue;
                }
                let raw: &mut [u8; INODE_SIZE] = &mut RAW;
                e.node.encode(raw);
                (e.ino, raw)
            };
            let blk = (sb.inode_table_start + ino / INODES_PER_BLOCK) as u64;
            let off = (ino % INODES_PER_BLOCK) as usize * INODE_SIZE;
            let buf: &mut [u8; BLOCK_SIZE] = &mut BUF;
            if !fs_read_block(blk, buf) {
                return false;
            }
            buf[off..off + INODE_SIZE].copy_from_slice(raw);
            if !crate::wal::stage(blk, buf) {
                return false;
            }
            ITABLE[i].state = IState::Clean;
        }
        true
    })
}

/// Выделяет свободный блок данных. Возвращает абсолютный номер блока.
pub fn alloc_data_block(sb: &Superblock) -> Option<u64> {
    let cap = data_region_bits(sb);
    let mut buf = [0u8; BLOCK_SIZE];
    // Геометрия даёт один блок битмапа на ~32К бит; цикл по подблокам
    // оставлен для будущих больших дисков.
    let subs = bitmap_blocks_needed(cap as u32);
    for sub in 0..subs {
        if !load_bitmap(BLOCK_BITMAP_START, sub, &mut buf) {
            return None;
        }
        let base = sub as usize * BLOCK_SIZE * 8;
        let limit = core::cmp::min(BLOCK_SIZE * 8, cap - base);
        if let Some(p) = first_zero(&buf, limit) {
            bit_set(&mut buf, p);
            if !save_bitmap(BLOCK_BITMAP_START, sub, &buf) {
                return None;
            }
            return Some(sb.data_start as u64 + base as u64 + p as u64);
        }
    }
    None
}

/// Освобождает блок данных. Вне диапазона - игнор (true = бит был снят/уже чист).
pub fn free_data_block(sb: &Superblock, bno: u64) -> bool {
    let cap = data_region_bits(sb);
    if bno < sb.data_start as u64 || (bno - sb.data_start as u64) as usize >= cap {
        return false;
    }
    let p = (bno - sb.data_start as u64) as usize;
    let sub = (p / (BLOCK_SIZE * 8)) as u32;
    let mut buf = [0u8; BLOCK_SIZE];
    if !load_bitmap(BLOCK_BITMAP_START, sub, &mut buf) {
        return false;
    }
    bit_clear(&mut buf, p % (BLOCK_SIZE * 8));
    save_bitmap(BLOCK_BITMAP_START, sub, &buf)
}

/// Выделяет свободный inode (номер >= 3). Резерв 0..=2 не выдаётся никогда.
pub fn alloc_inode(sb: &Superblock) -> Option<u32> {
    let cap = sb.total_inodes as usize;
    if cap < 4 {
        return None;
    }
    let mut buf = [0u8; BLOCK_SIZE];
    for sub in 0..bitmap_blocks_needed(sb.total_inodes) {
        if !load_bitmap(INODE_BITMAP_START, sub, &mut buf) {
            return None;
        }
        let base = sub as usize * BLOCK_SIZE * 8;
        let limit = core::cmp::min(BLOCK_SIZE * 8, cap - base);
        while let Some(p) = first_zero(&buf, limit) {
            let ino = base + p;
            if ino < 3 {
                // Резервные 0..=2 должны быть помечены форматом; страховка.
                bit_set(&mut buf, p);
                continue;
            }
            bit_set(&mut buf, p);
            if !save_bitmap(INODE_BITMAP_START, sub, &buf) {
                return None;
            }
            return Some(ino as u32);
        }
    }
    None
}

pub fn free_inode(sb: &Superblock, ino: u32) -> bool {
    if ino < 3 || ino >= sb.total_inodes {
        return false;
    }
    let sub = ino as usize / (BLOCK_SIZE * 8);
    let mut buf = [0u8; BLOCK_SIZE];
    if !load_bitmap(INODE_BITMAP_START, sub as u32, &mut buf) {
        return false;
    }
    bit_clear(&mut buf, ino as usize % (BLOCK_SIZE * 8));
    save_bitmap(INODE_BITMAP_START, sub as u32, &buf)
}

/// Свободных блоков данных (для fsck-lite и диагностики).
pub fn count_free_data_blocks(sb: &Superblock) -> u32 {
    let cap = data_region_bits(sb);
    let mut buf = [0u8; BLOCK_SIZE];
    let mut total = 0u32;
    for sub in 0..bitmap_blocks_needed(cap as u32) {
        if !load_bitmap(BLOCK_BITMAP_START, sub, &mut buf) {
            return u32::MAX;
        }
        let base = sub as usize * BLOCK_SIZE * 8;
        let limit = core::cmp::min(BLOCK_SIZE * 8, cap - base);
        total += count_zeros(&buf, limit);
    }
    total
}

/// Свободных inode (резерв 0..=2 считается занятым).
pub fn count_free_inodes(sb: &Superblock) -> u32 {
    let mut buf = [0u8; BLOCK_SIZE];
    if !load_bitmap(INODE_BITMAP_START, 0, &mut buf) {
        return u32::MAX;
    }
    count_zeros(&buf, sb.total_inodes as usize)
}

/// Чистая логическая самопроверка битовых операций (без диска).
fn bitmap_selftest_pure() -> bool {
    let mut m = [0u8; 64]; // 512 бит
    let cap = 512;
    // Всё свободно.
    if count_zeros(&m, cap) != 512 || first_zero(&m, cap) != Some(0) {
        return false;
    }
    // Занять подряд 0..9, проверить границы байтов.
    for i in 0..10 {
        bit_set(&mut m, i);
    }
    if count_zeros(&m, cap) != 502 || first_zero(&m, cap) != Some(10) {
        return false;
    }
    // Занять последний бит (граница лимита).
    bit_set(&mut m, 511);
    if first_zero(&m, 512) != Some(10) {
        return false;
    }
    // Лимит меньше реального размера: бит за лимитом не виден.
    bit_clear(&mut m, 511);
    if first_zero(&m, 511) != Some(10) {
        return false;
    }
    // Освобождение первого делает его целью first_zero.
    bit_clear(&mut m, 3);
    if first_zero(&m, 512) != Some(3) {
        return false;
    }
    bit_set(&mut m, 3);
    // Полностью забить и получить None.
    for i in 0..cap {
        bit_set(&mut m, i);
    }
    if first_zero(&m, cap).is_some() || count_zeros(&m, cap) != 0 {
        return false;
    }
    true
}

/// Дисковая самопроверка аллокаторов на геометрии смонтированной ФС.
/// Трогает только блоки битмапов (65 и 67); подменяет их свежим форматом,
/// гоняет аллокатор и восстанавливает исходное содержимое.
pub fn bitmap_selftest_disk() -> bool {
    crate::bcache::drop_all();
    let mut orig_bb = [0u8; BLOCK_SIZE];
    let mut orig_ib = [0u8; BLOCK_SIZE];
    if crate::blockdev::read_block(BLOCK_BITMAP_START as u64, &mut orig_bb).is_err() {
        return false;
    }
    if crate::blockdev::read_block(INODE_BITMAP_START as u64, &mut orig_ib).is_err() {
        return false;
    }

    // Тест требует детерминированный старт: подкладываем битмапы
    // свежеотформатированного диска (на реальном образе заняты /bin и т.п.).
    // Пишем МИМО кэша: drop_all грязные записи выбрасывает, а не сбрасывает.
    let mut fresh_bb = [0u8; BLOCK_SIZE];
    let mut fresh_ib = [0u8; BLOCK_SIZE];
    for bit in 0..=ROOT_INODE as usize {
        bit_set(&mut fresh_ib, bit);
    }
    if crate::blockdev::write_block(BLOCK_BITMAP_START as u64, &fresh_bb).is_err() {
        return false;
    }
    if crate::blockdev::write_block(INODE_BITMAP_START as u64, &fresh_ib).is_err() {
        return false;
    }
    crate::bcache::drop_all();

    let sb = current_sb();
    let mut ok = false;
    'run: {
        let a = match alloc_data_block(&sb) {
            Some(v) => v,
            None => break 'run,
        };
        let b = match alloc_data_block(&sb) {
            Some(v) => v,
            None => break 'run,
        };
        if a != DATA_START as u64 || b != DATA_START as u64 + 1 {
            break 'run;
        }
        // First-fit после освобождения должен вернуть первый блок.
        if !free_data_block(&sb, a) {
            break 'run;
        }
        match alloc_data_block(&sb) {
            Some(v) if v == a => {}
            _ => break 'run,
        }
        // Inode: резерв 0..=2 пропускается.
        let i1 = match alloc_inode(&sb) {
            Some(v) => v,
            None => break 'run,
        };
        if i1 != 3 {
            break 'run;
        }
        if !free_inode(&sb, i1) {
            break 'run;
        }
        match alloc_inode(&sb) {
            Some(v) if v == i1 => {}
            _ => break 'run,
        }
        // Счётчики: занято 2 блока данных; из inode заняты резерв 0..=2
        // (защита аллокатора помечает их при первом скане) и один живой.
        if count_free_data_blocks(&sb) != sb.total_blocks - sb.data_start - 2 {
            break 'run;
        }
        if count_free_inodes(&sb) != sb.total_inodes - 4 {
            break 'run;
        }
        ok = true;
    }

    // Восстановление исходного состояния диска и кэша.
    let r1 = crate::blockdev::write_block(BLOCK_BITMAP_START as u64, &orig_bb);
    let r2 = crate::blockdev::write_block(INODE_BITMAP_START as u64, &orig_ib);
    crate::bcache::drop_all();
    r1.is_ok() && r2.is_ok() && ok
}

// ---------------------------------------------------------------------------
// Дисковый inode (3.1.3). Раскладка 128 байт - см. docs/fs.md.

// Задействуются в 3.1.5+ (слой каталогов) и 3.3 (devfs); до тех пор
// компилятор честно ругается - глушим точечно.
#[allow(dead_code)]
pub const TYPE_FREE: u16 = 0;
pub const TYPE_FILE: u16 = 1;
pub const TYPE_DIR: u16 = 2;
pub const TYPE_SYMLINK: u16 = 3;
#[allow(dead_code)]
pub const TYPE_CHARDEV: u16 = 4;
#[allow(dead_code)]
pub const TYPE_BLKDEV: u16 = 5;

pub const SYMLINK_MAX: usize = 60;
pub const DIRECT_BLOCKS: usize = 12;
pub const PTRS_PER_BLOCK: usize = BLOCK_SIZE / 4; // 1024
pub const MAX_FILE_BLOCKS: u32 = (DIRECT_BLOCKS + PTRS_PER_BLOCK) as u32; // 1036

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DiskInode {
    pub itype: u16,
    pub perms: u16,
    pub size: u32,
    pub links: u32,
    pub atime: u32,
    pub mtime: u32,
    /// Номер драйвера для chardev/blkdev.
    pub device_id: u32,
    /// Быстрый симлинк: цель в области указателей (24..84), как в ext2.
    pub target: [u8; SYMLINK_MAX],
    pub direct: [u32; DIRECT_BLOCKS],
    pub indirect1: u32,
    /// Резерв на будущее; на диске обязан быть 0.
    pub indirect2: u32,
    pub indirect3: u32,
}

impl DiskInode {
    pub const fn zeroed(itype: u16) -> DiskInode {
        DiskInode {
            itype,
            perms: 0o644,
            size: 0,
            links: 1,
            atime: 0,
            mtime: 0,
            device_id: 0,
            target: [0; SYMLINK_MAX],
            direct: [0; DIRECT_BLOCKS],
            indirect1: 0,
            indirect2: 0,
            indirect3: 0,
        }
    }

    pub fn decode(buf: &[u8; INODE_SIZE]) -> Option<DiskInode> {
        let g16 = |o: usize| -> u16 { u16::from_le_bytes([buf[o], buf[o + 1]]) };
        let g32 = |o: usize| -> u32 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&buf[o..o + 4]);
            u32::from_le_bytes(b)
        };
        let itype = g16(0);
        if itype > TYPE_BLKDEV {
            return None;
        }
        let mut target = [0u8; SYMLINK_MAX];
        target.copy_from_slice(&buf[24..24 + SYMLINK_MAX]);
        let mut direct = [0u32; DIRECT_BLOCKS];
        for (i, d) in direct.iter_mut().enumerate() {
            *d = g32(24 + i * 4);
        }
        let size = g32(4);
        let mut inode = DiskInode {
            itype,
            perms: g16(2),
            size,
            links: g32(8),
            atime: g32(12),
            mtime: g32(16),
            device_id: g32(20),
            target,
            direct,
            indirect1: g32(72),
            indirect2: g32(76),
            indirect3: g32(80),
        };
        if itype != TYPE_SYMLINK {
            // У не-симлинков область цели - это указатели; в памяти
            // target считаем невалидным (нули), чтобы PartialEq был честным.
            inode.target = [0; SYMLINK_MAX];
        }
        if itype == TYPE_SYMLINK && size <= SYMLINK_MAX as u32 {
            // Fast-symlink: область 24..84 - это строка цели, указатели
            // не используются (семантика ext2). Резервные байты - часть
            // строки, их не валидируем.
            inode.direct = [0; DIRECT_BLOCKS];
            inode.indirect1 = 0;
            inode.indirect2 = 0;
            inode.indirect3 = 0;
            return Some(inode);
        }
        if inode.indirect2 != 0 || inode.indirect3 != 0 {
            return None; // резервы должны быть нулями
        }
        Some(inode)
    }

    pub fn encode(&self, out: &mut [u8; INODE_SIZE]) {
        *out = [0u8; INODE_SIZE];
        out[0..2].copy_from_slice(&self.itype.to_le_bytes());
        out[2..4].copy_from_slice(&self.perms.to_le_bytes());
        out[4..8].copy_from_slice(&self.size.to_le_bytes());
        out[8..12].copy_from_slice(&self.links.to_le_bytes());
        out[12..16].copy_from_slice(&self.atime.to_le_bytes());
        out[16..20].copy_from_slice(&self.mtime.to_le_bytes());
        out[20..24].copy_from_slice(&self.device_id.to_le_bytes());
        for (i, d) in self.direct.iter().enumerate() {
            out[24 + i * 4..28 + i * 4].copy_from_slice(&d.to_le_bytes());
        }
        out[72..76].copy_from_slice(&self.indirect1.to_le_bytes());
        out[76..80].copy_from_slice(&self.indirect2.to_le_bytes());
        out[80..84].copy_from_slice(&self.indirect3.to_le_bytes());
        // Цель симлинка живёт в той же области, что и указатели (union).
        if self.itype == TYPE_SYMLINK {
            out[24..24 + SYMLINK_MAX].copy_from_slice(&self.target);
        }
    }

    /// Цель симлинка как &str (до NUL или конца поля); None при мусоре.
    pub fn target_str(&self) -> Option<&str> {
        let len = self.target.iter().position(|&b| b == 0).unwrap_or(SYMLINK_MAX);
        core::str::from_utf8(&self.target[..len]).ok()
    }

    /// Установить цель симлинка; false если не влезает или не UTF-8.
    pub fn set_target(&mut self, s: &str) -> bool {
        let b = s.as_bytes();
        if b.is_empty() || b.len() > SYMLINK_MAX - 1 {
            return false;
        }
        self.target = [0; SYMLINK_MAX];
        self.target[..b.len()].copy_from_slice(b);
        true
    }

    /// Сколько блоков данных занимает файл данного размера.
    /// Fast-symlink хранит цель в самом inode и блоков не имеет.
    pub fn n_data_blocks(&self) -> u32 {
        match self.itype {
            TYPE_DIR | TYPE_FILE => (self.size + BLOCK_SIZE as u32 - 1) / BLOCK_SIZE as u32,
            _ => 0,
        }
    }

    /// Список номеров блоков данных (прямые + раскрытый косвенный).
    /// Косвенный блок читается через bcache; None при ошибке диска.
    /// Используется слоем файлов/каталогов (3.1.5+).
    #[allow(dead_code)]
    pub fn block_list(&self) -> Option<alloc::vec::Vec<u32>> {
        let n = self.n_data_blocks() as usize;
        if self.itype == TYPE_SYMLINK {
            return Some(alloc::vec::Vec::new());
        }
        let mut v = alloc::vec::Vec::with_capacity(n);
        for i in 0..core::cmp::min(n, DIRECT_BLOCKS) {
            if self.direct[i] == 0 {
                return None; // дыра в середине недопустима
            }
            v.push(self.direct[i]);
        }
        if n > DIRECT_BLOCKS {
            let mut blk = [0u8; BLOCK_SIZE];
            crate::bcache::read(self.indirect1 as u64, &mut blk).ok()?;
            for i in 0..n - DIRECT_BLOCKS {
                let o = i * 4;
                let p = u32::from_le_bytes(blk[o..o + 4].try_into().ok()?);
                if p == 0 {
                    return None;
                }
                v.push(p);
            }
        }
        Some(v)
    }
}

/// Читает inode по абсолютному номеру из таблицы через bcache.
pub fn read_inode(sb: &Superblock, ino: u32) -> Option<DiskInode> {
    if ino == 0 || ino >= sb.total_inodes {
        return None;
    }
    let per_block = INODES_PER_BLOCK as u32;
    let block = sb.inode_table_start + ino / per_block;
    let slot = (ino % per_block) as usize * INODE_SIZE;
    let mut buf = [0u8; BLOCK_SIZE];
    crate::bcache::read(block as u64, &mut buf).ok()?;
    let mut raw = [0u8; INODE_SIZE];
    raw.copy_from_slice(&buf[slot..slot + INODE_SIZE]);
    DiskInode::decode(&raw)
}

/// Пишет inode в таблицу через bcache (dirty до flush_all).
pub fn write_inode(sb: &Superblock, ino: u32, node: &DiskInode) -> bool {
    if ino == 0 || ino >= sb.total_inodes {
        return false;
    }
    let per_block = INODES_PER_BLOCK as u32;
    let block = (sb.inode_table_start + ino / per_block) as u64;
    let slot = (ino % per_block) as usize * INODE_SIZE;
    let mut buf = [0u8; BLOCK_SIZE];
    if !fs_read_block(block, &mut buf) {
        return false;
    }
    let mut raw = [0u8; INODE_SIZE];
    node.encode(&mut raw);
    buf[slot..slot + INODE_SIZE].copy_from_slice(&raw);
    fs_write_block(block, &buf)
}

/// Самопроверка inode-слоя: чистый roundtrip + границы симлинка +
/// дисковый roundtrip на последнем слоте таблицы с восстановлением.
pub fn inode_selftest_disk() -> bool {
    if !inode_selftest_pure() {
        return false;
    }
    let sb = current_sb();
    // Последний inode таблицы: 1023 -> блок 100, слот 31.
    let probe = sb.total_inodes - 1;
    let table_block = (sb.inode_table_start + probe / INODES_PER_BLOCK) as u64;
    let mut orig = [0u8; BLOCK_SIZE];
    crate::bcache::drop_all();
    if crate::blockdev::read_block(table_block, &mut orig).is_err() {
        return false;
    }
    let mut ok = false;
    'run: {
        let mut n = DiskInode::zeroed(TYPE_FILE);
        n.size = 7777;
        n.atime = 111;
        n.mtime = 222;
        n.direct[0] = DATA_START;
        n.direct[11] = DATA_START + 11;
        if !write_inode(&sb, probe, &n) {
            break 'run;
        }
        match read_inode(&sb, probe) {
            Some(back) if back == n => {}
            _ => break 'run,
        }
        // Чтение за границей и нулевой номер отклоняются.
        if read_inode(&sb, sb.total_inodes).is_some() || read_inode(&sb, 0).is_some() {
            break 'run;
        }
        ok = true;
    }
    let r = crate::blockdev::write_block(table_block, &orig);
    crate::bcache::drop_all();
    r.is_ok() && ok
}

fn inode_selftest_pure() -> bool {
    let mut raw = [0u8; INODE_SIZE];

    // Файл максимального размера: полный roundtrip всех полей.
    let mut f = DiskInode::zeroed(TYPE_FILE);
    f.size = MAX_FILE_BLOCKS * BLOCK_SIZE as u32;
    f.atime = 111;
    f.mtime = 222;
    f.direct[0] = DATA_START;
    f.indirect1 = DATA_START + 999;
    f.encode(&mut raw);
    if DiskInode::decode(&raw) != Some(f) {
        return false;
    }

    // Fast-symlink: цель длиной SYMLINK_MAX-1 (+NUL), указатели невалидны.
    let mut s = DiskInode::zeroed(TYPE_SYMLINK);
    assert!(s.set_target("/bin/echoargs"));
    if !s.set_target(&"x".repeat(SYMLINK_MAX)) {
        // ровно SYMLINK_MAX не влезает (нужен NUL)
    } else {
        return false;
    }
    let long = "y".repeat(SYMLINK_MAX - 1);
    if !s.set_target(&long) {
        return false;
    }
    s.size = SYMLINK_MAX as u32 - 1;
    s.encode(&mut raw);
    match DiskInode::decode(&raw) {
        Some(b) if b == s && b.target_str() == Some(long.as_str()) => {}
        _ => return false,
    }

    // Мусорный тип отбрасывается.
    raw[0] = 0xFF;
    raw[1] = 0xFF;
    if DiskInode::decode(&raw).is_some() {
        return false;
    }
    // n_data_blocks: 0 байт -> 0, 1 байт -> 1, ровно блок -> 1.
    let mut f = DiskInode::zeroed(TYPE_FILE);
    if f.n_data_blocks() != 0 {
        return false;
    }
    f.size = 1;
    if f.n_data_blocks() != 1 {
        return false;
    }
    f.size = BLOCK_SIZE as u32;
    if f.n_data_blocks() != 1 {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// In-memory кэш inode (3.1.4).
//
// Фиксированная таблица в .bss, LRU по штампу обращений, dirty-флаги.
// pin()/unpin() защищают запись от вытеснения (для будущих файловых
// дескрипторов). Вытеснение грязной записи немедленно пишет её на диск.
//
// Инвариант union'а: у не-симлинков target в кэше всегда нули (это
// гарантирует decode), поэтому кэш можно сравнивать и писать как есть.
//
// Геометрия, отличную от 8-МиБ константы, несёт СМОНТИРОВАННЫЙ суперблок
// (заполняется из sb на диске) — вся работа с ФС идёт через current_sb().
// boot_sb() остаётся только для этапа форматирования, когда диска ещё нет.

/// Канонический суперблок загрузочного образа (этап форматирования,
/// fallback при недоступной ёмкости устройства).
pub fn boot_sb() -> Superblock {
    Superblock::for_image(8 * 1024 * 1024)
}

/// Суперблок текущей ФС: смонтированный (реальная геометрия диска);
/// до появления mount-контекста — канонический загрузочный.
pub fn current_sb() -> Superblock {
    mounted_sb().copied().unwrap_or_else(boot_sb)
}


pub const INODE_CACHE_SLOTS: usize = 128;

#[derive(Clone, Copy, PartialEq)]
enum IState {
    Free,
    Clean,
    Dirty,
}

#[derive(Clone, Copy)]
struct IEntry {
    ino: u32,
    node: DiskInode,
    state: IState,
    stamp: u64,
    pins: u32,
}

static mut ICLOCK: u64 = 0;
static mut ITABLE: [IEntry; INODE_CACHE_SLOTS] = [IEntry {
    ino: 0,
    node: DiskInode {
        itype: 0,
        perms: 0,
        size: 0,
        links: 0,
        atime: 0,
        mtime: 0,
        device_id: 0,
        target: [0; SYMLINK_MAX],
        direct: [0; DIRECT_BLOCKS],
        indirect1: 0,
        indirect2: 0,
        indirect3: 0,
    },
    state: IState::Free,
    stamp: 0,
    pins: 0,
}; INODE_CACHE_SLOTS];

// Журнал отката inode-кэша для WAL-abort. Проблема: wal::abort() чистил
// только staging, а мутации icache (iput -> Dirty, iget-miss -> заполнение
// слота, ipick_victim -> вытеснение) оставались. Если destroy_node падал
// ПОСЛЕ обнуления inode (например, dir_remove возвращал false), «обнулённый»
// inode висел грязным в кэше и уезжал на диск при следующем commit — при
// этом запись в каталоге оставалась: битый dangling-указатель.
//
// Здесь мы храним ИСХОДНОЕ состояние каждого мутированного слота (первый
// snapshot побеждает, чтобы оригинал не затёрся) и восстанавливаем его при
// abort. Commit просто сбрасывает журнал — зафиксированное состояние новое.
#[derive(Clone, Copy)]
struct IcacheUndo {
    idx: usize,
    ino: u32,
    node: DiskInode,
    state: IState,
    pins: u32,
}

static mut ICACHE_UNDO: [IcacheUndo; INODE_CACHE_SLOTS] = [IcacheUndo {
    idx: 0,
    ino: 0,
    node: DiskInode::zeroed(TYPE_FREE),
    state: IState::Free,
    pins: 0,
}; INODE_CACHE_SLOTS];
static mut ICACHE_UNDO_N: usize = 0;

/// Снимает ПЕРВЫЙ snapshot слота idx при его мутации, если активна
/// транзакция. Повторные вызовы для того же слота игнорируются.
/// Вызывается из ipick_victim (до вытеснения/заполнения слота).
unsafe fn icache_note(idx: usize) {
    if !crate::wal::txn_active() {
        return;
    }
    for i in 0..ICACHE_UNDO_N {
        if ICACHE_UNDO[i].idx == idx {
            return;
        }
    }
    if ICACHE_UNDO_N >= INODE_CACHE_SLOTS {
        return; // слотов не больше, чем слотов кэша
    }
    let e = &ITABLE[idx];
    ICACHE_UNDO[ICACHE_UNDO_N] = IcacheUndo {
        idx,
        ino: e.ino,
        node: e.node,
        state: e.state,
        pins: e.pins,
    };
    ICACHE_UNDO_N += 1;
}

/// Возвращает мутированные слоты к исходному состоянию (вызов: wal::abort
/// при закрытии ВНЕШНЕЙ транзакции).
pub fn icache_rollback() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        while ICACHE_UNDO_N > 0 {
            ICACHE_UNDO_N -= 1;
            let u = ICACHE_UNDO[ICACHE_UNDO_N];
            let e = &mut ITABLE[u.idx];
            e.ino = u.ino;
            e.node = u.node;
            e.state = u.state;
            e.pins = u.pins;
        }
    });
}

/// Сбрасывает журнал (успешный commit / старт новой транзакции).
pub fn icache_undo_clear() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        ICACHE_UNDO_N = 0;
    });
}

static mut ICACHE_STATS: (u64, u64) = (0, 0); // (hits, misses)

unsafe fn itick() -> u64 {
    let p = &mut ICLOCK as *mut u64;
    let v = core::ptr::read_volatile(p) + 1;
    core::ptr::write_volatile(p, v);
    v
}

/// Грязную жертву выгоняет на диск через write_inode.
unsafe fn ipick_victim(ino: u32) -> Option<usize> {
    let mut free_idx = None;
    let mut lru_idx = None;
    let mut lru_stamp = u64::MAX;
    for i in 0..INODE_CACHE_SLOTS {
        let e = &ITABLE[i];
        match e.state {
            IState::Free => {
                if free_idx.is_none() {
                    free_idx = Some(i);
                }
            }
            _ => {
                if e.ino == ino {
                    icache_note(i);
                    return Some(i);
                }
                if e.pins == 0 && e.stamp < lru_stamp {
                    lru_stamp = e.stamp;
                    lru_idx = Some(i);
                }
            }
        }
    }
    let idx = free_idx.or(lru_idx)?;
    icache_note(idx);
    if ITABLE[idx].state == IState::Dirty {
        let e = ITABLE[idx];
        if !write_inode(&current_sb(), e.ino, &e.node) {
            return None;
        }
    }
    Some(idx)
}

/// Читает inode через кэш: попадание - копия из кэша, промах - с диска.
pub fn iget(sb: &Superblock, ino: u32) -> Option<DiskInode> {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        for i in 0..INODE_CACHE_SLOTS {
            if ITABLE[i].state != IState::Free && ITABLE[i].ino == ino {
                ITABLE[i].stamp = itick();
                let (hp, _) = &mut ICACHE_STATS;
                core::ptr::write_volatile(hp, core::ptr::read_volatile(hp) + 1);
                return Some(ITABLE[i].node);
            }
        }
        let (_, mp) = &mut ICACHE_STATS;
        core::ptr::write_volatile(mp, core::ptr::read_volatile(mp) + 1);
        let node = read_inode(sb, ino)?;
        let idx = ipick_victim(ino)?;
        let e = &mut ITABLE[idx];
        e.ino = ino;
        e.node = node;
        e.state = IState::Clean;
        e.stamp = itick();
        e.pins = 0;
        Some(node)
    })
}

/// Кладёт копию в кэш и помечает грязной. Дисковая запись отложена до
/// iflush_all(); при переполнении грязная жертва выгоняется немедленно.
pub fn iput(_sb: &Superblock, ino: u32, node: &DiskInode) -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let idx = match ipick_victim(ino) {
            Some(i) => i,
            None => return false,
        };
        let e = &mut ITABLE[idx];
        e.ino = ino;
        e.node = *node;
        e.state = IState::Dirty;
        e.stamp = itick();
        true
    })
}

/// Полная синхронизация: inode-кэш -> bcache -> диск.
pub fn sync_all() -> bool {
    iflush_all() && crate::bcache::flush_all()
}

/// Выгоняет все грязные inode на диск.
pub fn iflush_all() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let sb = current_sb();
        let mut ok = true;
        for i in 0..INODE_CACHE_SLOTS {
            if ITABLE[i].state == IState::Dirty {
                if write_inode(&sb, ITABLE[i].ino, &ITABLE[i].node) {
                    ITABLE[i].state = IState::Clean;
                } else {
                    ok = false;
                }
            }
        }
        ok
    })
}

/// Полный сброс кэша без записи (тесты/размонтирование после flush).
#[allow(dead_code)]
pub fn idrop_all() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        for e in ITABLE.iter_mut() {
            e.state = IState::Free;
            e.pins = 0;
        }
    });
}

/// Защита от вытеснения (будущие файловые дескрипторы).
#[allow(dead_code)]
pub fn pin(ino: u32) -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        for i in 0..INODE_CACHE_SLOTS {
            if ITABLE[i].state != IState::Free && ITABLE[i].ino == ino {
                ITABLE[i].pins += 1;
                return true;
            }
        }
        false
    })
}

#[allow(dead_code)]
pub fn unpin(ino: u32) {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        for i in 0..INODE_CACHE_SLOTS {
            if ITABLE[i].state != IState::Free && ITABLE[i].ino == ino && ITABLE[i].pins > 0 {
                ITABLE[i].pins -= 1;
                return;
            }
        }
    });
}

/// Самопроверка кэша: put->get hit, выгрузка на диск, чтение мимо кэша.
pub fn icache_selftest_disk() -> bool {
    idrop_all();
    crate::bcache::drop_all();
    let sb = current_sb();
    // Соседний с probe слот: 1022 -> блок 100.
    let probe = sb.total_inodes - 2;
    let table_block = (sb.inode_table_start + probe / INODES_PER_BLOCK) as u64;
    let mut orig = [0u8; BLOCK_SIZE];
    if crate::blockdev::read_block(table_block, &mut orig).is_err() {
        return false;
    }
    let mut ok = false;
    'run: {
        let mut n = DiskInode::zeroed(TYPE_FILE);
        n.size = 12345;
        n.mtime = 42;
        if !iput(&sb, probe, &n) {
            break 'run;
        }
        match iget(&sb, probe) {
            Some(got) if got.size == 12345 => {}
            _ => break 'run,
        }
        // Полная синхронизация (inode-кэш -> bcache -> диск), затем читаем
        // напрямую блоком, минуя оба кэша.
        if !sync_all() {
            break 'run;
        }
        idrop_all();
        crate::bcache::drop_all();
        match read_inode(&sb, probe) {
            Some(back) if back.size == 12345 && back.mtime == 42 => {}
            _ => break 'run,
        }
        ok = true;
    }
    let r = crate::blockdev::write_block(table_block, &orig);
    idrop_all();
    crate::bcache::drop_all();
    r.is_ok() && ok
}

// ---------------------------------------------------------------------------
// Файловые блоки и каталоги (3.1.5).
//
// Каталог - обычный файл типа dir; содержимое - массив записей по 64 байта:
//   [u32 inode][u8 name_len][59 байт имени]; inode=0 означает свободный слот.
// Точечных записей "." и ".." нет - их роль играет нормализация путей.

pub const DIRENT_SIZE: usize = 64;
pub const DIRENT_NAME_MAX: usize = 58;

/// Читает блок данных файла по индексу (0..n_data_blocks).
pub fn file_read_block(sb: &Superblock, ino: u32, idx: u32, buf: &mut [u8; BLOCK_SIZE]) -> Option<()> {
    let node = iget(sb, ino)?;
    let list = node.block_list()?;
    let bno = *list.get(idx as usize)? as u64;
    fs_read_block(bno, buf).then_some(())
}

/// Пишет блок данных файла, при необходимости выделяя новый (прямой или
/// через косвенный). Указатели обновляет В КОПИИ ВЫЗЫВАЮЩЕГО (`node`) и на
/// диск НЕ пишет - iput с той же копией делает вызывающий (иначе последняя
/// запись затирает указатели устаревшей версией узла).
/// Возвращает абсолютный номер блока.
pub fn file_write_block(
    sb: &Superblock,
    node: &mut DiskInode,
    idx: u32,
    buf: &[u8; BLOCK_SIZE],
) -> Option<u64> {
    let bno: u32;
    if (idx as usize) < DIRECT_BLOCKS {
        if node.direct[idx as usize] == 0 {
            node.direct[idx as usize] = alloc_data_block(sb)? as u32;
        }
        bno = node.direct[idx as usize];
    } else {
        let iidx = idx as usize - DIRECT_BLOCKS;
        if iidx >= PTRS_PER_BLOCK {
            return None;
        }
        if node.indirect1 == 0 {
            let iblk = alloc_data_block(sb)?;
            let zeros = [0u8; BLOCK_SIZE];
            if !fs_write_block(iblk, &zeros) {
                free_data_block(sb, iblk);
                return None;
            }
            node.indirect1 = iblk as u32;
        }
        let mut ind = [0u8; BLOCK_SIZE];
        if !fs_read_block(node.indirect1 as u64, &mut ind) {
            return None;
        }
        let o = iidx * 4;
        let existing = u32::from_le_bytes(ind[o..o + 4].try_into().ok()?);
        let target = if existing != 0 {
            existing
        } else {
            let fresh = alloc_data_block(sb)? as u32;
            ind[o..o + 4].copy_from_slice(&fresh.to_le_bytes());
            if !fs_write_block(node.indirect1 as u64, &ind) {
                free_data_block(sb, fresh as u64);
                return None;
            }
            fresh
        };
        bno = target;
    }
    if !fs_write_block(bno as u64, buf) {
        return None;
    }
    Some(bno as u64)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Dirent {
    pub ino: u32,
    pub name: alloc::string::String,
}

fn parse_dirent(raw: &[u8]) -> Option<Dirent> {
    let ino = u32::from_le_bytes(raw[0..4].try_into().ok()?);
    if ino == 0 {
        return None;
    }
    let len = raw[4] as usize;
    if len == 0 || len > DIRENT_NAME_MAX {
        return None;
    }
    let name = core::str::from_utf8(&raw[5..5 + len]).ok()?;
    Some(Dirent {
        ino,
        name: alloc::string::String::from(name),
    })
}

fn encode_dirent(e: (&str, u32), out: &mut [u8; BLOCK_SIZE], slot: usize) -> bool {
    let (name, ino) = e;
    let b = name.as_bytes();
    if b.is_empty() || b.len() > DIRENT_NAME_MAX {
        return false;
    }
    let o = slot * DIRENT_SIZE;
    out[o..o + DIRENT_SIZE].fill(0);
    out[o..o + 4].copy_from_slice(&ino.to_le_bytes());
    out[o + 4] = b.len() as u8;
    out[o + 5..o + 5 + b.len()].copy_from_slice(b);
    true
}

/// Поиск имени в каталоге. Возвращает inode ребёнка.
pub fn dir_lookup(sb: &Superblock, dir_ino: u32, name: &str) -> Option<u32> {
    let node = iget(sb, dir_ino)?;
    if node.itype != TYPE_DIR {
        return None;
    }
    let nblocks = node.n_data_blocks();
    let mut buf = [0u8; BLOCK_SIZE];
    for bi in 0..nblocks {
        file_read_block(sb, dir_ino, bi, &mut buf)?;
        let slots = BLOCK_SIZE / DIRENT_SIZE;
        for s in 0..slots {
            if let Some(e) = parse_dirent(&buf[s * DIRENT_SIZE..(s + 1) * DIRENT_SIZE]) {
                if e.name == name {
                    return Some(e.ino);
                }
            }
        }
    }
    None
}

/// Перечисление валидных записей каталога (дырки пропускаются).
pub fn dir_readdir(sb: &Superblock, dir_ino: u32) -> Option<alloc::vec::Vec<Dirent>> {
    let node = iget(sb, dir_ino)?;
    if node.itype != TYPE_DIR {
        return None;
    }
    let mut out = alloc::vec::Vec::new();
    let nblocks = node.n_data_blocks();
    let mut buf = [0u8; BLOCK_SIZE];
    for bi in 0..nblocks {
        file_read_block(sb, dir_ino, bi, &mut buf)?;
        let slots = BLOCK_SIZE / DIRENT_SIZE;
        for s in 0..slots {
            if let Some(e) = parse_dirent(&buf[s * DIRENT_SIZE..(s + 1) * DIRENT_SIZE]) {
                out.push(e);
            }
        }
    }
    Some(out)
}

/// Добавляет запись. Ищет свободный слот (ino==0) в существующих блоках,
/// иначе дописывает в конец, расширяя файл каталога.
pub fn dir_add(sb: &Superblock, dir_ino: u32, name: &str, child_ino: u32) -> bool {
    if child_ino == 0 || dir_ino == 0 {
        return false;
    }
    let mut node = match iget(sb, dir_ino) {
        Some(n) if n.itype == TYPE_DIR => n,
        _ => return false,
    };
    let slots_per_block = BLOCK_SIZE / DIRENT_SIZE;
    let nblocks = node.n_data_blocks();
    let mut buf = [0u8; BLOCK_SIZE];

    // 1) Свободный слот в существующих блоках.
    for bi in 0..nblocks {
        if file_read_block(sb, dir_ino, bi, &mut buf).is_none() {
            return false;
        }
        for s in 0..slots_per_block {
            let o = s * DIRENT_SIZE;
            if u32::from_le_bytes(buf[o..o + 4].try_into().unwrap_or([0; 4])) == 0 {
                if !encode_dirent((name, child_ino), &mut buf, s) {
                    return false;
                }
                if file_write_block(sb, &mut node, bi, &buf).is_none() {
                    return false;
                }
                // Слот мог лежать за пределами size (дырки в хвосте):
                // корректно расширяем файл каталога.
                let end = bi * BLOCK_SIZE as u32 + (s as u32 + 1) * DIRENT_SIZE as u32;
                if end > node.size {
                    node.size = end;
                }
                node.mtime = crate::rtc::now_unix() as u32;
                return iput(sb, dir_ino, &node);
            }
        }
    }

    // 2) Дописываем в конец: возможно, нужен новый блок.
    let total_slots = nblocks as usize * slots_per_block;
    let new_bi = (total_slots / slots_per_block) as u32;
    let new_s = total_slots % slots_per_block;
    if new_s == 0 {
        buf = [0u8; BLOCK_SIZE];
    } else if file_read_block(sb, dir_ino, new_bi, &mut buf).is_none() {
        return false;
    }
    if !encode_dirent((name, child_ino), &mut buf, new_s) {
        return false;
    }
    if file_write_block(sb, &mut node, new_bi, &buf).is_none() {
        return false;
    }
    node.size += DIRENT_SIZE as u32;
    node.mtime = crate::rtc::now_unix() as u32;
    iput(sb, dir_ino, &node)
}

/// Удаляет запись, превращая слот в дырку (ino=0). Имя освобождается.
pub fn dir_remove(sb: &Superblock, dir_ino: u32, name: &str) -> bool {
    let mut node = match iget(sb, dir_ino) {
        Some(n) if n.itype == TYPE_DIR => n,
        _ => return false,
    };
    let slots_per_block = BLOCK_SIZE / DIRENT_SIZE;
    let nblocks = node.n_data_blocks();
    let mut buf = [0u8; BLOCK_SIZE];
    for bi in 0..nblocks {
        if file_read_block(sb, dir_ino, bi, &mut buf).is_none() {
            return false;
        }
        for s in 0..slots_per_block {
            let o = s * DIRENT_SIZE;
            if let Some(e) = parse_dirent(&buf[o..o + DIRENT_SIZE]) {
                if e.name == name {
                    buf[o..o + DIRENT_SIZE].fill(0);
                    if file_write_block(sb, &mut node, bi, &buf).is_none() {
                        return false;
                    }
                    node.mtime = crate::rtc::now_unix() as u32;
                    return iput(sb, dir_ino, &node);
                }
            }
        }
    }
    false
}

/// Самопроверка слоя каталогов на реальном диске: использует inode 3..8
/// и восстанавливает затронутые блоки (битмапы 65/67, таблица 69).
pub fn dir_selftest_disk() -> bool {
    idrop_all();
    crate::bcache::drop_all();
    let sb = current_sb();
    // Затрагиваемые метаблоки: битмапы и таблица inode (блок 69 = inodes 32..63,
    // но наши 3..8 тоже в блоке 69? 3..31 -> да, блок 69).
    // Буферы восстановления - static, чтобы не раздувать 16КиБ boot-стек
    // (см. также увеличение стека в boot.asm).
    static mut ORIG_BB: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
    static mut ORIG_IB: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
    static mut ORIG_TB: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
    let orig_bb: &mut [u8; BLOCK_SIZE] = unsafe { &mut ORIG_BB };
    let orig_ib: &mut [u8; BLOCK_SIZE] = unsafe { &mut ORIG_IB };
    let orig_tb: &mut [u8; BLOCK_SIZE] = unsafe { &mut ORIG_TB };
    if crate::blockdev::read_block(BLOCK_BITMAP_START as u64, orig_bb).is_err() {
        return false;
    }
    if crate::blockdev::read_block(INODE_BITMAP_START as u64, orig_ib).is_err() {
        return false;
    }
    if crate::blockdev::read_block(INODE_TABLE_START as u64, orig_tb).is_err() {
        return false;
    }

    let mut ok = false;
    'run: {
        // Корневой тестовый каталог в inode 3.
        let root = DiskInode::zeroed(TYPE_DIR);
        if !iput(&sb, 3, &root) {
            break 'run;
        }
        for (name, ino) in [("alpha", 4), ("beta", 5), ("gamma", 6)] {
            if !dir_add(&sb, 3, name, ino) {
                break 'run;
            }
        }
        if dir_lookup(&sb, 3, "beta") != Some(5) {
            break 'run;
        }
        if dir_lookup(&sb, 3, "nope").is_some() {
            break 'run;
        }
        match dir_readdir(&sb, 3) {
            Some(list) if list.len() == 3 => {}
            _ => break 'run,
        }
        // Удаление превращает запись в дырку; новый add переиспользует слот.
        if !dir_remove(&sb, 3, "beta") {
            break 'run;
        }
        if dir_lookup(&sb, 3, "beta").is_some() {
            break 'run;
        }
        if !dir_add(&sb, 3, "delta", 7) {
            break 'run;
        }
        match dir_readdir(&sb, 3) {
            Some(list) if list.len() == 3 => {}
            _ => break 'run,
        }
        // Рост за границу блока: 64 записи на блок, добавим до 70 детей.
        for i in 0..67u32 {
            let name = alloc::format!("n{:03}", i);
            if !dir_add(&sb, 3, &name, 8) {
                break 'run;
            }
        }
        let last = alloc::format!("n{:03}", 66);
        if dir_lookup(&sb, 3, &last) != Some(8) {
            break 'run;
        }
        match dir_readdir(&sb, 3) {
            Some(list) if list.len() == 70 => {}
            _ => break 'run,
        }
        // Размер каталога кратен DIRENT_SIZE.
        match iget(&sb, 3) {
            Some(n) if n.size == 70 * DIRENT_SIZE as u32 => {}
            _ => break 'run,
        }
        ok = true;
    }

    sync_all();
    let r1 = crate::blockdev::write_block(BLOCK_BITMAP_START as u64, orig_bb);
    let r2 = crate::blockdev::write_block(INODE_BITMAP_START as u64, orig_ib);
    let r3 = crate::blockdev::write_block(INODE_TABLE_START as u64, orig_tb);
    idrop_all();
    crate::bcache::drop_all();
    r1.is_ok() && r2.is_ok() && r3.is_ok() && ok
}

// ---------------------------------------------------------------------------
// Файловый слой, форматирование и монтирование (3.1.6).
//
// Политика размеров: куча ядра всего 1 МиБ, поэтому read_all/write_all
// работают с файлами до MAX_FILE_IO байт (с запасом покрывает логи 8КиБ и
// бинарники /bin); большие запросы отклоняются.

pub const MAX_FILE_IO: usize = 512 * 1024;

/// Канонический суперблок смонтированной ФС. Заполняется mount()/format().
static mut MOUNT_SB: Option<Superblock> = None;

pub fn mounted_sb() -> Option<&'static Superblock> {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        MOUNT_SB.as_ref()
    })
}

/// Форматирует устройство под VITAFS: суперблок, нулевые битмапы с резервными
/// битами, нулевая таблица inode, корневой каталог (inode 2). Возвращает true
/// при успехе. ВНИМАНИЕ: стирает всё (включая легаси-слепок).
pub fn format_image() -> bool {
    if !crate::blockdev::present() {
        return false;
    }
    idrop_all();
    crate::bcache::drop_all();
    // Геометрия выводится из РЕАЛЬНОЙ ёмкости устройства (IDENTIFY), а не из
    // константы 8 МиБ: диск другого размера форматируется целиком. Кламп на
    // вместимость фиксированных битмапов (MAX_TOTAL_BLOCKS). Без ёмкости
    // (ошибка IDENTIFY) - запасной канонический 8-МиБ суперблок.
    let sb = match crate::blockdev::capacity_blocks() {
        Some(blocks) => {
            let total = core::cmp::min(blocks, MAX_TOTAL_BLOCKS);
            Superblock::for_image(total as u64 * BLOCK_SIZE as u64)
        }
        None => boot_sb(),
    };
    let mut ok = true;

    // Суперблок.
    let mut buf = [0u8; BLOCK_SIZE];
    sb.encode(&mut buf);
    ok &= crate::bcache::write(0, &buf).is_ok();

    // Битмапы: сначала нули.
    let zeros = [0u8; BLOCK_SIZE];
    for b in [
        BLOCK_BITMAP_START,
        BLOCK_BITMAP_START + 1,
        INODE_BITMAP_START,
        INODE_BITMAP_START + 1,
    ] {
        ok &= crate::bcache::write(b as u64, &zeros).is_ok();
    }
    // Таблица inode - нули.
    for i in 0..INODE_TABLE_BLOCKS {
        ok &= crate::bcache::write((INODE_TABLE_START + i) as u64, &zeros).is_ok();
    }
    if !ok {
        return false;
    }

    // Примечание: блочные биты отображаются на блоки data_start+bit, поэтому
    // мета-область (блоки 0..DATA_START) в битмапе не представлена вовсе -
    // заносить резерв не нужно. Резерв inode 0..=2 (0 невалидный, 1 служебный,
    // 2 корень) отмечаем явно.
    let mut ibm = [0u8; BLOCK_SIZE];
    if crate::bcache::read(INODE_BITMAP_START as u64, &mut ibm).is_err() {
        return false;
    }
    for bit in 0..=ROOT_INODE as usize {
        bit_set(&mut ibm, bit);
    }
    if crate::bcache::write(INODE_BITMAP_START as u64, &ibm).is_err() {
        return false;
    }

    // Корневой каталог.
    let now = crate::rtc::now_unix() as u32;
    let root = DiskInode {
        itype: TYPE_DIR,
        perms: 0o755,
        size: 0,
        links: 1,
        atime: now,
        mtime: now,
        device_id: 0,
        direct: [0; DIRECT_BLOCKS],
        indirect1: 0,
        indirect2: 0,
        indirect3: 0,
        target: [0; SYMLINK_MAX],
    };
    if !iput(&sb, ROOT_INODE, &root) {
        return false;
    }
    sync_all();

    // Контрольная проверка: суперблок читается и валиден.
    match Superblock::decode(&buf) {
        Some(decoded) if decoded.validate() => {}
        _ => return false,
    }
    unsafe { MOUNT_SB = Some(sb) };
    // Свежий журнал: старые записи не должны переиграться на новую ФС.
    crate::wal::reset();
    true
}

/// Монтирование: читает суперблок с диска и валидирует. При успехе
/// восстанавливает журнал (recover) и запускает fsck-lite.
pub fn try_mount() -> bool {
    if !crate::blockdev::present() {
        return false;
    }
    idrop_all();
    crate::bcache::drop_all();
    let mut buf = [0u8; BLOCK_SIZE];
    if crate::bcache::read(0, &mut buf).is_err() {
        return false;
    }
    match Superblock::decode(&buf) {
        Some(sb) if sb.validate() => {
            // 3.2.3: восстановление журнала до любых обращений к ФС.
            crate::wal::init();
            crate::wal::recover();
            unsafe { MOUNT_SB = Some(sb) };
            fsck_lite(&sb);
            sync_all();
            true
        }
        _ => false,
    }
}

/// Монтирует ФС, форматируя при необходимости. Единая точка входа для загрузки.
pub fn mount_or_format() -> bool {
    if try_mount() {
        return true;
    }
    format_image()
}

/// Читает содержимое файла целиком (обрезается по inode.size).
pub fn file_read_all(sb: &Superblock, ino: u32) -> Option<alloc::vec::Vec<u8>> {
    let node = iget(sb, ino)?;
    if node.itype != TYPE_FILE && node.itype != TYPE_DIR {
        return None;
    }
    let len = node.size as usize;
    if len > MAX_FILE_IO {
        return None;
    }
    let mut out = alloc::vec::Vec::with_capacity(len);
    let nblocks = node.n_data_blocks();
    let mut buf = [0u8; BLOCK_SIZE];
    for bi in 0..nblocks {
        file_read_block(sb, ino, bi, &mut buf)?;
        let start = bi as usize * BLOCK_SIZE;
        let take = core::cmp::min(BLOCK_SIZE, len - start);
        out.extend_from_slice(&buf[..take]);
        if out.len() >= len {
            break;
        }
    }
    Some(out)
}

/// Освобождает блок данных по абсолютному номеру (с сбросом грязной копии).
/// Использует СМОНТИРОВАННЫЙ суперблок (настоящий размер диска), а не
/// захардкоженную 8MiB-константу — иначе на нестандартных образах битмап
/// свободных блоков писался бы по смещениям для 8MiB.
fn release_data_block(bno: u32) -> bool {
    free_data_block(&current_sb(), bno as u64)
}

/// Пишет файл ЦЕЛИКОМ поверх старого содержимого (create-or-truncate).
/// Освобождает ставшие лишними блоки, включая косвенный.
pub fn file_write_all(sb: &Superblock, ino: u32, data: &[u8]) -> bool {
    if data.len() > MAX_FILE_IO {
        return false;
    }
    let mut node = match iget(sb, ino) {
        Some(n) if n.itype == TYPE_FILE || n.itype == TYPE_DIR => n,
        _ => return false,
    };
    let needed = if data.is_empty() {
        0
    } else {
        ((data.len() + BLOCK_SIZE - 1) / BLOCK_SIZE) as u32
    };

    // 1) Освобождаем хвост: прямые слоты за пределами needed.
    for i in needed as usize..DIRECT_BLOCKS {
        if node.direct[i] != 0 {
            let b = node.direct[i];
            node.direct[i] = 0;
            release_data_block(b);
        }
    }
    // 2) Косвенный: освобождаем записи за пределами needed, сам блок - если
    //    косвенный уровень больше не нужен.
    if node.indirect1 != 0 {
        let keep_indirect = needed as usize > DIRECT_BLOCKS;
        let mut ind = [0u8; BLOCK_SIZE];
        if fs_read_block(node.indirect1 as u64, &mut ind) {
            let rel_needed = if keep_indirect {
                needed as usize - DIRECT_BLOCKS
            } else {
                0
            };
            for i in rel_needed..PTRS_PER_BLOCK {
                let o = i * 4;
                let p = u32::from_le_bytes(ind[o..o + 4].try_into().unwrap_or([0; 4]));
                if p != 0 {
                    ind[o..o + 4].copy_from_slice(&[0; 4]);
                    release_data_block(p);
                }
            }
            if keep_indirect {
                fs_write_block(node.indirect1 as u64, &ind);
            } else {
                let ib = node.indirect1;
                node.indirect1 = 0;
                release_data_block(ib);
            }
        } else {
            // Не смогли прочитать косвенный - не рискуем, оставляем как есть.
            return false;
        }
    }
    // 3) Пишем данные (аллокация недостающих - внутри file_write_block).
    let mut chunk = [0u8; BLOCK_SIZE];
    for bi in 0..needed {
        let start = bi as usize * BLOCK_SIZE;
        let end = core::cmp::min(start + BLOCK_SIZE, data.len());
        chunk.fill(0);
        chunk[..end - start].copy_from_slice(&data[start..end]);
        if file_write_block(sb, &mut node, bi, &chunk).is_none() {
            return false;
        }
    }
    node.size = data.len() as u32;
    node.mtime = crate::rtc::now_unix() as u32;
    iput(sb, ino, &node)
}

/// Создаёт узел (файл/каталог) в каталоге parent_dir. Дубликаты запрещены.
/// Возвращает номер нового inode.
pub fn create_node(sb: &Superblock, parent_dir: u32, name: &str, itype: u16) -> Option<u32> {
    if dir_lookup(sb, parent_dir, name).is_some() {
        return None;
    }
    let ino = alloc_inode(sb)?;
    let now = crate::rtc::now_unix() as u32;
    let node = DiskInode {
        itype,
        perms: if itype == TYPE_DIR { 0o755 } else { 0o644 },
        size: 0,
        links: 1,
        atime: now,
        mtime: now,
        device_id: 0,
        direct: [0; DIRECT_BLOCKS],
        indirect1: 0,
        indirect2: 0,
        indirect3: 0,
        target: [0; SYMLINK_MAX],
    };
    if !iput(sb, ino, &node) {
        free_inode(sb, ino);
        return None;
    }
    if !dir_add(sb, parent_dir, name, ino) {
        // Узел без записи в каталоге - мусор; освобождаем.
        let zeroed = DiskInode::zeroed(TYPE_FREE);
        let _ = iput(sb, ino, &zeroed);
        free_inode(sb, ino);
        return None;
    }
    Some(ino)
}

/// Удаляет узел из каталога parent_dir: освобождает данные и inode.
/// `expect_type`: TYPE_FILE для rm, TYPE_DIR для rmdir (пустой!).
pub fn destroy_node(sb: &Superblock, parent_dir: u32, name: &str, expect_type: u16) -> bool {
    let child = match dir_lookup(sb, parent_dir, name) {
        Some(c) => c,
        None => return false,
    };
    let node = match iget(sb, child) {
        Some(n) => n,
        None => return false,
    };
    if node.itype != expect_type {
        return false;
    }
    if expect_type == TYPE_DIR {
        match dir_readdir(sb, child) {
            Some(list) if list.is_empty() => {}
            _ => return false,
        }
    }
    // Порядок освобождения важен для отката: сначала убираем запись из
    // каталога, потом освобождаем данные и только в самом конце обнуляем
    // inode и чистим его битмап. Если что-то упадёт после начала мутаций —
    // wal::abort откатит staging + inode-кэш (журнал отката icache), и ФС
    // останется консистентной: запись на месте, inode не «обнулён», блоки
    // не зависли занятыми в битмапе.
    if !dir_remove(sb, parent_dir, name) {
        return false;
    }
    // Данные: прямые + косвенный уровень.
    for d in node.direct.iter() {
        if *d != 0 {
            release_data_block(*d);
        }
    }
    if node.indirect1 != 0 {
        let mut ind = [0u8; BLOCK_SIZE];
        if fs_read_block(node.indirect1 as u64, &mut ind) {
            for i in 0..PTRS_PER_BLOCK {
                let o = i * 4;
                let p = u32::from_le_bytes(ind[o..o + 4].try_into().unwrap_or([0; 4]));
                if p != 0 {
                    release_data_block(p);
                }
            }
        }
        release_data_block(node.indirect1);
    }
    let zeroed = DiskInode::zeroed(TYPE_FREE);
    if !iput(sb, child, &zeroed) {
        return false;
    }
    free_inode(sb, child)
}

/// Самопроверка файлового слоя: create/write/read/shrink/grow/rm на
/// scratch-inode 3..8 c восстановлением метаблоков.
pub fn fileops_selftest_disk() -> bool {
    // Тестовый каталог живёт на ДАЛЬНЕМ иноду, чтобы не пересекаться с
    // ранними номерами, которые выдаёт аллокатор (3,4,...).
    const TEST_ROOT: u32 = 1000;
    idrop_all();
    crate::bcache::drop_all();
    let sb = current_sb();
    static mut ORIG_BB: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
    static mut ORIG_IB: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
    static mut ORIG_TB: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
    let orig_bb: &mut [u8; BLOCK_SIZE] = unsafe { &mut ORIG_BB };
    let orig_ib: &mut [u8; BLOCK_SIZE] = unsafe { &mut ORIG_IB };
    let orig_tb: &mut [u8; BLOCK_SIZE] = unsafe { &mut ORIG_TB };
    if crate::blockdev::read_block(BLOCK_BITMAP_START as u64, orig_bb).is_err() {
        return false;
    }
    if crate::blockdev::read_block(INODE_BITMAP_START as u64, orig_ib).is_err() {
        return false;
    }
    if crate::blockdev::read_block(INODE_TABLE_START as u64, orig_tb).is_err() {
        return false;
    }
    // Готовим инодный битмап как в отформатированной ФС (резерв 0..=2),
    // чтобы аллокатор не выдал служебные иноды.
    let mut ibm = [0u8; BLOCK_SIZE];
    for bit in 0..=ROOT_INODE as usize {
        bit_set(&mut ibm, bit);
    }
    if crate::bcache::write(INODE_BITMAP_START as u64, &ibm).is_err() {
        return false;
    }

    let mut ok = false;
    'run: {
        // Тестовый каталог в inode 3.
        let now = crate::rtc::now_unix() as u32;
        let root = DiskInode {
            itype: TYPE_DIR,
            perms: 0o755,
            size: 0,
            links: 1,
            atime: now,
            mtime: now,
            device_id: 0,
            direct: [0; DIRECT_BLOCKS],
            indirect1: 0,
            indirect2: 0,
            indirect3: 0,
            target: [0; SYMLINK_MAX],
        };
        if !iput(&sb, TEST_ROOT, &root) {
            break 'run;
        }
        // Маленький файл: запись/чтение.
        let f = match create_node(&sb, TEST_ROOT, "a.txt", TYPE_FILE) {
            Some(f) => f,
            None => break 'run,
        };
        let payload: alloc::vec::Vec<u8> = b"hello vitafs!".iter().cycle().take(5000).copied().collect();
        if !file_write_all(&sb, f, &payload) {
            break 'run;
        }
        match file_read_all(&sb, f) {
            Some(d) if d == payload => {}
            _ => break 'run,
        }
        // Сжатие в один блок и обратно.
        if !file_write_all(&sb, f, b"tiny") {
            break 'run;
        }
        match file_read_all(&sb, f) {
            Some(d) if d == b"tiny" => {}
            _ => break 'run,
        }
        if !file_write_all(&sb, f, &payload) {
            break 'run;
        }
        match file_read_all(&sb, f) {
            Some(d) if d == payload => {}
            _ => break 'run,
        }
        // Большой файл через косвенный блок (>48КиБ).
        let big: alloc::vec::Vec<u8> = (0..300 * 1024).map(|i| (i % 251) as u8).collect();
        if !file_write_all(&sb, f, &big) {
            break 'run;
        }
        match file_read_all(&sb, f) {
            Some(d) if d == big => {}
            _ => break 'run,
        }
        // Пустая запись.
        if !file_write_all(&sb, f, b"") {
            break 'run;
        }
        match iget(&sb, f) {
            Some(n) if n.size == 0 && n.direct[0] == 0 && n.indirect1 == 0 => {}
            _ => break 'run,
        }
        // Подкаталог, вложенный файл, удаление.
        let d = match create_node(&sb, TEST_ROOT, "sub", TYPE_DIR) {
            Some(d) => d,
            None => break 'run,
        };
        let f2 = match create_node(&sb, d, "b.bin", TYPE_FILE) {
            Some(f) => f,
            None => break 'run,
        };
        if !file_write_all(&sb, f2, b"nested") {
            break 'run;
        }
        // Непустой каталог удалять нельзя.
        if destroy_node(&sb, TEST_ROOT, "sub", TYPE_DIR) {
            break 'run;
        }
        if !destroy_node(&sb, d, "b.bin", TYPE_FILE) {
            break 'run;
        }
        if !destroy_node(&sb, TEST_ROOT, "sub", TYPE_DIR) {
            break 'run;
        }
        if dir_lookup(&sb, TEST_ROOT, "sub").is_some() {
            break 'run;
        }
        // Файл удаляется вместе со своими данными.
        if !destroy_node(&sb, TEST_ROOT, "a.txt", TYPE_FILE) {
            break 'run;
        }
        ok = true;
    }

    sync_all();
    let r1 = crate::blockdev::write_block(BLOCK_BITMAP_START as u64, orig_bb);
    let r2 = crate::blockdev::write_block(INODE_BITMAP_START as u64, orig_ib);
    let r3 = crate::blockdev::write_block(INODE_TABLE_START as u64, orig_tb);
    idrop_all();
    crate::bcache::drop_all();
    r1.is_ok() && r2.is_ok() && r3.is_ok() && ok
}

// ---------------------------------------------------------------------------
// fsck-lite (3.2.3): сверка битмапов с фактическим использованием.
//
// Авторитет - таблица inode: проходим по всем inode, собираем множества
// занятых inode и блоков (прямые + косвенный уровень), пересобираем битмапы
// и чиним расхождения одной транзакцией. Обновляет sb.free_inodes.

/// Отмечает блок в пересобранном битмапе данных. Возвращает false, если
/// указатель вне диапазона данных (повреждённый inode).
fn fsck_mark_block(bbm: &mut [u8; BLOCK_SIZE], sb: &Superblock, bno: u32) -> bool {
    if (bno as u64) < sb.data_start as u64 || bno >= sb.total_blocks {
        return false;
    }
    let bit = bno as usize - sb.data_start as usize;
    if bit < BLOCK_SIZE * 8 {
        bit_set(bbm, bit);
    }
    true
}

pub fn fsck_lite(sb: &Superblock) -> bool {
    let mut ibm_new = [0u8; BLOCK_SIZE];
    let mut bbm_new = [0u8; BLOCK_SIZE];
    for bit in 0..=ROOT_INODE as usize {
        bit_set(&mut ibm_new, bit);
    }
    let mut bad_ptrs = 0u32;

    // Проход по таблице inode (сырые слоты, decode отсеивает мусор).
    for tb in 0..INODE_TABLE_BLOCKS {
        let mut blk = [0u8; BLOCK_SIZE];
        if !fs_read_block((INODE_TABLE_START + tb) as u64, &mut blk) {
            return false;
        }
        for s in 0..INODES_PER_BLOCK as usize {
            let o = s * INODE_SIZE;
            let mut raw = [0u8; INODE_SIZE];
            raw.copy_from_slice(&blk[o..o + INODE_SIZE]);
            let Some(node) = DiskInode::decode(&raw) else {
                continue;
            };
            if node.itype == TYPE_FREE {
                continue;
            }
            let ino = tb * INODES_PER_BLOCK + s as u32;
            bit_set(&mut ibm_new, ino as usize);
            if node.itype == TYPE_SYMLINK && node.size <= SYMLINK_MAX as u32 {
                continue; // fast-symlink: данные внутри inode
            }
            for d in node.direct.iter() {
                if *d != 0 && !fsck_mark_block(&mut bbm_new, sb, *d) {
                    bad_ptrs += 1;
                }
            }
            if node.indirect1 != 0 {
                if !fsck_mark_block(&mut bbm_new, sb, node.indirect1) {
                    bad_ptrs += 1;
                    continue;
                }
                let mut ind = [0u8; BLOCK_SIZE];
                if fs_read_block(node.indirect1 as u64, &mut ind) {
                    for i in 0..PTRS_PER_BLOCK {
                        let p = u32::from_le_bytes(ind[i * 4..i * 4 + 4].try_into().unwrap_or([0; 4]));
                        if p != 0 && !fsck_mark_block(&mut bbm_new, sb, p) {
                            bad_ptrs += 1;
                        }
                    }
                }
            }
        }
    }

    // Сверка с битмапами на диске.
    let mut ibm_disk = [0u8; BLOCK_SIZE];
    let mut bbm_disk = [0u8; BLOCK_SIZE];
    let ib_ok = crate::bcache::read(INODE_BITMAP_START as u64, &mut ibm_disk).is_ok();
    let bb_ok = crate::bcache::read(BLOCK_BITMAP_START as u64, &mut bbm_disk).is_ok();
    if !ib_ok || !bb_ok {
        return false;
    }

    let cap_bits = core::cmp::min(data_region_bits(sb), BLOCK_SIZE * 8);
    let mut diff_inodes = 0u32;
    let mut diff_blocks = 0u32;
    for i in 0..sb.total_inodes as usize {
        if bit_get(&ibm_new, i) != bit_get(&ibm_disk, i) {
            diff_inodes += 1;
        }
    }
    for i in 0..cap_bits {
        if bit_get(&bbm_new, i) != bit_get(&bbm_disk, i) {
            diff_blocks += 1;
        }
    }

    // Занятость инодов = popcount пересобранного битмапа (включая резерв 0..=2).
    let mut used_inodes = 0u32;
    for i in 0..sb.total_inodes as usize {
        if bit_get(&ibm_new, i) {
            used_inodes += 1;
        }
    }

    let sb_free_mismatch = sb.free_inodes != sb.total_inodes.saturating_sub(used_inodes);

    if diff_inodes == 0 && diff_blocks == 0 && !sb_free_mismatch && bad_ptrs == 0 {
        return true;
    }

    // Чиним одной транзакцией: битмапы + суперблок.
    crate::wal::begin();
    let mut ok = save_bitmap(INODE_BITMAP_START, 0, &ibm_new);
    ok &= save_bitmap(BLOCK_BITMAP_START, 0, &bbm_new);
    if sb_free_mismatch || bad_ptrs > 0 {
        let mut fixed = *sb;
        fixed.free_inodes = sb.total_inodes.saturating_sub(used_inodes);
        let mut sbuf = [0u8; BLOCK_SIZE];
        fixed.encode(&mut sbuf);
        ok &= fs_write_block(0, &sbuf);
    }
    ok &= crate::wal::commit();
    if !ok {
        crate::wal::abort();
        return false;
    }

    crate::vga::serial_write_atomic("[fsck] fixed: inodes=");
    crate::vga::serial_u64(diff_inodes as u64);
    crate::vga::serial_write_atomic(" blocks=");
    crate::vga::serial_u64(diff_blocks as u64);
    if bad_ptrs > 0 {
        crate::vga::serial_write_atomic(" bad_ptrs=");
        crate::vga::serial_u64(bad_ptrs as u64);
    }
    crate::vga::serial_write_atomic("\n");
    true
}
