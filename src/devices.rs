// src/devices.rs
//
// Спецфайлы устройств (bigtodo 3.3): null, zero, tty, keyboard, vga.
//
// Устройство живёт на диске как inode TYPE_CHARDEV с полем device_id
// (1..=DEV_NAMES.len()). Данные через инод не ходят: cat()/echo > в VFS
// видят CHARDEV и перенаправляют вызов сюда по имени последней компоненты
// пути. Поэтому /dev можно хоть пересоздать - маршрутизация не меняется.
//
// install() идемпотентен: создаёт /dev и ноды только если их нет.

use alloc::vec;
use alloc::vec::Vec;
pub const DEV_NAMES: [&str; 5] = ["null", "zero", "tty", "keyboard", "vga"];

pub fn dev_id(name: &str) -> Option<u32> {
    DEV_NAMES
        .iter()
        .position(|&n| n == name)
        .map(|i| i as u32 + 1)
}

/// Чтение из устройства. None - устройство не читается (tty, vga).
pub fn dev_read(name: &str) -> Option<Vec<u8>> {
    match name {
        "null" => Some(Vec::new()),
        "zero" => Some(vec![0u8; 256]),
        "keyboard" => Some(crate::keyboard::drain_chars(256)),
        _ => None,
    }
}

/// Запись в устройство. false - устройство только для чтения.
pub fn dev_write(name: &str, data: &[u8]) -> bool {
    match name {
        "null" | "zero" => true,
        "tty" | "vga" => {
            // Печатаем с текущей позиции аппаратного курсора: writer шелла
            // недоступен отсюда, а CRTC всегда отражает реальное место вывода.
            let mut w = crate::vga::Writer::at_hw_cursor();
            for &b in data {
                if b == b'\n' || (0x20..0x7F).contains(&b) {
                    w.write_byte(b);
                }
            }
            true
        }
        _ => false,
    }
}

/// Создаёт /dev и спецфайлы, если их ещё нет. Вызывается один раз при загрузке.
pub fn install() {
    crate::scheduler::with_vfs(|vfs| {
        let _ = vfs.install_devices();
    });
}
