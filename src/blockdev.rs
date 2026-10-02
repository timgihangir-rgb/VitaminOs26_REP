// src/blockdev.rs
//
// Блочный слой: логический блок 4 КиБ = 8 секторов по 512. Здесь живут
// низкоуровневые порт-примитивы ATA PIO (переехали из disk.rs), диспетчер
// между бэкендами, публичный API read_block/write_block и счётчики статистики.
//
// Бэкенд выбирается один раз при загрузке (см. init): если на шине есть
// AHCI/SATA-контроллер с диском, работает он (DMA, LBA48), иначе - legacy
// ATA PIO через порты 0x1F0. Наружу всегда смотрит один и тот же API.
//
// Реентерабельность: PIO-последовательности выполняются с запрещёнными
// маскируемыми прерываниями, чтобы тик таймера не переключил задачу посреди
// программирования контроллера (иначе порты перемешаются между задачами).
// AHCI-путь полагается на то же самое: его вызывают из-под without_interrupts
// (bcache, vitafs), а на случай прямого вызова у драйвера есть спин-лок.
// Ожидание у обоих бэкендов ограничено по итерациям.

use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;

pub const SECTOR_SIZE: usize = 512;
pub const BLOCK_SIZE: usize = 4096;
pub const SECTORS_PER_BLOCK: u32 = (BLOCK_SIZE / SECTOR_SIZE) as u32;

const ATA_BASE: u16 = 0x1F0;
const ATA_STATUS: u16 = ATA_BASE + 7;
const ATA_CMD: u16 = ATA_BASE + 7;
const ATA_DRIVE: u16 = ATA_BASE + 6;
const ATA_SECCOUNT: u16 = ATA_BASE + 2;
const ATA_LBA_LOW: u16 = ATA_BASE + 3;
const ATA_LBA_MID: u16 = ATA_BASE + 4;
const ATA_LBA_HIGH: u16 = ATA_BASE + 5;
const ATA_CONTROL: u16 = 0x3F6;

const ATA_CMD_READ_PIO: u8 = 0x20;
const ATA_CMD_WRITE_PIO: u8 = 0x30;
const ATA_CMD_CACHE_FLUSH: u8 = 0xE7;
const ATA_CMD_IDENTIFY: u8 = 0xEC;

/// Ёмкость диска, считанная через IDENTIFY (в 4-КиБ блоках); None - не снята.
static mut CAPACITY_BLOCKS: Option<u32> = None;

/// Статистика блочного устройства. Счётчики монотонные, насыщаются.
pub struct BlockStats {
    pub reads: u64,
    pub writes: u64,
    pub errors: u64,
}

static mut STATS: BlockStats = BlockStats {
    reads: 0,
    writes: 0,
    errors: 0,
};

pub fn stats() -> BlockStats {
    unsafe {
        BlockStats {
            reads: core::ptr::read_volatile(&STATS.reads),
            writes: core::ptr::read_volatile(&STATS.writes),
            errors: core::ptr::read_volatile(&STATS.errors),
        }
    }
}

fn bump_errors() {
    unsafe {
        let p = &mut STATS.errors as *mut u64;
        let v = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, v.saturating_add(1));
    }
}

unsafe fn outb(port: u16, value: u8) {
    let mut p = Port::new(port);
    p.write(value);
}

unsafe fn inb(port: u16) -> u8 {
    let mut p = Port::new(port);
    p.read()
}

unsafe fn inw(port: u16) -> u16 {
    let mut p: Port<u16> = Port::new(port);
    p.read()
}

unsafe fn outw(port: u16, value: u16) {
    let mut p: Port<u16> = Port::new(port);
    p.write(value);
}

/// Ждёт, пока в регистре статуса появится/исчезнет бит `mask`.
/// Возвращает false при ошибке диска (ERR/DF) или по таймауту.
unsafe fn wait_status(mask: u8, set: bool) -> bool {
    for _ in 0..50_000_000 {
        let st = inb(ATA_STATUS);
        if st & 0x01 != 0 || st & 0x20 != 0 {
            return false;
        }
        if st == 0 {
            // drives gone
            return false;
        }
        if (st & mask != 0) == set {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

unsafe fn wait_idle() -> bool {
    wait_status(0x80, false)
}

unsafe fn wait_drq() -> bool {
    wait_status(0x08, true)
}

/// Проба наличия диска: выбираем primary master и читаем статус.
/// 0xFF на шине означает "устройства нет".
fn probe() -> bool {
    unsafe {
        outb(ATA_DRIVE, 0xA0);
        let status = inb(ATA_STATUS);
        status != 0xFF
    }
}

/// Бэкенд блочного доступа. До загрузки (если init() не вызывали) считаем,
/// что работает legacy PIO, и тогда диск ищется лениво пробой порта.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Ahci,
    AtaPio,
}

static mut DISK_PRESENT: Option<bool> = None;
static mut BACKEND: Backend = Backend::AtaPio;

/// Ищет AHCI/SATA-контроллер и поднимает порт. Возвращает выбранный бэкенд.
/// Вызывается один раз при загрузке до первого обращения к ФС.
pub fn init() -> Backend {
    let backend = if crate::ahci::init() {
        Backend::Ahci
    } else {
        Backend::AtaPio
    };
    unsafe {
        BACKEND = backend;
        if backend == Backend::AtaPio {
            // Диск на портах 0x1F0 проверяем сразу: у AHCI такой пробы нет.
            DISK_PRESENT = Some(probe());
        }
    }
    backend
}

/// Активный бэкенд.
pub fn backend() -> Backend {
    unsafe { BACKEND }
}

/// Короткое имя бэкенда для диагностики.
pub fn backend_name() -> &'static str {
    match backend() {
        Backend::Ahci => "ahci",
        Backend::AtaPio => "ata-pio",
    }
}

pub fn present() -> bool {
    unsafe {
        match BACKEND {
            Backend::Ahci => return crate::ahci::present(),
            Backend::AtaPio => {}
        }
        if let Some(p) = DISK_PRESENT {
            return p;
        }
        let p = probe();
        DISK_PRESENT = Some(p);
        p
    }
}

/// ATA IDENTIFY DEVICE: число секторов LBA28 (words 60-61).
/// None при отсутствии ответа или ошибке шины.
fn ata_identify_sectors() -> Option<u32> {
    unsafe {
        outb(ATA_DRIVE, 0xA0); // primary master, CHS-режим
        outb(ATA_SECCOUNT, 0);
        outb(ATA_LBA_LOW, 0);
        outb(ATA_LBA_MID, 0);
        outb(ATA_LBA_HIGH, 0);
        outb(ATA_CMD, ATA_CMD_IDENTIFY);
        if !wait_drq() {
            return None;
        }
        let mut data = [0u16; 256];
        for w in data.iter_mut() {
            *w = inw(ATA_BASE);
        }
        Some((data[60] as u32) | ((data[61] as u32) << 16))
    }
}

/// Число 4-КиБ блоков на устройстве (ёмкость). Кэшируется после первого
/// вызова. Используется для вывода геометрии ФС из реального размера диска.
pub fn capacity_blocks() -> Option<u32> {
    unsafe {
        if let Some(c) = CAPACITY_BLOCKS {
            return Some(c);
        }
        let blocks = match BACKEND {
            Backend::Ahci => crate::ahci::capacity_sectors()? / SECTORS_PER_BLOCK as u64,
            Backend::AtaPio => (ata_identify_sectors()? / SECTORS_PER_BLOCK) as u64,
        };
        // Наружу отдаём u32: столько 4-КиБ блоков всё равно не адресуемо
        // указателями в текущих потребителях.
        let blocks = core::cmp::min(blocks, u32::MAX as u64) as u32;
        CAPACITY_BLOCKS = Some(blocks);
        Some(blocks)
    }
}

/// Читает `count` секторов начиная с LBA в buf.
/// Вызывается только изнутри без прерываний (см. without_interrupts ниже).
unsafe fn read_sectors_raw(lba: u32, count: u8, buf: &mut [u8]) -> Result<(), ()> {
    if buf.len() < count as usize * SECTOR_SIZE {
        return Err(());
    }
    select_drive(lba);
    outb(ATA_SECCOUNT, count);
    outb(ATA_LBA_LOW, lba as u8);
    outb(ATA_LBA_MID, (lba >> 8) as u8);
    outb(ATA_LBA_HIGH, (lba >> 16) as u8);
    outb(ATA_CMD, ATA_CMD_READ_PIO);
    for s in 0..count as usize {
        if !wait_drq() {
            return Err(());
        }
        for i in 0..256 {
            let w = inw(ATA_BASE);
            let b = s * SECTOR_SIZE + i * 2;
            buf[b] = w as u8;
            buf[b + 1] = (w >> 8) as u8;
        }
    }
    Ok(())
}

/// Пишет `count` секторов из buf, затем ждёт завершения и сбрасывает кэш диска.
unsafe fn write_sectors_raw(lba: u32, count: u8, buf: &[u8]) -> Result<(), ()> {
    if buf.len() < count as usize * SECTOR_SIZE {
        return Err(());
    }
    select_drive(lba);
    outb(ATA_SECCOUNT, count);
    outb(ATA_LBA_LOW, lba as u8);
    outb(ATA_LBA_MID, (lba >> 8) as u8);
    outb(ATA_LBA_HIGH, (lba >> 16) as u8);
    outb(ATA_CMD, ATA_CMD_WRITE_PIO);
    for s in 0..count as usize {
        if !wait_drq() {
            return Err(());
        }
        for i in 0..256 {
            let b = s * SECTOR_SIZE + i * 2;
            let w = (buf[b] as u16) | ((buf[b + 1] as u16) << 8);
            outw(ATA_BASE, w);
        }
    }
    if !wait_idle() {
        return Err(());
    }
    // Сброс внутреннего кэша диска, чтобы данные реально легли на носитель.
    outb(ATA_DRIVE, 0xE0);
    outb(ATA_CMD, ATA_CMD_CACHE_FLUSH);
    if !wait_idle() {
        return Err(());
    }
    Ok(())
}

unsafe fn select_drive(lba: u32) {
    outb(ATA_DRIVE, 0xE0 | ((lba >> 24) & 0x0F) as u8);
}

/// Верхняя граница сектора для выбранного бэкенда. У AHCI адресация LBA48,
/// у PIO - только 28 бит.
fn last_sector() -> u64 {
    match backend() {
        Backend::Ahci => u64::MAX,
        Backend::AtaPio => u32::MAX as u64,
    }
}

/// Проверка адреса блока: не выходит ли он за адресуемое блочное устройство.
fn block_ok(block_no: u64) -> bool {
    let sector = match block_no.checked_mul(SECTORS_PER_BLOCK as u64) {
        Some(s) => s,
        None => return false,
    };
    sector <= last_sector()
}

/// Читает `count` секторов начиная с `sector` выбранным бэкендом.
fn dispatch_read(sector: u64, count: u8, buf: &mut [u8]) -> Result<(), ()> {
    if sector > last_sector() {
        return Err(());
    }
    match backend() {
        Backend::Ahci => crate::ahci::read_sectors(sector, count as u16, buf),
        Backend::AtaPio => interrupts::without_interrupts(|| unsafe {
            read_sectors_raw(sector as u32, count, buf)
        }),
    }
}

/// Пишет `count` секторов из buf выбранным бэкендом.
fn dispatch_write(sector: u64, count: u8, buf: &[u8]) -> Result<(), ()> {
    if sector > last_sector() {
        return Err(());
    }
    match backend() {
        Backend::Ahci => crate::ahci::write_sectors(sector, count as u16, buf),
        Backend::AtaPio => interrupts::without_interrupts(|| unsafe {
            write_sectors_raw(sector as u32, count, buf)
        }),
    }
}

/// Читает один 4-КиБ блок. lba - номер блока (не сектора).
pub fn read_block(block_no: u64, buf: &mut [u8; BLOCK_SIZE]) -> Result<(), ()> {
    if !present() || !block_ok(block_no) {
        bump_errors();
        return Err(());
    }
    let ok = dispatch_read(
        block_no * SECTORS_PER_BLOCK as u64,
        SECTORS_PER_BLOCK as u8,
        buf,
    );
    unsafe {
        let p = &mut STATS.reads as *mut u64;
        let v = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, v.saturating_add(1));
    }
    if ok.is_err() {
        bump_errors();
    }
    ok
}

/// Чтение произвольного числа секторов - для легаси-путей (слепок VFS).
pub fn read_sectors_pub(lba: u32, count: u8, buf: &mut [u8]) -> Result<(), ()> {
    if !present() {
        bump_errors();
        return Err(());
    }
    let ok = dispatch_read(lba as u64, count, buf);
    unsafe {
        let p = &mut STATS.reads as *mut u64;
        let v = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, v.saturating_add(1));
    }
    if ok.is_err() {
        bump_errors();
    }
    ok
}

/// Запись произвольного числа секторов - для легаси-путей (слепок VFS).
pub fn write_sectors_pub(lba: u32, count: u8, buf: &[u8]) -> Result<(), ()> {
    if !present() {
        bump_errors();
        return Err(());
    }
    let ok = dispatch_write(lba as u64, count, buf);
    unsafe {
        let p = &mut STATS.writes as *mut u64;
        let v = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, v.saturating_add(1));
    }
    if ok.is_err() {
        bump_errors();
    }
    ok
}

/// Пишет один 4-КиБ блок.
pub fn write_block(block_no: u64, buf: &[u8; BLOCK_SIZE]) -> Result<(), ()> {
    if !present() || !block_ok(block_no) {
        bump_errors();
        return Err(());
    }
    let ok = dispatch_write(
        block_no * SECTORS_PER_BLOCK as u64,
        SECTORS_PER_BLOCK as u8,
        buf,
    );
    unsafe {
        let p = &mut STATS.writes as *mut u64;
        let v = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, v.saturating_add(1));
    }
    if ok.is_err() {
        bump_errors();
    }
    ok
}

/// Программный сброс контроллера (используется при ошибках).
#[allow(dead_code)]
pub fn reset_controller() {
    interrupts::without_interrupts(|| unsafe {
        outb(ATA_CONTROL, 0x04); // SRST
        outb(ATA_CONTROL, 0x00);
        wait_idle();
    });
}

/// Самопроверка блочного слоя: roundtrip тестовый блок в конце образа.
/// Тестовый блок выводится из реальной ёмкости, но не выше блока 2047
/// (8-МиБ лимита) — на больших образах не трогаем хвост, на малых не
/// выходим за пределы. Слепок VFS живёт в первых секторах после
/// суперблока, так что низ диска не задеваем.
pub fn selftest() -> bool {
    let probe_block: u64 = capacity_blocks()
        .map(|c| core::cmp::max(1, core::cmp::min(c, 2048) - 1) as u64)
        .unwrap_or(2047);
    let mut w = [0u8; BLOCK_SIZE];
    let mut r = [0u8; BLOCK_SIZE];
    for (i, b) in w.iter_mut().enumerate() {
        *b = (i as u8) ^ (probe_block as u8);
    }
    if write_block(probe_block, &w).is_err() {
        return false;
    }
    if read_block(probe_block, &mut r).is_err() {
        return false;
    }
    if r != w {
        return false;
    }
    // Восстанавливаем нули, чтобы не пачкать образ мусором.
    let zeros = [0u8; BLOCK_SIZE];
    write_block(probe_block, &zeros).is_ok()
}
