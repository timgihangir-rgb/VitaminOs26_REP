//! Линейный фреймбуфер: адаптация экрана под монитор.
//!
//! Проблема: ядро и ring3-программы исторически пишут текстовые ячейки
//! (символ+атрибут) прямо в окно VGA `0xB8000` и переключают CRTC в режим
//! 80x30. На мониторе с нативным разрешением 1920x1080 такой режим мылится
//! и растягивается.
//!
//! Решение без ломки архитектуры: загрузчик (GRUB) ставит графический режим
//! под монитор по EDID и передаёт multiboot2-тег 8 (framebuffer). Ядро
//! отображает этот линейный фреймбуфер и ПЕРИОДИЧЕСКИ блитит в него
//! текстовый буфер `0xB8000`. Все, кто пишет в `0xB8000` (ядро, шелл,
//! рабочие столы, ring3-программы, жёстко зашитые на 0xB8000), продолжают
//! работать без изменений: в графическом режиме `0xB8000` — это память VGA,
//! доступная на чтение, а визуализацией занимается этот модуль.
//!
//! Тайл 8x16 берётся из встроенного шрифта `font8x16.bin` (CP437, совпадает
//! с аппаратным шрифтом), потому что после перехода в VBE plane 2 может быть
//! уже недоступна.
//!
//! Если тега нет (свой 2-стадийный загрузчик) — модуль не активируется, и
//! система работает в обычном текстовом режиме, как раньше.

use crate::paging;

/// Текстовый буфер VGA. В текстовом, и в графическом режиме это одна и та
/// же физическая область: писать в неё можно всегда, и модуль её читает.
const TEXT_BASE: *mut u8 = 0xb8000 as *mut u8;
const TEXT_WIDTH: usize = 80;
const TEXT_HEIGHT: usize = 30;
const CELL_W: usize = 8;
const CELL_H: usize = 16;

/// Потолок масштаба текста. Ограничивает буфер строки в растеризаторе:
/// CELL_W * MAX_SCALE пикселей на строку глифа.
const MAX_SCALE: usize = 8;

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
}

static mut FB: Option<Fb> = None;

/// Последнее отбличенное содержимое текстового буфера и позиция курсора.
static mut LAST: [u8; TEXT_WIDTH * TEXT_HEIGHT * 2] = [0; TEXT_WIDTH * TEXT_HEIGHT * 2];
static mut LAST_CURSOR: (usize, usize) = (usize::MAX, usize::MAX);

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

/// Активирует фреймбуфер: отображает MMIO и вычисляет масштаб текстовой сетки
/// под разрешение монитора. Возвращает false, если отобразить не удалось.
pub fn init(raw: RawFb) -> bool {
    let len = raw.pitch as u64 * raw.height as u64;
    let va = match unsafe { paging::map_phys_in_kernel(raw.phys, len) } {
        Some(v) => v,
        None => {
            crate::vga::serial_write_atomic("[N] fb map failed\n");
            return false;
        }
    };

    // В VBE-режиме физический 0xB8000 текст не хранит: до первого чтения
    // композитором подменяем его на RAM-тень (и в ядре, и в будущих задачах).
    let shadow = match unsafe { paging::install_vga_shadow() } {
        Some(s) => s,
        None => {
            crate::vga::serial_write_atomic("[N] vga shadow failed\n");
            return false;
        }
    };
    // Адрес тени в serial: автотесты читают экран через pmemsave и после
    // появления композитора физический 0xB8000 больше не содержит текст.
    // Без этой строки им пришлось бы угадывать адрес. Печатаем с префиксом
    // 0x: выражение HMP без него не разбирает адрес, начинающийся с буквы.
    crate::vga::serial_write_atomic("[N] vga shadow phys=0x");
    crate::vga::serial_write_atomic(hex32(shadow.0).as_str());
    crate::vga::serial_putchar(b'\n');

    // Масштаб: максимальный целочисленный, одинаковый по обеим осям (иначе
    // шрифт растягивается по горизонтали), сетка центрируется. Верхняя
    // граница ограничивает размер буфера строки в растеризаторе.
    let grid_w = (TEXT_WIDTH * CELL_W) as u32;
    let grid_h = (TEXT_HEIGHT * CELL_H) as u32;
    let scale = (raw.width / grid_w).min(raw.height / grid_h).clamp(1, MAX_SCALE as u32);
    let draw_w = grid_w * scale;
    let draw_h = grid_h * scale;

    crate::vga::serial_write_atomic("[N] fb ");
    crate::vga::serial_u64(raw.width as u64);
    crate::vga::serial_write_atomic("x");
    crate::vga::serial_u64(raw.height as u64);
    crate::vga::serial_write_atomic(" scale=");
    crate::vga::serial_u64(scale as u64);
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
    crate::vga::serial_u64(
        ((raw.b_pos as u64) << 8) | (raw.b_mask as u64),
    );
    crate::vga::serial_write_atomic(" phys=");
    crate::vga::serial_u64(raw.phys);
    crate::vga::serial_write_atomic("\n");

    let fb = Fb {
        addr: va as *mut u8,
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
        off_x: (raw.width - draw_w) / 2,
        off_y: (raw.height - draw_h) / 2,
    };

    unsafe {
        FB = Some(fb);
        // Первичная заливка: старый мусор видеопамяти не должен мелькать.
        clear(fb);
        // Заставляем первый tick отрисовать все строки.
        LAST = [0xFF; TEXT_WIDTH * TEXT_HEIGHT * 2];
        LAST_CURSOR = (usize::MAX, usize::MAX);
    }
    true
}

pub fn active() -> bool {
    unsafe { FB.is_some() }
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
/// сразу по два одним 64-битным volatile-.store: в LFB (устройство, а не
/// обычная память) широкая запись заметно дешевле. Порядок байт — little
/// endian, как и на единственной поддерживаемой архитектуре.
#[inline]
unsafe fn copy_row(dst: *mut u8, src: &[u32], n: usize) {
    let mut i = 0usize;
    while i + 2 <= n {
        let w = ((src[i + 1] as u64) << 32) | src[i] as u64;
        core::ptr::write_volatile(dst.add(i * 4) as *mut u64, w);
        i += 2;
    }
    if i < n {
        core::ptr::write_volatile(dst.add(i * 4) as *mut u32, src[i]);
    }
}

unsafe fn clear(fb: Fb) {
    let black = pack(&fb, 0, 0, 0);
    let pitch = fb.pitch as usize;
    let bytes = pitch * fb.height as usize;
    // Заливка по 8 байт: LFB — обычная память, но одиночные записи в QEMU и на
    // старых чипах обходятся дороже всего остального. 32 бит на пиксель — почти
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

/// Рисует одну текстовую ячейку. `cursor` инвертирует цвета (курсор).
unsafe fn render_cell(fb: &Fb, row: usize, col: usize, cursor: bool) {
    let off = (row * TEXT_WIDTH + col) * 2;
    let ch = core::ptr::read_volatile(TEXT_BASE.add(off));
    let attr = core::ptr::read_volatile(TEXT_BASE.add(off + 1));
    let mut fg = attr & 0x0F;
    let mut bg = (attr >> 4) & 0x07;
    if cursor {
        core::mem::swap(&mut fg, &mut bg);
    }
    let (fr, fgc, fb_) = palette(fg);
    let (br, bgc, bb) = palette(bg);
    let fg_v = pack(fb, fr, fgc, fb_);
    let bg_v = pack(fb, br, bgc, bb);

    let glyph = &FONT[ch as usize * 16..ch as usize * 16 + 16];
    let x0 = fb.off_x + col as u32 * CELL_W as u32 * fb.scale_x;
    let y0 = fb.off_y + row as u32 * CELL_H as u32 * fb.scale_y;
    if x0 >= fb.width || y0 >= fb.height {
        return;
    }

    // Растеризуем строку глифа в буфер (растянутые по X пиксели), затем
    // копируем её в кадр сколько нужно раз по Y. Гораздо дешевле, чем
    // писать каждый пиксель отдельно: на 32-битном режиме одна запись u64
    // кладёт сразу два пикселя.
    let px_w = (CELL_W * fb.scale_x as usize).min((fb.width - x0) as usize);
    let mut line = [0u32; CELL_W * MAX_SCALE];
    for gy in 0..CELL_H {
        let bits = glyph[gy];
        let mut i = 0;
        for gx in 0..CELL_W {
            let v = if (bits >> (7 - gx)) & 1 != 0 { fg_v } else { bg_v };
            for _ in 0..fb.scale_x as usize {
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
                copy_row(dst, &line[..px_w], px_w);
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

/// Периодический вызов: сравнивает текст с последним кадром и дорисовывает
/// только изменившиеся ячейки. Дёшево (4800 байт сравнения) и без блокировок.
pub fn tick() {
    let fb = unsafe {
        match FB {
            Some(f) => f,
            None => return,
        }
    };

    for row in 0..TEXT_HEIGHT {
        let base = row * TEXT_WIDTH * 2;
        unsafe {
            // Границы изменений: перерисовываем только диапазон колонок, где
            // текст отличается от прошлого кадра. Иначе обновление приглашения
            // шелла стоило бы целой строки из 80 ячеек.
            let mut first = TEXT_WIDTH;
            let mut last = 0usize;
            for col in 0..TEXT_WIDTH {
                let i = col * 2;
                let ch = core::ptr::read_volatile(TEXT_BASE.add(base + i));
                let at = core::ptr::read_volatile(TEXT_BASE.add(base + i + 1));
                if LAST[base + i] != ch || LAST[base + i + 1] != at {
                    LAST[base + i] = ch;
                    LAST[base + i + 1] = at;
                    if col < first {
                        first = col;
                    }
                    last = col;
                }
            }
            if first > last {
                continue;
            }
            for col in first..=last {
                render_cell(&fb, row, col, false);
            }
        }
    }

    // Курсор: стираем старую позицию, рисуем новую. Не мигает — так надёжнее
    // для тестов (состояние детерминировано).
    let (cur_r, cur_c) = crate::vga::hw_cursor_pos();
    unsafe {
        if LAST_CURSOR != (cur_r, cur_c) {
            let (or, oc) = LAST_CURSOR;
            if or < TEXT_HEIGHT && oc < TEXT_WIDTH {
                render_cell(&fb, or, oc, false);
            }
            LAST_CURSOR = (cur_r, cur_c);
        }
        if cur_r < TEXT_HEIGHT && cur_c < TEXT_WIDTH {
            render_cell(&fb, cur_r, cur_c, true);
        }
    }
}
