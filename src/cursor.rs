//! Аппаратный курсор VGA (CRTC 0x3D4/0x3D5).
//!
//! Позиция курсора — это word-офсет в текстовом буфере: row*80+col.
//! Writer обновляет её при каждом изменении позиции, а планировщик
//! сохраняет/восстанавливает при переключении задач (поле `cursor_pos`
//! в TCB), чтобы курсор не «убегал» между шеллом и программами.

use x86_64::instructions::port::Port;

const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;

/// Подчёркивание в нижних 3 сканлинах клетки 8x16 — как в обычном шелле.
pub const UNDERLINE: (u8, u8) = (13, 15);

pub fn set_position(row: usize, col: usize) {
    let pos = ((row as u16) * crate::vga::SCREEN_WIDTH as u16 + col as u16) as u16;
    set_raw(pos);
}

pub fn set_raw(pos: u16) {
    unsafe {
        let mut idx = Port::new(CRTC_INDEX);
        let mut data = Port::new(CRTC_DATA);
        idx.write(0x0Fu8);
        data.write((pos & 0xFF) as u8);
        idx.write(0x0Eu8);
        data.write((pos >> 8) as u8);
    }
}

pub fn get_position() -> u16 {
    unsafe {
        let mut idx: Port<u8> = Port::new(CRTC_INDEX);
        let mut data: Port<u8> = Port::new(CRTC_DATA);
        idx.write(0x0Fu8);
        let lo = data.read() as u16;
        idx.write(0x0Eu8);
        let hi = data.read() as u16;
        (hi << 8) | lo
    }
}

/// Форма курсора: CRTC 0x0A — первая сканлиня, 0x0B — последняя.
///
/// Без явной установки этих регистров курсор остаётся в том состоянии,
/// в котором его оставил GRUB. QEMU рисует курсор только при
/// `!(cr[0x0A] & 0x20) && (cr[0x0A] & 0x1F) <= (cr[0x0B] & 0x1F)`, а бит 5
/// в 0x0A — это «курсор выключен». GRUB его ставит, и курсор не виден нигде:
/// ни в промпте шелла, ни в vita. Поэтому форму программируем явно при
/// каждой установке текстового режима.
pub fn set_shape(start: u8, end: u8) {
    unsafe {
        let mut idx: Port<u8> = Port::new(CRTC_INDEX);
        let mut data: Port<u8> = Port::new(CRTC_DATA);
        // бит 5 в 0x0A — «курсор выключен», поэтому пишем его нулевым
        idx.write(0x0Au8);
        data.write(start & 0x1F);
        idx.write(0x0Bu8);
        data.write(end & 0x1F);
    }
}
