// src/vga.rs
const VGA_BUFFER: *mut u8 = 0xb8000 as *mut u8;
pub const SCREEN_WIDTH: usize = 80;
pub const SCREEN_HEIGHT: usize = 30;

pub const COLOR_LIGHT_GREEN: u8 = 0x0A;
pub const COLOR_LIGHT_CYAN: u8 = 0x0B;
pub const COLOR_WHITE: u8 = 0x0F;

const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;
const SEQ_INDEX: u16 = 0x3C4;
const SEQ_DATA: u16 = 0x3C5;
const MISC_OUTPUT: u16 = 0x3C2;

/// Буфер для чтения текущего аппаратного шрифта 8x16 из plane 2.
static mut FONT_BUF: [u8; 8192] = [0; 8192];

/// Переключает VGA в текстовый режим 80x30: 480 сканлайнов, 16 сканлайнов на
/// символ (cr09 = 0x0F) и полный BIOS-шрифт 8x16 без сжатия. 30 строк по 16px
/// дают буквы в 2 раза больше, чем в 80x60, и естественный зазор между
/// строками (у 8x16-шрифта непустые строки не доходят до низа клетки).
/// Вызывается до первого вывода, пока экран ещё в BIOS-режиме 80x25.
///
/// Значения вертикальных регистров CRTC взяты из Linux (vga_set_480_scanlines,
/// drivers/video/console/vgacon.c) — этот набор проверен на реальном железе
/// (MS-7383). Порядок важный: сначала 480-строчный режим, затем установка
/// шрифта, затем включение 16-строчных строк (cr09 = 0x0F) — при обратном
/// порядке на некоторых платах экран становится нечитаемым.
///
/// Расшифровка: cr12 (VDE) = 0xDF | 0x100 = 479, вместе с cr07 (overflow) даёт
/// 480 сканлайнов = 30 строк по 16 px. cr11 = 0x0C снимает защиту CR0-CR7.
pub fn set_80x30() {
    outb(MISC_OUTPUT, 0x23);

    outb(CRTC_INDEX, 0x11);
    outb(CRTC_DATA, 0x0C);

    outb(CRTC_INDEX, 0x06);
    outb(CRTC_DATA, 0x0B);
    outb(CRTC_INDEX, 0x07);
    outb(CRTC_DATA, 0x3E);
    outb(CRTC_INDEX, 0x10);
    outb(CRTC_DATA, 0xEA);
    outb(CRTC_INDEX, 0x12);
    outb(CRTC_DATA, 0xDF);
    outb(CRTC_INDEX, 0x15);
    outb(CRTC_DATA, 0xE7);
    outb(CRTC_INDEX, 0x16);
    outb(CRTC_DATA, 0x04);

    load_font_8x16();

    outb(CRTC_INDEX, 0x09);
    outb(CRTC_DATA, 0x0F);
}

fn seq_write(idx: u8, val: u8) {
    outb(SEQ_INDEX, idx);
    outb(SEQ_DATA, val);
}

/// Копирует загруженный BIOS-шрифт 8x16 в font plane (plane 2) без сжатия:
/// каждый глиф занимает все 32 байта (16 строк). Никакой обрезки — буквы
/// полные, их высота 16px (в клетке 16px, режим 80x30).
///
/// Раскладка plane 2 (подтверждена чтением через graphics controller):
/// глиф символа `c` лежит на адресе `c*32`, 32 байта на глиф; сканлайны
/// идут подряд - байты 0..15 блока это 16 строк 8x16, байты 16..31 нули.
/// Чтение plane 2: gc mode=0x00 (без odd/even), gc misc=0x0C (окно B8000),
/// gc read map select=0x02; чтение через буфер 0xB8000.
///
/// Запись в plane 2 через sequencer: map mask=0x04 (plane 2), memory mode=0x06
/// (sequential addressing, бит 2) - без этого word-режим текста смещает адрес
/// (addr>>=1) и режет маску по чётности. После записи память возвращается в
/// текстовый word-режим (memory mode=0x03).
///
/// Активный шрифт кладётся в верхние блоки plane 2 (8192/16384/24576), а не
/// в блок 0: в текстовом word-режиме запись в видеобуфер (даже обычного
/// символа) задевает также plane 2 на адресах 0..0x12BF и затёрла бы шрифт
/// в блоке 0. Блок 0 остаётся занят BIOS-шрифтом 8x16. Выбор блока - через
/// sequencer Character Map Select (0x35): перекрывает оба генератора и QEMU,
/// и реальное железо.
fn load_font_8x16() {
    unsafe {
        gc_write(0x05, 0x00);
        gc_write(0x06, 0x0C);
        gc_write(0x04, 0x02);
        for i in 0..8192 {
            FONT_BUF[i] = core::ptr::read_volatile(VGA_BUFFER.add(i));
        }

        seq_write(0x02, 0x04);
        seq_write(0x04, 0x06);
        gc_write(0x08, 0xFF);
        gc_write(0x03, 0x00);
        gc_write(0x01, 0x00);
        gc_write(0x00, 0x00);

        for base in [8192usize, 16384, 24576] {
            for i in 0..8192 {
                core::ptr::write_volatile(VGA_BUFFER.add(base + i), FONT_BUF[i]);
            }
        }

        seq_write(0x03, 0x35);
        seq_write(0x02, 0x0F);
        seq_write(0x04, 0x03);
        gc_write(0x04, 0x00);
        gc_write(0x05, 0x10);
        gc_write(0x06, 0x0C);
    }
}

fn gc_write(idx: u8, val: u8) {
    outb(0x3CE, idx);
    outb(0x3CF, val);
}

pub struct Writer {
    column_position: usize,
    row_position: usize,
    color_code: u8,
}

impl Writer {
    pub fn new() -> Writer {
        Writer {
            column_position: 0,
            row_position: 0,
            color_code: COLOR_LIGHT_GREEN,
        }
    }

    pub fn set_color(&mut self, color: u8) {
        self.color_code = color;
    }

    pub fn write_byte(&mut self, byte: u8) {
        match byte {
            b'\n' => self.new_line(),
            byte => {
                if self.column_position >= SCREEN_WIDTH {
                    self.new_line();
                }

                let row = self.row_position;
                let col = self.column_position;

                unsafe {
                    let offset = (row * SCREEN_WIDTH + col) * 2;
                    *VGA_BUFFER.add(offset) = byte;
                    *VGA_BUFFER.add(offset + 1) = self.color_code;
                }

                self.column_position += 1;
            }
        }
        self.update_hw_cursor();
    }

    pub fn backspace(&mut self) {
        if self.column_position > 0 {
            self.column_position -= 1;
            let row = self.row_position;
            let col = self.column_position;
            unsafe {
                let offset = (row * SCREEN_WIDTH + col) * 2;
                *VGA_BUFFER.add(offset) = b' ';
                *VGA_BUFFER.add(offset + 1) = self.color_code;
            }
            self.update_hw_cursor();
        }
    }

    fn new_line(&mut self) {
        self.column_position = 0;
        self.row_position += 1;
        if self.row_position >= SCREEN_HEIGHT {
            self.scroll_up();
            self.row_position = SCREEN_HEIGHT - 1;
        }
    }

    fn scroll_up(&mut self) {
        unsafe {
            for row in 1..SCREEN_HEIGHT {
                for col in 0..SCREEN_WIDTH {
                    let src_offset = (row * SCREEN_WIDTH + col) * 2;
                    let dst_offset = ((row - 1) * SCREEN_WIDTH + col) * 2;
                    *VGA_BUFFER.add(dst_offset) = *VGA_BUFFER.add(src_offset);
                    *VGA_BUFFER.add(dst_offset + 1) = *VGA_BUFFER.add(src_offset + 1);
                }
            }

            for col in 0..SCREEN_WIDTH {
                let offset = ((SCREEN_HEIGHT - 1) * SCREEN_WIDTH + col) * 2;
                *VGA_BUFFER.add(offset) = b' ';
                *VGA_BUFFER.add(offset + 1) = self.color_code;
            }
        }
    }

    pub fn write_string(&mut self, s: &str) {
        for byte in s.bytes() {
            match byte {
                0x20..=0x7e | b'\n' => self.write_byte(byte),
                _ => self.write_byte(0xFE),
            }
        }
    }

    pub fn clear_screen(&mut self) {
        unsafe {
            for row in 0..SCREEN_HEIGHT {
                for col in 0..SCREEN_WIDTH {
                    let offset = (row * SCREEN_WIDTH + col) * 2;
                    *VGA_BUFFER.add(offset) = b' ';
                    *VGA_BUFFER.add(offset + 1) = self.color_code;
                }
            }
        }
        self.row_position = 0;
        self.column_position = 0;
        self.update_hw_cursor();
    }

    pub fn ensure_lines_available(&mut self, needed: usize) {
        if needed >= SCREEN_HEIGHT {
            for _ in 0..SCREEN_HEIGHT {
                self.scroll_up();
            }
            self.row_position = 0;
            self.column_position = 0;
            self.update_hw_cursor();
            return;
        }

        while self.row_position + needed > SCREEN_HEIGHT {
            self.scroll_up();
            if self.row_position > 0 {
                self.row_position -= 1;
            }
        }
        self.update_hw_cursor();
    }

    pub fn row(&self) -> usize {
        self.row_position
    }

    pub fn column(&self) -> usize {
        self.column_position
    }

    /// Возвращает байтовый offset в VGA-буфере для текущей позиции курсора.
    /// Нужен C-программам, чтобы писать в VGA с правильной позиции.
    pub fn vga_offset(&self) -> usize {
        (self.row_position * SCREEN_WIDTH + self.column_position) * 2
    }

    pub fn set_cursor(&mut self, row: usize, col: usize) {
        // Раньше значения принимались без проверки: set_cursor(33, ...) на
        // экране 80x25 означал прямую запись в VGA_BUFFER далеко за пределами
        // его реальных 8000 байт - undefined behavior, потенциальный источник
        // "случайных" крэшей/порчи памяти. Зажимаем в границы экрана.
        self.row_position = row.min(SCREEN_HEIGHT - 1);
        self.column_position = col.min(SCREEN_WIDTH - 1);
        self.update_hw_cursor();
    }

    /// Синхронизирует аппаратный CRTC-курсор с позицией Writer.
    fn update_hw_cursor(&self) {
        crate::cursor::set_position(self.row_position, self.column_position);
    }

    /// Writer, продолжающий вывод с текущей аппаратной позиции курсора.
    /// Так ядро может печатать на экран без доступа к writer'у шелла
    /// (устройства /dev/tty, /dev/vga).
    pub fn at_hw_cursor() -> Writer {
        let pos = crate::cursor::get_position() as usize;
        Writer {
            row_position: (pos / SCREEN_WIDTH).min(SCREEN_HEIGHT - 1),
            column_position: pos % SCREEN_WIDTH,
        }
    }
}

/// Текущая позиция курсора как (row, col).
pub fn hw_cursor_pos() -> (usize, usize) {
    let pos = crate::cursor::get_position() as usize;
    ((pos / SCREEN_WIDTH).min(SCREEN_HEIGHT - 1), pos % SCREEN_WIDTH)
}

/// Переставляет аппаратный курсор в (row, col) с зажимом в границы экрана.
pub fn set_hw_cursor(row: usize, col: usize) {
    crate::cursor::set_position(
        row.min(SCREEN_HEIGHT - 1),
        col.min(SCREEN_WIDTH - 1),
    );
}

use x86_64::instructions::port::Port;

const COM1: u16 = 0x3F8;

fn outb(port: u16, val: u8) {
    unsafe { Port::new(port).write(val); }
}

fn inb(port: u16) -> u8 {
    unsafe { Port::new(port).read() }
}

fn serial_init() {
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x80);
    outb(COM1 + 0, 0x01);
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03);
    outb(COM1 + 2, 0xC7);
    outb(COM1 + 4, 0x0B);
}

pub fn serial_putchar(c: u8) {
    while (inb(COM1 + 5) & 0x20) == 0 {}
    outb(COM1 + 0, c);
}

/// Пишет строку в COM1 без аллокаций (для debug-вывода из прерываний).
/// Пишет строку в COM1 без вытеснения (маркеры диагностики не рвутся).
pub fn serial_write_atomic(s: &str) {
    x86_64::instructions::interrupts::without_interrupts(|| {
        for b in s.bytes() {
            serial_putchar(b);
        }
    });
}

pub fn serial_write(s: &str) {
    for b in s.bytes() {
        serial_putchar(b);
    }
}

/// Пишет беззнаковое число в COM1 (десятичное) без аллокаций.
pub fn serial_u64(mut v: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for &b in &buf[i..] {
        serial_putchar(b);
    }
}

/// Oчищает терминал через COM1 (ANSI-escape). Работает в QEMU с -nographic.
pub fn clear_screen_com1() {
    serial_init();
    let seq = [0x1b, b'[', b'2', b'J', 0x1b, b'[', b'H'];
    for &b in &seq {
        serial_putchar(b);
    }
}