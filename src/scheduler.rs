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
//! `with_vfs`. Лок отслеживает владельца, а kill()/reap()/pause()
//! принудительно снимают его, если жертва держала лок (CRITICAL-3): иначе
//! система навсегда зависла бы на vfs_lock_yield.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::arch::global_asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::interrupts::{KERNEL_CS, KERNEL_DS, USER_CS, USER_DS};
use crate::paging::AddressSpace;
use crate::vfs::Vfs;

/// Слотов задач. Четыре шелла рабочих столов + супервизор + его службы +
/// foreground-программы + фоновые задачи с запасом.
pub const MAX_TASKS: usize = 24;
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
    /// 0 — не просыпаться по таймеру: задача выйдет из Blocked только явно
    /// (`resume`). Устанавливается `block_until_tick`/`sleep_until`.
    wake_tick: u64,
    /// Задача приостановлена и не участвует в ротации (см. `pause`/`resume`).
    /// Нужна рабочим столам: неактивный терминал не должен получать кванты
    /// CPU — иначе его шелл читал бы клавиатуру и писал бы в экран активного.
    suspended: bool,
    #[allow(dead_code)]
    stack: Box<[u8]>,
}

static mut TASKS: [Option<Task>; MAX_TASKS] = [
    None, None, None, None, None, None, None, None, None, None, None, None,
    None, None, None, None, None, None, None, None, None, None, None, None,
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
/// `force_unlock()` для принудительного снятия у задачи, которая лок уже не
/// отпустит (убита или приостановлена).
///
/// Состояние и владелец живут в ОДНОМ атомике: 0 — свободен, иначе PID
/// держателя. Раньше это были отдельные `locked: AtomicBool` и
/// `VFS_LOCK_OWNER: AtomicUsize`, и «проверить, держит ли задача лок» было
/// неатомарной парой — между чтениями владелец мог смениться, и принудительный
/// сброс промахивался бы мимо настоящего держателя. С одним полем такая гонка
/// невозможна по построению.
pub struct VfsLock {
    owner: AtomicUsize,
}

impl VfsLock {
    pub const fn new() -> VfsLock {
        VfsLock {
            owner: AtomicUsize::new(0),
        }
    }

    fn try_lock(&self) -> Option<VfsLockGuard<'_>> {
        let me = CURRENT.load(Ordering::SeqCst);
        self.owner
            .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
            .then(|| VfsLockGuard { lock: self, me })
    }

    fn release(&self, me: usize) {
        // Снимаем лок, только если он всё ещё наш. CAS провалится, если нас
        // уже сняли принудительно (kill/pause) и лок успел взять кто-то ещё —
        // тогда guard не должен снимать чужой захват.
        let _ = self.owner.compare_exchange(me, 0, Ordering::Release, Ordering::Relaxed);
    }

    /// PID текущего держателя (0 — лок свободен).
    fn holder(&self) -> usize {
        self.owner.load(Ordering::SeqCst)
    }

    /// Снимает лок ПРИНУДИТЕЛЬНО, игнорируя владельца. Вызов оправдан ТОЛЬКО
    /// для задачи, которая больше не снимет лок сама: её убили (kill/reap —
    /// слот освобождается, стек уничтожается) или приостановили (pause —
    /// guard лежит на стеке задачи, которая не идёт). Guard такой задачи при
    /// возможном возобновлении окажется no-op: CAS не сойдётся, если лок к
    /// тому моменту уже заняли другие.
    unsafe fn force_unlock(&self) {
        self.owner.store(0, Ordering::Release);
    }
}

/// RAII-guard VFS_LOCK: снимает лок при drop, если лок всё ещё принадлежит
/// той же задаче. Живёт на стеке задачи, между задачами не передаётся.
pub struct VfsLockGuard<'a> {
    lock: &'a VfsLock,
    /// PID, под которым лок был взят: по нему же идёт CAS при drop.
    me: usize,
}

impl Drop for VfsLockGuard<'_> {
    fn drop(&mut self) {
        self.lock.release(self.me);
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
            suspended: false,
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

/// Сырой указатель на общий Vfs БЕЗ захвата лока — для долгоживущих
/// владельцев. Так работает и основной шелл (`main` передаёт `&mut vfs` на
/// всю сессию); тем же способом дополнительные шеллы рабочих столов держат
/// свой `&mut Vfs`. Одновременно активен только один стол, поэтому
/// пересечений по этой ссылке не возникает.
pub fn vfs_ptr() -> *mut Vfs {
    VFS_PTR.load(Ordering::SeqCst) as *mut Vfs
}

/// PID текущей (выполняющейся) задачи. Для диагностики (print в задачах).
pub fn current_pid() -> usize {
    CURRENT.load(Ordering::SeqCst)
}

/// Число активных задач (для sysinfo/meminfo).
pub fn task_count() -> usize {
    unsafe { TASKS.iter().filter(|s| s.is_some()).count() }
}

/// Сколько задач готово к запуску (размер очереди готовых, как load average
/// в Linux). Считаем все, кроме текущей: она уже выполняется.
pub fn ready_count() -> usize {
    let cur = CURRENT.load(Ordering::SeqCst);
    unsafe {
        TASKS.iter()
            .enumerate()
            .filter(|(i, s)| *i != cur && s.as_ref().map_or(false, |t| t.state == TaskState::Ready))
            .count()
    }
}

/// Захват VFS_LOCK с уступкой CPU. Спин-лок нереентерабельный, а при
/// вытесняющей многозадачности крутящийся на локе поток способен навсегда
/// отобрать процессор у держателя лока (тикер не передаст ему управление,
/// пока "крутильщик" Ready). hlt() спит до следующего тика - планировщик
/// ротируется, держатель завершает критическую секцию, лок освобождается.
fn vfs_lock_yield() -> VfsLockGuard<'static> {
    loop {
        // Захват атомарен относительно kill()/pause(): владелец пишется тем же
        // CAS, поэтому проверка «лок занят И владелец == pid» не может
        // разъехаться (CRITICAL-3).
        if let Some(g) = x86_64::instructions::interrupts::without_interrupts(|| VFS_LOCK.try_lock())
        {
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
    x86_64::instructions::interrupts::without_interrupts(|| VFS_LOCK.try_lock())
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
    spawn_flagged(name, entry, false)
}

/// Как `spawn`, но задача сразу приостановлена: в ротацию не попадает, пока
/// не вызовут `resume`. Нужно рабочим столам — их шеллы стартуют незаметно,
/// иначе первый же из них напечатал бы пригла поверх активного экрана.
pub fn spawn_suspended(name: &str, entry: Box<dyn FnOnce()>) -> Option<usize> {
    spawn_flagged(name, entry, true)
}

fn spawn_flagged(name: &str, entry: Box<dyn FnOnce()>, suspended: bool) -> Option<usize> {
    // Создание задачи атомарно против тиков: гонки спавна с вытеснением
    // приводили к порче контекстов (см. bigtodo, zero-writer).
    SCHED_FROZEN.fetch_add(1, Ordering::SeqCst);
    let r = spawn_inner(name, entry, suspended);
    SCHED_FROZEN.fetch_sub(1, Ordering::SeqCst);
    r
}

fn spawn_inner(name: &str, entry: Box<dyn FnOnce()>, suspended: bool) -> Option<usize> {
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
        suspended,
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
        suspended: false,
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
        if pid != cur && VFS_LOCK.holder() == pid {
            VFS_LOCK.force_unlock();
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
        if pid != cur && VFS_LOCK.holder() == pid {
            VFS_LOCK.force_unlock();
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

/// Блокирует шелл до завершения foreground-задачи `pid`, отдавая CPU программе.
///
/// Каждый проход зовёт `poll(pid)`: вернул true — задача убивается, ожидание
/// заканчивается. Возвращает true, если ожидание прервано, false — если
/// программа завершилась сама.
///
/// Ждать приходится не «до упора», а по тику: шелл, ушедший в Blocked до конца
/// программы, планировщиком больше не запускается, и `poll` (а с ним Ctrl+C) не
/// выполнился бы никогда. 100 пробуждений в секунду — ровно столько же, сколько
/// обычных preempt-переключений.
///
/// Прерываемость нужна шеллу: пока программа работает, шеллу некуда деться,
/// а нажать Ctrl+C может быть нужно именно сейчас — snake/web/vita сами
/// Ctrl+C не понимают.
pub fn wait_for_polled(pid: usize, poll: fn(usize) -> bool) -> bool {
    let mut interrupted = false;
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
        if poll(pid) {
            kill(pid);
            interrupted = true;
            break;
        }
        // Ждём следующего тика, но остаёмся в ротации: задача, ушедшая в
        // Blocked до конца программы, планировщиком больше не запускается —
        // и poll (а с ним Ctrl+C) не выполнился бы никогда. Поэтому
        // засыпаем ровно на тик и просыпаемся: это 100 раз в секунду, ровно
        // столько же, сколько обычных preempt-переключений.
        let target = TICKS.load(Ordering::SeqCst) + 1;
        block_until_tick(target);
        while TICKS.load(Ordering::SeqCst) < target {
            x86_64::instructions::hlt();
        }
    }
    interrupted
}

/// Приостанавливает задачу: она исключается из ротации до `resume`.
/// Состояние (Running/Ready/Blocked) не трогаем — задача просто перестаёт
/// получать кванты CPU, но остаётся на своём месте стека и в своём адресном
/// пространстве.
///
/// Если задача держит VFS_LOCK, лок снимается принудительно: guard лежит на
/// стеке задачи, которая не пойдёт, и все остальные задачи (включая шеллы
/// других рабочих столов) зависли бы в `vfs_lock_yield` навсегда. При
/// `resume` guard вернённой задачи окажется no-op: CAS не сойдётся, если лок
/// к тому моменту уже заняли другие (см. `VfsLock::release`).
pub fn pause(pid: usize) -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        match TASKS.get_mut(pid).and_then(|t| t.as_mut()) {
            Some(t) => {
                t.suspended = true;
                if VFS_LOCK.holder() == pid {
                    VFS_LOCK.force_unlock();
                    crate::vga::serial_write_atomic("[pause] VFS_LOCK held by target; forced release\n");
                }
                true
            }
            None => false,
        }
    })
}

/// Возобновляет приостановленную задачу. Задача, замороженная в Running
/// (её состояние не успело превратиться в Ready, потому что вытеснения не
/// было), возвращается в ротацию. Спящая с wake_tick == 0 — тоже: шелл
/// рабочего стола после `pause` лежит именно в таком состоянии.
pub fn resume(pid: usize) -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        match TASKS.get_mut(pid).and_then(|t| t.as_mut()) {
            Some(t) => {
                t.suspended = false;
                if (t.state == TaskState::Running || t.state == TaskState::Blocked)
                    && t.wake_tick == 0
                {
                    t.state = TaskState::Ready;
                }
                true
            }
            None => false,
        }
    })
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
    /// Приостановлена ли задача (`pause`) — у неактивных рабочих столов.
    pub suspended: bool,
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
                    suspended: t.suspended,
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
                // Приостановленные задачи (неактивные рабочие столы) из
                // ротации выпадают.
                if t.state == TaskState::Ready && !t.suspended {
                    next = idx;
                    break;
                }
            }
        }

        // Аппаратный курсор планировщик НЕ трогает. Раньше он сохранялся при
        // вытеснении задачи и восстанавливался при входе, но видеопамять и
        // CRTC-курсор у всех задач общие, а значение «своё» у задачи не
        // появляется само: как только сохранённое значение однажды устарело,
        // петля «restore устаревшего → save устаревшего» замыкается навсегда.
        // На практике курсор застревал в произвольной пустой клетке экрана и
        // «пропадал» из промпта. Позицию курсора и так восстанавливает
        // владелец экрана: шелл ставит её в Writer при каждом выводе, а
        // exec.rs — после завершения программы (exit_row/exit_col).

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
        // Время выполнения уже учтено выше, в ветке уходящей задачи: там
        // стоит `ticks += ticks - t.last_tick`. Здесь её `last_tick` уже
        // равен текущему тику, и повторный += списывал бы на входящую
        // задачу время, которое отработали другие: сумма %CPU по задачам
        // доходила до 600% при шести задачах. Здесь только переносим начало
        // отсчёта: с этого момента задача выполняется.
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
    let _ = pid;
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
