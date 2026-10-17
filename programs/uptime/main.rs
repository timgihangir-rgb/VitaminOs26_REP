#![no_std]
#![no_main]

//! uptime: время работы системы из тиков PIT (100 Гц) в формате HH:MM:SS.
//! Читает сискол ticks() через ABI-трамплин (progabi), пишет прямо в VGA.

const VGA: *mut u8 = 0xB8000 as *mut u8;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Сетка в ячейках, объявленная ядром (см. `progabi::publish_grid` и его
/// аналог на C в programs/common/vgasize.h). Композитор выбирает сетку под
/// разрешение монитора, и программа, зашитая на 80x30, на 120x33 писала бы
/// мимо экрана. Пока слоты пусты (ядро не включило композитор) -
/// исторические 80x30.
///
/// Держим в атомиках, а не в `static mut`: значение читается отовсюду, и
/// безопасный доступ к нему избавляет от `unsafe` на каждой строке.
static SCREEN_W: AtomicUsize = AtomicUsize::new(80);
static SCREEN_H: AtomicUsize = AtomicUsize::new(30);

fn screen_w() -> usize {
    SCREEN_W.load(Ordering::Relaxed)
}

fn screen_h() -> usize {
    SCREEN_H.load(Ordering::Relaxed)
}

/// Читает сетку один раз в начале `_start`. Границы отсекают мусор в слотах
/// и страхуют от выхода за пределы тени экрана.
fn read_grid() -> (usize, usize) {
    let c = unsafe { (0x78A0usize as *const u32).read_volatile() } as usize;
    let r = unsafe { (0x78A4usize as *const u32).read_volatile() } as usize;
    if (16..=1024).contains(&c) && (4..=512).contains(&r) {
        (c, r)
    } else {
        (80, 30)
    }
}

const TICKS_PTR: *const fn() -> u64 = 0x7800 as *const fn() -> u64;
const EXIT_ROW: *mut i32 = 0x708C as *mut i32;
const EXIT_COL: *mut i32 = 0x7090 as *mut i32;

const WHITE: u8 = 0x0F;
const CYAN: u8 = 0x0B;
const GREEN: u8 = 0x0A;

struct Screen {
    row: usize,
    col: usize,
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(_argc: usize, _argv: *const *const u8, vga_offset: usize) {
    // Сетку читаем до любого обращения к VGA: с неё считаются и стартовая
    // строка (vga_offset - это смещение в байтах от начала текста), и все
    // границы экрана дальше.
    let (gw, gh) = read_grid();
    SCREEN_W.store(gw, Ordering::Relaxed);
    SCREEN_H.store(gh, Ordering::Relaxed);

    let ticks: u64 = unsafe { (*TICKS_PTR)() };

    let start_row = vga_offset / (screen_w() * 2);
    let mut s = Screen {
        row: start_row,
        col: 0,
    };

    let secs = ticks / 100; // PIT 100 Гц -> секунды
    let hours = (secs / 3600) % 24;
    let mins = (secs / 60) % 60;
    let secs = secs % 60;

    write_bytes(&mut s, b"Uptime: ", CYAN);
    write_u64(&mut s, hours, 2, WHITE);
    put(&mut s, b':', WHITE);
    write_u64(&mut s, mins, 2, WHITE);
    put(&mut s, b':', WHITE);
    write_u64(&mut s, secs, 2, WHITE);

    s.row = s.row.saturating_add(1).min(screen_h() - 1);
    s.col = 0;
    write_bytes(&mut s, b"Ticks:  ", CYAN);
    write_u64(&mut s, ticks, 0, WHITE);

    unsafe {
        // Курсор шелла — на последнюю строку вывода.
        core::ptr::write_volatile(EXIT_ROW, s.row as i32);
        core::ptr::write_volatile(EXIT_COL, s.col as i32);
    }
}

impl Screen {
    fn set_cursor(&mut self, row: usize, col: usize) {
        self.row = row.min(screen_h() - 1);
        self.col = col.min(screen_w() - 1);
    }
}

fn put(s: &mut Screen, ch: u8, attr: u8) {
    unsafe {
        let off = (s.row * screen_w() + s.col) * 2;
        if off + 1 < screen_w() * screen_h() * 2 {
            core::ptr::write_volatile(VGA.add(off), ch);
            core::ptr::write_volatile(VGA.add(off + 1), attr);
        }
    }
    s.col += 1;
    if s.col >= screen_w() {
        s.col = 0;
        s.row = s.row.saturating_add(1).min(screen_h() - 1);
    }
}

fn write_bytes(s: &mut Screen, b: &[u8], attr: u8) {
    for &ch in b {
        put(s, ch, attr);
    }
}

/// Печатает число с дополнением нулями до ширины `width` (0 = без дополнения).
fn write_u64(s: &mut Screen, mut n: u64, width: usize, attr: u8) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    let len = buf.len() - i;
    if len < width {
        for _ in 0..(width - len) {
            put(s, b'0', attr);
        }
    }
    write_bytes(s, &buf[i..], attr);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}