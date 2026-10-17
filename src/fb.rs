//! Линейный фреймбуфер: адаптация экрана под монитор.
//!
//! Проблема: ядро и ring3-программы исторически пишут текстовые ячейки
//! (символ+атрибут) прямо в окно VGA `0xB8000` и переключают CRTC в режим
//! 80x30. На мониторе с нативным разрешением 1920x1080 такой режим мылится
//! и растягивается.
//!
//! Решение без ломки архитектуры: загрузчик (GRUB) ставит графический режим
//! под монитор по EDID и передаёт multiboot2-тег 8 (framebuffer). Ядро
//! отображает этот линейный фреймбуфер и блитит в него текстовый буфер
//! `0xB8000`. Все, кто пишет в `0xB8000` (ядро, шелл, рабочие столы,
//! ring3-программы, жёстко зашитые на 0xB8000), продолжают работать без
//! изменений: в графическом режиме `0xB8000` — это RAM-тень, а
//! визуализацией занимается этот модуль.
//!
//! Тайл 8x16 берётся из встроенного шрифта `font8x16.bin`, потому что после
//! перехода в VBE plane 2 может быть уже недоступна. Шрифт собран
//! `tools/mkfont.py`: латиница 0x00..0x7F — классический VGA (CP437), поверх
//! неё кириллица в позициях CP1251 (0xC0..0xDF — А-Я, 0xE0..0xFF — а-я, плюс
//! Ё/ё), потому что ячейка экрана — один байт, и обе половины алфавита
//! обязаны жить в одном байте.
//!
//! Если тега нет (свой 2-стадийный загрузчик) — модуль не активируется, и
//! система работает в обычном текстовом режиме 80x30, как раньше.
//!
//! # Почему вывод появляется весь и сразу
//!
//! Долго вывод ложился на экран ступеньками по двум разным причинам.
//!
//! Первая — расписание. Рисование жило только в задаче-композиторе, которую
//! будил тик PIT раз в 10 мс. На single-core это означало: (1) задержку до
//! 10 мс на каждое обновление, (2) конкуренцию с шеллом за квант, (3) полную
//! остановку отрисовки на время дискового PIO — ATA/AHCI работают с IF=0, тик
//! таймера не идёт, а `wal::commit_portion` ещё и замораживает планирование.
//!
//! Вторая — сам темп отрисовки. Прокрутка `web` меняет весь экран, а
//! растеризация глифа шла по одной операции на пиксель; полный кадр 80x30 —
//! 307200 пикселей, единицы миллионов операций, десятки миллисекунд. Композитор
//! с тиком в 10 мс такого кадра не успевал никогда, поэтому страница
//! проезжала перед глазами кусками.
//!
//! Что сделано:
//!
//! * Отрисовка **синхронная и по событию, а не по строке**. `flush()` зовёт
//!   `Writer` там, где порция вывода кончилась по смыслу: шелл — один раз на
//!   команду (перед приглашением), редактор строки — раз на нажатую клавишу.
//!   На переводе строки и в конце `write_string` ничего не рисуется, поэтому
//!   команда с тридцатью строками выводится одним кадром, а не тридцатью.
//!   Задержка от вывода до картинки — время растеризации одного кадра, а не 10 мс.
//! * Ячейки **кешируются в готовом виде** (`CELL_CACHE`): символ с атрибутом
//!   растеризуется один раз в пиксельный блок, дальше в кадр идёт копирование
//!   блока вместо построчной растеризации. Это то, что делает полный кадр и
//!   прокрутку мгновенными.
//!
//! Задача-композитор (`tick`) осталась как страховка для ring3-программ,
//! которые пишут в `0xB8000` в обход `Writer` (snake, vita, fetch, web): им
//! синхронной отрисовки от шелла не достать.

use crate::paging;
use core::sync::atomic::{AtomicBool, Ordering};

/// Текстовый буфер VGA. В текстовом, и в графическом режиме это одна и та
/// же физическая область: писать в неё можно всегда, и модуль её читает.
const TEXT_BASE: *mut u8 = 0xb8000 as *mut u8;
const CELL_W: usize = 8;
const CELL_H: usize = 16;

/// Потолок масштаба текста. Ограничивает буфер строки в растеризаторе:
/// CELL_W * MAX_SCALE пикселей на строку глифа.
const MAX_SCALE: usize = 8;

// ─── Как выбирается сетка ───────────────────────────────────────────────────
//
// Раньше сетка была жёстко 80x30, а масштаб подбирался под неё:
// `min(ширина/640, высота/480)`. На мониторе 1280x800 это давало scale=1, то
// есть буквы 8x16 пикселей посреди экрана, занимающие четверть кадра: чем
// больше разрешение, тем мельче и «глубже» текст.
//
// Теперь наоборот: сначала выбирается масштаб, потом под него считается
// сетка. Это ровно то, что делает настоящий текстовый режим VBE: ты говоришь
// «сколько колонок и строк», а железо/драйвер подбирает размер ячейки.
//
// Правило: шрифт всегда x1, сетка разворачивается под весь кадр
// (8x16 на ячейку). Масштаб подрастает только тогда, когда сетка
// перестала бы удерживать MAX_COLS x MAX_ROWS на широких стенах:
//
//     640x480   -> scale 1, 80x30   (исторический режим, ничего не меняется)
//     1280x800  -> scale 1, 160x50  (весь кадр, мелких строк вдвое больше)
//     1280x1024 -> scale 1, 160x64  (весь кадр)
//     1920x1080 -> scale 1, 200x67  (ширина упирается в MAX_COLS)
//     2560x1440 -> scale 2, 160x45  (x1 дал бы 320x90 > MAX_COLS)
//     3840x2160 -> scale 3, 160x45  (x2 дал бы 240x67 > MAX_COLS)
//
// MAX_COLS/MAX_ROWS держат `LAST` статическим массивом: без них на
// ultrawide 20000x6000 сетка поплыла бы за 200 колонок.
const MAX_COLS: usize = 200;
const MAX_ROWS: usize = 100;
const TEXT_BYTES: usize = MAX_COLS * MAX_ROWS * 2;

/// Встроенный шрифт 8x16 по индексу CP437 (256 глифов * 16 байт).
static FONT: &[u8; 4096] = include_bytes!("font8x16.bin");

/// Сырой тег 8, разобранный до инициализации аллокатора (только чтение mb2).
#[derive(Clone, Copy)]
pub struct RawFb {
    pub phys: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u8,
    r_pos: u8,
    r_mask: u8,
    g_pos: u8,
    g_mask: u8,
    b_pos: u8,
    b_mask: u8,
}

#[derive(Clone, Copy)]
struct Fb {
    addr: *mut u8,
    pitch: u32,
    width: u32,
    height: u32,
    bpp: u8,
    r_pos: u8,
    r_mask: u8,
    g_pos: u8,
    g_mask: u8,
    b_pos: u8,
    b_mask: u8,
    scale_x: u32,
    scale_y: u32,
    off_x: u32,
    off_y: u32,
    /// Сетка в ячейках. Дублирует `vga::screen_cols/rows`, но живёт рядом с
    /// геометрией кадра - растеризатору она нужна на каждой строке, и
    /// доставать её из другого модуля значило бы платить за лишний запрос.
    cols: usize,
    rows: usize,
    /// Промежуточный буфер на ширину текстовой строки. Пиксели копятся в
    /// нём (это обычные записи в память) и оттуда уходят в кадр крупными
    /// кусками: прямые записи каждого пикселя в LFB дороже всего остального.
    rowbuf: *mut u32,
}

static mut FB: Option<Fb> = None;

/// Последнее отрисованное содержимое текстового буфера и позиция курсора.
/// Последнее отрисованное содержимое текстового буфера и позиция курсора.
static mut LAST: [u8; TEXT_BYTES] = [0; TEXT_BYTES];
static mut LAST_CURSOR: (usize, usize) = (usize::MAX, usize::MAX);

/// Первый `flush` обязан перерисовать ВСЁ: до него содержимое кадра — мусор
/// видеопамяти, а `LAST` ещё не с чем сравнивать. Флаг снимается после
/// первого прохода, дальше работает обычное сравнение.
static mut NEED_FULL: bool = true;

/// Готовые цвета всех 256 атрибутов, посчитанные один раз в `init`.
/// Растеризатору остаётся только два чтения массива на ячейку вместо
/// вызова `palette` + `pack` с тремя делениями на каждую.
/// Индекс — байт атрибута; отдельно лежит инвертированная пара для курсора.
static mut ATTR_FG: [u32; 256] = [0; 256];
static mut ATTR_BG: [u32; 256] = [0; 256];
static mut ATTR_FG_INV: [u32; 256] = [0; 256];
static mut ATTR_BG_INV: [u32; 256] = [0; 256];

/// Кто сейчас рисует. `flush()` вызывается и из `Writer` (контекст шелла), и
/// из задачи-композитора, поэтому нужен арбитраж. Блокировки нет и быть не
/// должно: не дождавшись, мы просто уходим — рисующий контекст сам дойдёт
/// до конца и отрисует всё накопленное (он читает текст, а не флаг).
static RENDERING: AtomicBool = AtomicBool::new(false);

// ─── Кеш готовых ячеек ───────────────────────────────────────────────────────
//
// Растеризация глифа «на лету» стоит ~15 операций на пиксель: сдвиг бита,
// сравнение, выбор цвета, запись в промежуточный буфер — и всё это по внутренним
// циклам, которые на opt-level=0 не разворачиваются. Полный кадр 80x30 —
// это 307200 пикселей, то есть единицы миллионов операций: экран 1280x800
// успевал перерисовываться за десятки миллисекунд, и прокрутка в `web`
// дёргалась кусками.
//
// Лечится кешем: ячейка (символ + атрибут) растеризуется ОДИН раз в пиксельный
// блок, дальше в кадр идёт копирование блока. На типичном экране из ~2400 ячеек
// уникальных пар «символ+атрибут» несколько сотен, так что прямого кеша на
// несколько сотен ячеек хватает с большим запасом, а промах стоит ровно
// старой ценой.
//
// Память фиксированная и не растёт от масштаба: общий бюджет делится на размер
// ячейки в пикселях. На масштабе 1 ячейка — 512 байт (512 ячеек в кеше),
// на масштабе 8 ячейка — 32 КБ, и кеш честно выключается (медленный путь).
//
// Хранится сразу в `u32`, а не в байтах: указатель на блок кеша должен быть
// выровнен под пиксель, иначе каждый `add` на указателе спотыкается о
// проверку выравнивания. Размер и шаг считаются в пикселях.
const CELL_CACHE_U32: usize = 64 * 1024;
#[repr(C, align(16))]
struct CellCache([u32; CELL_CACHE_U32]);
static mut CELL_CACHE: CellCache = CellCache([0; CELL_CACHE_U32]);
static mut CELL_KEYS: [u32; 1024] = [0; 1024];
/// Сколько ячеек влезает в бюджет и сколько пикселей занимает одна.
/// 0 — кеш выключен.
static mut CELL_ENTRIES: usize = 0;
static mut CELL_STRIDE: usize = 0;

/// Палитра EGA/CGA: 16 цветов атрибута текстового режима.
static PALETTE: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00), // 0 black
    (0x00, 0x00, 0xAA), // 1 blue
    (0x00, 0xAA, 0x00), // 2 green
    (0x00, 0xAA, 0xAA), // 3 cyan
    (0xAA, 0x00, 0x00), // 4 red
    (0xAA, 0x00, 0xAA), // 5 magenta
    (0xAA, 0x55, 0x00), // 6 brown
    (0xAA, 0xAA, 0xAA), // 7 light grey
    (0x55, 0x55, 0x55), // 8 dark grey
    (0x55, 0x55, 0xFF), // 9 light blue
    (0x55, 0xFF, 0x55), // 10 light green
    (0x55, 0xFF, 0xFF), // 11 light cyan
    (0xFF, 0x55, 0x55), // 12 light red
    (0xFF, 0x55, 0xFF), // 13 light magenta
    (0xFF, 0xFF, 0x55), // 14 yellow
    (0xFF, 0xFF, 0xFF), // 15 white
];

unsafe fn rd_u32(p: *const u8, off: usize) -> u32 {
    core::ptr::read_unaligned(p.add(off) as *const u32)
}
unsafe fn rd_u64(p: *const u8, off: usize) -> u64 {
    core::ptr::read_unaligned(p.add(off) as *const u64)
}
unsafe fn rd_u8(p: *const u8, off: usize) -> u8 {
    core::ptr::read_unaligned(p.add(off))
}

/// Ищет в multiboot2-инфо тег 8 (framebuffer). Чистое чтение, без аллокаций —
/// вызывается до инициализации памяти, чтобы решить, трогать ли CRTC.
pub fn probe(mb_info_ptr: u64) -> Option<RawFb> {
    if mb_info_ptr == 0 {
        return None;
    }
    unsafe {
        let p = mb_info_ptr as *const u8;
        let total = rd_u32(p, 0) as usize;
        let mut off = 8usize;
        while off + 8 <= total {
            let tag = rd_u32(p, off);
            let size = rd_u32(p, off + 4) as usize;
            if tag == 0 {
                break;
            }
            // Минимальный размер тега 8 с RGB-описанием — 38 байт.
            if tag == 8 && size >= 38 && off + 38 <= total {
                let phys = rd_u64(p, off + 8);
                let pitch = rd_u32(p, off + 16);
                let width = rd_u32(p, off + 20);
                let height = rd_u32(p, off + 24);
                let bpp = rd_u8(p, off + 28);
                let ftype = rd_u8(p, off + 29);
                if ftype == 1
                    && phys != 0
                    && pitch >= width
                    && width >= 320
                    && height >= 200
                    && matches!(bpp, 16 | 24 | 32)
                {
                    // После bpp/type идёт u16 reserved, затем цветовые поля.
                    return Some(RawFb {
                        phys,
                        pitch,
                        width,
                        height,
                        bpp,
                        r_pos: rd_u8(p, off + 32),
                        r_mask: rd_u8(p, off + 33),
                        g_pos: rd_u8(p, off + 34),
                        g_mask: rd_u8(p, off + 35),
                        b_pos: rd_u8(p, off + 36),
                        b_mask: rd_u8(p, off + 37),
                    });
                }
            }
            if size == 0 {
                break;
            }
            off += size;
            off = (off + 7) & !7;
        }
    }
    crate::vga::serial_write_atomic("[N] no fb tag\n");
    None
}

/// Восьмизначный hex без префикса: адрес тени в serial читают автотесты.
fn hex32(v: u64) -> alloc::string::String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = alloc::string::String::with_capacity(16);
    let mut started = false;
    for i in (0..16).rev() {
        let d = ((v >> (i * 4)) & 0xF) as usize;
        if d != 0 || started {
            started = true;
            s.push(D[d] as char);
        }
    }
    if !started {
        s.push('0');
    }
    s
}

/// Подбирает масштаб и размер текстовой сетки под кадр `w` x `h`.
///
/// Возвращает `(scale, cols, rows)`. Ячейка всегда 8x16 пикселей, поэтому
/// сетка - это просто `w / (8*scale)` на `h / (16*scale)`.
///
/// Ограничения, в порядке важности:
///   * масштаб не больше MAX_SCALE - иначе не влезает буфер строки
///     растеризатора (CELL_W * MAX_SCALE пикселей);
///   * сетка не шире MAX_COLS и не выше MAX_ROWS - держат `LAST` статикой.
///
/// Если кадр настолько мал, что даже x1 не дотянуть до MAX_COLS -
/// берём что получилось и растягиваем сетку вниз по рамкам.
fn fit_grid(w: u32, h: u32) -> (u32, usize, usize) {
    let cell_w = (CELL_W as u32).max(1);
    let cell_h = (CELL_H as u32).max(1);

    // Шрифт оставляем в масштабе 1 — крупные буквы x2 на 1280x800 не
    // вмещали ни одного десятка строк, пользоваться было неудобно (см.
    // обратную связь пользователя). Сетка при этом разворачивается под
    // весь кадр: иначе при 1280x800 текст снова осел бы в углу 640x480.
    // Масштаб увеличиваем только чтобы удержать сетку внутри MAX_COLS/
    // MAX_ROWS на стенах 3840x2160 и шире.
    let mut scale = 1u32;
    loop {
        let too_wide = (w / (cell_w * scale)) as usize > MAX_COLS;
        let too_high = (h / (cell_h * scale)) as usize > MAX_ROWS;
        if (!too_wide && !too_high) || scale >= MAX_SCALE as u32 {
            break;
        }
        scale += 1;
    }

    let cols = ((w / (cell_w * scale)) as usize).clamp(1, MAX_COLS);
    let rows = ((h / (cell_h * scale)) as usize).clamp(1, MAX_ROWS);
    (scale, cols, rows)
}

/// Активирует фреймбуфер: отображает MMIO и вычисляет масштаб текстовой сетки
/// под разрешение монитора. Возвращает false, если отобразить не удалось.
pub fn init(raw: RawFb) -> bool {
    let len = raw.pitch as u64 * raw.height as u64;
    // Кадровый буфер - обычная RAM (VBE линейный кадр), поэтому отображаем
    // его с кэшированием. Через NO_CACHE каждая запись пикселя идёт мимо
    // кэша CPU: на 1280x800 это тысячи записей на строку текста, и композитор
    // не успевает рисовать - вывод выглядит построчным, а система кажется
    // заторможенной. Настоящее MMIO (AHCI, сетевая карта) продолжает
    // отображаться через paging::map_phys_in_kernel.
    let va = match unsafe { paging::map_phys_cached(raw.phys, len) } {
        Some(v) => v,
        None => {
            crate::vga::serial_write_atomic("[N] fb map failed\n");
            return false;
        }
    };

    // Сетка считается до тени: тень отображается ровно настолько страниц,
    // сколько сетка занимает, а не «на две страницы, как раньше».
    // Масштаб целочисленный и одинаковый по обеим осям - иначе шрифт
    // растянется по горизонтали; колонки и строки добирают остаток кадра
    // (подробности в блоке про fit_grid выше).
    let (scale, cols, rows) = fit_grid(raw.width, raw.height);

    // В VBE-режиме физический 0xB8000 текст не хранит: до первого чтения
    // композитором подменяем его на RAM-тень (и в ядре, и в будущих задачах).
    let text_bytes = cols * rows * 2;
    let shadow_pages = (text_bytes + 4095) / 4096;
    let shadow = match unsafe { paging::install_vga_shadow(shadow_pages) } {
        Some(s) => s,
        None => {
            crate::vga::serial_write_atomic("[N] vga shadow failed\n");
            return false;
        }
    };
    // Адрес тени в serial: автотесты читают экран через pmemsave и после
    // появления композитора физический 0xB8000 больше не содержит текст.
    // Печатаем с префиксом 0x - выражение HMP без него не разбирает адрес,
    // начинающийся с буквы.
    crate::vga::serial_write_atomic("[N] vga shadow phys=0x");
    crate::vga::serial_write_atomic(hex32(shadow[0]).as_str());
    crate::vga::serial_write_atomic(" pages=");
    crate::vga::serial_u64(shadow.len() as u64);
    crate::vga::serial_putchar(b'\n');

    let draw_w = cols as u32 * (CELL_W as u32) * scale;
    let draw_h = rows as u32 * (CELL_H as u32) * scale;
    // Сетка по построению не больше кадра, но при нестандартном pitch/EDID
    // лучше не рисковать вычитанием без проверки: в debug-сборке включены
    // overflow-checks, и underflow здесь был бы паникой ядра на старте.
    let off_x = raw.width.saturating_sub(draw_w) / 2;
    let off_y = raw.height.saturating_sub(draw_h) / 2;

    // Сетка - общее дело ядра и ring 3: пока её не объявили, программа
    // считает экран по умолчанию 80x30 и рисует не туда.
    crate::vga::set_grid(cols, rows);
    crate::progabi::publish_grid(cols as u32, rows as u32);

    crate::vga::serial_write_atomic("[N] fb ");
    crate::vga::serial_u64(raw.width as u64);
    crate::vga::serial_write_atomic("x");
    crate::vga::serial_u64(raw.height as u64);
    crate::vga::serial_write_atomic(" scale=");
    crate::vga::serial_u64(scale as u64);
    crate::vga::serial_write_atomic(" grid=");
    crate::vga::serial_u64(cols as u64);
    crate::vga::serial_write_atomic("x");
    crate::vga::serial_u64(rows as u64);
    crate::vga::serial_write_atomic(" bpp=");
    crate::vga::serial_u64(raw.bpp as u64);
    crate::vga::serial_write_atomic(" pitch=");
    crate::vga::serial_u64(raw.pitch as u64);
    crate::vga::serial_write_atomic(" rgb=");
    crate::vga::serial_u64(
        ((raw.r_pos as u64) << 24)
            | ((raw.r_mask as u64) << 16)
            | ((raw.g_pos as u64) << 8)
            | (raw.g_mask as u64),
    );
    crate::vga::serial_write_atomic("/");
    crate::vga::serial_u64(((raw.b_pos as u64) << 8) | (raw.b_mask as u64));
    crate::vga::serial_write_atomic(" phys=");
    crate::vga::serial_u64(raw.phys);
    crate::vga::serial_write_atomic("\n");

    // Буфер строки живёт в куче: в bss места впритык (ядро грузится на 2 МиБ,
    // а trampoline отображает первые 4 МиБ).
    let rowbuf: &'static mut [u32] = alloc::boxed::Box::leak(
        alloc::vec![0u32; cols * CELL_W * MAX_SCALE].into_boxed_slice(),
    );

    let fb = Fb {
        addr: va as *mut u8,
        rowbuf: rowbuf.as_mut_ptr(),
        pitch: raw.pitch,
        width: raw.width,
        height: raw.height,
        bpp: raw.bpp,
        r_pos: raw.r_pos,
        r_mask: raw.r_mask,
        g_pos: raw.g_pos,
        g_mask: raw.g_mask,
        b_pos: raw.b_pos,
        b_mask: raw.b_mask,
        scale_x: scale,
        scale_y: scale,
        off_x,
        off_y,
        cols,
        rows,
    };

    unsafe {
        // Таблица цветов всех 256 атрибутов - один раз, а не на каждую ячейку
        // каждой строки при каждой отрисовке.
        for a in 0..256usize {
            let attr = a as u8;
            let (fr, fg, fb_) = palette(attr & 0x0F);
            let (br, bg, bb) = palette((attr >> 4) & 0x07);
            ATTR_FG[a] = pack(&fb, fr, fg, fb_);
            ATTR_BG[a] = pack(&fb, br, bg, bb);
            ATTR_FG_INV[a] = ATTR_BG[a];
            ATTR_BG_INV[a] = ATTR_FG[a];
        }

        FB = Some(fb);
        // Бюджет кеша ячеек: столько ячеек в него влезает, сколько помещается
        // при текущем масштабе. При очень крупном масштабе ячейка не помещается
        // даже одна — тогда кеш выключается целиком и работает прямой путь.
        let cell_px = CELL_W * fb.scale_x as usize * CELL_H * fb.scale_y as usize;
        let mut entries = CELL_CACHE_U32 / cell_px.max(4);
        if entries > 1024 {
            entries = 1024;
        }
        if entries < 16 {
            entries = 0;
        }
        CELL_ENTRIES = entries;
        CELL_STRIDE = cell_px;
        for k in CELL_KEYS.iter_mut() {
            *k = 0;
        }
        // Первичная заливка: старый мусор видеопамяти не должен мелькать.
        clear(fb);
        // И первый проход обязан перерисовать все строки целиком.
        NEED_FULL = true;
        LAST = [0; TEXT_BYTES];
        LAST_CURSOR = (usize::MAX, usize::MAX);
    }
    true
}

pub fn active() -> bool {
    unsafe { FB.is_some() }
}

/// Параметры активного кадрового буфера для sysinfo/fetch:
/// (ширина, высота, бит на пиксель, масштаб сетки, активен ли композитор).
/// `(0, 0, 0, 0, false)` — линейного кадрового буфера нет, система в
/// текстовом режиме 80x30.
pub fn info() -> (u32, u32, u8, u8, bool) {
    unsafe {
        match &FB {
            Some(f) => (
                f.width,
                f.height,
                f.bpp,
                f.scale_x.max(1) as u8,
                true,
            ),
            None => (0, 0, 0, 0, false),
        }
    }
}

/// Отрисовывает всё, что изменилось в текстовом буфере. Основная точка входа:
/// её зовёт `vga::Writer` в конце порции вывода, поэтому текст появляется на
/// экране в том же контексте, без ожидания тика таймера.
pub fn flush() {
    if !active() {
        return;
    }
    // Арбитраж с задачей-композитором. Синхронный вызов (конец порции
    // вывода) обязан дождаться своей очереди: если сразу отдать кадр
    // композитору, вывод нарисуется на следующем тике, то есть с задержкой
    // и отдельно от остального текста. Композитор ждёт не дольше, чем
    // длится одна отрисовка, поэтому ожидание здесь короткое и не
    // перепродавливает кадр надолго.
    let mut spins = 0u32;
    loop {
        if RENDERING
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            break;
        }
        spins += 1;
        if spins > 64 {
            // Кадр держит кто-то, кто вот-вот закончит: отдаём ему и
            // выходим, следующий тик дорисует остаток.
            return;
        }
        core::hint::spin_loop();
    }
    unsafe { render_dirty() };
    RENDERING.store(false, Ordering::Release);
}

/// Периодический вызов из задачи-композитора: страховка для ring3-программ,
/// которые пишут в `0xB8000` в обход `Writer` и не могут позвать `flush`.
///
/// Пока идёт вывод (`set_quiet(true)`), композитор молчит. Иначе он успевает
/// поймать экран на середине вывода — а между первыми и последними строками
/// команды почти всегда есть дисковый ввод-вывод, во время которого шелл
/// отдаёт планировщик, — и текст снова поедет по строкам, то есть ровно то,
/// что мы убирали. Свой вывод такая задача показывает сама, одним кадром
/// в конце команды.
pub fn tick() {
    if QUIET.load(Ordering::Relaxed) {
        return;
    }
    flush();
}

/// Экран занят выводом: печать через `Writer` идёт своим кадром, композитор не
/// вмешивается. Ставится шеллом на время команды и снимается, когда экран
/// уходит другой программе (`exec::run_program`, `bg`) или шеллу нечего
/// печатать и он ждёт ввода.
static QUIET: AtomicBool = AtomicBool::new(false);

/// true — экран занят выводом, `tick()` обязан молчать.
pub fn quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// Занять/освободить экран вывода (см. `QUIET`).
pub fn set_quiet(q: bool) {
    QUIET.store(q, Ordering::Relaxed);
}

unsafe fn pack(fb: &Fb, r: u8, g: u8, b: u8) -> u32 {
    fn field(c: u8, pos: u8, mask: u8) -> u32 {
        if mask == 0 {
            return 0;
        }
        let m = ((1u32 << mask) - 1) as u32;
        (((c as u32 * m) / 255) & m) << pos
    }
    field(r, fb.r_pos, fb.r_mask)
        | field(g, fb.g_pos, fb.g_mask)
        | field(b, fb.b_pos, fb.b_mask)
}

#[inline]
unsafe fn put_px(fb: &Fb, x: u32, y: u32, v: u32) {
    if x >= fb.width || y >= fb.height {
        return;
    }
    let off = y as usize * fb.pitch as usize + x as usize * (fb.bpp as usize / 8);
    let p = fb.addr.add(off);
    match fb.bpp {
        32 => core::ptr::write_volatile(p as *mut u32, v),
        24 => {
            core::ptr::write_volatile(p, v as u8);
            core::ptr::write_volatile(p.add(1), (v >> 8) as u8);
            core::ptr::write_volatile(p.add(2), (v >> 16) as u8);
        }
        16 => core::ptr::write_volatile(p as *mut u16, v as u16),
        _ => {}
    }
}

/// Копирует строку пикселей в кадр. Пиксели по 4 байта, поэтому грузим
/// сразу по два одним 64-битным volatile-.store: широкая запись заметно
/// дешевле восьми отдельных. Порядок байт - little endian, как и на
/// единственной поддерживаемой архитектуре.
#[inline]
unsafe fn copy_row(dst: *mut u8, src: *const u32, n: usize) {
    let mut i = 0usize;
    while i + 2 <= n {
        let w = ((*src.add(i + 1) as u64) << 32) | *src.add(i) as u64;
        core::ptr::write_volatile(dst.add(i * 4) as *mut u64, w);
        i += 2;
    }
    if i < n {
        core::ptr::write_volatile(dst.add(i * 4) as *mut u32, *src.add(i));
    }
}

/// Возвращает готовый пиксельный блок ячейки из кеша, растеризуя его при
/// промахе. `key` однозначно описывает содержимое блока (символ, атрибут и
/// признак курсора), поэтому попадание в кеш всегда даёт ровно ту же картинку,
/// что и прямая растеризация. None — кеш выключен.
#[inline(always)]
unsafe fn cell_block(fb: &Fb, key: u32, ch: u8, fg: u32, bg: u32) -> Option<*const u32> {
    let entries = CELL_ENTRIES;
    if entries == 0 {
        return None;
    }
    // Ключ хранится со сдвигом на единицу: ноль означает «пусто», поэтому не
    // нужно отдельно тратить байт на признак заполненности.
    let want = key.wrapping_add(1);
    let idx = ((key as usize).wrapping_mul(0x9E37_79B1)) % entries;
    let base = CELL_CACHE.0.as_mut_ptr().add(idx * CELL_STRIDE);
    if CELL_KEYS[idx] == want {
        return Some(base);
    }

    let scale_x = fb.scale_x as usize;
    let scale_y = fb.scale_y as usize;
    let row_px = CELL_W * scale_x;
    let mut p = base;
    for gy in 0..CELL_H {
        let bits = FONT[ch as usize * 16 + gy];
        // Одна строка глифа, растянутая по X. Курсор блока (`p`) двигается
        // только здесь и ниже: если сбрасывать его на каждой строке глифа,
        // все 16 строк лягут на одно и то же место, и в кадр уйдёт мусор.
        let row = p;
        let mut i = 0usize;
        for gx in 0..CELL_W {
            let v = if (bits >> (7 - gx)) & 1 != 0 { fg } else { bg };
            for _ in 0..scale_x {
                row.add(i).write(v);
                i += 1;
            }
        }
        p = row.add(row_px);
        // При масштабе по Y строка глифа просто повторяется.
        for _ in 1..scale_y {
            core::ptr::copy_nonoverlapping(p.sub(row_px), p, row_px);
            p = p.add(row_px);
        }
    }
    CELL_KEYS[idx] = want;
    Some(base)
}

/// Копирует `bytes` байт из `src` в `dst` блоками по 16 байт.
///
/// Скорость вывода в кадр определяется числом инструкций на запись, а не
/// объёмом памяти: запись в LFB стоит столько же, сколько в обычную память,
/// поэтому выигрывает не «меньше байт», а «меньше команд на байт». Обычный
/// цикл с `write_volatile` при opt-level = 0 даёт ~30 команд на 8 байт
/// (проверки переполнения в `ptr::add`, счётчики, хвост цикла), а `movups`
/// кладёт 16 байт двумя командами. Выравнивание не требуется, поэтому и
/// кадр, и блок кеша подходят любыми.
#[inline(always)]
#[allow(unused_variables)] // w0..w3 — выходные операнды asm, читаются в шаблоне
unsafe fn copy_bytes(dst: *mut u8, src: *const u8, bytes: usize) {
    let mut n = bytes;
    let mut d = dst;
    let mut s = src;
    // Запись в кадровый буфер стоит примерно в 20 раз дороже любой команды
    // ядра: в кадр уходит каждый байт, и на нём не сэкономишь вычислениями.
    // Поэтому выгодно не «меньше байт», а меньше команд на байт: обычный
    // цикл с write_volatile на opt-level = 0 даёт около 30 команд на 8 байт
    // (проверки переполнения в ptr::add, счётчики, хвост цикла), а mov
    // кладёт 8 байт двумя командами. Развёртка по 32 байта срезает и
    // накладные расходы самого цикла.
    //
    // SSE (movdqu, 16 байт за команду) не используется намеренно: в модели
    // процессора QEMU по умолчанию SSE нет, и первая же такая команда даёт
    // #UD - проверялось, кадр гаснет.
    while n >= 32 {
        let w0: u64;
        let w1: u64;
        let w2: u64;
        let w3: u64;
        core::arch::asm!(
            "mov {w0}, [{s}]",
            "mov {w1}, [{s}+8]",
            "mov {w2}, [{s}+16]",
            "mov {w3}, [{s}+24]",
            "mov [{d}], {w0}",
            "mov [{d}+8], {w1}",
            "mov [{d}+16], {w2}",
            "mov [{d}+24], {w3}",
            s = in(reg) s,
            d = in(reg) d,
            w0 = out(reg) w0,
            w1 = out(reg) w1,
            w2 = out(reg) w2,
            w3 = out(reg) w3,
            options(nostack, preserves_flags)
        );
        d = d.wrapping_add(32);
        s = s.wrapping_add(32);
        n -= 32;
    }
    while n >= 8 {
        let w: u64;
        core::arch::asm!(
            "mov {w}, [{s}]",
            "mov [{d}], {w}",
            s = in(reg) s,
            d = in(reg) d,
            w = out(reg) w,
            options(nostack, preserves_flags)
        );
        d = d.wrapping_add(8);
        s = s.wrapping_add(8);
        n -= 8;
    }
    if n >= 4 {
        let w = s.cast::<u32>().read_volatile();
        d.cast::<u32>().write_volatile(w);
        d = d.wrapping_add(4);
        s = s.wrapping_add(4);
        n -= 4;
    }
    while n > 0 {
        let b = s.read_volatile();
        d.write_volatile(b);
        d = d.wrapping_add(1);
        s = s.wrapping_add(1);
        n -= 1;
    }
}

/// Копирует готовый блок ячейки в кадр: 16 байт за одну `movups`, то есть
/// 32 записи на ячейку вместо 128 при побайтовой растеризации.
#[inline(always)]
unsafe fn blit_cell(fb: &Fb, x0: u32, y0: u32, blk: *const u32, row_px: usize) {
    if x0 >= fb.width || y0 >= fb.height {
        return;
    }
    let w = row_px.min((fb.width - x0) as usize);
    let bytes = w * 4;
    let stride = fb.pitch as usize;
    let rows = CELL_H * fb.scale_y as usize;
    let h = fb.height as usize;
    let mut d = fb.addr.add(y0 as usize * stride + x0 as usize * 4);
    let mut s = blk.cast::<u8>();
    let step = row_px * 4;
    let mut y = y0 as usize;
    for _ in 0..rows {
        if y >= h {
            break;
        }
        copy_bytes(d, s, bytes);
        d = d.wrapping_add(stride);
        s = s.wrapping_add(step);
        y += 1;
    }
}

/// Растеризует диапазон ячеек строки через кеш готовых блоков. Ключ кеша —
/// символ и атрибут, поэтому строка рисуется тем же кодом, что и раньше,
/// просто повторяющиеся ячейки не растеризуются заново.
unsafe fn render_text_row_cached(fb: &Fb, row: usize, first: usize, last: usize) {
    if first > last || row >= fb.rows {
        return;
    }
    let scale_x = fb.scale_x as usize;
    let cell_px = (CELL_W * scale_x) as u32;
    let y0 = fb.off_y + row as u32 * CELL_H as u32 * fb.scale_y;
    let x_first = fb.off_x + first as u32 * cell_px;
    if y0 >= fb.height || x_first >= fb.width {
        return;
    }
    // Правый край может выйти за кадр: обрезаем по ячейкам, а не по пикселям,
    // — `LAST` обязан описывать только реально попавшее в кадр (см. render_text_row).
    let vis_cells = ((fb.width - x_first) / cell_px) as usize;
    if vis_cells == 0 {
        return;
    }
    let last = last.min(first + vis_cells - 1);

    for col in first..=last {
        let off = (row * fb.cols + col) * 2;
        let c = core::ptr::read_volatile(TEXT_BASE.add(off));
        let attr = core::ptr::read_volatile(TEXT_BASE.add(off + 1));
        LAST[off] = c;
        LAST[off + 1] = attr;
        let key = c as u32 | (attr as u32) << 8;
        match cell_block(fb, key, c, ATTR_FG[attr as usize], ATTR_BG[attr as usize]) {
            Some(blk) => blit_cell(fb, fb.off_x + col as u32 * cell_px, y0, blk, CELL_W * scale_x),
            // Кеш выключен (крупный масштаб) — медленный, но верный путь.
            None => render_text_row(fb, row, col, col),
        }
    }
}

/// Выбор пути отрисовки строки. 32-битный кадр с включённым кешем — обычный
/// случай (VBE), остальное уходит на общий код.
unsafe fn render_text_row_fast(fb: &Fb, row: usize, first: usize, last: usize) {
    if fb.bpp == 32 && CELL_ENTRIES > 0 {
        render_text_row_cached(fb, row, first, last);
    } else {
        render_text_row(fb, row, first, last);
    }
}

unsafe fn clear(fb: Fb) {
    let black = pack(&fb, 0, 0, 0);
    let pitch = fb.pitch as usize;
    let bytes = pitch * fb.height as usize;
    // Заливка по 8 байт: LFB - обычная память, но одиночные записи в QEMU и на
    // старых чипах обходятся дороже всего остального. 32 бит на пиксель - почти
    // всегда так (VBE), 16/24 обрабатываем общим медленным путём.
    if fb.bpp == 32 && pitch % 8 == 0 && bytes % 8 == 0 {
        let word = (black as u64).to_le();
        let words = bytes / 8;
        let p = fb.addr as *mut u64;
        for i in 0..words {
            core::ptr::write_volatile(p.add(i), word);
        }
    } else {
        for y in 0..fb.height {
            for x in 0..fb.width {
                put_px(&fb, x, y, black);
            }
        }
    }
}

fn palette(i: u8) -> (u8, u8, u8) {
    PALETTE[(i & 0x0F) as usize]
}

/// Сравнивает текстовый буфер с последним кадром и дорисовывает изменившиеся
/// строки. Дёшево, когда ничего не менялось: 4800 байт сравнения.
/// Есть ли в строке текста хоть одно отличие от прошлого кадра.
///
/// Сравнение идёт по 8 байт, а не по ячейкам: пустой экран - это 30 строк
/// х 20 слов, и на opt-level = 0 покадровое сравнение по ячейкам обходилось
/// примерно в 30 тысяч команд на КАЖДЫЙ flush, то есть композитор вхолостую
/// жег CPU 100 раз в секунду. Поиск границ изменения нужен только строке,
/// которая действительно отличается, а таких единицы.
#[inline(always)]
unsafe fn row_changed(fb: &Fb, row: usize) -> bool {
    let base = row * fb.cols * 2;
    let t = TEXT_BASE.add(base) as *const u64;
    let l = LAST.as_ptr().add(base) as *const u64;
    let words = fb.cols * 2 / 8;
    for w in 0..words {
        if core::ptr::read_volatile(t.add(w)) != *l.add(w) {
            return true;
        }
    }
    false
}

unsafe fn render_dirty() {
    let fb = match FB {
        Some(f) => f,
        None => return,
    };
    let full = NEED_FULL;

    for row in 0..fb.rows {
        let base = row * fb.cols * 2;
        // Границы изменений: перерисовываем только диапазон колонок, где текст
        // отличается от прошлого кадра. Иначе обновление приглашения шелла
        // стоило бы целой строки из 80 ячеек.
        let (first, last) = if full {
            (0usize, fb.cols - 1)
        } else {
            if !row_changed(&fb, row) {
                continue;
            }
            let mut first = fb.cols;
            let mut last = 0usize;
            for col in 0..fb.cols {
                let i = col * 2;
                let ch = core::ptr::read_volatile(TEXT_BASE.add(base + i));
                let at = core::ptr::read_volatile(TEXT_BASE.add(base + i + 1));
                if LAST[base + i] != ch || LAST[base + i + 1] != at {
                    if col < first {
                        first = col;
                    }
                    last = col;
                }
            }
            (first, last)
        };
        if first > last {
            continue;
        }
        // LAST обновляется внутри render_text_row, из того же снимка
        // текста, который попал в кадр.
        render_text_row_fast(&fb, row, first, last);
    }
    NEED_FULL = false;

    // Курсор: стираем старую позицию, рисуем новую. Не мигает - так надёжнее
    // для тестов (состояние детерминировано).
    let (cur_r, cur_c) = crate::vga::hw_cursor_pos();
    if LAST_CURSOR != (cur_r, cur_c) {
        let (or, oc) = LAST_CURSOR;
        if or < fb.rows && oc < fb.cols {
            render_cell(&fb, or, oc, false);
        }
        LAST_CURSOR = (cur_r, cur_c);
    }
    if cur_r < fb.rows && cur_c < fb.cols {
        render_cell(&fb, cur_r, cur_c, true);
    }
}

/// Растеризует диапазон ячеек строки текста в промежуточный буфер и кладёт его
/// в кадр. Заменяет вызов `render_cell` на каждую ячейку по трём причинам:
///
///  * палитра и цвета ячеек читаются ОДИН раз на строку (из таблицы
///    `ATTR_FG`/`ATTR_BG`), а не для каждой из 16 строк глифа;
///  * в кадр идёт одна крупная запись на строку глифа вместо мелких по две
///    пикселя - в кадре это самая дорогая операция;
///  * `LAST` обновляется здесь же, из того же снимка текста, что попал в кадр.
unsafe fn render_text_row(fb: &Fb, row: usize, first: usize, last: usize) {
    if first > last || row >= fb.rows {
        return;
    }
    let scale_x = fb.scale_x as usize;
    let scale_y = fb.scale_y as usize;
    let cell_px = CELL_W * scale_x;
    let x0 = fb.off_x + first as u32 * cell_px as u32;
    let y0 = fb.off_y + row as u32 * CELL_H as u32 * fb.scale_y;
    if x0 >= fb.width || y0 >= fb.height {
        return;
    }
    // Правый край может выйти за кадр: у неполной сетки такое бывает.
    // Обрезаем по ЯЧЕЙКАМ, а не по пикселям: `LAST` обязан описывать только
    // то, что действительно попало в кадр, иначе следующий проход посчитает
    // эти ячейки нарисованными и оставит на экране мусор видеопамяти.
    let vis_px = (fb.width - x0) as usize;
    let vis_cells = vis_px / cell_px;
    if vis_cells == 0 {
        return;
    }
    let last = last.min(first + vis_cells - 1);
    let span = last - first + 1;
    let px_w = span * cell_px;
    let buf = fb.rowbuf;

    // Символы и цвета читаем один раз на строку.
    let mut fg = [0u32; MAX_COLS];
    let mut bg = [0u32; MAX_COLS];
    let mut ch = [0u8; MAX_COLS];
    for col in first..=last {
        let off = (row * fb.cols + col) * 2;
        ch[col - first] = core::ptr::read_volatile(TEXT_BASE.add(off));
        let attr = core::ptr::read_volatile(TEXT_BASE.add(off + 1));
        // LAST обновляем здесь же: обновлять его отдельно, после отрисовки
        // всей строки, нельзя - за это время шелл успевает дописать новые
        // символы, и они будут помечены «уже нарисованными», то есть пропадут.
        LAST[off] = ch[col - first];
        LAST[off + 1] = attr;
        fg[col - first] = ATTR_FG[attr as usize];
        bg[col - first] = ATTR_BG[attr as usize];
    }

    for gy in 0..CELL_H {
        let mut x = 0usize;
        for col in first..=last {
            let bits = FONT[ch[col - first] as usize * 16 + gy];
            let f = fg[col - first];
            let b = bg[col - first];
            for gx in 0..CELL_W {
                let v = if (bits >> (7 - gx)) & 1 != 0 { f } else { b };
                for _ in 0..scale_x {
                    buf.add(x).write(v);
                    x += 1;
                }
            }
        }
        for dy in 0..scale_y {
            let y = y0 + (gy * scale_y + dy) as u32;
            if y >= fb.height {
                break;
            }
            let dst = fb
                .addr
                .add(y as usize * fb.pitch as usize + x0 as usize * 4);
            if fb.bpp == 32 {
                copy_row(dst, buf, px_w);
            } else {
                for gx in 0..px_w {
                    put_px(fb, x0 + gx as u32, y, buf.add(gx).read());
                }
            }
        }
    }
}

/// Рисует одну текстовую ячейку. `cursor` инвертирует цвета (курсор).
unsafe fn render_cell(fb: &Fb, row: usize, col: usize, cursor: bool) {
    if row >= fb.rows || col >= fb.cols {
        return;
    }
    let off = (row * fb.cols + col) * 2;
    let ch = core::ptr::read_volatile(TEXT_BASE.add(off));
    let attr = core::ptr::read_volatile(TEXT_BASE.add(off + 1));
    let (fg_v, bg_v) = if cursor {
        (ATTR_FG_INV[attr as usize], ATTR_BG_INV[attr as usize])
    } else {
        (ATTR_FG[attr as usize], ATTR_BG[attr as usize])
    };

    let scale_x = fb.scale_x as usize;
    let x0 = fb.off_x + col as u32 * CELL_W as u32 * fb.scale_x;
    let y0 = fb.off_y + row as u32 * CELL_H as u32 * fb.scale_y;
    if x0 >= fb.width || y0 >= fb.height {
        return;
    }

    // Быстрый путь: та же ячейка из кеша. Признак курсора входит в ключ,
    // поэтому инверсный блок не путается с обычным.
    if fb.bpp == 32 {
        let mut key = ch as u32 | (attr as u32) << 8;
        if cursor {
            key |= 1 << 17;
        }
        if let Some(blk) = cell_block(fb, key, ch, fg_v, bg_v) {
            blit_cell(fb, x0, y0, blk, CELL_W * scale_x);
            return;
        }
    }

    let glyph = &FONT[ch as usize * 16..ch as usize * 16 + 16];
    let px_w = (CELL_W * scale_x).min((fb.width - x0) as usize);

    // Растеризуем строку глифа в буфер (растянутые по X пиксели), затем
    // копируем её в кадр сколько нужно раз по Y. На 32-битном режиме одна
    // запись u64 кладёт сразу два пикселя.
    let mut line = [0u32; CELL_W * MAX_SCALE];
    for gy in 0..CELL_H {
        let bits = glyph[gy];
        let mut i = 0;
        for gx in 0..CELL_W {
            let v = if (bits >> (7 - gx)) & 1 != 0 { fg_v } else { bg_v };
            for _ in 0..scale_x {
                if i < line.len() {
                    line[i] = v;
                    i += 1;
                }
            }
        }
        let py = y0 + gy as u32 * fb.scale_y;
        if fb.bpp == 32 {
            for dy in 0..fb.scale_y {
                let y = py + dy;
                if y >= fb.height {
                    break;
                }
                let dst = fb.addr.add(y as usize * fb.pitch as usize + x0 as usize * 4);
                copy_row(dst, line.as_ptr(), px_w);
            }
        } else {
            for gx in 0..px_w {
                let v = line[gx];
                for dy in 0..fb.scale_y {
                    let y = py + dy;
                    if y < fb.height {
                        put_px(fb, x0 + gx as u32, y, v);
                    }
                }
            }
        }
    }
}