use crate::vga::{screen_cols, screen_rows, Writer};
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
static MOD_ALT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Номер рабочего стола из последнего Ctrl+Shift+1..4.
static DESK_TARGET: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Активная раскладка: 0 - латиница (US), 1 - кириллица (ЙЦУКЕН).
///
/// Хранится рядом с модификаторами, потому что переключается прямо в
/// обработчике прерывания, а читается из `translate` при разборе потока
/// сканкодов - из другого контекста.
static LAYOUT: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Активная раскладка: `false` - латиница, `true` - ЙЦУКЕН.
pub fn layout_russian() -> bool {
    LAYOUT.load(core::sync::atomic::Ordering::Relaxed) != 0
}

extern "x86-interrupt" fn keyboard_irq_handler(_frame: x86_64::structures::idt::InterruptStackFrame) {
    use x86_64::instructions::port::Port;
    let status: u8 = unsafe { Port::new(STATUS_PORT).read() };
    if status & 0x1 != 0 {
        let sc: u8 = unsafe { Port::new(DATA_PORT).read() };
        // Если клавиатура подключена по USB и обслуживается драйвером usb.rs,
        // 8042 в BIOS с legacy-эмуляцией повторяет те же нажатия. Читаем
        // порт, чтобы не переполнить буфер контроллера, но в поток не идём.
        if !crate::usb::owns_keyboard() {
            handle_scancode(sc);
        }
    }
    unsafe {
        let mut eoi = Port::new(0x20);
        eoi.write(0x20u8);
    }
    KBD_COUNT.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
}

/// Общая часть для обоих источников ввода (PS/2 по IRQ1 и USB по отчётам
/// HID): модификаторы, горячие клавиши и отправка в кольцевой буфер.
fn handle_scancode(sc: u8) {
    use core::sync::atomic::Ordering;
    let ctrl = MOD_CTRL.load(Ordering::Relaxed);
    let shift = MOD_SHIFT.load(Ordering::Relaxed);
    let alt = MOD_ALT.load(Ordering::Relaxed);
    let make = sc & 0x80 == 0;
    // Горячие клавиши наружу не отдаём: ни строке ввода, ни тем более
    // foreground-программе (snake/web Ctrl+C не понимают — а цифра от
    // Ctrl+Shift+2 иначе вставлялась бы прямо в набираемую команду).
    let mut consumed = false;
    if make {
        match sc {
            0x1D => MOD_CTRL.store(true, Ordering::Relaxed),
            0x2A | 0x36 => MOD_SHIFT.store(true, Ordering::Relaxed),
            0x38 => MOD_ALT.store(true, Ordering::Relaxed),
            // Ctrl+C. Что с ней делать, решает потребитель: прервать
            // foreground-программу или отменить строку ввода.
            0x2E if ctrl && !alt => {
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
            // Ctrl+Alt+буква — переключение раскладки (как в Windows).
            //
            // Именно буква, а не любая клавиша: цифры, скобки и Enter в
            // ЙЦУКЕН остаются теми же, что в латинице, так что
            // Ctrl+Alt+2 это «2», а не «переключить и набрать ф». Саму
            // клавишу при этом съедаем - иначе буква от Ctrl+Alt+F влетела бы
            // в командную строку.
            0x10..=0x39 if ctrl && alt && is_letter_key(sc) => {
                LAYOUT.store(1 - LAYOUT.load(Ordering::Relaxed), Ordering::Relaxed);
                consumed = true;
                // Модификаторы сбрасываем, как и после других горячих клавиш:
                // иначе, отпустив Ctrl раньше Alt, пользователь переключит
                // раскладку ещё и на следующей букве.
                MOD_CTRL.store(false, Ordering::Relaxed);
                MOD_ALT.store(false, Ordering::Relaxed);
            }
            _ => {}
        }
    } else {
        match sc {
            0x9D => MOD_CTRL.store(false, Ordering::Relaxed),
            0xAA | 0xB6 => MOD_SHIFT.store(false, Ordering::Relaxed),
            0xB8 => MOD_ALT.store(false, Ordering::Relaxed),
            _ => {}
        }
    }
    if !consumed {
        push_scancode(sc);
    }
}

/// Буквенная клавиша в раскладке US: 0x10..=0x1B и 0x1E..=0x29 плюс ряд
/// 0x2C..=0x35. Ровно этот набор ЙЦУКЕН переводит в другую букву.
fn is_letter_key(sc: u8) -> bool {
    matches!(sc, 0x10..=0x1B | 0x1E..=0x29 | 0x2C..=0x35)
}

/// Запись сканкода в общий поток из драйвера USB-клавиатуры (см. usb.rs).
/// Прерывания глушим: состояние модификаторов общее с обработчиком IRQ1.
pub fn push_scancode_usb(sc: u8) {
    x86_64::instructions::interrupts::without_interrupts(|| handle_scancode(sc));
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

/// Клавиша для простых меню (установщик): стрелки/Esc/Tab/Space/Enter и
/// печатный символ. Блокирующая: без клавиши уходит в hlt.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Enter,
    Space,
    Tab,
    Esc,
    /// Символ в Unicode, а не байтом кодовой страницы: ЙЦУКЕН даёт
    /// кириллицу, и по дороге до ячейки экрана её ещё предстоит перевести
    /// через `vga::code_page`.
    Char(char),
    Other,
}

/// Ждёт следующую клавишу (с учётом префикса E0). Горячие клавиши ядра
/// (Ctrl+C, переключение столов) здесь не обрабатываются: установщик их
/// игнорирует.
pub fn read_key() -> Key {
    let mut e0 = false;
    loop {
        let sc = match x86_64::instructions::interrupts::without_interrupts(pop_scancode) {
            Some(s) => s,
            None => {
                x86_64::instructions::hlt();
                continue;
            }
        };
        if sc == 0xE0 {
            e0 = true;
            continue;
        }
        if e0 {
            e0 = false;
            if sc & 0x80 != 0 {
                return Key::Other;
            }
            return match sc {
                0x48 => Key::Up,
                0x50 => Key::Down,
                0x4B => Key::Left,
                0x4D => Key::Right,
                0x1C => Key::Enter, // Enter на цифровом блоке
                _ => Key::Other,
            };
        }
        if sc & 0x80 != 0 {
            continue; // отпускание обычной клавиши пропускаем
        }
        match sc {
            0x01 => return Key::Esc,
            0x0F => return Key::Tab,
            0x1C => return Key::Enter,
            0x39 => return Key::Space,
            _ => {
                if let Some(c) = translate(sc, false) {
                    return Key::Char(c);
                }
            }
        }
    }
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

/// ЙЦУКЕН: индекс = сканкод буквенной клавиши, значение = кириллица
/// строчными. Номера сканкодов — те же, что у US в `translate`, поэтому
/// раскладки просто складываются в пару по индексу.
///
/// Ряды в порядке QWERTY-клавиатуры:
///   й ц у к е н г ш щ з х ъ
///   ф ы в а п р о л д ж э
///   я ч с м и т ь б ю .    (последние четыре — . и / в US)
///
/// Знаки препинания, цифры и Enter в ЙЦУКЕН совпадают с US, поэтому
/// таблица покрывает только буквы: для остальных `translate` берёт US.
#[rustfmt::skip]
const JC: [char; 256] = {
    let mut t = ['\0'; 256];
    // 0x10..=0x1B: q w e r t y u i o p [ ]
    t[0x10] = 'й'; t[0x11] = 'ц'; t[0x12] = 'у'; t[0x13] = 'к';
    t[0x14] = 'е'; t[0x15] = 'н'; t[0x16] = 'г'; t[0x17] = 'ш';
    t[0x18] = 'щ'; t[0x19] = 'з'; t[0x1A] = 'х'; t[0x1B] = 'ъ';
    // 0x1E..=0x29: a s d f g h j k l ; ' `
    t[0x1E] = 'ф'; t[0x1F] = 'ы'; t[0x20] = 'в'; t[0x21] = 'а';
    t[0x22] = 'п'; t[0x23] = 'р'; t[0x24] = 'о'; t[0x25] = 'л';
    t[0x26] = 'д'; t[0x27] = 'ж'; t[0x28] = 'э'; t[0x29] = 'ё';
    // 0x2C..=0x35: z x c v b n m , . /
    t[0x2C] = 'я'; t[0x2D] = 'ч'; t[0x2E] = 'с'; t[0x2F] = 'м';
    t[0x30] = 'и'; t[0x31] = 'т'; t[0x32] = 'ь'; t[0x33] = 'б';
    t[0x34] = 'ю';
    t
};

/// Сканкод в символ. `char`, а не `u8`: кириллица в CP1251 не помещается в
/// ASCII, и переносить её в ядре надо в Unicode. В ячейку экрана символ
/// попадёт через `vga::code_page`.
fn translate(scancode: u8, shift: bool) -> Option<char> {
    // Раскладка смотрится здесь, а не при нажатии: нажатие обрабатывается
    // в IRQ, а перевод делает поток, и между ними может прийти ещё одно
    // событие. Для буквы это безобидно (буквы не меняют регистр), а вот
    // Ctrl+Alt смотрит на состояние на момент нажатия - там LAYOUT не нужен.
    let russian = layout_russian();
    if russian {
        let c = JC[scancode as usize];
        if c != '\0' {
            return Some(if shift { upper_cyrillic(c) } else { c });
        }
    }
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
    }?;
    Some(if shift {
        shift_char(base) as char
    } else {
        base as char
    })
}

/// Заглавная кириллица. Работает по арифметике кодовых точек Unicode:
/// А-Я лежат подряд с U+0410, а-я - с U+0430, Ё и ё стоят отдельно и
/// отличаются от соседей, поэтому их разбираем явно.
fn upper_cyrillic(c: char) -> char {
    match c {
        'ё' => 'Ё',
        'а'..='я' => char::from_u32(c as u32 - 0x20).unwrap_or(c),
        _ => c,
    }
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

/// Вычитывает всё, что накопилось в буфере, и переводит в UTF-8
/// (для чтения из /dev/keyboard). Не блокируется: пустой буфер - пустой ответ.
///
/// Отдаёт UTF-8, а не байты кодовой страницы: потребитель сискола ждёт
/// строку, а русская буква в CP1251 - это 0xE9, такой же байт, как у
/// `й` в cp1252, и без указания кодировки файл с русским текстом
/// прочитался бы как мусор. На экран текст попадёт уже через
/// `vga::code_page`.
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
                    match c {
                        '\n' | '\r' => out.push(b'\n'),
                        // Пропускаем управляющие: в файле им не место, а
                        // 0x08 (Backspace) — это не символ, а команда
                        // редактирования, разобранная по дороге.
                        '\t' => out.push(b'\t'),
                        c if (c as u32) < 0x20 => {}
                        c => {
                            let mut buf = [0u8; 4];
                            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                        }
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

/// Предельная длина строки ввода (в символах, не в байтах: кириллица в
/// UTF-8 занимает два байта, и байтовый предел обрезал бы русскую команду
/// вдвое раньше, чем латинскую).
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
    /// Индекс курсора в `char`, а не в байт: `String` индексируется по
    /// границам UTF-8, и русская буква - два байта, так что байтовый курсор
    /// встал бы на середину буквы. Все операции ниже считают символы.
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

    /// Длина строки в символах (= ячейках экрана: один символ - одна
    /// ячейка независимо от кодировки).
    fn len(&self) -> usize {
        self.line.chars().count()
    }

    /// Сколько строк экрана нужно под текст и под курсор в конце строки.
    /// Единица запаса: если текст ровно кончил ячейку, курсор стоит на
    /// начале следующей строки, и её тоже надо иметь на экране.
    fn rows_needed(&self) -> usize {
        (self.col + self.len()) / screen_cols() + 1
    }

    /// Экранные координаты символа с индексом `i`, отсчитанные от якоря.
    fn cell(&self, i: usize) -> (usize, usize) {
        let abs = self.col + i;
        (self.row + abs / screen_cols(), abs % screen_cols())
    }

    /// Ячейка последнего напечатанного символа (или якорь для пустой строки).
    /// От неё шелл пишет перевод строки по Enter — сразу под введённым текстом.
    fn end_cell(&self) -> (usize, usize) {
        let n = self.len();
        if n == 0 {
            (self.row, self.col)
        } else {
            self.cell(n - 1)
        }
    }

    /// Стирает область ввода и забывает текст — для Ctrl+C.
    fn discard(&mut self, writer: &mut Writer) {
        writer.fill_rect(self.row, self.col, self.drawn.max(self.rows_needed()), screen_cols());
        self.line.clear();
        self.cursor = 0;
        self.drawn = 0;
    }

    fn redraw(&mut self, writer: &mut Writer) {
        let need = self.rows_needed();
        // Стираем прежний рисунок: первая строка — с якоря (промпт слева
        // не трогаем), остальные целиком.
        writer.fill_rect(self.row, self.col, self.drawn.max(need), screen_cols());
        // Не помещается — прокручиваем; якорь уезжает вверх вместе с экраном.
        if self.row + need > screen_rows() {
            let up = self.row + need - screen_rows();
            writer.scroll_up_by(up);
            self.row -= up;
        }
        // Рисуем по символам, а не по байтам UTF-8: каждый символ занимает
        // ровно одну ячейку, а кириллический байт в неё попасть не может -
        // байт кодовой страницы выбирает `vga::code_page`.
        for (i, ch) in self.line.chars().enumerate() {
            let (r, c) = self.cell(i);
            writer.put_char(r, c, ch);
        }
        self.drawn = need;
        let (r, c) = self.cell(self.cursor);
        writer.set_cursor(r, c);
    }

    fn insert(&mut self, ch: char) {
        if self.len() >= MAX_LINE {
            return;
        }
        let at = byte_index(&self.line, self.cursor);
        self.line.insert(at, ch);
        self.cursor += 1;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let at = byte_index(&self.line, self.cursor - 1);
            self.line.remove(at);
            self.cursor -= 1;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.len() {
            let at = byte_index(&self.line, self.cursor);
            self.line.remove(at);
        }
    }

    fn kill_before(&mut self) {
        if self.cursor > 0 {
            let at = byte_index(&self.line, self.cursor);
            self.line.replace_range(..at, "");
            self.cursor = 0;
        }
    }

    fn kill_after(&mut self) {
        let at = byte_index(&self.line, self.cursor);
        self.line.truncate(at);
    }

    fn kill_word_before(&mut self) {
        let chars: Vec<char> = self.line.chars().collect();
        let mut i = self.cursor;
        while i > 0 && chars[i - 1] == ' ' {
            i -= 1;
        }
        while i > 0 && chars[i - 1] != ' ' {
            i -= 1;
        }
        let from = byte_index(&self.line, i);
        let to = byte_index(&self.line, self.cursor);
        self.line.replace_range(from..to, "");
        self.cursor = i;
    }

    fn set_line(&mut self, s: &str) {
        self.line = s.chars().take(MAX_LINE).collect();
        self.cursor = self.len();
    }
}

/// Байтовая позиция `n`-го символа строки. Курсор везде считается в
/// символах, а `String::insert`/`remove`/`truncate` ждут байт на границе
/// UTF-8, поэтому перед каждой такой операцией переводим индекс.
fn byte_index(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
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
                    if let Some(c) = translate(scancode, shift) {
                        match c {
                            '\n' | '\r' => done = true,
                            '\u{8}' => {
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
            // Enter нажат — начинается вывод команды. Занимаем экран ДО
            // перевода строки и до записи истории в файл: и то и другое
            // отдаёт планировщик (диск), и композитор успел бы нарисовать
            // экран, где есть только перевод строки, — а потом ещё один
            // кадр с выводом. Вместе это ровно то «печатание», которое
            // убираем. Шелл освобождает экран в конце команды.
            crate::fb::set_quiet(true);
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
            // Ввод, в отличие от вывода, должен быть виден сразу же: это
            // отклик на нажатую клавишу, а не порция текста. Один кадр на
            // клавишу — нажатий за секунду десятки, кадр дешёвый.
            writer.sync();
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
