//! ABI для .bin-программ: ядро пишет в shared memory указатели на
//! user-трамплины — маленькие функции в низкой памяти, которые выполняют
//! `int 0x80` и тем самым переходят ring 3 -> ring 0 (см. `syscall`).
//!
//! Адреса фиксированы и лежат в низкой памяти (ниже 1 MiB, её не раздаёт
//! фрейм-аллокатор), чтобы они совпадали в адресных пространствах: в boot
//! identity-маппинге (ring-0 .bin) и в 2 MiB user identity-маппинге (ring 3).
//!
//! Трамплин = `mov rax, <nr>; int 0x80; ret` (exit вместо ret делает `hlt`).
//! Программы никогда не вызывают код ядра напрямую.

pub const TICKS_PTR: usize = 0x7800;pub const HLT_PTR: usize = 0x7808;
pub const VFS_WRITE_PTR: usize = 0x7810;
pub const VFS_READ_PTR: usize = 0x7818;
pub const SLEEP_PTR: usize = 0x7820;
pub const KBHIT_PTR: usize = 0x7828;
pub const KBREAD_PTR: usize = 0x7830;

pub const EXIT_STUB: usize = 0x7100;
pub const TICKS_STUB: usize = 0x7110;
pub const HLT_STUB: usize = 0x7120;
pub const VFS_WRITE_STUB: usize = 0x7130;
pub const VFS_READ_STUB: usize = 0x7140;
pub const SLEEP_STUB: usize = 0x7150;
pub const KBHIT_STUB: usize = 0x7160;
pub const KBREAD_STUB: usize = 0x7168;

const SYS_TICKS: usize = 0;
const SYS_HLT: usize = 1;
const SYS_VFS_WRITE: usize = 2;
const SYS_VFS_READ: usize = 3;
const SYS_EXIT: usize = 4;
const SYS_SLEEP: usize = 5;
const SYS_KBHIT: usize = 6;
const SYS_KBREAD: usize = 7;

fn store_pointer(at: usize, f: usize) {
    unsafe {
        (at as *mut u64).write_volatile(f as u64);
    }
}

fn write_stub(at: usize, syscall: usize, ret: bool) {
    // B8 <4 байта nr> CD 80 C3 | F4
    let bytes = [
        0xB8,
        (syscall & 0xff) as u8,
        ((syscall >> 8) & 0xff) as u8,
        ((syscall >> 16) & 0xff) as u8,
        ((syscall >> 24) & 0xff) as u8,
        0xCD,
        0x80,
        if ret { 0xC3 } else { 0xF4 },
    ];
    unsafe {
        for (i, b) in bytes.iter().enumerate() {
            (at as *mut u8).add(i).write_volatile(*b);
        }
    }
}

/// Ставит трамплины в низкую память и заполняет блок указателей. Вызывается
/// перед запуском .bin-программы (и в `run`, и в `bg`).
pub fn install() {
    write_stub(EXIT_STUB, SYS_EXIT, false);
    write_stub(TICKS_STUB, SYS_TICKS, true);
    write_stub(HLT_STUB, SYS_HLT, true);
    write_stub(VFS_WRITE_STUB, SYS_VFS_WRITE, true);
    write_stub(VFS_READ_STUB, SYS_VFS_READ, true);
    write_stub(SLEEP_STUB, SYS_SLEEP, true);
    write_stub(KBHIT_STUB, SYS_KBHIT, true);
    write_stub(KBREAD_STUB, SYS_KBREAD, true);

    store_pointer(TICKS_PTR, TICKS_STUB);
    store_pointer(HLT_PTR, HLT_STUB);
    store_pointer(VFS_WRITE_PTR, VFS_WRITE_STUB);
    store_pointer(VFS_READ_PTR, VFS_READ_STUB);
    store_pointer(SLEEP_PTR, SLEEP_STUB);
    store_pointer(KBHIT_PTR, KBHIT_STUB);
    store_pointer(KBREAD_PTR, KBREAD_STUB);
}
