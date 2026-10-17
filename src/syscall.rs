//! Системные вызовы через `int 0x80` (DPL 3): переход ring 3 -> ring 0.
//!
//! Пользовательские программы вызывают не функции ядра напрямую, а маленькие
//! user-трамплины (см. `progabi::install`), которые выполняют `int 0x80` с
//! номером вызова в `rax`. Ядро обрабатывает вызов, кладёт результат в `rax`
//! слота сохранённых регистров и `iretq` обратно в ring 3.
//!
//! Номера вызовов:
//!   0 ticks() -> u64
//!   1 hlt()            (блокирует задачу до следующего тика)
//!   2 vfs_write(path, data, len) -> i32
//!   3 vfs_read(path, buf, maxlen) -> i32
//!   4 exit()  (не возвращается)
//!   5 sleep(target_tick) — блокирует задачу до тика target_tick (не вернётся
//!     раньше: спящие задачи не получают кванты round-robin)
//!   6 kbhit() -> int (есть ли сканкод в буфере клавиатуры)
//!   7 kbread() -> int (вынуть сканкод из буфера клавиатуры; 0 — буфер пуст)
//!   8 ioctl(cmd, arg) -> i32
//!       cmd 1 CURSOR_GET      -> (row<<16)|col
//!       cmd 2 CURSOR_SET arg  = (row<<16)|col
//!       cmd 3 CLEAR           - очистить экран
//!       cmd 4 SCREEN_SIZE     -> (height<<16)|width
//!       cmd 5 ECHO_GET        -> 0/1
//!       cmd 6 ECHO_SET arg    = 0 выкл / 1 вкл
//!   9 net_connect(ip_str, port) -> i32  (0 = установлено, -2 = в процессе)
//!  10 net_send(buf, len) -> i32         (байт отправлено / -3 ошибка)
//!  11 net_recv(buf, max) -> i32         (>0 байт / -2 нет данных / -1 EOF / -3)
//!  12 net_close() -> i32
//!  14 auth(password) -> 0/-1   (пароль самого вызывателя; успех = root)
//!  15 getuid() -> i32
//!  16 whoami(buf, max) -> i32
//!  17 useradd(name, password) -> i32   (только root)
//!  18 userdel(name) -> i32            (только root)
//!  19 exec(path, argc, argv) -> pid   (от uid вызывающего)
//!  20 wait(pid) -> 0                   (блокируется до конца задачи)
//!
//!   net_* — НЕ-блокирующие: ядро продвигает TCP-состояние и возвращает
//!   NET_EAGAIN (-2), когда ждёт сети; программа сама повторяет вызов со
//!   `sleep`-отпуском CPU (см. src/net.rs и programs/web).
//!
//! Аргументы (по SysV) приходят в rdi/rsi/rdx и лежат в сохранённых регистрах
//! с той же раскладкой, что в `timer_entry` планировщика. Вершина стека после
//! 15 push'ей — это r15, поэтому индексы (rsp[0]..rsp[14]) идут от r15 к rax:
//!   [0]=r15 [1]=r14 [2]=r13 [3]=r12 [4]=r11 [5]=r10 [6]=r9 [7]=r8
//!   [8]=rbx [9]=rbp [10]=rdi [11]=rsi [12]=rdx [13]=rcx [14]=rax

use alloc::string::ToString;
use core::arch::global_asm;

pub const SYS_TICKS: usize = 0;
pub const SYS_HLT: usize = 1;
pub const SYS_VFS_WRITE: usize = 2;
pub const SYS_VFS_READ: usize = 3;
pub const SYS_EXIT: usize = 4;
pub const SYS_SLEEP: usize = 5;
pub const SYS_KBHIT: usize = 6;
pub const SYS_KBREAD: usize = 7;
pub const SYS_IOCTL: usize = 8;
pub const SYS_NET_CONNECT: usize = 9;
pub const SYS_NET_SEND: usize = 10;
pub const SYS_NET_RECV: usize = 11;
pub const SYS_NET_CLOSE: usize = 12;
pub const SYS_NET_RESOLVE: usize = 13;
/// auth(password) -> 0 / -1. Проверяет пароль САМОГО вызывающего (uid берётся
/// из его задачи) и, если он верный, делает задачу root. Отдельного "setuid"
/// нет намеренно: иначе любая программа могла бы повысить себя до root.
pub const SYS_AUTH: usize = 14;
/// getuid() -> uid текущей задачи.
pub const SYS_GETUID: usize = 15;
/// whoami(buf, max) -> длина имени / -1.
pub const SYS_WHOAMI: usize = 16;
/// useradd(name, password) -> 0 / -1. Только от root.
pub const SYS_USERADD: usize = 17;
/// userdel(name) -> 0 / -1. Только от root.
pub const SYS_USERDEL: usize = 18;
/// exec(path, argc, argv) -> pid / -1. Запуск программы от текущего uid.
pub const SYS_EXEC: usize = 19;
/// wait(pid) -> 0. Блокируется до завершения задачи pid.
pub const SYS_WAIT: usize = 20;
/// ioperm(port, count, enable) → 0/-1.
///
/// Разрешает (или запрещает) доступ к I/O-портам из ring 3. Только root.
/// Траплен в программе выходит через SYS_IOPERM (программа вызывает его
/// номер 33 напрямую, либо через ABI-трамплин в progabi - пока не добавлен,
/// al вынужден сочинять вызов `int 80h` + rax=33).
pub const SYS_IOPERM: usize = 33;
pub const SYS_NET_STATE: usize = 34;

global_asm!(
    ".global int80_entry",
    ".align 16",
    "int80_entry:",
    "push rax",
    "push rcx",
    "push rdx",
    "push rsi",
    "push rdi",
    "push rbp",
    "push rbx",
    "push r8",
    "push r9",
    "push r10",
    "push r11",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov rdi, rsp",
    "call syscall_dispatch",
    "test rax, rax",
    "jnz 1f",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbx",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rax",
    "iretq",
    "1:",
    "mov rax, [rip + CURRENT_SAVE_SLOT]",
    "mov [rax], rsp",
    "call schedule",
    "mov rsp, rax",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop r11",
    "pop r10",
    "pop r9",
    "pop r8",
    "pop rbx",
    "pop rbp",
    "pop rdi",
    "pop rsi",
    "pop rdx",
    "pop rcx",
    "pop rax",
    "iretq",
);

extern "C" {
    fn int80_entry();
}

pub fn int80_entry_addr() -> u64 {
    let f: unsafe extern "C" fn() = int80_entry;
    f as usize as u64
}

fn set_result(regs: *mut u64, v: u64) {
    unsafe {
        *regs.add(14) = v; // слот rax
    }
}

fn user_ptr_ok(p: u64) -> bool {
    // Указатели user-программы лежат в её адресном пространстве (низкие
    // адреса). Запрещаем доступ к high-half ядра и к самым низким страницам.
    p >= 0x5000 && p < crate::memory::PHYS_MEM_OFFSET
}

extern "C" fn sys_vfs_write(regs: *mut u64) {
    let path = unsafe { *regs.add(10) };
    let data = unsafe { *regs.add(11) };
    if !user_ptr_ok(path) || !user_ptr_ok(data) {
        set_result(regs, -1i64 as u64);
        return;
    }
    let len = (unsafe { *regs.add(12) }).min(65536) as usize;
    let path = unsafe { core::slice::from_raw_parts(path as *const u8, 4096) };
    let path = match path.iter().position(|&b| b == 0) {
        Some(n) => match core::str::from_utf8(&path[..n]) {
            Ok(s) => s,
            Err(_) => {
                set_result(regs, -2i64 as u64);
                return;
            }
        },
        None => {
            set_result(regs, -2i64 as u64);
            return;
        }
    };
    let data = unsafe { core::slice::from_raw_parts(data as *const u8, len) };
    let ok = crate::scheduler::with_vfs(|vfs| vfs.write_file(path, data).is_ok());
    set_result(regs, if ok { 0 } else { -1i64 } as u64);
}

extern "C" fn sys_vfs_read(regs: *mut u64) {
    let path = unsafe { *regs.add(10) };
    let buf = unsafe { *regs.add(11) };
    let maxlen = unsafe { *regs.add(12) }.min(65536) as usize;
    if !user_ptr_ok(path) || !user_ptr_ok(buf) {
        set_result(regs, -1i64 as u64);
        return;
    }
    let path = unsafe { core::slice::from_raw_parts(path as *const u8, 4096) };
    let path = match path.iter().position(|&b| b == 0) {
        Some(n) => match core::str::from_utf8(&path[..n]) {
            Ok(s) => s,
            Err(_) => {
                set_result(regs, -2i64 as u64);
                return;
            }
        },
        None => {
            set_result(regs, -2i64 as u64);
            return;
        }
    };
    crate::scheduler::with_vfs(|vfs| match vfs.cat(path) {
        Some(data) => {
            let n = data.len().min(maxlen);
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, n) };
            set_result(regs, n as u64);
        }
        None => set_result(regs, -1i64 as u64),
    });
}

/// C-строка из адресного пространства вызывающей программы. `max` ограничивает
/// чтение, чтобы кривой указатель не увёл нас далеко.
fn read_user_str(ptr: u64, max: usize) -> Option<alloc::string::String> {
    if !user_ptr_ok(ptr) {
        return None;
    }
    let bytes = unsafe { core::slice::from_raw_parts(ptr as *const u8, max) };
    let n = bytes.iter().position(|&b| b == 0)?;
    core::str::from_utf8(&bytes[..n])
        .ok()
        .map(|s| s.to_string())
}

/// auth: пароль самого вызывающего. Успех переводит задачу в root.
extern "C" fn sys_auth(regs: *mut u64) {
    let uid = crate::user::current_uid();
    if uid == crate::user::ROOT_UID {
        set_result(regs, 0); // уже root
        return;
    }
    let pass = match read_user_str(unsafe { *regs.add(10) }, 128) {
        Some(s) => s,
        None => {
            set_result(regs, -2i64 as u64);
            return;
        }
    };
    let account = match crate::user::lookup_uid(uid) {
        Some(a) => a,
        None => {
            set_result(regs, -1i64 as u64);
            return;
        }
    };
    if crate::user::verify(&account.name, &pass) {
        crate::user::set_current_uid(crate::user::ROOT_UID);
        set_result(regs, 0);
    } else {
        set_result(regs, -1i64 as u64);
    }
}

/// useradd/userdel: правка учётных записей, доступна только root.
extern "C" fn sys_useradd(regs: *mut u64) {
    if crate::user::current_uid() != crate::user::ROOT_UID {
        set_result(regs, -1i64 as u64);
        return;
    }
    let (Some(name), Some(pass)) = (
        read_user_str(unsafe { *regs.add(10) }, 64),
        read_user_str(unsafe { *regs.add(11) }, 128),
    ) else {
        set_result(regs, -2i64 as u64);
        return;
    };
    if !crate::linein::valid_user_name(&name) {
        set_result(regs, -3i64 as u64);
        return;
    }
    let r = match crate::user::add_with_home(&name, &pass) {
        Ok(_) => 0i64,
        Err(e) => {
            crate::vga::serial_write_atomic("[auth] useradd: ");
            crate::vga::serial_write_atomic(e);
            crate::vga::serial_write_atomic("\n");
            -1i64
        }
    };
    set_result(regs, r as u64);
}

extern "C" fn sys_userdel(regs: *mut u64) {
    if crate::user::current_uid() != crate::user::ROOT_UID {
        set_result(regs, -1i64 as u64);
        return;
    }
    let Some(name) = read_user_str(unsafe { *regs.add(10) }, 64) else {
        set_result(regs, -2i64 as u64);
        return;
    };
    let r = match crate::user::remove(&name) {
        Ok(()) => 0i64,
        Err(_) => -1i64,
    };
    set_result(regs, r as u64);
}

/// exec(path, argc, argv): запускает программу от текущего uid вызывающего.
/// Нужен `sudo`: подняв права себе, он запускает команду уже с uid 0.
extern "C" fn sys_exec(regs: *mut u64) {
    let path_ptr = unsafe { *regs.add(10) };
    let argc = (unsafe { *regs.add(11) }) as usize;
    let argv_ptr = unsafe { *regs.add(12) };
    if argc == 0 || argc > 32 {
        set_result(regs, -2i64 as u64);
        return;
    }
    let Some(path) = read_user_str(path_ptr, 256) else {
        set_result(regs, -2i64 as u64);
        return;
    };
    if !user_ptr_ok(argv_ptr) {
        set_result(regs, -2i64 as u64);
        return;
    }
    // argv[0] заменяем на полный путь - так же, как это делает шелл.
    let mut argv: alloc::vec::Vec<alloc::string::String> =
        alloc::vec::Vec::with_capacity(argc);
    for i in 0..argc {
        let slot = unsafe { (argv_ptr as *const u64).add(i).read_volatile() };
        let arg = if i == 0 {
            path.clone()
        } else {
            match read_user_str(slot, 256) {
                Some(s) => s,
                None => {
                    set_result(regs, -2i64 as u64);
                    return;
                }
            }
        };
        argv.push(arg);
    }
    let name = path.rsplit('/').next().unwrap_or(&path).to_string();
    let binpath = if path.starts_with('/') {
        path.clone()
    } else {
        alloc::format!("/bin/{}", path)
    };
    let data = crate::scheduler::with_vfs(|v| v.cat(&binpath).map(|d| d.to_vec()));
    let Some(data) = data else {
        set_result(regs, -1i64 as u64); // нет такой программы
        return;
    };
    crate::progabi::install();
    let refs: alloc::vec::Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
    let uid = crate::user::current_uid();
    match crate::exec::launch_user(&name, &data, &refs, 0, uid) {
        Some(pid) => {
            // Ребёнок становится передним планом стола: Ctrl+C должен
            // доставать команду, а не ждущий sudo.
            crate::desk::set_fg(pid);
            set_result(regs, pid as u64);
        }
        None => set_result(regs, -1i64 as u64),
    }
}

/// Диспетчер системных вызовов. Вызывается из asm `int80_entry` с rdi =
/// указателем на сохранённые регистры. Результат пишется в слот rax.
///
/// Возвращает 0 — продолжить выполнение user-задачи (pop + iretq), любое
/// ненулевое значение — задача завершилась, переключиться на следующую.
#[no_mangle]
pub extern "C" fn syscall_dispatch(regs: *mut u64) -> usize {
    // На входе IF=0 (interrupt gate). Разрешаем вытеснение на время работы с
    // VFS (иначе спин-лок VFS_LOCK может заблокироваться навсегда).
    x86_64::instructions::interrupts::enable();
    let n = unsafe { *regs.add(14) } as usize;
    let result = match n {
        SYS_TICKS => {
            let t = crate::scheduler::ticks();
            set_result(regs, t);
            0
        }
        SYS_HLT => {
            // Блокируем задачу до следующего тика и уходим с ротации (return 1).
            // hlt() (IF=1) подстраховывает случай, когда других Ready-задач нет
            // и планировщик вернул бы нас сразу же (задачи не должны жечь CPU).
            crate::scheduler::block_until_tick(crate::scheduler::ticks() + 1);
            x86_64::instructions::hlt();
            set_result(regs, 0);
            1
        }
        SYS_SLEEP => {
            let target = unsafe { *regs.add(10) };
            crate::scheduler::block_until_tick(target);
            x86_64::instructions::hlt();
            set_result(regs, 0);
            1
        }
        SYS_VFS_WRITE => {
            sys_vfs_write(regs);
            0
        }
        SYS_VFS_READ => {
            sys_vfs_read(regs);
            0
        }
        SYS_KBHIT => {
            set_result(regs, if crate::keyboard::kb_hit() { 1 } else { 0 });
            0
        }
        SYS_KBREAD => {
            set_result(regs, crate::keyboard::kb_read() as u64);
            0
        }
        SYS_IOCTL => {
            let cmd = unsafe { *regs.add(10) };
            let arg = unsafe { *regs.add(11) };
            use crate::vga::{screen_cols, screen_rows};
            let r = match cmd {
                1 => {
                    let (row, col) = crate::vga::hw_cursor_pos();
                    ((row << 16) | col) as i64
                }
                2 => {
                    crate::vga::set_hw_cursor((arg >> 16) as usize, (arg & 0xFFFF) as usize);
                    0
                }
                3 => {
                    let mut w = crate::vga::Writer::new();
                    w.clear_screen();
                    0
                }
                4 => ((screen_rows() << 16) | screen_cols()) as i64,
                5 => {
                    if crate::keyboard::echo_get() {
                        1
                    } else {
                        0
                    }
                }
                6 => {
                    crate::keyboard::echo_set(arg != 0);
                    0
                }
                _ => -1i64,
            };
            set_result(regs, r as u64);
            0
        }
        SYS_NET_CONNECT => {
            let ip_ptr = unsafe { *regs.add(10) };
            let port = unsafe { *regs.add(11) } as u16;
            if !user_ptr_ok(ip_ptr) {
                set_result(regs, -1i64 as u64);
                0
            } else {
                let ip_slice =
                    unsafe { core::slice::from_raw_parts(ip_ptr as *const u8, 64) };
                let ip_str = match ip_slice.iter().position(|&b| b == 0) {
                    Some(n) => match core::str::from_utf8(&ip_slice[..n]) {
                        Ok(s) => s,
                        Err(_) => {
                            set_result(regs, -1i64 as u64);
                            return 0;
                        }
                    },
                    None => {
                        set_result(regs, -1i64 as u64);
                        return 0;
                    }
                };
                let r = match crate::net::parse_ip(ip_str) {
                    Some(ip) => crate::net::tcp_connect(&ip, port),
                    None => -1i32,
                };
                set_result(regs, r as u64);
                0
            }
        }
        SYS_NET_STATE => {
            let sid = unsafe { *regs.add(10) } as usize;
            match crate::net::tcp_state(sid) {
                Ok(state) => set_result(regs, state as i64 as u64),
                Err(()) => set_result(regs, -1i64 as u64),
            }
            0
        }
        SYS_NET_SEND => {
            let sid = unsafe { *regs.add(10) } as usize;
            let buf = unsafe { *regs.add(11) };
            let len = (unsafe { *regs.add(12) }).min(1400) as usize;
            if !user_ptr_ok(buf) {
                set_result(regs, -1i64 as u64);
            } else {
                let data = unsafe { core::slice::from_raw_parts(buf as *const u8, len) };
                let r = crate::net::tcp_send(sid, data);
                set_result(regs, r as u64);
            }
            0
        }
        SYS_NET_RECV => {
            let sid = unsafe { *regs.add(10) } as usize;
            let buf = unsafe { *regs.add(11) };
            let max = (unsafe { *regs.add(12) }).min(16384) as usize;
            if !user_ptr_ok(buf) {
                set_result(regs, -1i64 as u64);
            } else {
                let out = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, max) };
                let r = crate::net::tcp_recv(sid, out);
                set_result(regs, r as u64);
            }
            0
        }
        SYS_NET_CLOSE => {
            let sid = unsafe { *regs.add(10) } as usize;
            let r = crate::net::tcp_close(sid);
            set_result(regs, r as u64);
            0
        }
        SYS_NET_RESOLVE => {
            let name_ptr = unsafe { *regs.add(10) };
            let out_ptr = unsafe { *regs.add(11) };
            let max_ips = (unsafe { *regs.add(12) }).min(8) as usize;
            if !user_ptr_ok(name_ptr) || !user_ptr_ok(out_ptr) || max_ips == 0 {
                set_result(regs, -1i64 as u64);
                return 0;
            }
            // Имя хоста: C-строка, максимум 255 байт.
            let name_bytes = unsafe { core::slice::from_raw_parts(name_ptr as *const u8, 255) };
            let name_len = name_bytes
                .iter()
                .position(|&b| b == 0)
                .unwrap_or(255);
            let name_str = match core::str::from_utf8(&name_bytes[..name_len]) {
                Ok(s) => s,
                Err(_) => {
                    set_result(regs, -1i64 as u64);
                    return 0;
                }
            };
            // Числовой адрес — резолвить нечего, сразу отдаём как есть.
            if let Some(ip) = crate::net::parse_ip(name_str) {
                unsafe { core::ptr::copy_nonoverlapping(ip.as_ptr(), out_ptr as *mut u8, 4) };
                set_result(regs, 1i64 as u64);
                return 0;
            }
            let mut ips = [[0u8; 4]; 8];
            let n = crate::net::dns_resolve(name_str, &mut ips);
            if n > 0 {
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        ips.as_ptr() as *const u8,
                        out_ptr as *mut u8,
                        n * 4,
                    );
                }
                set_result(regs, n as u64);
            } else {
                set_result(regs, -1i64 as u64);
            }
            0
        }
        SYS_AUTH => {
            sys_auth(regs);
            0
        }
        SYS_GETUID => {
            set_result(regs, crate::user::current_uid() as u64);
            0
        }
        SYS_WHOAMI => {
            let buf = unsafe { *regs.add(10) };
            let max = (unsafe { *regs.add(11) }).min(64) as usize;
            let name = crate::user::current_name();
            let b = name.as_bytes();
            if !user_ptr_ok(buf) || max == 0 {
                set_result(regs, -1i64 as u64);
            } else {
                let n = b.len().min(max - 1);
                unsafe {
                    core::ptr::copy_nonoverlapping(b.as_ptr(), buf as *mut u8, n);
                    (buf as *mut u8).add(n).write_volatile(0);
                }
                set_result(regs, n as u64);
            }
            0
        }
        SYS_USERADD => {
            sys_useradd(regs);
            0
        }
        SYS_USERDEL => {
            sys_userdel(regs);
            0
        }
        SYS_EXEC => {
            sys_exec(regs);
            0
        }
        SYS_WAIT => {
            let pid = unsafe { *regs.add(10) } as usize;
            crate::scheduler::wait_for(pid);
            crate::desk::clear_fg();
            set_result(regs, 0);
            // Ждали внутри вызова: задача уходит в Blocked, asm переключится.
            1
        }
        SYS_IOPERM => {
            let r = if crate::user::current_uid() != crate::user::ROOT_UID {
                -1i64 as u64
            } else {
                let port = unsafe { *regs.add(10) } as u16;
                let count = unsafe { *regs.add(11) } as u16;
                let enable = unsafe { *regs.add(12) } != 0;
                let end = (port as u32).saturating_add(count as u32);
                if end > 65536 {
                    -1i64 as u64
                } else {
                    unsafe {
                        crate::interrupts::io_bitmap_set(port as u32, end, enable);
                    }
                    0
                }
            };
            set_result(regs, r);
            0
        }
        SYS_EXIT => {
            crate::scheduler::exit_current();
            1
        }
        _ => {
            set_result(regs, -1i64 as u64);
            0
        }
    };
    // Перед возвратом в asm прерывания обязаны быть выключены: asm-часть
    // (pop/iretq или schedule) не должна прерываться.
    x86_64::instructions::interrupts::disable();
    result
}
