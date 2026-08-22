//! Аппаратный курсор VGA (CRTC 0x3D4/0x3D5).
//!
//! Позиция курсора — это word-офсет в текстовом буфере: row*80+col.
//! Writer обновляет её при каждом изменении позиции, а планировщик
//! сохраняет/восстанавливает при переключении задач (поле `cursor_pos`
//! в TCB), чтобы курсор не «убегал» между шеллом и программами.

use x86_64::instructions::port::Port;

const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;

/// Значение «позиция не установлена» (невалидно: максимум 80*30-1 = 2399).
pub const NO_POSITION: u16 = u16::MAX;

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