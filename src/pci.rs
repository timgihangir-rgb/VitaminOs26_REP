//! Обследование PCI Configuration Space через порты 0xCF8/0xCFC.
//!
//! Нужен минимальный объём: найти Realtek rtl8139 (vendor 0x10EC,
//! device 0x8139) и прочитать её BAR0 (I/O base), по которому дальше
//! работает драйвер в src/net.rs.

use x86_64::instructions::port::Port;

const CONFIG_ADDRESS: u16 = 0xCF8;
const CONFIG_DATA: u16 = 0xCFC;

/// Формирует адрес конфигурационного регистра (шина 0, слот, функция, смещение).
fn config_addr(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((slot as u32) << 11)
        | ((func as u32) << 8)
        | ((offset as u32) & 0xFC)
}

/// Читает 32-битный регистр конфигурации.
pub fn read_u32(bus: u8, slot: u8, func: u8, offset: u8) -> u32 {
    let mut addr = Port::new(CONFIG_ADDRESS);
    let mut data = Port::new(CONFIG_DATA);
    unsafe {
        addr.write(config_addr(bus, slot, func, offset));
        data.read()
    }
}

/// Двухбайтовая часть регистра.
pub fn read_u16(bus: u8, slot: u8, func: u8, offset: u8) -> u16 {
    (read_u32(bus, slot, func, offset) >> (((offset as u32) & 2) * 8)) as u16
}

/// Меняет двухбайтовую часть регистра (read-modify-write внутри dword).
pub fn write_u16(bus: u8, slot: u8, func: u8, offset: u8, val: u16) {
    let shift = ((offset as u32) & 2) * 8;
    let mut dw = read_u32(bus, slot, func, offset & !3);
    dw &= !(0xFFFFu32 << shift);
    dw |= (val as u32) << shift;
    let mut addr = Port::new(CONFIG_ADDRESS);
    let mut data = Port::new(CONFIG_DATA);
    unsafe {
        addr.write(config_addr(bus, slot, func, offset & !3));
        data.write(dw);
    }
}

/// Однобайтовая часть регистра.
pub fn read_u8(bus: u8, slot: u8, func: u8, offset: u8) -> u8 {
    (read_u32(bus, slot, func, offset) >> (((offset as u32) & 3) * 8)) as u8
}

/// Найденное устройство (основные поля для драйвера).
#[derive(Clone, Copy)]
pub struct PciDev {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub bar0: u32,
}

/// Ищет устройство по vendor/device на шине 0.
pub fn find_device(vendor: u16, device: u16) -> Option<PciDev> {
    for slot in 0..32u8 {
        let v = read_u16(0, slot, 0, 0);
        if v == 0xFFFF {
            continue; // пустой слот
        }
        let d = read_u16(0, slot, 0, 2);
        if v == vendor && d == device {
            return Some(PciDev {
                bus: 0,
                slot,
                func: 0,
                vendor: v,
                device: d,
                class: read_u8(0, slot, 0, 0x0B),
                subclass: read_u8(0, slot, 0, 0x0A),
                bar0: read_u32(0, slot, 0, 0x10),
            });
        }
    }
    None
}

/// SATA/AHCI-контроллер, найденный на шине.
#[derive(Clone, Copy)]
pub struct SataCtrl {
    pub bus: u8,
    pub slot: u8,
    pub func: u8,
    /// Физический базовый адрес регистров (BAR5, 64-битный memory BAR).
    pub abar: u64,
}

/// Ищет AHCI/SATA-контроллер (класс 0x01, подкласс 0x06) на шине 0.
///
/// В отличие от find_device сканируются все функции слота, а не только нулевая:
/// у Intel ICH9 контроллер живёт в 00:1f.2. BAR5 - это memory-BAR, и его адрес
/// берётся прямо из конфигурационного пространства: контроллер настроен BIOS,
/// но MMIO-окно в ядро мы маппим сами (см. paging::map_phys_in_kernel).
pub fn find_sata() -> Option<SataCtrl> {
    for slot in 0..32u8 {
        let vendor0 = read_u16(0, slot, 0, 0);
        if vendor0 == 0xFFFF {
            continue; // пустой слот
        }
        let multifunction = read_u8(0, slot, 0, 0x0E) & 0x80 != 0;
        let funcs = if multifunction { 8 } else { 1 };
        for func in 0..funcs {
            let vendor = if func == 0 { vendor0 } else { read_u16(0, slot, func, 0) };
            if vendor == 0xFFFF {
                continue;
            }
            if read_u8(0, slot, func, 0x0B) != 0x01 || read_u8(0, slot, func, 0x0A) != 0x06 {
                continue;
            }
            let bar5 = read_u32(0, slot, func, 0x24);
            if bar5 & 1 != 0 {
                continue; // I/O-BAR: у AHCI BAR5 всегда memory
            }
            // BAR5 64-битный: младшие два бита 10 означают "64-битный memory BAR",
            // старшая половина лежит в BAR6.
            let mut abar = (bar5 & !0x0F) as u64;
            if bar5 & 0x6 == 0x2 {
                abar |= (read_u32(0, slot, func, 0x28) as u64) << 32;
            }
            if abar == 0 {
                continue; // BAR не назначен BIOS
            }
            return Some(SataCtrl { bus: 0, slot, func, abar });
        }
    }
    None
}