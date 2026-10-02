// src/desk.rs
//
// Виртуальные рабочие столы: четыре независимых терминала, переключаются
// Ctrl+Shift+1..4.
//
// У каждого стола своя задача-шелл, и одновременно активен только один из
// них: неактивные столы приостановлены (`scheduler::pause`) и не получают
// квантов CPU. Это ключевое условие — иначе четыре шелла делили бы один
// буфер клавиатуры (все четверо читали бы одни и те же нажатия) и один
// текстовый экран.
//
// Экран переключается копированием: уходящий стол снимает содержимое
// видеопамяти себе, новый — выводит своё. Поэтому активный стол всегда
// рисует прямо в 0xB8000, и ring3-программы (vita, web, snake), которые
// пишут туда напрямую, работают без всяких переадресаций.
//
// Активен ровно один стол, и его задача никогда не приостановлена: пока фокус
// не вернулся, стол вместе со своей foreground-программой просто спит.

use crate::keyboard;
use crate::scheduler;
use crate::vfs::Vfs;
use crate::vga::{self, Writer};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Сколько рабочих столов.
pub const COUNT: usize = 4;

/// Состояние одного рабочего стола.
struct Desk {
    /// Копия текстового экрана: символ + атрибут на ячейку.
    screen: Box<[u8]>,
    /// Позиция аппаратного курсора, чтобы вернуть её при возврате.
    cur: (usize, usize),
    /// Рабочий каталог: он хранится в общем Vfs, поэтому при переключении
    /// мы его выгружаем и загружаем заново — иначе у всех столов был бы
    /// один и тот же `cd`.
    cwd: String,
    /// PID задачи-шелла этого стола.
    shell: usize,
    /// PID foreground-программы стола (vita/web/snake), если она работает.
    fg: Option<usize>,
}

static mut DESKS: [Option<Desk>; COUNT] = [None, None, None, None];
/// Индекс активного стола.
static FOCUS: AtomicUsize = AtomicUsize::new(0);

fn desks() -> &'static mut [Option<Desk>; COUNT] {
    unsafe { &mut DESKS }
}

fn desk(i: usize) -> Option<&'static mut Desk> {
    if i >= COUNT {
        return None;
    }
    unsafe { DESKS.as_mut().get_unchecked_mut(i).as_mut() }
}

/// Активный стол.
pub fn focused() -> usize {
    FOCUS.load(Ordering::SeqCst)
}

/// Индекс стола, в котором работает текущая задача. Обычно совпадает с
/// `focused()`, но не сразу после переключения: старый стол ещё выполняет
/// `wait_for_polled` у программы, которую он только что запустил, и должен
/// знать, что foreground — его, а не активного.
pub fn current() -> usize {
    let pid = scheduler::current_pid();
    for i in 0..COUNT {
        if let Some(d) = desk(i) {
            if d.shell == pid {
                return i;
            }
        }
    }
    focused()
}

/// Активен ли стол текущей задачи.
pub fn is_current_active() -> bool {
    current() == focused()
}

/// Ждёт, пока активным станет стол текущей задачи. Нужен `top`: он занимает
/// весь экран, поэтому пока на экране чужой стол — обязан молчать. Обычно
/// задача в этот момент уже приостановлена (`activate` вызывает `pause`), и
/// цикл просто не исполняется до возврата стола.
pub fn wait_while_inactive() {
    while !is_current_active() {
        let now = scheduler::ticks();
        scheduler::sleep_until(now + 1);
    }
}

fn new_desk(shell: usize, cwd: String) -> Desk {
    let mut screen = alloc::vec![0u8; vga::SCREEN_BYTES].into_boxed_slice();
    vga::screen_blank(&mut screen);
    Desk {
        screen,
        cur: (0, 0),
        cwd,
        shell,
        fg: None,
    }
}

/// Снимает содержимое экрана стола `i` в его личный буфер.
fn save(i: usize) {
    let Some(d) = desk(i) else { return };
    vga::screen_snapshot(&mut d.screen);
    d.cur = vga::hw_cursor_pos();
    let p = scheduler::vfs_ptr();
    if !p.is_null() {
        d.cwd = unsafe { (*p).pwd() };
    }
}

/// Выводит сохранённый экран стола `i` на дисплей.
fn load(i: usize) {
    let Some(d) = desk(i) else { return };
    vga::screen_restore(&d.screen);
    vga::set_hw_cursor(d.cur.0, d.cur.1);
    let p = scheduler::vfs_ptr();
    if !p.is_null() {
        unsafe { (*p).set_cwd(&d.cwd) };
    }
}

/// Забывает foreground-программу стола текущей задачи: она завершилась, и
/// стол снова свободен для следующей.
pub fn clear_fg() {
    let me = current();
    if let Some(d) = desk(me) {
        d.fg = None;
    }
}

/// Запоминает foreground-программу стола текущей задачи. Вызывается из
/// `exec::run_program` перед `wait_for_polled`, чтобы переключение стола
/// приостановило и её тоже.
pub fn set_fg(pid: usize) {
    let me = current();
    if let Some(d) = desk(me) {
        d.fg = Some(pid);
    }
}

/// Ставит стол `n` активным. false — если такой стол есть, но он уже активен
/// или не создан.
///
/// Вызывается из хука клавиатуры, поэтому здесь нельзя блокироваться: всё
/// делается без аллокаций и без захвата VFS (cwd читаем/пишем напрямую).
pub fn activate(n: usize) -> bool {
    if n >= COUNT {
        return false;
    }
    let prev = FOCUS.load(Ordering::SeqCst);
    if n == prev {
        return false;
    }
    let Some(_) = desk(n) else {
        return false;
    };
    FOCUS.store(n, Ordering::SeqCst);

    // 1. Уходящий стол запоминает экран и засыпает. Его foreground-программа
    //    тоже засыпает: она пишет прямо в 0xB8000 и иначе затирала бы экран
    //    нового стола.
    save(prev);
    if let Some(d) = desk(prev) {
        if let Some(pid) = d.fg {
            scheduler::pause(pid);
        }
        scheduler::pause(d.shell);
    }

    // 2. Новый стол выводит своё и просыпается.
    load(n);
    if let Some(d) = desk(n) {
        if let Some(pid) = d.fg {
            scheduler::resume(pid);
        }
        scheduler::resume(d.shell);
    }
    true
}

/// Хук клавиатуры: Ctrl+Shift+1..4 приходит сюда как индекс стола.
fn on_hotkey(target: usize) {
    activate(target);
}

/// Точка входа задачи-шелла рабочего стола. Отдельная задача, а не
/// дополнительный цикл внутри основного шелла: иначе `run vita` занял бы
/// единственный канал ввода и переключать столы было бы нечем.
fn desk_shell(index: usize, mem: crate::sysinfo::MemInfo) -> ! {
    let mut writer = Writer::new();
    // `&mut Vfs` на всю сессию — так же, как основной шелл в main(). Активен
    // стол в любой момент только один, поэтому ссылка используется
    // одновременно ровно одним шеллом.
    let vfs: &mut Vfs = unsafe { &mut *scheduler::vfs_ptr() };
    crate::shell::run_shell(&mut writer, mem, vfs, index as u8);
}

/// Поднимает рабочие столы. `mem` нужен шеллам, `vfs` — чтобы снять
/// начальный cwd (у стола он свой, а не общий).
pub fn boot(mem: crate::sysinfo::MemInfo, vfs: &mut Vfs) {
    keyboard::set_hotkey_hook(on_hotkey);
    // Клавиши, нажатые на экране загрузки, не должны всплыть в промпте.
    keyboard::clear_hotkeys();

    let root = vfs.pwd();
    // Стол 0 — это уже работающий основной шелл (pid 0): его экран сейчас на
    // дисплее, а задача создана в scheduler::init().
    let d0 = new_desk(0, root);
    if let Some(slot) = desks().first_mut() {
        *slot = Some(d0);
    }

    // Остальные столы — отдельные задачи, стартующие уже приостановленными:
    // иначе они нарисуют своё приглашение поверх первого экрана.
    for i in 1..COUNT {
        let cwd = vfs.pwd();
        let entry = Box::new(move || desk_shell(i, mem));
        match scheduler::spawn_suspended(&format!("desk{}", i + 1), entry) {
            Some(pid) => {
                let d = new_desk(pid, cwd);
                if let Some(slot) = desks().get_mut(i) {
                    *slot = Some(d);
                }
            }
            None => {
                vga::serial_write_atomic("[desk] no free task slot for desk");
                vga::serial_u64(i as u64);
                vga::serial_write_atomic("\n");
            }
        }
    }
}
