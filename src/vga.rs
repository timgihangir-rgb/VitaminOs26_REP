// src/vga.rs
use core::sync::atomic::{AtomicUsize, Ordering::Relaxed};

const VGA_BUFFER: *mut u8 = 0xb8000 as *mut u8;

/// Размер сетки до включения композитора. Та же пара, что была раньше
/// единственной константой на весь проект: 80x30. Всё, что успевает
/// напечатать до `fb::activate` (сплэш, установщик), рисуется в этих
/// границах, и они же достаются ring 3-программам через progabi.
pub const DEFAULT_COLS: usize = 80;
pub const DEFAULT_ROWS: usize = 30;

// Сетка стала величиной времени выполнения, а не константой: композитор
// подбирает её под разрешение монитора (см. `fb::activate`), и текстовый
// буфер — это уже не 4800 байт, а cols*rows*2. Значения переживают смену
// сетки, поэтому читать их надо через `screen_cols`/`screen_rows`, а не
// копировать в константу при старте.
static COLS: AtomicUsize = AtomicUsize::new(DEFAULT_COLS);
static ROWS: AtomicUsize = AtomicUsize::new(DEFAULT_ROWS);

/// Ширина текстовой сетки в ячейках.
pub fn screen_cols() -> usize {
    COLS.load(Relaxed).max(1)
}

/// Высота текстовой сетки в ячейках.
pub fn screen_rows() -> usize {
    ROWS.load(Relaxed).max(1)
}

/// Размер текстового экрана в байтах (символ + атрибут на ячейку).
pub fn screen_bytes() -> usize {
    screen_cols() * screen_rows() * 2
}

/// Новая сетка. Ставит композитор, когда он включил графический режим и
/// вычислил масштаб; ring 3-программам размер уходит через progabi.
pub fn set_grid(cols: usize, rows: usize) {
    COLS.store(cols.max(1), Relaxed);
    ROWS.store(rows.max(1), Relaxed);
}

/// Считывает текстовый экран из видеопамяти в `buf` — рабочему столу надо
/// запомнить своё содержимое перед переключением на другой.
pub fn screen_snapshot(buf: &mut [u8]) {
    let n = screen_bytes().min(buf.len());
    unsafe {
        core::ptr::copy_nonoverlapping(VGA_BUFFER, buf.as_mut_ptr(), n);
    }
}

/// Выводит `buf` на текстовый экран.
pub fn screen_restore(buf: &[u8]) {
    let n = screen_bytes().min(buf.len());
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), VGA_BUFFER, n);
    }
}

/// Заполняет `buf` пустым экраном (пробелы на чёрном, атрибут 0x07).
pub fn screen_blank(buf: &mut [u8]) {
    for cell in buf.chunks_exact_mut(2).take(screen_cols() * screen_rows()) {
        cell[0] = b' ';
        cell[1] = 0x07;
    }
}

/// Байт кодовой страницы экрана для одного символа Unicode.
///
/// Ячейка текстового экрана - один байт плюс атрибут, и в одном байте надо
/// уместить и латиницу, и кириллицу. Страницу выбрали CP1251: её
/// кириллица ложится в 0xC0..=0xFF (а-я) и 0xA8/0xB8 (Ё/ё), а ASCII
/// остаётся на своём месте, так что программы, печатающие английский
/// текст, не меняются ни на байт. Раньше страницей был CP437, но
/// псевдографика (0xB0-0xDF) в проекте не используется нигде, а
/// кириллицы в нём нет.
///
/// `src/font8x16.bin` собран ровно под эту нумерацию: индекс глифа =
/// байт ячейки, кириллица лежит в слотах 0xA8, 0xB8 и 0xC0..=0xFF.
/// Генерируется `tools/mkfont.py`.
///
/// Чего кодовая страница не умеет - emoji, стрелок и прочего Unicode
/// вне Latin-1: такие символы рисуются как '?'. Это осознанный размен:
/// хранить в ячейке два байта UTF-8 нельзя, сломается всё, что считает
/// ширину строки в ячейках.
pub fn code_page(c: char) -> u8 {
    match c as u32 {
        0x0401 => 0xA8,             // Ё
        0x0451 => 0xB8,             // ё
        0x0410..=0x042F => (c as u32 - 0x0410 + 0xC0) as u8, // А..Я
        0x0430..=0x044F => (c as u32 - 0x0430 + 0xE0) as u8, // а..я
        0x20..=0x7E => c as u8,     // ASCII
        0xA0..=0xFF => c as u8,     // Latin-1: файлы в cp1252 читаются почти как есть
        _ => b'?',
    }
}

pub const COLOR_LIGHT_GREEN: u8 = 0x0A;
pub const COLOR_LIGHT_CYAN: u8 = 0x0B;
pub const COLOR_LIGHT_RED: u8 = 0x0C;
pub const COLOR_WHITE: u8 = 0x0F;
pub const COLOR_DARK_GREY: u8 = 0x08;

const CRTC_INDEX: u16 = 0x3D4;
const CRTC_DATA: u16 = 0x3D5;
const SEQ_INDEX: u16 = 0x3C4;
const SEQ_DATA: u16 = 0x3C5;
const MISC_OUTPUT: u16 = 0x3C2;

/// Буфер для чтения текущего аппаратного шрифта 8x16 из plane 2.
static mut FONT_BUF: [u8; 8192] = [0; 8192];

/// Тайл 8x16: 256 глифов по 16 байт, индекс глифа = байт кодовой страницы
/// (см. `code_page`). Тот же бинарник, что у композитора, - и кириллица
/// рисуется одинаково в графическом и в текстовом режиме.
static FONT: &[u8; 4096] = include_bytes!("font8x16.bin");

/// Слоты кодовой страницы CP1251, занятые кириллицей: Ё, ё и А..Я, а..я.
/// Латиница 0x00..0x7F и Latin-1 0xA0..0xFF не трогаем - там BIOS-шрифт
/// уже годен, а 0xA0..0xA7 в CP1251 это не кириллица.
const CYRILLIC_CODES: &[u8] = &[
    0xA8, 0xB8, 0xC0, 0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9, 0xCA, 0xCB, 0xCC, 0xCD,
    0xCE, 0xCF, 0xD0, 0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xDB, 0xDC, 0xDD,
    0xDE, 0xDF, 0xE0, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xEB, 0xEC, 0xED,
    0xEE, 0xEF, 0xF0, 0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFB, 0xFC, 0xFD,
    0xFE, 0xFF,
];

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

    // Форма курсора. Без этого курсор невидим: GRUB оставляет в CRTC 0x0A
    // бит 5 («курсор выключен»). Подчёркивание внизу клетки — как в обычном
    // терминале.
    crate::cursor::set_shape(crate::cursor::UNDERLINE.0, crate::cursor::UNDERLINE.1);
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
        // Латиницу оставляем BIOS-овскую: она в 8x16 отполирована годами и
        // ASCII у нас с BIOS совпадает байт в байт. А кириллицу BIOS не
        // содержит - в его кодовой странице на слотах 0xA8/0xB8/0xC0..0xFF
        // лежит псевдографика CP437, и русский текст в чистом текстовом
        // режиме печатался бы рамками. Подставляем свои глифы из того же
        // font8x16.bin, которым рисует композитор (см. `fb::FONT`), чтобы
        // буквы выглядели одинаково в обоих режимах.
        for &code in CYRILLIC_CODES {
            let c = code as usize;
            FONT_BUF[c * 32..c * 32 + 16].copy_from_slice(&FONT[c * 16..c * 16 + 16]);
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
            b'\n' => {
                self.new_line();
                self.update_hw_cursor();
                // Перевод строки НЕ отрисовывает экран. Если рисовать здесь,
                // команда из тридцати строк выводилась бы тридцатью кадрами —
                // глаз видит текст, «печатающийся» сам, а не готовый результат.
                // Отрисовка идёт по событию: шелл зовёт fb::flush один раз на
                // команду, редактор строки — раз на нажатую клавишу.
                return;
            }
            byte => {
                if self.column_position >= screen_cols() {
                    self.new_line();
                }

                let row = self.row_position;
                let col = self.column_position;

                unsafe {
                    let offset = (row * screen_cols() + col) * 2;
                    *VGA_BUFFER.add(offset) = byte;
                    *VGA_BUFFER.add(offset + 1) = self.color_code;
                }

                self.column_position += 1;
            }
        }
        self.update_hw_cursor();
    }

    fn new_line(&mut self) {
        self.column_position = 0;
        self.row_position += 1;
        if self.row_position >= screen_rows() {
            self.scroll_up();
            self.row_position = screen_rows() - 1;
        }
    }

    fn scroll_up(&mut self) {
        unsafe {
            for row in 1..screen_rows() {
                for col in 0..screen_cols() {
                    let src_offset = (row * screen_cols() + col) * 2;
                    let dst_offset = ((row - 1) * screen_cols() + col) * 2;
                    *VGA_BUFFER.add(dst_offset) = *VGA_BUFFER.add(src_offset);
                    *VGA_BUFFER.add(dst_offset + 1) = *VGA_BUFFER.add(src_offset + 1);
                }
            }

            for col in 0..screen_cols() {
                let offset = ((screen_rows() - 1) * screen_cols() + col) * 2;
                *VGA_BUFFER.add(offset) = b' ';
                *VGA_BUFFER.add(offset + 1) = self.color_code;
            }
        }
    }

    pub fn write_string(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.write_byte(b'\n');
            } else {
                self.write_byte(code_page(c));
            }
        }
        // Здесь экран НЕ перерисовывается: вызов идёт на каждую строку
        // команды, и отрисовка здесь означала бы ровно то построчное
        // «печатание», которого мы убираем. Точка отрисовки — у вызывающего
        // (см. `sync`).
    }

    /// Показать накопленный вывод на экране. Звать нужно там, где порция
    /// вывода кончилась по смыслу, а не на каждой строке: конец команды в
    /// шелле, перерисовка полноэкранного режима, нажатая клавиша в редакторе
    /// строки.
    pub fn sync(&mut self) {
        crate::fb::flush();
    }

    pub fn clear_screen(&mut self) {
        unsafe {
            for row in 0..screen_rows() {
                for col in 0..screen_cols() {
                    let offset = (row * screen_cols() + col) * 2;
                    *VGA_BUFFER.add(offset) = b' ';
                    *VGA_BUFFER.add(offset + 1) = self.color_code;
                }
            }
        }
        self.row_position = 0;
        self.column_position = 0;
        self.update_hw_cursor();
        // Очистку тоже не отрисовываем сразу: у вызывающего (`clear`,
        // установщик, splash) следом идёт содержимое, и мигание пустым экраном
        // между кадрами заметно глазом.
    }

    pub fn ensure_lines_available(&mut self, needed: usize) {
        if needed >= screen_rows() {
            for _ in 0..screen_rows() {
                self.scroll_up();
            }
            self.row_position = 0;
            self.column_position = 0;
            self.update_hw_cursor();
            return;
        }

        while self.row_position + needed > screen_rows() {
            self.scroll_up();
            if self.row_position > 0 {
                self.row_position -= 1;
            }
        }
        self.update_hw_cursor();
    }

    /// Прокручивает экран вверх на `n` строк, поднимая позицию вывода.
    /// Нужно редактору строки, который сам следит, где на экране его текст.
    pub fn scroll_up_by(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        for _ in 0..n {
            self.scroll_up();
        }
        self.row_position = self.row_position.saturating_sub(n);
        self.update_hw_cursor();
    }

    /// Пишет байт в конкретную ячейку, не трогая позицию вывода: так
    /// редактор строки рисует текст по своему собственному отсчёту от
    /// начала ввода (промпт может начинаться с середины экрана).
    pub fn put(&mut self, row: usize, col: usize, byte: u8) {
        if row >= screen_rows() || col >= screen_cols() {
            return;
        }
        unsafe {
            let offset = (row * screen_cols() + col) * 2;
            *VGA_BUFFER.add(offset) = byte;
            *VGA_BUFFER.add(offset + 1) = self.color_code;
        }
    }

            /// Выводит содержимое файла: байты из VFS, а не готовая строка.
    ///
    /// Файл может быть в UTF-8 (так его пишет `echo` и любой текстовый
    /// редактор) или в CP1251 (так записаны старые .conf и файлы из
    /// Windows). Разбираем UTF-8, а всё, что не образует корректную
    /// последовательность, выводим как байт кодовой страницы: тогда русский
    /// UTF-8 и русский CP1251 показываются одинаково, а не превращаются в
    /// «кракозябры» из отдельных байтов на каждую букву.
    ///
    /// Отдельный метод, а не `write_string`: там вход — `&str`, и
    /// невалидный UTF-8 там уже не разобрать.
    pub fn write_raw_text(&mut self, data: &[u8]) {
        let mut rest = data;
        loop {
            match core::str::from_utf8(rest) {
                Ok(s) => {
                    self.write_string(s);
                    return;
                }
                Err(e) => {
                    let (good, bad) = rest.split_at(e.valid_up_to());
                    // `good` по построению валиден - иначе from_utf8 вернул бы
                    // valid_up_to == 0 и мы бы зациклились.
                    self.write_string(core::str::from_utf8(good).unwrap_or(""));
                    match e.error_len() {
                        // Неполная последовательность на конце: это обрывок
                        // файла (или буфер, в который ещё допишут). Печатаем
                        // байты как есть - в кодовой странице это буквы.
                        None => {
                            for &b in bad {
                                self.write_byte(b);
                            }
                            return;
                        }
                        Some(n) => {
                            for &b in &bad[..n] {
                                self.write_byte(b);
                            }
                            rest = &bad[n..];
                        }
                    }
                }
            }
        }
    }

    /// Пишет один символ Unicode в текущую позицию вывода, переведя его в
    /// байт кодовой страницы. Эхо ввода (`linein`) идёт через неё.
    pub fn write_char(&mut self, c: char) {
        self.write_byte(code_page(c));
    }

    /// То же, что `put`, но для символа Unicode: байт кодовой страницы
    /// выбирает `code_page`. Редактор строки рисует так - иначе русский
    /// текст в командной строке распался бы на байты UTF-8.
    pub fn put_char(&mut self, row: usize, col: usize, c: char) {
        self.put(row, col, code_page(c));
    }

    /// Затирает пробелами прямоугольник `rows` x `cols`, отсчёт от (row, col).
    /// Позиция вывода не меняется — её ставит вызывающий.
    pub fn fill_rect(&mut self, row: usize, col: usize, rows: usize, cols: usize) {
        for r in 0..rows {
            let rr = row + r;
            if rr >= screen_rows() {
                break;
            }
            let from = if r == 0 { col } else { 0 };
            let to = if r == 0 { cols } else { screen_cols() };
            for c in from..to.min(screen_cols()) {
                self.put(rr, c, b' ');
            }
        }
        // Отрисовки здесь нет: редактор строки зовёт `sync` один раз на
        // нажатую клавишу, после всех `put`/`fill_rect` подряд.
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
        (self.row_position * screen_cols() + self.column_position) * 2
    }

    pub fn set_cursor(&mut self, row: usize, col: usize) {
        // Раньше значения принимались без проверки: set_cursor(33, ...) на
        // экране 80x25 означал прямую запись в VGA_BUFFER далеко за пределами
        // его реальных 8000 байт - undefined behavior, потенциальный источник
        // "случайных" крэшей/порчи памяти. Зажимаем в границы экрана. Тем
        // более обязательно теперь, когда сетка подстраивается под
        // разрешение: буфер меняет размер вместе с ней.
        self.row_position = row.min(screen_rows() - 1);
        self.column_position = col.min(screen_cols() - 1);
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
            row_position: (pos / screen_cols()).min(screen_rows() - 1),
            column_position: pos % screen_cols(),
            color_code: COLOR_LIGHT_GREEN,
        }
    }
}

/// Текущая позиция курсора как (row, col).
pub fn hw_cursor_pos() -> (usize, usize) {
    let pos = crate::cursor::get_position() as usize;
    ((pos / screen_cols()).min(screen_rows() - 1), pos % screen_cols())
}

/// Переставляет аппаратный курсор в (row, col) с зажимом в границы экрана.
pub fn set_hw_cursor(row: usize, col: usize) {
    crate::cursor::set_position(
        row.min(screen_rows() - 1),
        col.min(screen_cols() - 1),
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