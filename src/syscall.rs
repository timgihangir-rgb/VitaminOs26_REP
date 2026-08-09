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
//!
//! Аргументы (по SysV) приходят в rdi/rsi/rdx и лежат в сохранённых регистрах
//! с той же раскладкой, что в `timer_entry` планировщика. Вершина стека после
//! 15 push'ей — это r15, поэтому индексы (rsp[0]..rsp[14]) идут от r15 к rax:
//!   [0]=r15 [1]=r14 [2]=r13 [3]=r12 [4]=r11 [5]=r10 [6]=r9 [7]=r8
//!   [8]=rbx [9]=rbp [10]=rdi [11]=rsi [12]=rdx [13]=rcx [14]=rax

use core::arch::global_asm;

pub const SYS_TICKS: usize = 0;
pub const SYS_HLT: usize = 1;
pub const SYS_VFS_WRITE: usize = 2;
pub const SYS_VFS_READ: usize = 3;
pub const SYS_EXIT: usize = 4;
pub const SYS_SLEEP: usize = 5;

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
