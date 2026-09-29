//! Вытесняющий планировщик задач (single-core, все задачи в ring 0).
//!
//! Задача 0 — оболочка (shell), она создаётся в `init` и живёт всегда.
//! Фоновые задачи запускаются командой `bg` и получают CPU от таймера
//! (PIT, IRQ0). Переключение контекста происходит в обработчике таймера:
//!
//! ```asm
//! timer_entry:
//!     push regs
//!     eoi
//!     mov [CURRENT_SAVE_SLOT], rsp     ; сохранить стек текущей задачи
//!     call schedule                     ; rax = saved_rsp следующей задачи
//!     mov rsp, rax
//!     pop regs
//!     iretq
//! ```
//!
//! `schedule()` — round-robin по массиву TASKS, работает с прерываниями
//! выключенными (внутри обработчика IRQ0) и НИКОГДА не аллоцирует память
//! (иначе возможен дедлок с lock аллокатора, если задача была вытеснена
//! посреди аллокации).
//!
//! VFS общая для шелла и фоновых задач. Доступ к ней сериализуется
//! глобальным `VFS_LOCK` (spinning, см. vfs_lock_yield). Шелл держит лок на
//! время выполнения команды, фоновые задачи — на время своих операций через
//! `with_vfs`. Лок отслеживает владельца (VFS_LOCK_OWNER), а kill()/reap()
//! принудительно снимают его, если жертва держала лок (CRITICAL-3): иначе
//! система навсегда зависла бы на vfs_lock_yield.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::interrupts::{KERNEL_CS, KERNEL_DS, USER_CS, USER_DS};
use crate::paging::AddressSpace;
use crate::vfs::Vfs;

pub const MAX_TASKS: usize = 16;
pub const STACK_SIZE: usize = 64 * 1024;

/// Размер стартового контекста на стеке задачи: 15 регистров + фрейм iretq.
const CONTEXT_SIZE: usize = 20 * 8;

/// RFLAGS при входе в ring 3: IF=1 (прерывания разрешены), IOPL=3 (программы
/// используют прямой in/out для PS/2-клавиатуры и CMOS RTC), бит 1 всегда 1.
const USER_RFLAGS: u64 = 0x3202;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskState {
    Ready,
    Running,
    Blocked,
    Finished,
}

fn state_str(s: TaskState) -> &'static str {
    match s {
        TaskState::Ready => "ready",
        TaskState::Running => "running",
        TaskState::Blocked => "blocked",
        TaskState::Finished => "finished",
    }
}

/// Обёртка над замыканием задачи. Передаётся как тонкий указатель через
/// стартовый контекст (fat-указатель `Box<dyn FnOnce()>` нельзя кастовать в
/// usize — потеряется vtable).
struct TaskClosure(Option<Box<dyn FnOnce()>>);

struct Task {
    name: String,
    state: TaskState,
    ticks: u64,
    last_tick: u64,
    saved_rsp: usize,
    /// Верхушка kernel-стека задачи: пишется в TSS.RSP0 при переключении на
    /// user-задачу, чтобы прерывание/`int 0x80` из ring 3 получило стек ядра.
    kernel_stack_top: usize,
    /// Корень адресного пространства user-задачи (0 = пространство загрузки).
    cr3: u64,
    /// Адресное пространство user-задачи; освобождается при kill/reap.
    space: Option<AddressSpace>,
    /// Тик, после которого Blocked-задача автоматически станет Ready.
    /// 0 — не просыпаться по таймеру (shell в wait_for будится только через
    /// WAITING_ON/exit_current). Устанавливается `block_until_tick`/`sleep_until`.
    wake_tick: u64,
    /// Аппаратная позиция CRTC-курсора, сохранённая при вытеснении задачи.
    /// `cursor::NO_POSITION` — задача ещё не сохраняла (не восстанавливать).
    cursor_pos: u16,
    #[allow(dead_code)]
    stack: Box<[u8]>,
}

static mut TASKS: [Option<Task>; MAX_TASKS] = [
    None, None, None, None, None, None, None, None,
    None, None, None, None, None, None, None, None,
];

/// Индекс текущей (выполняющейся) задачи.
static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// Сюда обработчик таймера сохраняет rsp текущей задачи. Перед переключением
/// `schedule` записывает сюда адрес поля `saved_rsp` следующей задачи.
#[no_mangle]
pub static mut CURRENT_SAVE_SLOT: usize = 0;

/// Глобальный счётчик тиков таймера (~100 Гц).
static TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Корень адресного пространства загрузки (CR3 при старте). В него
/// возвращается планировщик при переключении на kernel-задачу.
static mut BOOT_ROOT: u64 = 0;

/// PID, который ждёт шелл (задача 0). 0 — никто не ждёт. Когда ждущая
/// задача завершается, `exit_current` переводит шелл из Blocked в Ready.
static WAITING_ON: AtomicUsize = AtomicUsize::new(0);

/// Указатель на единственный экземпляр Vfs (устанавливается в init).
static VFS_PTR: AtomicUsize = AtomicUsize::new(0);

/// Сериализует доступ к VFS между шеллом и фоновыми задачами.
///
/// Собственный атомарный lock вместо spinning_top::Spinlock: необходим
/// принудительный сброс из kill()/reap() (CRITICAL-3). Если задача убита,
/// пока держала лок, а лок не снять — все задачи (включая шелл) навсегда
/// застрянут в vfs_lock_yield().
pub static VFS_LOCK: VfsLock = VfsLock::new();

/// Атомарный spin-lock для VFS. Семантика та же, что у спинлока, плюс
/// `force_unlock()` для принудительного снятия у убитой задачи.
pub struct VfsLock {
    locked: AtomicBool,
}

impl VfsLock {
    pub const fn new() -> VfsLock {
        VfsLock {
            locked: AtomicBool::new(false),
        }
    }

    fn try_lock(&self) -> Option<VfsLockGuard<'_>> {
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(VfsLockGuard { lock: self })
        } else {
            None
        }
    }

    fn release(&self) {
        self.locked.store(false, Ordering::Release);
    }

    fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }

    /// Снимает лок ПРИНУДИТЕЛЬНО, игнорируя владельца. Вызов оправдан ТОЛЬКО
    /// из kill_inner/reap_inner для задачи, которая больше никогда не
    /// выполнится (слот освобождается, kernel-стек уничтожается) — её guard
    /// не запустится и не сможет снять чужой захват.
    unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

/// PID владельца VFS_LOCK (0 — нет/устарело). Пишется при КАЖДОМ успешном
/// try_lock (атомарно с ним, под без-прерываний). При drop guard'а НЕ
/// обнуляется намеренно: stale-значение безвредно, потому что kill() смотрит
/// строго на пару «лок занят И владелец == pid», а вот дыра между unlock и
/// записью владельца позволила бы kill() промахнуться мимо владельца и не
/// снять лок (deadlock).
static VFS_LOCK_OWNER: AtomicUsize = AtomicUsize::new(0);

/// RAII-guard VFS_LOCKa: снимает лок при drop. Владельца не трогает
/// (см. VFS_LOCK_OWNER). Живёт на стеке задачи, между задачами не передаётся.
pub struct VfsLockGuard<'a> {
    lock: &'a VfsLock,
}

impl Drop for VfsLockGuard<'_> {
    fn drop(&mut self) {
        self.lock.release();
    }
}

global_asm!(
    ".global timer_entry",
    ".align 16",
    "timer_entry:",
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
    "mov al, 0x20",
    "out 0x20, al",
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
    fn timer_entry();
}

pub fn timer_entry_addr() -> u64 {
    let f: unsafe extern "C" fn() = timer_entry;
    f as usize as u64
}

/// Инициализация: создаёт задачу 0 (shell) и привязывает VFS.
pub fn init() {
    let name = String::from("shell");
    let stack = vec![0u8; STACK_SIZE].into_boxed_slice();
    let kernel_stack_top = stack.as_ptr() as usize + STACK_SIZE;
    unsafe {
        BOOT_ROOT = x86_64::registers::control::Cr3::read().0.start_address().as_u64();
        TASKS[0] = Some(Task {
            name,
            state: TaskState::Running,
            ticks: 0,
            last_tick: 0,
            saved_rsp: 0,
            kernel_stack_top,
            cr3: 0,
            space: None,
            wake_tick: 0,
            cursor_pos: crate::cursor::NO_POSITION,
            stack,
        });
        CURRENT.store(0, Ordering::SeqCst);
        let t: &mut Task = TASKS[0].as_mut().unwrap();
        CURRENT_SAVE_SLOT = &mut t.saved_rsp as *mut usize as usize;
    }
}

/// Устанавливает глобальный указатель на Vfs (вызывается до включения таймера).
pub fn set_vfs(vfs: &mut Vfs) {
    VFS_PTR.store(vfs as *mut Vfs as usize, Ordering::SeqCst);
}

pub fn ticks() -> u64 {
    TICKS.load(Ordering::SeqCst)
}

/// PID текущей (выполняющейся) задачи. Для диагностики (print в задачах).
pub fn current_pid() -> usize {
    CURRENT.load(Ordering::SeqCst)
}

/// Захват VFS_LOCK с уступкой CPU. Спин-лок нереентерабельный, а при
/// вытесняющей многозадачности крутящийся на локе поток способен навсегда
/// отобрать процессор у держателя лока (тикер не передаст ему управление,
/// пока "крутильщик" Ready). hlt() спит до следующего тика - планировщик
/// ротируется, держатель завершает критическую секцию, лок освобождается.
fn vfs_lock_yield() -> VfsLockGuard<'static> {
    loop {
        // Захват + запись владельца — атомарно относительно kill(): между
        // CAS и store(VFS_LOCK_OWNER) не должно быть точки вытеснения, иначе
        // kill() не опознал бы владельца и не снял бы лок (CRITICAL-3).
        if let Some(g) = x86_64::instructions::interrupts::without_interrupts(|| {
            let g = VFS_LOCK.try_lock();
            if g.is_some() {
                VFS_LOCK_OWNER.store(CURRENT.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            g
        }) {
            return g;
        }
        // hlt до следующего тика: планировщик ротируется, держатель лока
        // завершает критическую секцию. ВЫЗЫВАТЬ ТОЛЬКО С IF=1.
        x86_64::instructions::hlt();
    }
}

pub fn vfs_lock() -> VfsLockGuard<'static> {
    vfs_lock_yield()
}

/// Одна попытка захвата VFS_LOCK без ожидания. None — лок занят. Нужен там,
/// где ждать нельзя: append_history в шелле — если фоновая задача держит лок
/// (зависла/убита), шелл обязан остаться отзывчивым, чтобы её можно было
/// `kill`'нуть (CRITICAL-3).
pub fn try_vfs_lock() -> Option<VfsLockGuard<'static>> {
    x86_64::instructions::interrupts::without_interrupts(|| {
        let g = VFS_LOCK.try_lock();
        if g.is_some() {
            VFS_LOCK_OWNER.store(CURRENT.load(Ordering::SeqCst), Ordering::SeqCst);
        }
        g
    })
}

/// Выполняет `f` с доступом к VFS (внутри глобального лока).
pub fn with_vfs<R>(f: impl FnOnce(&mut Vfs) -> R) -> R {
    let _g = vfs_lock_yield();
    let ptr = VFS_PTR.load(Ordering::SeqCst);
    let vfs = unsafe { &mut *(ptr as *mut Vfs) };
    f(vfs)
}

/// Запускает фоновую задачу. Возвращает PID или None (нет свободных слотов).
///
/// Внимание: аллокации выполняются при включённых прерываниях (иначе дедлок
/// с аллокатором, если фоновая задача была вытеснена посреди аллокации).
pub fn spawn(name: &str, entry: Box<dyn FnOnce()>) -> Option<usize> {
    // Создание задачи атомарно против тиков: гонки спавна с вытеснением
    // приводили к порче контекстов (см. bigtodo, zero-writer).
    SCHED_FROZEN.fetch_add(1, Ordering::SeqCst);
    let r = spawn_inner(name, entry);
    SCHED_FROZEN.fetch_sub(1, Ordering::SeqCst);
    r
}

fn spawn_inner(name: &str, entry: Box<dyn FnOnce()>) -> Option<usize> {
    let idx = x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        (1..MAX_TASKS).find(|&i| TASKS[i].is_none())
    })?;

    let name_owned = String::from(name);
    let mut stack = vec![0u8; STACK_SIZE].into_boxed_slice();
    // Яд-маркер 0xBE: если к первому переключению в стеке появятся НУЛИ
    // там, где должен быть яд или контекст - память затёрта чужой записью.
    stack.fill(0xBE);
    let kernel_stack_top = stack.as_ptr() as usize + STACK_SIZE;
    let raw_entry = Box::into_raw(Box::new(TaskClosure(Some(entry)))) as usize;

    {
        let base = stack.as_mut_ptr() as usize;
        let top = base + STACK_SIZE;
        let saved_rsp = top - CONTEXT_SIZE;
        let p = saved_rsp as *mut u64;
        // Регистры (порядок pop в asm): r15..rax, rdi на позиции 10.
        for k in 0..15 {
            unsafe { p.add(k).write(0); }
        }
        unsafe {
            p.add(10).write(raw_entry as u64);        // rdi = TaskClosure*
            p.add(15).write(task_start as usize as u64); // rip
            p.add(16).write(KERNEL_CS as u64);        // cs
            p.add(17).write(0x202u64);                // rflags: IF=1
            p.add(18).write(top as u64);              // rsp
            p.add(19).write(KERNEL_DS as u64);        // ss
        }
        debug_assert!(saved_rsp >= base);
        let _ = base;
    }

    let task = Task {
        name: name_owned,
        state: TaskState::Ready,
        ticks: 0,
        last_tick: TICKS.load(Ordering::SeqCst),
        saved_rsp: (stack.as_ptr() as usize + STACK_SIZE) - CONTEXT_SIZE,
        kernel_stack_top,
        cr3: 0,
        space: None,
        wake_tick: 0,
        cursor_pos: crate::cursor::NO_POSITION,
        stack,
    };

    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        if TASKS[idx].is_some() {
            return None;
        }
        TASKS[idx] = Some(task);
        Some(idx)
    })
}

/// Запускает .bin/ELF-программу в ring 3 (user mode) в собственном адресном
/// пространстве `space`. Первый вызов — `entry_rip` (для ELF это e_entry,
/// для legacy .bin — paging::USER_START), стек — `user_rsp`. При входе
/// rdi/rsi/rdx получают `rdi_val`/`rsi_val`/`rdx_val`: ELF-путь передаёт
/// argc/&argv/vga_offset, legacy-путь — vga_offset/0/0.
#[allow(clippy::too_many_arguments)]
pub fn spawn_user(
    name: &str,
    space: AddressSpace,
    entry_rip: u64,
    user_rsp: u64,
    rdi_val: u64,
    rsi_val: u64,
    rdx_val: u64,
) -> Option<usize> {
    SCHED_FROZEN.fetch_add(1, Ordering::SeqCst);
    let r = spawn_user_inner(name, space, entry_rip, user_rsp, rdi_val, rsi_val, rdx_val);
    SCHED_FROZEN.fetch_sub(1, Ordering::SeqCst);
    r
}

fn spawn_user_inner(
    name: &str,
    space: AddressSpace,
    entry_rip: u64,
    user_rsp: u64,
    rdi_val: u64,
    rsi_val: u64,
    rdx_val: u64,
) -> Option<usize> {
    let idx = x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        (1..MAX_TASKS).find(|&i| TASKS[i].is_none())
    })?;

    let name_owned = String::from(name);
    let root = space.root_frame().start_address().as_u64();
    let mut stack = vec![0u8; STACK_SIZE].into_boxed_slice();
    // Яд-маркер 0xBE (см. spawn_inner).
    stack.fill(0xBE);
    let kernel_stack_top = stack.as_ptr() as usize + STACK_SIZE;

    {
        let base = stack.as_mut_ptr() as usize;
        let top = base + STACK_SIZE;
        let saved_rsp = top - CONTEXT_SIZE;
        let p = saved_rsp as *mut u64;
        for k in 0..15 {
            unsafe { p.add(k).write(0); }
        }
        unsafe {
            p.add(10).write(rdi_val);             // rdi = argc | vga_offset
            p.add(11).write(rsi_val);             // rsi = &argv[0] | 0
            p.add(12).write(rdx_val);             // rdx = vga_offset | 0
            p.add(15).write(entry_rip);           // rip = e_entry | USER_START
            p.add(16).write(USER_CS as u64);      // cs (ring 3)
            p.add(17).write(USER_RFLAGS);         // rflags: IF=1, IOPL=3
            p.add(18).write(user_rsp);            // rsp: setup_user_stack вернул
                                                  // rsp, указывающий на слот
                                                  // exit-стаба (ret из _start
                                                  // уходит туда)
            p.add(19).write(USER_DS as u64);      // ss (ring 3)
        }
        debug_assert!(saved_rsp >= base);
        crate::vga::serial_write_atomic("[spawn-frame] rsp=");
        crate::vga::serial_u64(saved_rsp as u64);
        crate::vga::serial_write_atomic(" name=");
        for &c in name.as_bytes().iter().take(8) { crate::vga::serial_putchar(c); }
        crate::vga::serial_write_atomic("\n");
        let _ = base;
    }

    let mut task = Task {
        name: name_owned,
        state: TaskState::Ready,
        ticks: 0,
        last_tick: TICKS.load(Ordering::SeqCst),
        saved_rsp: (stack.as_ptr() as usize + STACK_SIZE) - CONTEXT_SIZE,
        kernel_stack_top,
        cr3: root,
        space: Some(space),
        wake_tick: 0,
        cursor_pos: crate::cursor::NO_POSITION,
        stack,
    };

    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        if TASKS[idx].is_some() {
            if let Some(mut s) = task.space.take() {
                crate::paging::destroy_address_space(&mut s);
            }
            return None;
        }
        TASKS[idx] = Some(task);
        Some(idx)
    })
}

/// Свободен ли слот для новой задачи (до создания адресного пространства).
pub fn slot_free() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        (1..MAX_TASKS).any(|i| TASKS[i].is_none())
    })
}

/// Убивает задачу и освобождает её слот: задача исчезает из `ps`.
///
/// Задачу нельзя убить, пока она выполняется (текущий слот) — команда kill
/// приходит только из шелла (задача 0), поэтому убиваемая задача всегда либо
/// Ready, либо Finished и после удаления просто никогда не планируется.
/// Стек и адресное пространство освобождаются вне критической секции, чтобы
/// не дедлокнуть аллокатор, если другая задача была вытеснена посреди
/// аллокации.
pub fn kill(pid: usize) -> bool {
    SCHED_FROZEN.fetch_add(1, Ordering::SeqCst);
    let r = kill_inner(pid);
    SCHED_FROZEN.fetch_sub(1, Ordering::SeqCst);
    r
}

fn kill_inner(pid: usize) -> bool {
    let removed = x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        if pid == 0 || pid >= MAX_TASKS {
            return None;
        }
        // CRITICAL-3: жертва могла держать VFS_LOCK. init-супервизор и шелл
        // (ветка `kill` — до захвата лока) убивают задачу БЕЗ удержания
        // VFS_LOCK, а лок нереентерабельный: если жертва владела им, все
        // остальные задачи навсегда застряли бы в vfs_lock_yield. Снимаем
        // лок принудительно — жертва больше не выполнится (слот освобождается,
        // стек уничтожается), её guard не запустится и не затрёт чужой захват.
        // Исключение — сама текущая задача: её guard ещё жив, сбрасывать лок
        // нельзя (впрочем, kill текущей задачи невозможен — defensive).
        let cur = CURRENT.load(Ordering::SeqCst);
        if pid != cur && VFS_LOCK.is_locked() && VFS_LOCK_OWNER.load(Ordering::SeqCst) == pid {
            VFS_LOCK.force_unlock();
            VFS_LOCK_OWNER.store(0, Ordering::SeqCst);
            crate::vga::serial_write_atomic("[kill] VFS_LOCK held by target; forced release\n");
        }
        TASKS[pid].take()
    });
    let existed = removed.is_some();
    if let Some(mut t) = removed {
        if let Some(mut space) = t.space.take() {
            crate::paging::destroy_address_space(&mut space);
        }
    }
    existed
}

/// Забирает завершившуюся задачу у планировщика: освобождает kernel-стек и
/// адресное пространство. Вызывается шеллом после `wait_for`.
pub fn reap(pid: usize) {
    SCHED_FROZEN.fetch_add(1, Ordering::SeqCst);
    reap_inner(pid);
    SCHED_FROZEN.fetch_sub(1, Ordering::SeqCst);
}

fn reap_inner(pid: usize) {
    let removed = x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        if pid == 0 || pid >= MAX_TASKS {
            return None;
        }
        // Defensive-аналог kill_inner: завершившаяся задача лоб не держит,
        // но если между exit и reap случилось что-то нештатное — лок должен
        // быть снят, чтобы не повесить шелл.
        let cur = CURRENT.load(Ordering::SeqCst);
        if pid != cur && VFS_LOCK.is_locked() && VFS_LOCK_OWNER.load(Ordering::SeqCst) == pid {
            VFS_LOCK.force_unlock();
            VFS_LOCK_OWNER.store(0, Ordering::SeqCst);
            crate::vga::serial_write_atomic("[reap] VFS_LOCK held by reaped task; forced release\n");
        }
        TASKS[pid].take()
    });
    if let Some(mut t) = removed {
        if let Some(mut space) = t.space.take() {
            crate::paging::destroy_address_space(&mut space);
        }
    }
}

/// Блокирует шелл (задачу 0) до завершения задачи `pid` (user-программы,
/// запущенной в foreground). Шелл остаётся Blocked, таймер переключает CPU на
/// user-задачу; когда та завершается (`exit`/фолт), `exit_current` будит шелл.
pub fn wait_for(pid: usize) {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        if CURRENT.load(Ordering::SeqCst) == 0 {
            if let Some(t) = TASKS[0].as_mut() {
                if t.state == TaskState::Running {
                    t.state = TaskState::Blocked;
                }
                // wake_tick остаётся 0: шелл будит только exit_current.
            }
            WAITING_ON.store(pid, Ordering::SeqCst);
        }
    });
    loop {
        let done = x86_64::instructions::interrupts::without_interrupts(|| unsafe {
            match TASKS.get(pid).and_then(|t| t.as_ref()) {
                None => true,
                Some(t) => t.state == TaskState::Finished,
            }
        });
        if done {
            break;
        }
        x86_64::instructions::hlt();
    }
    x86_64::instructions::interrupts::without_interrupts(|| {
        WAITING_ON.store(0, Ordering::SeqCst);
    });
}

/// Помечает текущую задачу Blocked до тика `target`; планировщик переведёт её
/// в Ready, когда `ticks >= target`. Сама задача при этом не спит (не делает
/// hlt) — она просто уходит с ротации до указанного момента, поэтому спящие
/// фоновые задачи не съедают кванты round-robin и не замедляют шелл.
///
/// Вызывается и из syscall-пути (SYS_HLT/SYS_SLEEP), где дальше идёт
/// переключение через `schedule`, и из `sleep_until` (kernel-задачи).
pub fn block_until_tick(target: u64) {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let cur = CURRENT.load(Ordering::SeqCst);
        if let Some(t) = TASKS[cur].as_mut() {
            if t.state == TaskState::Running {
                t.state = TaskState::Blocked;
            }
            t.wake_tick = target;
        }
    });
}

/// Блокирующий сон до тика `target` для kernel-задач (демоны, супервизор,
/// воркеры). Пока задача ждёт, она не в ротации: если ничего больше нет Ready,
/// планировщик вернётся к ней (fallback) и она пере-заблокируется на hlt.
pub fn sleep_until(target: u64) {
    while crate::scheduler::ticks() < target {
        block_until_tick(target);
        x86_64::instructions::hlt();
    }
}

/// Отмечает текущую задачу завершённой и будит шелл, если он ждёт именно её.
/// Вызывается из диспетчера `exit()` и из обработчика user-исключений.
/// Дальше asm (`int80`/фолт-путь) переключается на следующую задачу.
pub fn exit_current() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let cur = CURRENT.load(Ordering::SeqCst);
        if let Some(t) = TASKS[cur].as_mut() {
            if t.state != TaskState::Finished {
                t.state = TaskState::Finished;
            }
        }
        if WAITING_ON.load(Ordering::SeqCst) == cur {
            if let Some(shell) = TASKS[0].as_mut() {
                if shell.state == TaskState::Blocked {
                    shell.state = TaskState::Ready;
                }
            }
        }
    });
}

/// Завершение user-задачи из-за исключения (ring 3). Не возвращается:
/// помечает задачу завершённой, будит шелл и через `schedule` iretq'ом
/// переключается на следующую задачу (аналогично пути switch_away в int80).
pub fn user_fault_exit() -> ! {
    exit_current();
    unsafe {
        core::arch::asm!(
            "mov rax, [rip + CURRENT_SAVE_SLOT]",
            "mov [rax], rsp",
            "sub rsp, 8",
            "and rsp, -16",
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
            options(noreturn)
        );
    }
}

pub struct ProcInfo {
    pub pid: usize,
    pub name: [u8; 32],
    pub state: &'static str,
    pub ticks: u64,
}

/// Заполняет `out` информацией о задачах для команды `ps`.
/// Внутри критической секции не аллоцирует (out резервируется заранее).
pub fn list(out: &mut Vec<ProcInfo>) {
    out.reserve(MAX_TASKS);
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        out.clear();
        for i in 0..MAX_TASKS {
            if let Some(t) = TASKS[i].as_ref() {
                let mut name = [0u8; 32];
                let b = t.name.as_bytes();
                let n = b.len().min(31);
                name[..n].copy_from_slice(&b[..n]);
                out.push(ProcInfo {
                    pid: i,
                    name,
                    state: state_str(t.state),
                    ticks: t.ticks,
                });
            }
        }
    });
}

/// Текущее состояние задачи по PID (для init-супервизора). Не аллоцирует.
/// None — задача отсутствует (убита или не существовала).
pub fn task_state(pid: usize) -> Option<TaskState> {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        TASKS.get(pid).and_then(|t| t.as_ref()).map(|t| t.state)
    })
}

/// Имя текущей задачи (для сообщений об исключениях). Не аллоцирует.
pub fn current_name() -> [u8; 32] {
    let mut buf = [0u8; 32];
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let cur = CURRENT.load(Ordering::SeqCst);
        if let Some(t) = TASKS[cur].as_ref() {
            let b = t.name.as_bytes();
            let n = b.len().min(31);
            buf[..n].copy_from_slice(&b[..n]);
        }
    });
    buf
}

/// Заморозка планирования: критические секции ядра (фиксация WAL), которым
/// нужен монопольный доступ к данным задач/кучи БЕЗ переключения контекста,
/// но БЕЗ запрета прерываний (чтобы клавиатура/таймер продолжали работать).
pub static SCHED_FROZEN: core::sync::atomic::AtomicU32 =
    core::sync::atomic::AtomicU32::new(0);

/// Вызывается из asm-обработчика таймера: round-robin выбор следующей задачи.
#[no_mangle]
extern "C" fn schedule() -> usize {
    let t0 = TICKS.fetch_add(1, Ordering::SeqCst);
    // Планирование заморожено: вернуть контекст ТЕКУЩЕЙ задачи (уже
    // сохранённый в CURRENT_SAVE_SLOT) без выбора другой.
    if SCHED_FROZEN.load(Ordering::SeqCst) != 0 {
        unsafe {
            let cur = CURRENT.load(Ordering::SeqCst);
            if let Some(t) = TASKS[cur].as_mut() {
                t.ticks += 1;
            }
            return TASKS[cur].as_ref().unwrap().saved_rsp;
        }
    }
    unsafe {
        let cur = CURRENT.load(Ordering::SeqCst);
        let ticks = TICKS.load(Ordering::SeqCst);

        if let Some(t) = TASKS[cur].as_mut() {
            if t.state == TaskState::Running {
                t.state = TaskState::Ready;
                t.ticks += ticks - t.last_tick;
                t.last_tick = ticks;
            }
        }

        // Пробуждаем спящие задачи, чей срок настал: Blocked + wake_tick <= now
        // становятся Ready и участвуют в round-robin. Включая задачу 0 (шелл):
        // её sleep_until/block_until_tick тоже ставят wake_tick, без этого
        // пункта шелл мог бы проснуться только через fallback (пока все прочие
        // задачи спят) — первое же пробуждение фоновой задачи навсегда
        // оставляло шелл Blocked с просроченным wake_tick (зависание ping).
        // wait_for не задевается: там wake_tick == 0.
        for i in 0..MAX_TASKS {
            if let Some(t) = TASKS[i].as_mut() {
                if t.state == TaskState::Blocked && t.wake_tick != 0 && t.wake_tick <= ticks {
                    t.state = TaskState::Ready;
                    t.wake_tick = 0;
                }
            }
        }

        let mut next = cur;
        for i in 1..=MAX_TASKS {
            let idx = (cur + i) % MAX_TASKS;
            if let Some(t) = TASKS[idx].as_ref() {
                if t.state == TaskState::Ready {
                    next = idx;
                    break;
                }
            }
        }

        // Сохраняем аппаратный курсор уходящей задачи, восстанавливаем —
        // у входящей (в switch_to_task). Курсором владеют только задачи,
        // работающие с дисплеем: шелл (pid 0) и user-задачи (cr3 != 0).
        // Ядрёные демоны (ticker/init/clock-служба и т.п.) экран не трогают —
        // для них save/restore только перезаписал бы CRTC устаревшим значением.
        if next != cur {
            let cur_owns_display = cur == 0
                || TASKS[cur]
                    .as_ref()
                    .map_or(false, |t| t.cr3 != 0);
            if cur_owns_display {
                if let Some(t) = TASKS[cur].as_mut() {
                    t.cursor_pos = crate::cursor::get_position();
                }
            }
        }

        CURRENT.store(next, Ordering::SeqCst);
        let t = TASKS[next].as_mut().unwrap();
        // ДЕТЕКТОР zero-writer: у готовой задачи rip-слот контекста обязан
        // быть ненулевым, а яд 0xBE сразу ПОД контекстом - не тронутым.
        // Нули там = чужая запись. Печатаем адрес жертвы и встаём на int3:
        // под gdb это останов с полным контекстом, без gdb - видный сбой.
        {
            let base = t.stack.as_ptr() as usize;
            let rsp = t.saved_rsp;
            if rsp >= base && rsp + 160 <= base + t.stack.len() {
                unsafe {
                    let rip = *((rsp + 120) as *const u64);
                    let poison = *((rsp - 8) as *const u64);
                    if rip == 0 || poison == 0 {
                        crate::vga::serial_write_atomic("[ZERO-WRITER] pid=");
                        crate::vga::serial_u64(next as u64);
                        crate::vga::serial_write_atomic(" rsp=");
                        crate::vga::serial_u64(rsp as u64);
                        crate::vga::serial_write_atomic(" rip=");
                        crate::vga::serial_u64(rip);
                        crate::vga::serial_write_atomic(" poison=");
                        crate::vga::serial_u64(poison);
                        crate::vga::serial_write_atomic("\n");
                        t.state = TaskState::Running;
                        core::arch::asm!("int3", options(nomem));
                    }
                }
            }
        }
        t.state = TaskState::Running;
        t.ticks += ticks - t.last_tick;
        t.last_tick = ticks;
        CURRENT_SAVE_SLOT = core::ptr::addr_of_mut!(t.saved_rsp) as usize;
        switch_to_task(t, next);
        t.saved_rsp
    }
}

/// Переключает адресное пространство и стек ring 0 под задачу `t`, которая
/// сейчас выходит на исполнение. Для user-задач грузится её CR3 и в TSS.RSP0
/// пишется верхушка kernel-стека (туда CPU положит фрейм при прерывании из
/// ring 3). Для kernel-задач — пространство загрузки.
///
/// Дальнейшие инструкции asm (pop-регистров, iretq) обращаются только к
/// kernel-стеку, который зеркалируется в любом адресном пространстве, поэтому
/// смена CR3 здесь безопасна.
fn switch_to_task(t: &Task, pid: usize) {
    unsafe {
        let root = if t.cr3 != 0 {
            t.cr3
        } else {
            BOOT_ROOT
        };
        if root != x86_64::registers::control::Cr3::read().0.start_address().as_u64() {
            let frame = x86_64::structures::paging::PhysFrame::containing_address(
                x86_64::PhysAddr::new(root),
            );
            crate::paging::switch_to_root(frame);
        }
        crate::interrupts::set_tss_rsp0(t.kernel_stack_top as u64);
    }
    // Restore только для владельцев дисплея (см. save в schedule).
    if (pid == 0 || t.cr3 != 0) && t.cursor_pos != crate::cursor::NO_POSITION {
        crate::cursor::set_raw(t.cursor_pos);
    }
}

/// Точка входа фоновой задачи: вызывается с rdi = указатель на TaskClosure.
/// После завершения отмечает задачу Finished и вечно спит на hlt.
#[no_mangle]
extern "C" fn task_start(ptr: usize) {
    let tc = unsafe { &mut *(ptr as *mut TaskClosure) };
    let f = tc.0.take().expect("task closure already taken");
    f();
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let cur = CURRENT.load(Ordering::SeqCst);
        if let Some(t) = TASKS[cur].as_mut() {
            if t.state == TaskState::Running {
                t.state = TaskState::Finished;
            }
        }
    });
    loop {
        x86_64::instructions::hlt();
    }
}
