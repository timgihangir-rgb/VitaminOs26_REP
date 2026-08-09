use crate::vga::Writer;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

const DATA_PORT: u16 = 0x60;
const STATUS_PORT: u16 = 0x64;

/// Обработчик IRQ1 (клавиатура): читает сканкод и кладёт его в буфер.
/// Прерывания на время обработчика выключены (interrupt gate).
static KBD_COUNT: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

extern "x86-interrupt" fn keyboard_irq_handler(_frame: x86_64::structures::idt::InterruptStackFrame) {
    use x86_64::instructions::port::Port;
    let status: u8 = unsafe { Port::new(STATUS_PORT).read() };
    if status & 0x1 != 0 {
        let sc: u8 = unsafe { Port::new(DATA_PORT).read() };
        push_scancode(sc);
    }
    unsafe {
        let mut eoi = Port::new(0x20);
        eoi.write(0x20u8);
    }
    KBD_COUNT.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
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

fn erase_line(writer: &mut Writer, start_row: usize, start_col: usize, len: usize) {
    writer.set_cursor(start_row, start_col);
    for _ in 0..len {
        writer.write_byte(b' ');
    }
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

pub fn read_line(writer: &mut Writer, history: &mut History) -> String {
    let mut line = String::new();
    let mut shift = false;
    let mut e0 = false;
    let start_row = writer.row();
    let start_col = writer.column();

    loop {
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

        if e0 {
            e0 = false;
            if scancode & 0x80 != 0 {
                continue;
            }
            match scancode {
                0x48 => {
                    if let Some(entry) = history.up(&line) {
                        erase_line(writer, start_row, start_col, line.len());
                        writer.set_cursor(start_row, start_col);
                        for b in entry.bytes() {
                            writer.write_byte(b);
                        }
                        line = entry.to_string();
                    }
                }
                0x50 => {
                    if let Some(entry) = history.down() {
                        erase_line(writer, start_row, start_col, line.len());
                        writer.set_cursor(start_row, start_col);
                        for b in entry.bytes() {
                            writer.write_byte(b);
                        }
                        line = entry.to_string();
                    }
                }
                _ => {}
            }
            continue;
        }

        match scancode {
            0x2A | 0x36 => {
                shift = true;
                continue;
            }
            0xAA | 0xB6 => {
                shift = false;
                continue;
            }
            _ => {}
        }

        if scancode & 0x80 != 0 {
            continue;
        }

        if let Some(byte) = translate(scancode, shift) {
            match byte {
                b'\n' | b'\r' => {
                    writer.write_string("\n");
                    history.add(line.clone());
                    return line;
                }
                0x08 => {
                    if line.pop().is_some() {
                        writer.backspace();
                    }
                }
                c => {
                    line.push(c as char);
                    writer.write_byte(c);
                }
            }
        }
    }
}
