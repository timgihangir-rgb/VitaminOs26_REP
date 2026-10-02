use crate::vga::{SCREEN_HEIGHT, SCREEN_WIDTH, Writer};
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;

/// Обработчик IRQ1 (клавиатура): читает сканкод и кладёт его в буфер.
/// Прерывания на время обработчика выключены (interrupt gate).
static KBD_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Горячие клавиши, которые обрабатываются ядром, а не строкой ввода.
/// Вводят битовую маску HOTKEY_PENDING; забирает их тот, кто сейчас читает
/// клавиатуру (см. `poll_hotkeys`): шелл в `read_line`, ожидающий
/// foreground-программу в `scheduler::wait_for_polled` или `top`.
pub const HOTKEY_CTRL_C: u8 = 1 << 0;
pub const HOTKEY_DESK: u8 = 1 << 1;

static HOTKEY_PENDING: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);
static MOD_CTRL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static MOD_SHIFT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Номер рабочего стола из последнего Ctrl+Shift+1..4.
static DESK_TARGET: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

extern "x86-interrupt" fn keyboard_irq_handler(_frame: x86_64::structures::idt::InterruptStackFrame) {
    use core::sync::atomic::Ordering;
    use x86_64::instructions::port::Port;
    let status: u8 = unsafe { Port::new(STATUS_PORT).read() };
    if status & 0x1 != 0 {
        let sc: u8 = unsafe { Port::new(DATA_PORT).read() };
        let ctrl = MOD_CTRL.load(Ordering::Relaxed);
        let shift = MOD_SHIFT.load(Ordering::Relaxed);
        let make = sc & 0x80 == 0;
        // Горячие клавиши наружу не отдаём: ни строке ввода, ни тем более
        // foreground-программе (snake/web Ctrl+C не понимают — а цифра от
        // Ctrl+Shift+2 иначе вставлялась бы прямо в набираемую команду).
        let mut consumed = false;
        if make {
            match sc {
                0x1D => MOD_CTRL.store(true, Ordering::Relaxed),
                0x2A | 0x36 => MOD_SHIFT.store(true, Ordering::Relaxed),
                // Ctrl+C. Что с ней делать, решает потребитель: прервать
                // foreground-программу или отменить строку ввода.
                0x2E if ctrl => {
                    HOTKEY_PENDING.fetch_or(HOTKEY_CTRL_C, Ordering::SeqCst);
                    consumed = true;
                    // Сбрасываем Ctrl: если пользователь держит его дальше
                    // (скажем, выбирает текст Ctrl+Shift+стрелка), следующая
                    // буква не должна считаться новой Ctrl-комбинацией.
                    MOD_CTRL.store(false, Ordering::Relaxed);
                }
                // Ctrl+Shift+1..4 — переключение рабочих столов.
                0x02..=0x05 if ctrl && shift => {
                    DESK_TARGET.store(sc - 0x02, Ordering::SeqCst);
                    HOTKEY_PENDING.fetch_or(HOTKEY_DESK, Ordering::SeqCst);
                    consumed = true;
                    MOD_CTRL.store(false, Ordering::Relaxed);
                    MOD_SHIFT.store(false, Ordering::Relaxed);
                }
                _ => {}
            }
        } else {
            match sc {
                0x9D => MOD_CTRL.store(false, Ordering::Relaxed),
                0xAA | 0xB6 => MOD_SHIFT.store(false, Ordering::Relaxed),
                _ => {}
            }
        }
        if !consumed {
            push_scancode(sc);
        }
    }
    unsafe {
        let mut eoi = Port::new(0x20);
        eoi.write(0x20u8);
    }
    KBD_COUNT.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
}

/// Забирает «съеденную» горячую клавишу: true — была нажата (и теперь
/// сброшена), false — не было. Два потребителя на одну клавишу не
/// претендуют: кто первый позвонил, тот и обработал.
pub fn take_hotkey(kind: u8) -> bool {
    HOTKEY_PENDING.fetch_and(!kind, core::sync::atomic::Ordering::SeqCst) & kind != 0
}

/// Номер рабочего стола из последнего Ctrl+Shift+1..4 (0-based).
pub fn desk_target() -> usize {
    DESK_TARGET.load(core::sync::atomic::Ordering::SeqCst) as usize
}

/// Хук на горячие клавиши, которые не попадают в строку ввода. Ставится
/// рабочими столами (см. `desk::boot`); аргумент — индекс стола.
static mut HOTKEY_HOOK: Option<fn(usize)> = None;

pub fn set_hotkey_hook(f: fn(usize)) {
    unsafe { HOTKEY_HOOK = Some(f) }
}

/// Разбирает накопившиеся горячие клавиши. Возвращает true, если был
/// обработан Ctrl+C (вызывающий должен прервать программу или отменить
/// строку ввода).
///
/// Звать должен любой, кто читает клавиатуру: иначе неразобранная клавиша
/// «залипнет» в HOTKEY_PENDING и сработает позже, когда её уже никто не
/// ждал, — например Ctrl+Shift+2 напечатает «2» в следующую команду.
pub fn poll_hotkeys() -> bool {
    if take_hotkey(HOTKEY_DESK) {
        let target = desk_target();
        if let Some(f) = unsafe { HOTKEY_HOOK } {
            f(target);
        }
    }
    take_hotkey(HOTKEY_CTRL_C)
}

/// Сбрасывает все неразобранные горячие клавиши. Вызывается при старте
/// рабочих столов, чтобы нажатие на экране загрузки не всплыло в промпте.
pub fn clear_hotkeys() {
    HOTKEY_PENDING.store(0, core::sync::atomic::Ordering::SeqCst);
}

pub fn kbd_irq_count() -> u64 {
    KBD_COUNT.load(core::sync::atomic::Ordering::SeqCst)
}

pub fn keyboard_irq_handler_addr() -> u64 {
    keyboard_irq_handler as extern "x86-interrupt" fn(
        x86_64::structures::idt::InterruptStackFrame,
    ) as usize as u64
}

// Кольцевой буфер сканкодов: IRQ1-обработчик кладёт, read_line забирает.
// Single-producer (прерывание) / single-consumer (поток шелла), прерывания в
// обработчике выключены, поэтому безопасно без блокировок.
const BUF_SIZE: usize = 256;
static mut BUF: [u8; BUF_SIZE] = [0; BUF_SIZE];
static mut BUF_HEAD: usize = 0;
static mut BUF_TAIL: usize = 0;

/// Вызывается из IRQ1-обработчика. Не может быть прерван (IF=0 в обработчике).
pub fn push_scancode(scancode: u8) {
    unsafe {
        let next = (BUF_TAIL + 1) % BUF_SIZE;
        if next == BUF_HEAD {
            return; // буфер полон: теряем сканкод
        }
        BUF[BUF_TAIL] = scancode;
        BUF_TAIL = next;
    }
}

fn pop_scancode() -> Option<u8> {
    unsafe {
        if BUF_HEAD == BUF_TAIL {
            return None;
        }
        let sc = BUF[BUF_HEAD];
        BUF_HEAD = (BUF_HEAD + 1) % BUF_SIZE;
        Some(sc)
    }
}

/// Есть ли сканкод в буфере (для ABI-сисколов программ). Не аллоцирует.
pub fn kb_hit() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe { BUF_HEAD != BUF_TAIL })
}

/// Вынимает сканкод из буфера (для ABI-сисколов программ). 0 — буфер пуст.
pub fn kb_read() -> u8 {
    x86_64::instructions::interrupts::without_interrupts(|| pop_scancode().unwrap_or(0))
}

/// Очищает буфер сканкодов (перед запуском foreground-программы, чтобы она
/// не получила остатки от команд шелла).
pub fn flush() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        BUF_HEAD = BUF_TAIL;
    });
}

// Эхо ввода в read_line (управляется ioctl ECHO_GET/ECHO_SET).
static mut ECHO: bool = true;

pub fn echo_get() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe { ECHO })
}

pub fn echo_set(on: bool) {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe { ECHO = on });
}

fn translate(scancode: u8, shift: bool) -> Option<u8> {
    let base = match scancode {
        0x02 => Some(b'1'), 0x03 => Some(b'2'), 0x04 => Some(b'3'),
        0x05 => Some(b'4'), 0x06 => Some(b'5'), 0x07 => Some(b'6'),
        0x08 => Some(b'7'), 0x09 => Some(b'8'), 0x0A => Some(b'9'),
        0x0B => Some(b'0'), 0x0C => Some(b'-'), 0x0D => Some(b'='),
        0x10 => Some(b'q'), 0x11 => Some(b'w'), 0x12 => Some(b'e'),
        0x13 => Some(b'r'), 0x14 => Some(b't'), 0x15 => Some(b'y'),
        0x16 => Some(b'u'), 0x17 => Some(b'i'), 0x18 => Some(b'o'),
        0x19 => Some(b'p'), 0x1A => Some(b'['), 0x1B => Some(b']'),
        0x1E => Some(b'a'), 0x1F => Some(b's'), 0x20 => Some(b'd'),
        0x21 => Some(b'f'), 0x22 => Some(b'g'), 0x23 => Some(b'h'),
        0x24 => Some(b'j'), 0x25 => Some(b'k'), 0x26 => Some(b'l'),
        0x27 => Some(b';'), 0x28 => Some(b'\''),
        0x29 => Some(b'`'), 0x2B => Some(b'\\'),
        0x2C => Some(b'z'), 0x2D => Some(b'x'), 0x2E => Some(b'c'),
        0x2F => Some(b'v'), 0x30 => Some(b'b'), 0x31 => Some(b'n'),
        0x32 => Some(b'm'), 0x33 => Some(b','), 0x34 => Some(b'.'),
        0x35 => Some(b'/'),
        0x39 => Some(b' '),
        0x1C => Some(b'\n'),
        0x0E => Some(0x08),
        _ => None,
    };
    base.map(|c| if shift { shift_char(c) } else { c })
}

fn shift_char(c: u8) -> u8 {
    match c {
        b'1' => b'!', b'2' => b'@', b'3' => b'#', b'4' => b'$', b'5' => b'%',
        b'6' => b'^', b'7' => b'&', b'8' => b'*', b'9' => b'(', b'0' => b')',
        b'-' => b'_', b'=' => b'+', b',' => b'<', b'.' => b'>', b'/' => b'?',
        b'[' => b'{', b']' => b'}', b';' => b':', b'\'' => b'"',
        b'`' => b'~', b'\\' => b'|',
        b'a'..=b'z' => c - 32,
        _ => c,
    }
}

/// Вычитывает всё, что накопилось в буфере, и переводит в ASCII
/// (для чтения из /dev/keyboard). Не блокируется: пустой буфер - пустой ответ.
pub fn drain_chars(max: usize) -> alloc::vec::Vec<u8> {
    let mut out = alloc::vec::Vec::new();
    let mut shift = false;
    let mut e0 = false;
    while out.len() < max {
        let sc = x86_64::instructions::interrupts::without_interrupts(pop_scancode);
        match sc {
            None => break,
            Some(0xE0) => {
                e0 = true;
                continue;
            }
            Some(s) if e0 => {
                let _ = s;
                e0 = false;
                continue; // стрелки/модификаторы расширений пропускаем
            }
            Some(0x2A | 0x36) => {
                shift = true;
                continue;
            }
            Some(0xAA | 0xB6) => {
                shift = false;
                continue;
            }
            Some(s) if s & 0x80 != 0 => continue,
            Some(s) => {
                if let Some(c) = translate(s, shift) {
                    if c == b'\n' || c == b'\r' {
                        out.push(b'\n');
                    } else if c >= 0x20 && c < 0x7F {
                        out.push(c);
                    }
                }
            }
        }
    }
    out
}

pub struct History {
    entries: Vec<String>,
    nav_index: usize,
    saved: String,
}

impl History {
    pub fn new() -> Self {
        History {
            entries: Vec::new(),
            nav_index: 0,
            saved: String::new(),
        }
    }

    pub fn add(&mut self, entry: String) {
        if entry.is_empty() {
            return;
        }
        if self.entries.last().map_or(false, |last| *last == entry) {
            self.nav_index = self.entries.len();
            self.saved.clear();
            return;
        }
        self.entries.push(entry);
        self.nav_index = self.entries.len();
        self.saved.clear();
    }

    pub fn up(&mut self, current_line: &str) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        if self.nav_index == self.entries.len() {
            self.saved = current_line.to_string();
        }
        if self.nav_index > 0 {
            self.nav_index -= 1;
            Some(self.entries[self.nav_index].as_str())
        } else {
            None
        }
    }

    pub fn down(&mut self) -> Option<&str> {
        if self.entries.is_empty() {
            return None;
        }
        if self.nav_index < self.entries.len() - 1 {
            self.nav_index += 1;
            Some(self.entries[self.nav_index].as_str())
        } else if self.nav_index == self.entries.len() - 1 {
            self.nav_index = self.entries.len();
            if self.saved.is_empty() {
                Some("")
            } else {
                Some(self.saved.as_str())
            }
        } else {
            None
        }
    }
}

/// Предельная длина строки ввода (в байтах).
const MAX_LINE: usize = 1024;

/// Редактор одной строки ввода с настоящим курсором: держит текст, индекс
/// курсора и перерисовывает область ввода целиком после каждой правки.
///
/// Ввод начинается там, где шелл закончил промпт, то есть в произвольной
/// ячейке, а строка может быть длиннее экрана и переноситься на несколько
/// строк. Поэтому экранные координаты считаются от «якоря» — (row, col),
/// где было начало ввода, а сам якорь поднимается вверх при прокрутке.
struct LineEditor {
    line: String,
    cursor: usize,
    row: usize,
    col: usize,
    drawn: usize,
}

impl LineEditor {
    fn new(writer: &Writer) -> Self {
        LineEditor {
            line: String::new(),
            cursor: 0,
            row: writer.row(),
            col: writer.column(),
            drawn: 0,
        }
    }

    /// Сколько строк экрана нужно под текст и под курсор в конце строки.
    /// Единица запаса: если текст ровно кончил ячейку, курсор стоит на
    /// начале следующей строки, и её тоже надо иметь на экране.
    fn rows_needed(&self) -> usize {
        (self.col + self.line.len()) / SCREEN_WIDTH + 1
    }

    /// Экранные координаты символа с индексом `i`, отсчитанные от якоря.
    fn cell(&self, i: usize) -> (usize, usize) {
        let abs = self.col + i;
        (self.row + abs / SCREEN_WIDTH, abs % SCREEN_WIDTH)
    }

    /// Ячейка последнего напечатанного символа (или якорь для пустой строки).
    /// От неё шелл пишет перевод строки по Enter — сразу под введённым текстом.
    fn end_cell(&self) -> (usize, usize) {
        if self.line.is_empty() {
            (self.row, self.col)
        } else {
            self.cell(self.line.len() - 1)
        }
    }

    /// Стирает область ввода и забывает текст — для Ctrl+C.
    fn discard(&mut self, writer: &mut Writer) {
        writer.fill_rect(self.row, self.col, self.drawn.max(self.rows_needed()), SCREEN_WIDTH);
        self.line.clear();
        self.cursor = 0;
        self.drawn = 0;
    }

    fn redraw(&mut self, writer: &mut Writer) {
        let need = self.rows_needed();
        // Стираем прежний рисунок: первая строка — с якоря (промпт слева
        // не трогаем), остальные целиком.
        writer.fill_rect(self.row, self.col, self.drawn.max(need), SCREEN_WIDTH);
        // Не помещается — прокручиваем; якорь уезжает вверх вместе с экраном.
        if self.row + need > SCREEN_HEIGHT {
            let up = self.row + need - SCREEN_HEIGHT;
            writer.scroll_up_by(up);
            self.row -= up;
        }
        for (i, &b) in self.line.as_bytes().iter().enumerate() {
            let (r, c) = self.cell(i);
            writer.put(r, c, b);
        }
        self.drawn = need;
        let (r, c) = self.cell(self.cursor);
        writer.set_cursor(r, c);
    }

    fn insert(&mut self, b: u8) {
        if self.line.len() >= MAX_LINE {
            return;
        }
        self.line.insert(self.cursor, b as char);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            self.line.remove(self.cursor - 1);
            self.cursor -= 1;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.line.len() {
            self.line.remove(self.cursor);
        }
    }

    fn kill_before(&mut self) {
        if self.cursor > 0 {
            self.line.replace_range(..self.cursor, "");
            self.cursor = 0;
        }
    }

    fn kill_after(&mut self) {
        self.line.truncate(self.cursor);
    }

    fn kill_word_before(&mut self) {
        let bytes = self.line.as_bytes();
        let mut i = self.cursor;
        while i > 0 && bytes[i - 1] == b' ' {
            i -= 1;
        }
        while i > 0 && bytes[i - 1] != b' ' {
            i -= 1;
        }
        self.line.replace_range(i..self.cursor, "");
        self.cursor = i;
    }

    fn set_line(&mut self, s: &str) {
        self.line = s.chars().take(MAX_LINE).collect();
        self.cursor = self.line.len();
    }
}

pub fn read_line(writer: &mut Writer, history: &mut History) -> String {
    let mut ed = LineEditor::new(writer);
    let mut shift = false;
    let mut ctrl = false;
    let mut e0 = false;

    loop {
        // Горячие клавиши ядра: переключение рабочих столов не трогает
        // строку ввода, а Ctrl+C отменяет её (как в терминале).
        if poll_hotkeys() {
            if echo_get() {
                ed.discard(writer);
                writer.write_string("^C\n");
            }
            return String::new();
        }
        let scancode = match pop_scancode() {
            Some(sc) => sc,
            None => {
                x86_64::instructions::hlt();
                continue;
            }
        };

        if scancode == 0xE0 {
            e0 = true;
            continue;
        }

        let mut dirty = false;
        let mut done = false;

        if e0 {
            e0 = false;
            if scancode & 0x80 == 0 {
                match scancode {
                    0x48 => {
                        // Up — предыдущая команда из истории
                        if let Some(entry) = history.up(&ed.line) {
                            ed.set_line(entry);
                            dirty = true;
                        }
                    }
                    0x50 => {
                        // Down — следующая (или возврат к сохранённой строке)
                        if let Some(entry) = history.down() {
                            ed.set_line(entry);
                            dirty = true;
                        }
                    }
                    0x4B => {
                        if ed.cursor > 0 {
                            ed.cursor -= 1;
                            dirty = true;
                        }
                    }
                    0x4D => {
                        if ed.cursor < ed.line.len() {
                            ed.cursor += 1;
                            dirty = true;
                        }
                    }
                    0x47 => {
                        if ed.cursor != 0 {
                            ed.cursor = 0;
                            dirty = true;
                        }
                    }
                    0x4F => {
                        let end = ed.line.len();
                        if ed.cursor != end {
                            ed.cursor = end;
                            dirty = true;
                        }
                    }
                    0x53 => {
                        if ed.cursor < ed.line.len() {
                            ed.delete();
                            dirty = true;
                        }
                    }
                    0x1C => done = true, // Enter на цифровой клавиатуре
                    _ => {}
                }
            }
        } else {
            match scancode {
                0x2A | 0x36 => shift = true,
                0xAA | 0xB6 => shift = false,
                0x1D => ctrl = true,
                0x9D => ctrl = false,
                _ if scancode & 0x80 != 0 => {}
                // Ctrl-комбинации разбираем до translate(): их сканкоды
                // совпадают с буквами, иначе они не отличились бы от ввода.
                _ if ctrl => {
                    match scancode {
                        0x1E => ed.cursor = 0, // Ctrl+A — в начало строки
                        0x12 => ed.cursor = ed.line.len(), // Ctrl+E — в конец
                        0x16 => ed.kill_before(),          // Ctrl+U — сбросить влево
                        0x25 => ed.kill_after(),           // Ctrl+K — сбросить вправо
                        0x11 => ed.kill_word_before(),     // Ctrl+W — сбросить слово
                        0x1B => {
                            ed.line.clear();
                            ed.cursor = 0;
                        } // Ctrl+[ — выход без изменений
                        _ => {}
                    }
                    dirty = true;
                }
                _ => {
                    if let Some(byte) = translate(scancode, shift) {
                        match byte {
                            b'\n' | b'\r' => done = true,
                            0x08 => {
                                ed.backspace();
                                dirty = true;
                            }
                            c => {
                                ed.insert(c);
                                dirty = true;
                            }
                        }
                    }
                }
            }
        }

        if done {
            if echo_get() {
                let (r, c) = ed.end_cell();
                writer.set_cursor(r, c);
                writer.write_string("\n");
            }
            history.add(ed.line.clone());
            return ed.line;
        }

        if dirty && echo_get() {
            ed.redraw(writer);
        }
    }
}

/// Инициализация 8042 при старте ядра: включаем клавиатурный интерфейс
/// (0xAE) и вычитываем зависшие байты вывода. GRUB может оставить
/// контроллер в командном режиме (статус 0x28) - тогда он молча глотает
/// все нажатия; 0xAE сбрасывает это состояние.
pub fn controller_init() {
    // Каноническая инициализация 8042. Без неё контроллер может остаться в
    // состоянии, где данные принимаются, но IRQ1 не выставляется
    // (сброшен бит 0 конфига - "keyboard interrupt enable").
    unsafe {
        use x86_64::instructions::port::Port;
        let mut cmd: Port<u8> = Port::new(0x64);
        let mut data: Port<u8> = Port::new(0x60);
        let mut status: Port<u8> = Port::new(0x64);

        // 1) Вычитать зависшие байты вывода.
        let mut guard = 128u32;
        while guard > 0 {
            if status.read() & 1 == 0 {
                break;
            }
            let _ = data.read();
            guard -= 1;
        }

        // 2) Самотест контроллера (0xAA): сбрасывает внутреннее состояние.
        cmd.write(0xAAu8);
        let mut wait = 200000u32;
        while wait > 0 && status.read() & 1 == 0 {
            core::hint::spin_loop();
            wait -= 1;
        }
        if status.read() & 1 != 0 {
            let _ = data.read(); // ответ 0x55 нам не важен
        }

        // 3) Читаем конфиг (0x20), разрешаем IRQ1 (бит0) и трансляцию (бит6),
        //    включаем тактовую линию клавиатуры (снимаем бит4).
        cmd.write(0x20u8);
        let mut wait = 200000u32;
        while wait > 0 && status.read() & 1 == 0 {
            core::hint::spin_loop();
            wait -= 1;
        }
        let mut cfg = if status.read() & 1 != 0 { data.read() } else { 0x45 };
        cfg |= 0b0000_0001; // IRQ1 enable
        cfg |= 0b0100_0000; // translate scancodes
        cfg &= !0b0001_0000; // kbd clock enable
        cfg &= !0b0010_0000; // mouse clock enable (не мешает)
        cmd.write(0x60u8);
        data.write(cfg);

        // 4) Включаем интерфейсы и сканирование на устройстве.
        cmd.write(0xAEu8); // enable keyboard interface
        data.write(0xF4u8); // enable scanning (устройство ответит 0xFA - вычтем)
        let mut guard = 64u32;
        while guard > 0 {
            if status.read() & 1 == 0 {
                break;
            }
            let _ = data.read();
            guard -= 1;
        }
    }
}
