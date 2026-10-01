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
pub const IOCTL_PTR: usize = 0x7838;
pub const NET_CONNECT_PTR: usize = 0x7840;
pub const NET_SEND_PTR: usize = 0x7848;
pub const NET_RECV_PTR: usize = 0x7850;
pub const NET_CLOSE_PTR: usize = 0x7858;
pub const NET_RESOLVE_PTR: usize = 0x7860;

pub const EXIT_STUB: usize = 0x7100;
pub const TICKS_STUB: usize = 0x7110;
pub const HLT_STUB: usize = 0x7120;
pub const VFS_WRITE_STUB: usize = 0x7130;
pub const VFS_READ_STUB: usize = 0x7140;
pub const SLEEP_STUB: usize = 0x7150;
pub const KBHIT_STUB: usize = 0x7160;
pub const KBREAD_STUB: usize = 0x7168;
pub const IOCTL_STUB: usize = 0x7170;
pub const NET_CONNECT_STUB: usize = 0x7180;
pub const NET_SEND_STUB: usize = 0x7188;
pub const NET_RECV_STUB: usize = 0x7190;
pub const NET_CLOSE_STUB: usize = 0x7198;
pub const NET_RESOLVE_STUB: usize = 0x71A0;

const SYS_TICKS: usize = 0;
const SYS_HLT: usize = 1;
const SYS_VFS_WRITE: usize = 2;
const SYS_VFS_READ: usize = 3;
const SYS_EXIT: usize = 4;
const SYS_SLEEP: usize = 5;
const SYS_KBHIT: usize = 6;
const SYS_KBREAD: usize = 7;
const SYS_IOCTL: usize = 8;
const SYS_NET_CONNECT: usize = 9;
const SYS_NET_SEND: usize = 10;
const SYS_NET_RECV: usize = 11;
const SYS_NET_CLOSE: usize = 12;
const SYS_NET_RESOLVE: usize = 13;

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
    write_stub(IOCTL_STUB, SYS_IOCTL, true);
    write_stub(NET_CONNECT_STUB, SYS_NET_CONNECT, true);
    write_stub(NET_SEND_STUB, SYS_NET_SEND, true);
    write_stub(NET_RECV_STUB, SYS_NET_RECV, true);
    write_stub(NET_CLOSE_STUB, SYS_NET_CLOSE, true);
    write_stub(NET_RESOLVE_STUB, SYS_NET_RESOLVE, true);

    store_pointer(TICKS_PTR, TICKS_STUB);
    store_pointer(HLT_PTR, HLT_STUB);
    store_pointer(VFS_WRITE_PTR, VFS_WRITE_STUB);
    store_pointer(VFS_READ_PTR, VFS_READ_STUB);
    store_pointer(SLEEP_PTR, SLEEP_STUB);
    store_pointer(KBHIT_PTR, KBHIT_STUB);
    store_pointer(KBREAD_PTR, KBREAD_STUB);
    store_pointer(IOCTL_PTR, IOCTL_STUB);
    store_pointer(NET_CONNECT_PTR, NET_CONNECT_STUB);
    store_pointer(NET_SEND_PTR, NET_SEND_STUB);
    store_pointer(NET_RECV_PTR, NET_RECV_STUB);
    store_pointer(NET_CLOSE_PTR, NET_CLOSE_STUB);
    store_pointer(NET_RESOLVE_PTR, NET_RESOLVE_STUB);
}
