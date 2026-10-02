// src/top.rs
//
// Интерактивный `top` по образцу Linux: таблица задач, которая
// перерисовывается раз в секунду, сортировка по %CPU/TIME/PID, подсветка
// выполняющейся задачи и счётчики load average.
//
// Клавиатура:
//   q, Esc   — выход
//   <-/->    — смена поля сортировки
//   up/down  — выбор строки
//   k        — убить выбранную задачу (как в Linux top)
//
// Рисуем всегда одними и теми же координатами через Writer::put, без
// прокрутки терминала: экран не ползёт, меняются только цифры — как и в
// настоящем top.

use crate::keyboard;
use crate::ralloc;
use crate::scheduler::{self, ProcInfo};
use crate::vga::{Writer, SCREEN_HEIGHT, SCREEN_WIDTH};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// Тик PIT = 10 мс, поэтому один «рефреш» — это ровно секунда.
const REFRESH_TICKS: u64 = 100;
/// Пауза между опросами клавиатуры внутри окна обновления: 20 мс.
const POLL_TICKS: u64 = 2;

// ─── Раскладка ────────────────────────────────────────────────────────────
const ROW_TOP: usize = 0; // top - up ...
const ROW_LOAD: usize = 1; // load average: ...
const ROW_HEAD: usize = 2; //   PID  STATE       TIME    %CPU  COMMAND
const ROW_FIRST: usize = 3; // первая строка таблицы
const ROW_HELP: usize = SCREEN_HEIGHT - 2; // подсказка по клавишам
const ROW_MSG: usize = SCREEN_HEIGHT - 1; // строка сообщений
/// Сколько строк отдаём под таблицу.
const TABLE_ROWS: usize = ROW_HELP - ROW_FIRST;

// Начала колонок обязаны совпадать с форматной строкой в draw() и draw_field()
// ниже: строка задач начинается с маркера '>' (1 символ), поэтому PID сдвинут
// на 1 относительно заголовка.
const PID_COL: usize = 2;
const TIME_COL: usize = 19;
const CPU_COL: usize = 29;
const CPU_WIDTH: usize = 5;

// ─── Цвета (стандартные атрибуты VGA) ─────────────────────────────────────
const C_HEADER: u8 = 0x0B; // светло-голубой — шапка
const C_COLS: u8 = 0x0E; // жёлтый — заголовки колонок
const C_ROW: u8 = 0x07; // серый — обычные задачи
const C_RUN: u8 = 0x7A; // инверсия — выполняющаяся задача
const C_HELP: u8 = 0x08; // тёмно-серый — подсказка
const C_MSG: u8 = 0x0E; // жёлтый — сообщения
const C_HOT: u8 = 0x0C; // красный — высокая загрузка
const C_WARM: u8 = 0x0E; // жёлтый — заметная загрузка

/// Поле сортировки.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Cpu,
    Time,
    Pid,
}

impl Sort {
    fn title(self) -> &'static str {
        match self {
            Sort::Cpu => "%CPU",
            Sort::Time => "TIME",
            Sort::Pid => "PID",
        }
    }
    fn next(self) -> Sort {
        match self {
            Sort::Cpu => Sort::Time,
            Sort::Time => Sort::Pid,
            Sort::Pid => Sort::Cpu,
        }
    }
    fn prev(self) -> Sort {
        match self {
            Sort::Cpu => Sort::Pid,
            Sort::Time => Sort::Cpu,
            Sort::Pid => Sort::Time,
        }
    }
}

/// Строка таблицы: снимок задачи плюс колонки, вычисленные по дельтам.
struct Row {
    pid: usize,
    name: String,
    /// Однобуквенный код состояния в духе Linux: R/S/Z.
    state: char,
    /// Слово состояния для читаемости.
    state_word: &'static str,
    running: bool,
    /// Суммарное процессорное время в тиках.
    ticks: u64,
    /// Процент процессорного времени за интервал, ×10 (целые десятые).
    cpu_x10: u32,
}

/// Пишет `s` в колонку `col` строки `row`, дополняя пробелами до `width`.
/// Затирает остаток поля, поэтому таблица не оставляет хвостов от прошлого
/// кадра (имена задач меняются в длину при пересортировке).
fn draw_field(writer: &mut Writer, row: usize, col: usize, width: usize, s: &str, color: u8) {
    writer.set_color(color);
    for i in 0..width {
        writer.put(row, col + i, s.as_bytes().get(i).copied().unwrap_or(b' '));
    }
}

/// Пишет `s` без дополнения пробелами.
fn draw_text(writer: &mut Writer, row: usize, col: usize, s: &str, color: u8) {
    writer.set_color(color);
    for (i, &ch) in s.as_bytes().iter().enumerate() {
        if col + i >= SCREEN_WIDTH {
            break;
        }
        writer.put(row, col + i, ch);
    }
}

/// `h:mm:ss` из тиков PIT (1 тик = 10 мс).
fn uptime_str(ticks: u64) -> String {
    let secs = ticks / 100;
    format!("{}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60)
}

/// `mm:ss.s` — колонка TIME, как в Linux.
fn time_str(ticks: u64) -> String {
    let secs = ticks / 100;
    format!("{}:{:02}.{}", secs / 60, secs % 60, (ticks % 100) / 10)
}

/// Килобайты в компактном виде: 512K, 12M, 1.5G — как в Linux.
fn bytes_str(bytes: u64) -> String {
    const K: u64 = 1024;
    if bytes < K {
        format!("{}B", bytes)
    } else if bytes < 1024 * K {
        format!("{}K", bytes / K)
    } else if bytes < 1024 * 1024 * K {
        format!("{}.{}M", bytes / (1024 * K), (bytes / K) % 1024 / 100)
    } else {
        format!(
            "{}.{}G",
            bytes / (1024 * 1024 * K),
            (bytes / (1024 * K)) % 1024 / 100
        )
    }
}

/// Linux-подобные коды состояний: R — работает, S — спит, Z — зомби.
fn state_letter(s: &str) -> char {
    match s {
        "running" => 'R',
        "ready" | "blocked" => 'S',
        "finished" => 'Z',
        _ => '?',
    }
}

/// Имя задачи из фиксированного буфера `ProcInfo::name`.
fn proc_name(p: &ProcInfo) -> String {
    let s = core::str::from_utf8(&p.name).unwrap_or("?");
    String::from(s.split('\0').next().unwrap_or("?"))
}

/// Снимок задач с %CPU, посчитанным по приращению тиков за интервал.
/// `prev` — предыдущий снимок (pid → тики); обновляется на месте.
/// Первый вызов делает `prev` пустым, поэтому %CPU всех, кроме текущей
/// задачи, на первом кадре нулевой — дальше значения осмысленные.
fn sample(prev: &mut Vec<(usize, u64)>, span: u64, running_pid: usize) -> Vec<Row> {
    let mut raw = Vec::new();
    scheduler::list(&mut raw);

    let span = span.max(1);
    let mut rows: Vec<Row> = Vec::with_capacity(raw.len());
    for p in raw {
        let delta = match prev.iter().find(|(pid, _)| *pid == p.pid) {
            // Дельта тиков этой задачи за интервал.
            Some((_, t)) => p.ticks.saturating_sub(*t),
            // Первый кадр: у выполняющейся задачи дельта равна всему
            // интервалу, иначе на старте её %CPU был бы 0.
            None if p.pid == running_pid => span,
            None => 0,
        };
        rows.push(Row {
            pid: p.pid,
            name: proc_name(&p),
            state: state_letter(p.state),
            state_word: p.state,
            running: p.pid == running_pid,
            ticks: p.ticks,
            cpu_x10: (delta * 1000 / span) as u32,
        });
    }

    prev.clear();
    for r in &rows {
        prev.push((r.pid, r.ticks));
    }
    rows
}

/// Состояние цикла `top`.
struct Top {
    sort: Sort,
    sel: usize,
    msg: String,
    prev: Vec<(usize, u64)>,
    /// Три последних измерения длины очереди готовых задач.
    load: [f64; 3],
    /// Индекс следующей ячейки load ( oldest лежит сразу перед ним).
    load_next: usize,
}

impl Top {
    fn new() -> Top {
        Top {
            sort: Sort::Cpu,
            sel: 0,
            msg: String::new(),
            prev: Vec::new(),
            load: [0.0; 3],
            load_next: 0,
        }
    }

    /// Значения load average от свежего к самому старому: 1, 5, 15 минут
    /// (у нас 1, 2, 3 интервала — но подписи одинаковые, как в Linux).
    fn load_values(&self) -> (f64, f64, f64) {
        let newest = (self.load_next + 2) % 3;
        (
            self.load[newest],
            self.load[(newest + 1) % 3],
            self.load[(newest + 2) % 3],
        )
    }

    /// Один сканкод. Возвращает true, если пора выходить из `top`.
    fn key(&mut self, sc: u8, rows: &[Row]) -> bool {
        match sc {
            // q и Esc — выход.
            0x10 | 0x01 => return true,
            0x4B => {
                self.sort = self.sort.prev();
                self.msg.clear();
            }
            0x4D => {
                self.sort = self.sort.next();
                self.msg.clear();
            }
            0x48 => self.sel = 0,
            0x50 => self.sel = rows.len().saturating_sub(1),
            0x25 => {
                let pid = rows.get(self.sel).map(|r| r.pid);
                self.msg = match pid {
                    Some(pid) if scheduler::kill(pid) => format!("killed pid {}", pid),
                    Some(pid) => format!("cannot kill pid {}", pid),
                    None => String::from("no task selected"),
                };
            }
            _ => {}
        }
        false
    }
}

/// `top` в терминале. Возвращает управление шеллу.
pub fn run(writer: &mut Writer) {
    let mut t = Top::new();

    // Первый снимок — только чтобы заполнить prev; на экране он не покажется.
    let start = scheduler::ticks();
    sample(&mut t.prev, 0, scheduler::current_pid());
    let mut total_before = start;

    let mut rows = Vec::new();
    loop {
        // ─── Ждём секунду, попутно реагируя на клавиши ───────────────────
        let deadline = total_before + REFRESH_TICKS;
        let mut quit = false;
        while scheduler::ticks() < deadline {
            while keyboard::kb_hit() {
                let sc = keyboard::kb_read();
                // Модификаторы и break-коды (0x80) игнорируем, иначе `q`
                // «съедается» отпусканием Shift/Ctrl.
                if sc & 0x80 != 0 || matches!(sc, 0x1D | 0x2A | 0x36 | 0x38) {
                    continue;
                }
                if t.key(sc, &rows) {
                    quit = true;
                    break;
                }
            }
            if quit {
                break;
            }
            // Спим короткими отрезками, чтобы `q` ощущался мгновенно.
            let now = scheduler::ticks();
            scheduler::sleep_until((now + POLL_TICKS).min(deadline));
        }
        if quit {
            break;
        }

        // ─── Новый кадр ──────────────────────────────────────────────────
        let total_now = scheduler::ticks();
        rows = sample(&mut t.prev, total_now - total_before, scheduler::current_pid());
        total_before = total_now;
        t.load[t.load_next % 3] = scheduler::ready_count() as f64;
        t.load_next += 1;

        match t.sort {
            Sort::Cpu => rows.sort_by(|a, b| b.cpu_x10.cmp(&a.cpu_x10).then(a.pid.cmp(&b.pid))),
            Sort::Time => rows.sort_by(|a, b| b.ticks.cmp(&a.ticks).then(a.pid.cmp(&b.pid))),
            Sort::Pid => rows.sort_by(|a, b| a.pid.cmp(&b.pid)),
        }
        if t.sel >= rows.len() {
            t.sel = rows.len().saturating_sub(1);
        }

        draw(writer, &rows, &t);
    }

    // Убираем подсказку и возвращаем курсор вниз экрана под приглашение.
    for r in ROW_HELP..SCREEN_HEIGHT {
        draw_field(writer, r, 0, SCREEN_WIDTH, "", C_HELP);
    }
    writer.set_color(crate::vga::COLOR_WHITE);
    writer.set_cursor(ROW_MSG, 0);
}

/// Один кадр целиком.
fn draw(writer: &mut Writer, rows: &[Row], t: &Top) {
    // ─── Шапка ───────────────────────────────────────────────────────────
    let busy_x10: u32 = rows.iter().map(|r| r.cpu_x10).sum();
    draw_field(
        writer,
        ROW_TOP,
        0,
        SCREEN_WIDTH,
        &format!(
            "top - up {},  {} tasks,  {} running,  {}% CPU,  {}/{} mem",
            uptime_str(scheduler::ticks()),
            rows.len(),
            rows.iter().filter(|r| r.running).count(),
            busy_x10 / 10,
            bytes_str(ralloc::allocated_bytes() as u64),
            bytes_str(ralloc::heap_bytes() as u64),
        ),
        C_HEADER,
    );

    // ─── load average ────────────────────────────────────────────────────
    let (l1, l2, l3) = t.load_values();
    draw_field(
        writer,
        ROW_LOAD,
        0,
        SCREEN_WIDTH,
        &format!(
            "load average: {:.2}, {:.2}, {:.2}    heap peak {}",
            l1,
            l2,
            l3,
            bytes_str(ralloc::peak_bytes() as u64),
        ),
        C_HELP,
    );

    // ─── Заголовки колонок ───────────────────────────────────────────────
    draw_field(
        writer,
        ROW_HEAD,
        0,
        SCREEN_WIDTH,
        &format!(
            "  {:>4}  {:<11}{:>8}  {:>5}  {}",
            "PID", "STATE", "TIME", "%CPU", "COMMAND"
        ),
        C_COLS,
    );
    // Подсвечиваем колонку, по которой идёт сортировка.
    let (col, width) = match t.sort {
        Sort::Pid => (PID_COL, 4),
        Sort::Time => (TIME_COL, 8),
        Sort::Cpu => (CPU_COL, CPU_WIDTH),
    };
    let title = t.sort.title();
    let pad = width.saturating_sub(title.len());
    draw_text(writer, ROW_HEAD, col + pad, title, C_HOT);

    // ─── Строки задач ────────────────────────────────────────────────────
    // Если выбранная строка уехала за нижний край — прокручиваем таблицу.
    let start = if t.sel >= TABLE_ROWS { t.sel + 1 - TABLE_ROWS } else { 0 };
    for slot in 0..TABLE_ROWS {
        let r = ROW_FIRST + slot;
        let idx = start + slot;
        let Some(row) = rows.get(idx) else {
            draw_field(writer, r, 0, SCREEN_WIDTH, "", C_ROW);
            continue;
        };
        let base = if row.running { C_RUN } else { C_ROW };
        let marker = if idx == t.sel { '>' } else { ' ' };
        let cpu = format!("{}.{}", row.cpu_x10 / 10, row.cpu_x10 % 10);
        draw_field(
            writer,
            r,
            0,
            SCREEN_WIDTH,
            &format!(
                "{} {:>4}  {:<11}{:>8}  {:>5}  {}",
                marker,
                row.pid,
                format!("{} {}", row.state, row.state_word),
                time_str(row.ticks),
                cpu,
                row.name
            ),
            base,
        );
        // %CPU подсвечиваем отдельно: красный при высокой нагрузке.
        let cpu_color = if row.running {
            C_RUN
        } else if row.cpu_x10 >= 500 {
            C_HOT
        } else if row.cpu_x10 >= 100 {
            C_WARM
        } else {
            base
        };
        draw_text(
            writer,
            r,
            CPU_COL + CPU_WIDTH - cpu.len(),
            &cpu,
            cpu_color,
        );
    }

    // ─── Подсказка и сообщение ───────────────────────────────────────────
    draw_field(
        writer,
        ROW_HELP,
        0,
        SCREEN_WIDTH,
        &format!(
            "sort: {}   <-/->: field   up/down: select   k: kill   q: quit",
            t.sort.title()
        ),
        C_HELP,
    );
    draw_field(writer, ROW_MSG, 0, SCREEN_WIDTH, &t.msg, C_MSG);

    // Курсор — на строке сообщений, чтобы не мигал поверх таблицы.
    writer.set_cursor(ROW_MSG, 0);
}