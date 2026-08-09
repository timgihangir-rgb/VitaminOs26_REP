#![no_std]
#![no_main]

use core::arch::x86_64::{__cpuid, __cpuid_count};

const VGA: *mut u8 = 0xB8000 as *mut u8;
const SCREEN_WIDTH: usize = 80;
const SCREEN_HEIGHT: usize = 30;
const LABEL_COL: usize = 18;

const COLOR_GREEN: u8 = 0x0A;
const COLOR_CYAN: u8 = 0x0B;
const COLOR_WHITE: u8 = 0x0F;

const EXIT_ROW: *mut i32 = 0x708C as *mut i32;
const EXIT_COL: *mut i32 = 0x7090 as *mut i32;

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
}

struct Screen {
    row: usize,
    col: usize,
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(vga_offset: usize) {
    let info = unsafe { core::ptr::read_volatile(0x5000 as *const SysInfo) };
    let mut start_row = vga_offset / (SCREEN_WIDTH * 2);
    // Продолжение tty: если блок вывода fetch (13 строк) не помещается от
    // текущей строки, прокручиваем экран вверх на недостающие строки, чтобы
    // весь вывод был виден целиком (как это делают обычные команды).
    const BLOCK: usize = 13;
    if start_row + BLOCK > SCREEN_HEIGHT {
        let n = start_row + BLOCK - SCREEN_HEIGHT;
        scroll_up(n);
        start_row -= n;
    }
    let mut s = Screen {
        row: start_row,
        col: 0,
    };

    let art: [&[u8]; 11] = [
        b"    .---------.",
        b"   /           \\",
        b"  | V       V V |",
        b"  | V       V V |",
        b"  |  V     V V  |",
        b"  |   V   V V   |",
        b"  |    V V V    |",
        b"  |     V V     |",
        b"  |      v      |",
        b"   \\           /",
        b"    `---------'",
    ];

    for i in 0..art.len() {
        s.set_cursor(start_row + i, 0);
        write_bytes(&mut s, art[i], COLOR_GREEN);
    }

    let os = join2(trimmed(&info.os_name), trimmed(&info.os_version));
    s.set_cursor(start_row, LABEL_COL);
    write_bytes(&mut s, &os, COLOR_WHITE);

    s.set_cursor(start_row + 1, LABEL_COL);
    write_bytes(&mut s, b"OS: ", COLOR_CYAN);
    write_bytes(&mut s, &os, COLOR_WHITE);

    s.set_cursor(start_row + 2, LABEL_COL);
    write_bytes(&mut s, b"Kernel: ", COLOR_CYAN);
    write_bytes(&mut s, trimmed(&info.kernel_version), COLOR_WHITE);

    let host = trimmed(&info.cpu_brand);
    let host = if host.is_empty() { b"Unknown x86_64" } else { host };
    s.set_cursor(start_row + 3, LABEL_COL);
    write_bytes(&mut s, b"Host: ", COLOR_CYAN);
    write_bytes(&mut s, host, COLOR_WHITE);

    s.set_cursor(start_row + 4, LABEL_COL);
    write_bytes(&mut s, b"CPU: ", COLOR_CYAN);
    write_bytes(&mut s, trimmed(&info.cpu_vendor), COLOR_WHITE);
    write_bytes(&mut s, b" (Family ", COLOR_WHITE);
    write_u32(&mut s, info.cpu_family, COLOR_WHITE);
    write_bytes(&mut s, b" Model ", COLOR_WHITE);
    write_u32(&mut s, info.cpu_model, COLOR_WHITE);
    write_bytes(&mut s, b" Stepping ", COLOR_WHITE);
    write_u32(&mut s, info.cpu_stepping, COLOR_WHITE);
    put(&mut s, b')', COLOR_WHITE);

    let cpuid1 = unsafe { __cpuid(1) };
    let threads = ((cpuid1.ebx >> 16) & 0xFF) as u32;
    let max_leaf = unsafe { __cpuid(0) }.eax;
    let cores = if max_leaf >= 4 {
        let c = unsafe { __cpuid_count(4, 0) };
        ((c.eax >> 26) & 0x3F) + 1
    } else {
        threads
    };

    s.set_cursor(start_row + 5, LABEL_COL);
    write_bytes(&mut s, b"Cores: ", COLOR_CYAN);
    write_u32(&mut s, cores, COLOR_WHITE);
    write_bytes(&mut s, b"  Threads: ", COLOR_WHITE);
    write_u32(&mut s, threads, COLOR_WHITE);

    s.set_cursor(start_row + 6, LABEL_COL);
    write_bytes(&mut s, b"Arch: ", COLOR_CYAN);
    write_bytes(&mut s, b"x86_64", COLOR_WHITE);

    s.set_cursor(start_row + 7, LABEL_COL);
    write_bytes(&mut s, b"Memory: ", COLOR_CYAN);
    if info.total_kb >= 1024 {
        write_u32(&mut s, info.total_kb / 1024, COLOR_WHITE);
        put(&mut s, b'.', COLOR_WHITE);
        write_u32(&mut s, (info.total_kb % 1024) / 103, COLOR_WHITE);
        write_bytes(&mut s, b" MiB", COLOR_WHITE);
    } else {
        write_u32(&mut s, info.total_kb, COLOR_WHITE);
        write_bytes(&mut s, b" KiB", COLOR_WHITE);
    }

    let mut flag_buf = [0u8; 32];
    let mut pos = 0;
    let mut any = false;
    let entries: [(&[u8], u8); 4] = [
        (b"SSE", 0),
        (b"SSE2", 1),
        (b"AVX", 2),
        (b"RDRAND", 3),
    ];
    for (name, bit) in entries {
        if info.cpu_flags & (1 << bit) != 0 {
            if any {
                flag_buf[pos] = b' ';
                pos += 1;
            }
            for &ch in name {
                flag_buf[pos] = ch;
                pos += 1;
            }
            any = true;
        }
    }
    let flags: &[u8] = if any { &flag_buf[..pos] } else { b"none" };

    s.set_cursor(start_row + 8, LABEL_COL);
    write_bytes(&mut s, b"CPU Flags: ", COLOR_CYAN);
    write_bytes(&mut s, flags, COLOR_WHITE);

    s.set_cursor(start_row + 9, LABEL_COL);
    write_bytes(&mut s, b"Bootloader: ", COLOR_CYAN);
    write_bytes(&mut s, b"0.9.23", COLOR_WHITE);

    let shell = join2(trimmed(&info.shell_name), trimmed(&info.shell_version));
    s.set_cursor(start_row + 10, LABEL_COL);
    write_bytes(&mut s, b"Shell: ", COLOR_CYAN);
    write_bytes(&mut s, &shell, COLOR_WHITE);

    s.set_cursor(start_row + 11, LABEL_COL);
    write_bytes(&mut s, b"Terminal: ", COLOR_CYAN);
    write_bytes(&mut s, trimmed(&info.terminal), COLOR_WHITE);

    s.set_cursor(start_row + 12, LABEL_COL);
    write_bytes(&mut s, b"Resolution: ", COLOR_CYAN);
    write_bytes(&mut s, trimmed(&info.resolution), COLOR_WHITE);

    unsafe {
        core::ptr::write_volatile(EXIT_ROW, (start_row + 14).min(SCREEN_HEIGHT - 1) as i32);
        core::ptr::write_volatile(EXIT_COL, 0);
    }
}

/// Прокручивает текстовый буфер вверх на n строк: строки n..N-1 копируются
/// на позиции 0..N-1-n, нижние n строк затираются пробелами. Так вывод fetch
/// "продолжает" tty, когда не помещается от текущей позиции курсора.
fn scroll_up(n: usize) {
    unsafe {
        for r in n..SCREEN_HEIGHT {
            let src = r * SCREEN_WIDTH * 2;
            let dst = (r - n) * SCREEN_WIDTH * 2;
            for i in 0..SCREEN_WIDTH * 2 {
                core::ptr::write_volatile(VGA.add(dst + i), core::ptr::read_volatile(VGA.add(src + i)));
            }
        }
        for r in (SCREEN_HEIGHT - n)..SCREEN_HEIGHT {
            for c in 0..SCREEN_WIDTH {
                let off = (r * SCREEN_WIDTH + c) * 2;
                core::ptr::write_volatile(VGA.add(off), b' ');
                core::ptr::write_volatile(VGA.add(off + 1), COLOR_WHITE);
            }
        }
    }
}

fn join2(a: &[u8], b: &[u8]) -> [u8; 24] {
    let mut buf = [0u8; 24];
    let mut pos = 0;
    for &ch in a {
        buf[pos] = ch;
        pos += 1;
    }
    if pos > 0 {
        buf[pos] = b' ';
        pos += 1;
    }
    for &ch in b {
        buf[pos] = ch;
        pos += 1;
    }
    buf
}

fn trimmed(buf: &[u8]) -> &[u8] {
    let mut end = buf.len();
    while end > 0 && (buf[end - 1] == 0 || buf[end - 1] == b' ') {
        end -= 1;
    }
    &buf[..end]
}

impl Screen {
    fn set_cursor(&mut self, row: usize, col: usize) {
        self.row = row.min(SCREEN_HEIGHT - 1);
        self.col = col.min(SCREEN_WIDTH - 1);
    }
}

fn put(s: &mut Screen, ch: u8, attr: u8) {
    unsafe {
        let off = (s.row * SCREEN_WIDTH + s.col) * 2;
        core::ptr::write_volatile(VGA.add(off), ch);
        core::ptr::write_volatile(VGA.add(off + 1), attr);
    }
    s.col += 1;
    if s.col >= SCREEN_WIDTH {
        s.col = 0;
        s.row = s.row.saturating_add(1).min(SCREEN_HEIGHT - 1);
    }
}

fn write_bytes(s: &mut Screen, b: &[u8], attr: u8) {
    for &ch in b {
        put(s, ch, attr);
    }
}

fn write_u32(s: &mut Screen, n: u32, attr: u8) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    let mut m = n;
    if m == 0 {
        buf[9] = b'0';
        i = 9;
    }
    while m > 0 {
        i -= 1;
        buf[i] = b'0' + (m % 10) as u8;
        m /= 10;
    }
    write_bytes(s, &buf[i..], attr);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}
