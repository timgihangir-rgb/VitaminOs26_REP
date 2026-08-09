// src/disk.rs
//
// Персистентность данных ОС на сыром диске-образе (.img) через PIO ATA.
// Диск подключается в QEMU как primary master (hda): `-drive file=os.img,format=raw,if=ide`.
//
// Формат образа:
//   - сектор 0   : суперблок (magic "VOS26IMG", version u32=1, len u32)
//   - сектора 1..: сериализованный слепок VFS (см. Vfs::serialize)
//
// Сектора, не содержащие данных, остаются нулями; отсутствие magic на секторе 0
// означает "пустой/новый диск".

use crate::vfs::Vfs;
use x86_64::instructions::port::Port;

const ATA_BASE: u16 = 0x1F0;
const ATA_STATUS: u16 = ATA_BASE + 7;
const ATA_CMD: u16 = ATA_BASE + 7;
const ATA_DRIVE: u16 = ATA_BASE + 6;
const ATA_SECCOUNT: u16 = ATA_BASE + 2;
const ATA_LBA_LOW: u16 = ATA_BASE + 3;
const ATA_LBA_MID: u16 = ATA_BASE + 4;
const ATA_LBA_HIGH: u16 = ATA_BASE + 5;

const SECTOR_SIZE: usize = 512;

const SUPER_MAGIC: &[u8; 8] = b"VOS26IMG";
const SUPER_VERSION: u32 = 1;
const SUPER_LEN_OFF: usize = 12;

const MAX_SNAPSHOT: usize = 128 * 1024;

static mut SECTOR_BUF: [u8; SECTOR_SIZE] = [0; SECTOR_SIZE];

static mut DISK_PRESENT: Option<bool> = None;

unsafe fn outb(port: u16, value: u8) {
    let mut p = Port::new(port);
    p.write(value);
}

unsafe fn inb(port: u16) -> u8 {
    let mut p = Port::new(port);
    p.read()
}

unsafe fn inw(port: u16) -> u16 {
    let mut p = Port::new(port);
    p.read()
}

unsafe fn outw(port: u16, value: u16) {
    let mut p = Port::new(port);
    p.write(value);
}

/// Ждёт, пока в регистре статуса появится/исчезнет бит `mask`.
/// Возвращает false при ошибке диска или по таймауту.
unsafe fn wait_status(mask: u8, set: bool) -> bool {
    for _ in 0..50_000_000 {
        let st = inb(ATA_STATUS);
        if st & 0x01 != 0 {
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

pub fn present() -> bool {
    unsafe {
        if let Some(p) = DISK_PRESENT {
            return p;
        }
        let p = probe();
        DISK_PRESENT = Some(p);
        p
    }
}

unsafe fn select_drive(lba: u32) {
    outb(ATA_DRIVE, 0xE0 | ((lba >> 24) & 0x0F) as u8);
}

unsafe fn read_sectors(lba: u32, count: u8, buf: &mut [u8]) -> Result<(), ()> {
    if buf.len() < count as usize * SECTOR_SIZE {
        return Err(());
    }
    select_drive(lba);
    outb(ATA_SECCOUNT, count);
    outb(ATA_LBA_LOW, lba as u8);
    outb(ATA_LBA_MID, (lba >> 8) as u8);
    outb(ATA_LBA_HIGH, (lba >> 16) as u8);
    outb(ATA_CMD, 0x20);
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

unsafe fn write_sectors(lba: u32, count: u8, buf: &[u8]) -> Result<(), ()> {
    if buf.len() < count as usize * SECTOR_SIZE {
        return Err(());
    }
    select_drive(lba);
    outb(ATA_SECCOUNT, count);
    outb(ATA_LBA_LOW, lba as u8);
    outb(ATA_LBA_MID, (lba >> 8) as u8);
    outb(ATA_LBA_HIGH, (lba >> 16) as u8);
    outb(ATA_CMD, 0x30);
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
    if wait_idle() {
        Ok(())
    } else {
        Err(())
    }
}

fn save_snapshot(data: &[u8]) -> Result<(), ()> {
    if data.len() > MAX_SNAPSHOT {
        return Err(());
    }
    unsafe {
        SECTOR_BUF[0..8].copy_from_slice(SUPER_MAGIC);
        SECTOR_BUF[8..12].copy_from_slice(&SUPER_VERSION.to_le_bytes());
        SECTOR_BUF[SUPER_LEN_OFF..SUPER_LEN_OFF + 4].copy_from_slice(&(data.len() as u32).to_le_bytes());        SECTOR_BUF[16..SECTOR_SIZE].fill(0);
        write_sectors(0, 1, &SECTOR_BUF)?;
    }
    let mut lba = 1u32;
    let mut off = 0usize;
    while off < data.len() {
        let chunk = core::cmp::min(SECTOR_SIZE, data.len() - off);
        unsafe {
            SECTOR_BUF[..chunk].copy_from_slice(&data[off..off + chunk]);
            SECTOR_BUF[chunk..SECTOR_SIZE].fill(0);
            write_sectors(lba, 1, &SECTOR_BUF)?;
        }
        lba += 1;
        off += SECTOR_SIZE;
    }
    Ok(())
}

fn load_snapshot() -> Option<alloc::vec::Vec<u8>> {
    if !present() {
        return None;
    }
    unsafe {
        if read_sectors(0, 1, &mut SECTOR_BUF).is_err() {
            return None;
        }
        if &SECTOR_BUF[0..8] != SUPER_MAGIC {
            return None;
        }
        let version = u32::from_le_bytes(SECTOR_BUF[8..12].try_into().ok()?);
        if version != SUPER_VERSION {
            return None;
        }
        let len = u32::from_le_bytes(SECTOR_BUF[SUPER_LEN_OFF..SUPER_LEN_OFF + 4].try_into().ok()?) as usize;
        if len == 0 || len > MAX_SNAPSHOT {
            return None;
        }
        let mut data = alloc::vec![0u8; len];
        let mut lba = 1u32;
        let mut off = 0usize;
        while off < len {
            if read_sectors(lba, 1, &mut SECTOR_BUF).is_err() {
                return None;
            }
            let chunk = core::cmp::min(SECTOR_SIZE, len - off);
            data[off..off + chunk].copy_from_slice(&SECTOR_BUF[..chunk]);
            lba += 1;
            off += SECTOR_SIZE;
        }
        Some(data)
    }
}

/// Сохраняет слепок VFS на диск. Без диска — no-op, возвращает false.
pub fn flush(vfs: &Vfs) -> bool {
    if !present() {
        return false;
    }
    let data = vfs.serialize();
    save_snapshot(&data).is_ok()
}

/// Пытается загрузить слепок VFS с диска. Возвращает None, если диска нет
/// или данные не валидны.
pub fn load() -> Option<alloc::vec::Vec<u8>> {
    load_snapshot()
}
