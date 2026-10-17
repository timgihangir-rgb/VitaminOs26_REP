#![no_std]
#![no_main]

//! fetch: сводка о системе в стиле neofetch.
//!
//! Раньше программа умела ровно одно: напечатать фиксированные 13 строк про
//! CPU и память. К этому моменту в ядре появились сетевой драйвер rtl8139,
//! USB-HID, AHCI/ATA, ACPI, RTC и композитор линейного кадрового буфера —
//! и всё это было видно только в отладочном логе на COM1. Теперь ядро
//! публикует состояние драйверов в SysInfo (см. sysinfo.rs), а fetch умеет
//! выбирать, что именно показывать.
//!
//! Что показывать — из файла `/etc/fetch.conf`, одна строка = один пункт,
//! порядок строк = порядок вывода. Пункты можно переопределить аргументами
//! командной строки: `fetch cpu memory net` или `fetch cpu,memory,net`.
//! Ключи: `fetch --list`.
//!
//! Программа ring 3: всё, что она знает, лежит в SysInfo по адресу 0x5000
//! (ядро заполняет блок перед запуском каждой ELF-программы) и в одном
//! сисколе VFS_READ — ради файла конфигурации.

use core::arch::x86_64::{__cpuid, __cpuid_count};

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


const COLOR_GREEN: u8 = 0x0A;
const COLOR_CYAN: u8 = 0x0B;
const COLOR_WHITE: u8 = 0x0F;
const COLOR_BOLD: u8 = 0x1F;
const COLOR_DIM: u8 = 0x08;

const EXIT_ROW: *mut i32 = 0x708C as *mut i32;
const EXIT_COL: *mut i32 = 0x7090 as *mut i32;

/// Трамплины ядра в низкой памяти (см. progabi.rs). Через них программа
/// делает int 0x80 — сам kernel ELF она не вызывает.
const TICKS_PTR: *const fn() -> u64 = 0x7800 as *const fn() -> u64;
const VFS_READ_PTR: *const unsafe fn(*const u8, *mut u8, usize) -> i64 =
    0x7818 as *const unsafe fn(*const u8, *mut u8, usize) -> i64;

/// Куда читать конфиг. Статика, а не стек: адрес должен быть внутри
/// user-образа, а сискол проверяет указатель.
static mut CONFIG_BUF: [u8; 4096] = [0; 4096];
static mut CFG_PATH: [u8; 64] = [0; 64];

/// Раскладка SysInfo обязана совпадать с src/sysinfo.rs. Порядок полей
/// менять нельзя: блок по 0x5000 общий для всех ring3-программ.
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
    // ─── драйверы и устройства ───
    fs_type: [u8; 16],
    net_driver: [u8; 16],
    net_mac: [u8; 6],
    net_ip: [u8; 4],
    net_up: u8,
    disk_dev: [u8; 12],
    disk_model: [u8; 48],
    disk_blocks: u32,
    block_backend: [u8; 12],
    fb_width: u32,
    fb_height: u32,
    fb_bpp: u8,
    fb_scale: u8,
    fb_on: u8,
    usb_kbd: u8,
    usb_reports: u32,
    input_ps2: u8,
    acpi: [u8; 16],
    rtc_unix: u32,
}

// ─── Пункты вывода ────────────────────────────────────────────────────────
//
// Каждый пункт — ключ из конфига и функция, которая печатает строку.
// Порядок в ALL_KEYS используется для `--list` и `--all`.

const KEY_OS: usize = 0;
const KEY_KERNEL: usize = 1;
const KEY_HOST: usize = 2;
const KEY_CPU: usize = 3;
const KEY_CORES: usize = 4;
const KEY_ARCH: usize = 5;
const KEY_MEMORY: usize = 6;
const KEY_MEMUSED: usize = 7;
const KEY_FLAGS: usize = 8;
const KEY_DISK: usize = 9;
const KEY_FS: usize = 10;
const KEY_BLOCK: usize = 11;
const KEY_NET: usize = 12;
const KEY_DISPLAY: usize = 13;
const KEY_INPUT: usize = 14;
const KEY_ACPI: usize = 15;
const KEY_CLOCK: usize = 16;
const KEY_TASKS: usize = 17;
const KEY_UPTIME: usize = 18;
const KEY_SHELL: usize = 19;
const KEY_BOOTLOADER: usize = 20;
const KEY_TERMINAL: usize = 21;
const KEY_COUNT: usize = 22;

const ALL_KEYS: [&[u8]; KEY_COUNT] = [
    b"os",
    b"kernel",
    b"host",
    b"cpu",
    b"cores",
    b"arch",
    b"memory",
    b"memused",
    b"flags",
    b"disk",
    b"fs",
    b"block",
    b"net",
    b"display",
    b"input",
    b"acpi",
    b"clock",
    b"tasks",
    b"uptime",
    b"shell",
    b"bootloader",
    b"terminal",
];

/// Что показывать, если конфига нет. Совпадает с /etc/fetch.conf в
/// tools/mkfs.py — если меняешь одно, меняй и другое.
const DEFAULT_KEYS: [&[u8]; 14] = [
    b"os",
    b"kernel",
    b"host",
    b"cpu",
    b"cores",
    b"memory",
    b"disk",
    b"fs",
    b"net",
    b"display",
    b"input",
    b"tasks",
    b"uptime",
    b"shell",
];

/// Индексы рисунков в ARTS / ART_NAMES. Читаются только из файла конфига
/// (`art = ...`), поэтому как константы не используются.
#[allow(dead_code)]
const ART_CAPSULE: usize = 0;
const ART_PILL: usize = 1;
#[allow(dead_code)]
const ART_VCIRCLE: usize = 2;
#[allow(dead_code)]
const ART_NONE: usize = 3;

/// Рисунки. Ширина считается в runtime (максимум по строкам), поэтому
/// разная длина строк внутри рисунка не ломает выравнивание подписей.
const ARTS: [&[&[u8]]; 4] = [
    // capsule: рамка с большой V.
    &[
        b"    .---------.",
        b"   /           \\",
        b"  |             |",
        b"  |   \\     /   |",
        b"  |    \\   /    |",
        b"  |     \\ /     |",
        b"  |      V      |",
        b"  |     / \\     |",
        b"  |    /   \\    |",
        b"  |   /     \\   |",
        b"  |             |",
        b"   \\           /",
        b"    '---------'",
    ],
    // pill: капсула. Рисунок по умолчанию.
    &[
        b"      .---------------.",
        b"    .'              '.",
        b"   /                \\",
        b"  |                 |",
        b"  |                 |",
        b"  |                 |",
        b"  |                 |",
        b"  |                 |",
        b"   \\                /",
        b"    '.              .'",
        b"      '---------------'",
    ],
    // vcircle: прежний рисунок fetch - оставлен, чтобы конфиг мог вернуть его.
    &[
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
    ],
    // none: без рисунка, подписи с нулевой колонки.
    &[],
];

const ART_NAMES: [&[u8]; 4] = [b"capsule", b"pill", b"vcircle", b"none"];

struct Config {
    /// Какие пункты показывать, в порядке вывода.
    keys: [usize; 32],
    key_count: usize,
    art: usize,
    color: bool,
}

impl Config {
    fn empty() -> Self {
        Config {
            keys: [usize::MAX; 32],
            key_count: 0,
            art: ART_PILL,
            color: true,
        }
    }

    fn push(&mut self, k: usize) {
        if k != usize::MAX && self.key_count < self.keys.len() && !self.has(k) {
            self.keys[self.key_count] = k;
            self.key_count += 1;
        }
    }

    fn has(&self, k: usize) -> bool {
        for i in 0..self.key_count {
            if self.keys[i] == k {
                return true;
            }
        }
        false
    }

    /// Ничего не выбрано (пустой конфиг) — показываем набор по умолчанию,
    /// иначе `fetch` с пустым файлом конфига печатал бы пустой экран.
    fn effective_count(&self) -> usize {
        if self.key_count == 0 {
            DEFAULT_KEYS.len()
        } else {
            self.key_count
        }
    }

    fn key_at(&self, i: usize) -> usize {
        if self.key_count == 0 {
            match_key(DEFAULT_KEYS[i])
        } else {
            self.keys[i]
        }
    }
}

/// Индекс ключа в ALL_KEYS по имени. Имена сравниваются без учёта регистра
/// и без ведущего/хвостового пробела (его снимает парсер).
fn match_key(name: &[u8]) -> usize {
    for (i, k) in ALL_KEYS.iter().enumerate() {
        if eq_ci(name, k) {
            return i;
        }
    }
    usize::MAX
}

fn eq_ci(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for i in 0..a.len() {
        let x = lower(a[i]);
        let y = lower(b[i]);
        if x != y {
            return false;
        }
    }
    true
}

fn lower(b: u8) -> u8 {
    if b.is_ascii_uppercase() {
        b + 32
    } else {
        b
    }
}

fn is_space(b: u8) -> bool {
    b == b' ' || b == b'\t' || b == b'\r' || b == b'\n'
}

struct Screen {
    row: usize,
    col: usize,
}

#[no_mangle]
#[link_section = ".text._start"]
pub extern "C" fn _start(argc: usize, argv: *const *const u8, vga_offset: usize) {
    // Сетку читаем до любого обращения к VGA: с неё считаются и стартовая
    // строка (vga_offset - это смещение в байтах от начала текста), и все
    // границы экрана дальше.
    let (gw, gh) = read_grid();
    SCREEN_W.store(gw, Ordering::Relaxed);
    SCREEN_H.store(gh, Ordering::Relaxed);

    let mut cfg = Config::empty();
    let mut cfg_path: &[u8] = b"/etc/fetch.conf";
    let mut cli_keys = false;

    // ─── разбор командной строки ─────────────────────────────────────────
    let mut i = 1usize;
    while i < argc {
        let arg = unsafe { cstr_at(argv.add(i)) };
        if arg.is_empty() {
            i += 1;
            continue;
        }
        if eq_ci(arg, b"--help") || eq_ci(arg, b"-h") {
            print_help(vga_offset);
            return;
        } else if eq_ci(arg, b"--list") {
            print_list(vga_offset);
            return;
        } else if eq_ci(arg, b"--config") || eq_ci(arg, b"-c") {
            if i + 1 < argc {
                i += 1;
                cfg_path = unsafe { cstr_at(argv.add(i)) };
            }
            i += 1;
            continue;
        } else if eq_ci(arg, b"--all") {
            for k in 0..KEY_COUNT {
                cfg.push(k);
            }
            cli_keys = true;
        } else if eq_ci(arg, b"--no-art") {
            cfg.art = ART_NONE;
        } else if eq_ci(arg, b"--no-color") {
            cfg.color = false;
        } else if arg[0] == b'-' {
            // Неизвестный флаг — не молча игнорируем: печатаем usage.
            print_help(vga_offset);
            return;
        } else {
            // `fetch cpu memory` и `fetch cpu,memory` — одно и то же.
            for part in arg.split(|&b| b == b',') {
                let name = trim(part);
                if name.is_empty() {
                    continue;
                }
                let k = match_key(name);
                if k == usize::MAX {
                    print_unknown(name, vga_offset);
                    return;
                }
                cfg.push(k);
            }
            cli_keys = true;
        }
        i += 1;
    }

    // ─── конфиг ─────────────────────────────────────────────────────────
    // Явные ключи в командной строке важнее файла: как в neofetch,
    // `fetch net` показывает только сеть. Конфига нет или он пустой — берём
    // набор по умолчанию (Config::key_at подставит его сам).
    if !cli_keys {
        let _ = read_config(cfg_path, &mut cfg);
    }

    // ─── сбор данных ────────────────────────────────────────────────────
    let info = unsafe { core::ptr::read_volatile(0x5000 as *const SysInfo) };
    let art = if cfg.art < ARTS.len() { cfg.art } else { ART_PILL };

    // Ширина рисунка — максимум по его строкам; подписи начинаются правее.
    let mut art_w = 0usize;
    for line in ARTS[art] {
        if line.len() > art_w {
            art_w = line.len();
        }
    }
    let label_col = if art_w == 0 { 0 } else { art_w + 2 };
    let lines = cfg.effective_count();
    let rows = if ARTS[art].len() > lines {
        ARTS[art].len()
    } else {
        lines
    };

    let mut start_row = vga_offset / (screen_w() * 2);
    // Продолжение tty: если блок вывода не помещается от текущей строки,
    // прокручиваем экран вверх, чтобы весь вывод был виден целиком.
    if start_row + rows + 1 > screen_h() {
        let n = start_row + rows + 1 - screen_h();
        scroll_up(n);
        start_row -= n;
    }
    let mut s = Screen {
        row: start_row,
        col: 0,
    };

    for (n, line) in ARTS[art].iter().enumerate() {
        s.set_cursor(start_row + n, 0);
        write_bytes(&mut s, line, color(&cfg, COLOR_GREEN));
    }

    for n in 0..lines {
        let k = cfg.key_at(n);
        s.set_cursor(start_row + n, label_col);
        render(&mut s, k, &info, cfg.color, n == 0);
    }

    unsafe {
        core::ptr::write_volatile(EXIT_ROW, (start_row + rows + 1).min(screen_h() - 1) as i32);
        core::ptr::write_volatile(EXIT_COL, 0);
    }
}

/// Печать одного пункта. `header` — строка без метки (имя системы).
fn render(s: &mut Screen, k: usize, info: &SysInfo, colored: bool, header: bool) {
    let cy = if colored { COLOR_CYAN } else { COLOR_WHITE };
    let wh = if colored { COLOR_WHITE } else { COLOR_WHITE };
    let dim = if colored { COLOR_DIM } else { COLOR_WHITE };
    let bold = if colored { COLOR_BOLD } else { COLOR_WHITE };

    if header && k == KEY_OS {
        // Первая строка — имя и версия системы, без метки.
        let name = join2(trimmed(&info.os_name), trimmed(&info.os_version));
        write_bytes(s, &name, bold);
        return;
    }

    match k {
        KEY_OS => {
            let name = join2(trimmed(&info.os_name), trimmed(&info.os_version));
            label(s, b"OS: ", cy);
            write_bytes(s, &name, wh);
        }
        KEY_KERNEL => {
            label(s, b"Kernel: ", cy);
            write_bytes(s, trimmed(&info.kernel_version), wh);
        }
        KEY_HOST => {
            label(s, b"Host: ", cy);
            let mut brand = trimmed(&info.cpu_brand);
            if brand.is_empty() {
                brand = b"Unknown x86_64";
            }
            write_bytes(s, brand, wh);
        }
        KEY_CPU => {
            label(s, b"CPU: ", cy);
            write_bytes(s, trimmed(&info.cpu_vendor), wh);
            write_bytes(s, b" (Family ", wh);
            write_u32(s, info.cpu_family, wh);
            write_bytes(s, b" Model ", wh);
            write_u32(s, info.cpu_model, wh);
            write_bytes(s, b" Stepping ", wh);
            write_u32(s, info.cpu_stepping, wh);
            put(s, b')', wh);
        }
        KEY_CORES => {
            let (cores, threads) = core_topology();
            label(s, b"Cores: ", cy);
            write_u32(s, cores, wh);
            write_bytes(s, b"  Threads: ", wh);
            write_u32(s, threads, wh);
        }
        KEY_ARCH => {
            label(s, b"Arch: ", cy);
            write_bytes(s, b"x86_64", wh);
        }
        KEY_MEMORY => {
            label(s, b"Memory: ", cy);
            write_kb(s, info.total_kb);
        }
        KEY_MEMUSED => {
            label(s, b"Memory Used: ", cy);
            write_kb(s, info.used_kb);
            write_bytes(s, b" of ", wh);
            write_kb(s, info.total_kb);
        }
        KEY_FLAGS => {
            label(s, b"CPU Flags: ", cy);
            let n = flag_names(info.cpu_flags, s, wh);
            if n == 0 {
                write_bytes(s, b"none", wh);
            }
        }
        KEY_DISK => {
            label(s, b"Disk: ", cy);
            let dev = trimmed(&info.disk_dev);
            if dev.is_empty() {
                write_bytes(s, b"none", wh);
                return;
            }
            write_bytes(s, dev, wh);
            let model = trimmed(&info.disk_model);
            if !model.is_empty() {
                write_bytes(s, b" ", wh);
                write_bytes(s, model, wh);
            }
            if info.disk_blocks > 0 {
                write_bytes(s, b" (", wh);
                write_u32(s, info.disk_blocks / 256, wh);
                write_bytes(s, b" MiB)", wh);
            }
        }
        KEY_FS => {
            label(s, b"Filesystem: ", cy);
            write_bytes(s, trimmed(&info.fs_type), wh);
        }
        KEY_BLOCK => {
            label(s, b"Storage Controller: ", cy);
            write_bytes(s, trimmed(&info.block_backend), wh);
        }
        KEY_NET => {
            label(s, b"Network: ", cy);
            let drv = trimmed(&info.net_driver);
            if info.net_up == 0 {
                write_bytes(s, b"no driver", dim);
                return;
            }
            write_bytes(s, drv, wh);
            write_bytes(s, b" ", wh);
            write_mac(s, &info.net_mac);
            write_bytes(s, b" (", wh);
            write_ipv4(s, &info.net_ip);
            write_bytes(s, b")", wh);
        }
        KEY_DISPLAY => {
            label(s, b"Display: ", cy);
            if info.fb_on == 0 {
                write_bytes(s, b"VGA text ", wh);
                write_bytes(s, trimmed(&info.resolution), wh);
                return;
            }
            write_u32(s, info.fb_width, wh);
            write_bytes(s, b"x", wh);
            write_u32(s, info.fb_height, wh);
            write_bytes(s, b" ", wh);
            write_u32(s, info.fb_bpp as u32, wh);
            write_bytes(s, b"bpp, text grid 80x30 @", wh);
            write_u32(s, if info.fb_scale < 1 { 1 } else { info.fb_scale as u32 }, wh);
            write_bytes(s, b"x", wh);
        }
        KEY_INPUT => {
            label(s, b"Input: ", cy);
            if info.usb_kbd != 0 {
                write_bytes(s, b"USB HID keyboard", wh);
                write_bytes(s, b" (", dim);
                write_u32(s, info.usb_reports, dim);
                write_bytes(s, b" reports)", dim);
            } else {
                write_bytes(s, b"8042 PS/2 keyboard", wh);
            }
        }
        KEY_ACPI => {
            label(s, b"ACPI: ", cy);
            write_bytes(s, trimmed(&info.acpi), wh);
        }
        KEY_CLOCK => {
            label(s, b"Clock: ", cy);
            write_utc(s, info.rtc_unix);
        }
        KEY_TASKS => {
            label(s, b"Tasks: ", cy);
            write_u32(s, info.proc_count, wh);
        }
        KEY_UPTIME => {
            label(s, b"Uptime: ", cy);
            write_uptime(s);
        }
        KEY_SHELL => {
            label(s, b"Shell: ", cy);
            let shell = join2(trimmed(&info.shell_name), trimmed(&info.shell_version));
            write_bytes(s, &shell, wh);
        }
        KEY_BOOTLOADER => {
            label(s, b"Bootloader: ", cy);
            write_bytes(s, trimmed(&info.bootloader_version), wh);
        }
        KEY_TERMINAL => {
            label(s, b"Terminal: ", cy);
            write_bytes(s, trimmed(&info.terminal), wh);
        }
        _ => {}
    }
}

fn label(s: &mut Screen, text: &[u8], attr: u8) {
    write_bytes(s, text, attr);
}

fn color(cfg: &Config, attr: u8) -> u8 {
    if cfg.color {
        attr
    } else {
        COLOR_WHITE
    }
}

// ─── Вспомогательное ─────────────────────────────────────────────────────

fn core_topology() -> (u32, u32) {
    let c1 = unsafe { __cpuid(1) };
    let max_leaf = unsafe { __cpuid(0) }.eax;

    // Число логических процессоров. По спецификации лист 1 даёт его в
    // EBX[23:16], но на виртуальных CPU (в том числе на QEMU с моделью
    // AMD) это поле приходит нулём, поэтому берём лист Extended Topology,
    // где оно гарантированно непустое, и только потом откатываемся.
    let mut threads = ((c1.ebx >> 16) & 0xFF) as u32;
    if max_leaf >= 0xB {
        let b = unsafe { __cpuid_count(0xB, 0) }.ebx & 0xFFFF;
        if b > threads {
            threads = b;
        }
    }
    if threads == 0 {
        threads = 1; // ноль противоречит спецификации, но на встречается
    }

    let cores = if max_leaf >= 4 {
        let c = unsafe { __cpuid_count(4, 0) };
        ((c.eax >> 26) & 0x3F) + 1
    } else {
        threads
    };
    (cores, threads)
}

/// Печатает список поддерживаемых флагов прямо на экран, возвращает сколько
/// напечатано. Список расширен по сравнению с прежним fetch'ем: там были
/// только SSE/SSE2/AVX/RDRAND, хотя CPUID отдаёт куда больше, и знание
/// NX/FSGSBASE ничего не стоит, а полезно при разборе совместимости.
fn flag_names(flags: u8, s: &mut Screen, attr: u8) -> usize {
    // CPUID(1).ECX и CPUID(7,0).EBX. Ядро в SysInfo отдаёт только первые
    // четыре флага (SSE/SSE2/AVX/RDRAND) — на то, чтобы он знал про CPUID(7),
    // пришлось бы расширять и SysInfo. Кольцо 3 и так может спросить CPUID
    // сам, поэтому недостающие флаги читаем здесь напрямую.
    let c1 = unsafe { __cpuid(1) };
    let max_leaf = unsafe { __cpuid(0) }.eax;
    let c7 = if max_leaf >= 7 {
        unsafe { __cpuid_count(7, 0) }
    } else {
        // Листа 7 нет — все флаги из него останутся выключенными.
        unsafe { __cpuid(0) }
    };

    // (имя, значение бита)
    let entries: [(&[u8], bool); 13] = [
        (b"SSE", c1.edx & (1 << 25) != 0),
        (b"SSE2", c1.edx & (1 << 26) != 0),
        (b"SSE3", c1.ecx & (1 << 0) != 0),
        (b"SSSE3", c1.ecx & (1 << 9) != 0),
        (b"SSE4.1", c1.ecx & (1 << 19) != 0),
        (b"SSE4.2", c1.ecx & (1 << 20) != 0),
        (b"AES", c1.ecx & (1 << 25) != 0),
        (b"AVX", flags & (1 << 2) != 0),
        (b"OSXSAVE", c1.ecx & (1 << 27) != 0),
        (b"AVX2", c7.ebx & (1 << 5) != 0),
        (b"BMI2", c7.ebx & (1 << 8) != 0),
        (b"RDRAND", flags & (1 << 3) != 0),
        (b"NX", c1.edx & (1 << 20) != 0),
    ];

    let mut any = false;
    for (name, on) in entries {
        if on {
            if any {
                put(s, b' ', attr);
            }
            write_bytes(s, name, attr);
            any = true;
        }
    }
    if any {
        1
    } else {
        0
    }
}

fn write_kb(s: &mut Screen, kb: u32) {
    if kb >= 1024 {
        // МиБ с одной десятой: делить на 1024 целиком теряет полезную
        // точность (976 МиБ превращаются в "976", хотя это 976.5).
        write_u32(s, kb / 1024, COLOR_WHITE);
        put(s, b'.', COLOR_WHITE);
        write_u32(s, ((kb % 1024) * 10) / 1024, COLOR_WHITE);
        write_bytes(s, b" MiB", COLOR_WHITE);
    } else {
        write_u32(s, kb, COLOR_WHITE);
        write_bytes(s, b" KiB", COLOR_WHITE);
    }
}

fn write_mac(s: &mut Screen, mac: &[u8; 6]) {
    for i in 0..6 {
        if i == 3 {
            put(s, b':', COLOR_WHITE);
        }
        write_hex2(s, mac[i]);
    }
}

fn write_ipv4(s: &mut Screen, ip: &[u8; 4]) {
    for i in 0..4 {
        if i > 0 {
            put(s, b'.', COLOR_WHITE);
        }
        write_u32(s, ip[i] as u32, COLOR_WHITE);
    }
}

fn write_hex2(s: &mut Screen, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    put(s, HEX[(b >> 4) as usize], COLOR_WHITE);
    put(s, HEX[(b & 0xF) as usize], COLOR_WHITE);
}

/// Unix-время -> `YYYY-MM-DD HH:MM:SS UTC`. Разложение даты считаем сами
/// (civil_from_days), libc в ring 3 нет, а `rtc::now_unix()` отдаёт именно
/// секунды epoch.
fn write_utc(s: &mut Screen, unix: u32) {
    let days = (unix / 86400) as i64;
    let secs = unix % 86400;
    let (y, m, d) = civil_from_days(days);
    write_u32(s, y as u32, COLOR_WHITE);
    put(s, b'-', COLOR_WHITE);
    write_u32_padded(s, m as u32, 2, COLOR_WHITE);
    put(s, b'-', COLOR_WHITE);
    write_u32_padded(s, d as u32, 2, COLOR_WHITE);
    put(s, b' ', COLOR_WHITE);
    write_u32_padded(s, secs / 3600, 2, COLOR_WHITE);
    put(s, b':', COLOR_WHITE);
    write_u32_padded(s, (secs / 60) % 60, 2, COLOR_WHITE);
    put(s, b':', COLOR_WHITE);
    write_u32_padded(s, secs % 60, 2, COLOR_WHITE);
    write_bytes(s, b" UTC", COLOR_WHITE);
}

/// Ховард Хинтон, days since 1970-01-01 -> (год, месяц, день).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn write_uptime(s: &mut Screen) {
    let ticks: u64 = unsafe { (*TICKS_PTR)() };
    let total = (ticks / 100) as u32; // PIT 100 Гц
    let h = total / 3600;
    let m = (total / 60) % 60;
    let sec = total % 60;
    write_u32(s, h, COLOR_WHITE);
    put(s, b':', COLOR_WHITE);
    write_u32_padded(s, m, 2, COLOR_WHITE);
    put(s, b':', COLOR_WHITE);
    write_u32_padded(s, sec, 2, COLOR_WHITE);
}

// ─── Конфиг ──────────────────────────────────────────────────────────────

/// Читает `/etc/fetch.conf` через сискол VFS_READ. `Ok(n)` — прочитано n
/// байт, `Err(())` — файла нет или он не читается (тогда работают значения
/// по умолчанию).
fn read_config(path: &[u8], cfg: &mut Config) -> Result<usize, ()> {
    if path.len() >= 64 {
        return Err(());
    }
    unsafe {
        for (i, &b) in path.iter().enumerate() {
            CFG_PATH[i] = b;
        }
        CFG_PATH[path.len()] = 0;
        let read: unsafe fn(*const u8, *mut u8, usize) -> i64 = *VFS_READ_PTR;
        let n = read(CFG_PATH.as_ptr(), CONFIG_BUF.as_mut_ptr(), 4096);
        if n <= 0 {
            return Err(());
        }
        parse_config(&CONFIG_BUF[..n as usize], cfg);
        Ok(n as usize)
    }
}

/// Формат конфига — как у neofetch, но без обязательных символов:
///   строка без `=`  -> пункт вывода
///   `ключ = знач.` -> параметр (`art`, `color`)
///   `#` в начале   -> комментарий
fn parse_config(data: &[u8], cfg: &mut Config) {
    let mut i = 0usize;
    while i < data.len() {
        // Одна строка до \n.
        let start = i;
        while i < data.len() && data[i] != b'\n' {
            i += 1;
        }
        let line = trim(&data[start..i]);
        i += 1;

        // Комментарий и пустые строки.
        if line.is_empty() || line[0] == b'#' || line[0] == b';' {
            continue;
        }

        if let Some(eq) = line.iter().position(|&b| b == b'=') {
            let name = trim(&line[..eq]);
            let value = trim(&line[eq + 1..]);
            if eq_ci(name, b"art") {
                for (i, n) in ART_NAMES.iter().enumerate() {
                    if eq_ci(value, n) {
                        cfg.art = i;
                    }
                }
            } else if eq_ci(name, b"color") {
                cfg.color = !(eq_ci(value, b"off") || eq_ci(value, b"no") || eq_ci(value, b"0"));
            }
            continue;
        }

        let k = match_key(line);
        if k != usize::MAX {
            cfg.push(k);
        }
    }
}

// ─── Вывод служебных сообщений ───────────────────────────────────────────

/// Печатает блок от `row`, предварительно освобождая экран, если нужно.
/// Служебные сообщения не должны остаться за нижним краем.
/// Готовит экран под блок из `rows` строк, начиная с текущей позиции tty.
/// Не помещается — прокручивает вверх, чтобы блок был виден целиком.
fn simple_block(rows: usize, vga_offset: usize) -> Screen {
    let mut start_row = vga_offset / (screen_w() * 2);
    if start_row + rows > screen_h() {
        let n = start_row + rows - screen_h();
        scroll_up(n);
        start_row -= n;
    }
    Screen {
        row: start_row,
        col: 0,
    }
}

fn print_help(vga_offset: usize) {
    let mut s = simple_block(HELP.len(), vga_offset);
    let start = s.row;
    for (i, line) in HELP.iter().enumerate() {
        if line.is_empty() {
            continue;
        }
        s.set_cursor(start + i, 0);
        // Строки-примеры с отступом — подсказки, гасим их.
        let attr = if line.starts_with(b"  ") { COLOR_DIM } else { COLOR_WHITE };
        write_bytes(&mut s, line, attr);
    }
    finish(start + HELP.len());
}

const HELP: [&[u8]; 12] = [
    b"fetch - system summary",
    b"",
    b"usage: fetch [options] [keys...]",
    b"  keys may be comma-separated: fetch cpu,memory,net",
    b"",
    b"  -c, --config PATH   read keys from PATH (default /etc/fetch.conf)",
    b"      --all           show every key",
    b"      --no-art        no drawing, keys start at column 0",
    b"      --no-color      monochrome",
    b"      --list          list available keys",
    b"",
    b"art = capsule | pill | vcircle | none",
];

fn print_list(vga_offset: usize) {
    let mut s = simple_block(KEY_COUNT + 2, vga_offset);
    let start = s.row;
    s.set_cursor(start, 0);
    write_bytes(&mut s, b"keys:", COLOR_CYAN);
    for (i, k) in ALL_KEYS.iter().enumerate() {
        s.set_cursor(start + 1 + i / 3, (i % 3) * 18);
        write_bytes(&mut s, k, COLOR_WHITE);
    }
    finish(start + 2 + KEY_COUNT / 3);
}

fn print_unknown(name: &[u8], vga_offset: usize) {
    let mut s = simple_block(3, vga_offset);
    let start = s.row;
    s.set_cursor(start, 0);
    write_bytes(&mut s, b"fetch: unknown key '", COLOR_WHITE);
    write_bytes(&mut s, name, COLOR_CYAN);
    put(&mut s, b'\'', COLOR_WHITE);
    s.set_cursor(start + 1, 0);
    write_bytes(&mut s, b"try 'fetch --list'", COLOR_DIM);
    finish(start + 2);
}

/// Сообщаем ядру, куда поставить курсор после выхода, и даём `_start`
/// вернуться: возвращение уводит RIP на ноль (в начальном стеке задачи нет
/// обратного адреса), задача падает и ядро переводит её в Finished, а
/// `wait_for_polled` заканчивается._BUSY_LOOP в коде быть не должно —
/// задача, которая не уходит в Blocked, никогда не станет Finished, и шелл
/// зависнет намертво.
fn finish(row: usize) {
    unsafe {
        core::ptr::write_volatile(EXIT_ROW, row.min(screen_h() - 1) as i32);
        core::ptr::write_volatile(EXIT_COL, 0);
    }
}

// ─── Примитивы ───────────────────────────────────────────────────────────

/// C-строка из `argv[i]`. ring 3 читает только свою память, поэтому длину
/// ограничиваем: если разыменование ушло за пределы образа, лучше показать
/// меньше символов, чем упасть.
unsafe fn cstr_at(p: *const *const u8) -> &'static [u8] {
    let mut n = 0usize;
    while n < 128 && *(*p).add(n) != 0 {
        n += 1;
    }
    core::slice::from_raw_parts(*p, n)
}

/// Прокручивает текстовый буфер вверх на n строк: строки n..N-1 копируются
/// на позиции 0..N-1-n, нижние n строк затираются пробелами. Так вывод fetch
/// "продолжает" tty, когда не помещается от текущей позиции курсора.
fn scroll_up(n: usize) {
    unsafe {
        for r in n..screen_h() {
            let src = r * screen_w() * 2;
            let dst = (r - n) * screen_w() * 2;
            for i in 0..screen_w() * 2 {
                core::ptr::write_volatile(
                    VGA.add(dst + i),
                    core::ptr::read_volatile(VGA.add(src + i)),
                );
            }
        }
        for r in (screen_h() - n)..screen_h() {
            for c in 0..screen_w() {
                let off = (r * screen_w() + c) * 2;
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

fn trim(buf: &[u8]) -> &[u8] {
    let mut start = 0;
    while start < buf.len() && is_space(buf[start]) {
        start += 1;
    }
    let mut end = buf.len();
    while end > start && is_space(buf[end - 1]) {
        end -= 1;
    }
    &buf[start..end]
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
        core::ptr::write_volatile(VGA.add(off), ch);
        core::ptr::write_volatile(VGA.add(off + 1), attr);
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

fn write_u32(s: &mut Screen, n: u32, attr: u8) {
    write_u32_padded(s, n, 0, attr);
}

/// Число с ведущими нулями до `width` цифр (для времени и даты).
fn write_u32_padded(s: &mut Screen, n: u32, width: usize, attr: u8) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    let mut m = n;
    if m == 0 {
        i -= 1;
        buf[i] = b'0';
    }
    while m > 0 {
        i -= 1;
        buf[i] = b'0' + (m % 10) as u8;
        m /= 10;
    }
    while (buf.len() - i) < width && i > 0 {
        i -= 1;
        buf[i] = b'0';
    }
    write_bytes(s, &buf[i..], attr);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    loop {}
}