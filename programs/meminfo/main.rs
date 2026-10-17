#![no_std]
#![no_main]

//! meminfo: память (всего/занято/свободно) и число задач из SysInfo@0x5000.
//! Программа не делает сисколов — читает блок, который ядро заполняет перед
//! запуском любой ELF-программы (см. sysinfo::SysInfo::write_to_memory).

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

const SYSINFO: *const SysInfo = 0x5000 as *const SysInfo;
const EXIT_ROW: *mut i32 = 0x708C as *mut i32;
const EXIT_COL: *mut i32 = 0x7090 as *mut i32;

const WHITE: u8 = 0x0F;
const CYAN: u8 = 0x0B;
const GREEN: u8 = 0x0A;

/// Та же раскладка, что в src/sysinfo.rs (поля в конце добавлены позже).
#[repr(C, packed)]
struct SysInfo {
    total_kb: u32,
    vga_offset: u32,
    cpu_family: u32,
    cpu_model: u32,
    cpu_stepping: u32,
    cpu_flags: u8,
    _pad: [u8; 3],
    cpu_vendor: [u8; 12],
    cpu_brand: [u8; 48],
    os_name: [u8; 16],
    os_version: [u8; 8],
    kernel_version: [u8; 8],
    shell_name: [u8; 16],
    shell_version: [u8; 8],
    bootloader_version: [u8; 16],
    resolution: [u8; 16],
    terminal: [u8; 16],
    used_kb: u32,
    proc_count: u32,
}

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

    let info = unsafe { core::ptr::read_volatile(SYSINFO) };

    let start_row = vga_offset / (screen_w() * 2);
    let mut s = Screen {
        row: start_row,
        col: 0,
    };

    // Заголовок.
    write_bytes(&mut s, b"Memory", GREEN);
    write_bytes(&mut s, b"  (total / used / free)", CYAN);

    s.row = s.row.saturating_add(1).min(screen_h() - 1);
    s.col = 0;
    let total_mb = to_mib(info.total_kb);
    let used_mb = to_mib(info.used_kb);
    let free_mb = to_mib(info.total_kb.saturating_sub(info.used_kb));
    write_bytes(&mut s, b"  ", WHITE);
    write_u32(&mut s, total_mb, WHITE);
    write_bytes(&mut s, b" MiB / ", WHITE);
    write_u32(&mut s, used_mb, WHITE);
    write_bytes(&mut s, b" MiB / ", WHITE);
    write_u32(&mut s, free_mb, WHITE);
    write_bytes(&mut s, b" MiB", WHITE);

    s.row = s.row.saturating_add(1).min(screen_h() - 1);
    s.col = 0;
    write_bytes(&mut s, b"Tasks: ", CYAN);
    write_u32(&mut s, info.proc_count, WHITE);

    s.row = s.row.saturating_add(1).min(screen_h() - 1);
    s.col = 0;
    write_bytes(&mut s, b"CPU: ", CYAN);
    write_bytes(&mut s, trimmed(&info.cpu_vendor), WHITE);
    write_bytes(&mut s, b" (family ", WHITE);
    write_u32(&mut s, info.cpu_family, WHITE);
    write_bytes(&mut s, b" model ", WHITE);
    write_u32(&mut s, info.cpu_model, WHITE);
    write_bytes(&mut s, b")", WHITE);

    s.row = s.row.saturating_add(1).min(screen_h() - 1);
    s.col = 0;
    let os = join_str(&info.os_name, &info.os_version);
    write_bytes(&mut s, b"OS: ", CYAN);
    write_bytes(&mut s, &os, WHITE);

    unsafe {
        core::ptr::write_volatile(EXIT_ROW, s.row as i32);
        core::ptr::write_volatile(EXIT_COL, s.col as i32);
    }
}

fn to_mib(kb: u32) -> u32 {
    (kb + 512) / 1024
}

fn trimmed(buf: &[u8]) -> &[u8] {
    let mut end = buf.len();
    while end > 0 && (buf[end - 1] == 0 || buf[end - 1] == b' ') {
        end -= 1;
    }
    &buf[..end]
}

/// Две C-строки через пробел (макс. 40 байт).
fn join_str(a: &[u8], b: &[u8]) -> [u8; 40] {
    let a = trimmed(a);
    let b = trimmed(b);
    let mut out = [0u8; 40];
    let mut pos = 0;
    for &ch in a {
        if pos < 39 {
            out[pos] = ch;
            pos += 1;
        }
    }
    if pos > 0 && pos < 39 && !b.is_empty() {
        out[pos] = b' ';
        pos += 1;
    }
    for &ch in b {
        if pos < 39 {
            out[pos] = ch;
            pos += 1;
        }
    }
    out
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

fn write_u32(s: &mut Screen, mut n: u32, attr: u8) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if n == 0 {
        put(s, b'0', attr);
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write_bytes(s, &buf[i..], attr);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}