// src/splash.rs — загрузочный экран: ASCII-баннер + статусы подсистем.
use crate::vga::{Writer, COLOR_LIGHT_CYAN, COLOR_LIGHT_GREEN, COLOR_WHITE, SCREEN_WIDTH};

const BANNER: &[&str] = &[
    r" _   _ _ _                  _       _____       _____   ____ ",
    r"| | | (_) |                (_)     |  _  |     / __  \ / ___|",
    r"| | | |_| |_ __ _ _ __ ___  _ _ __ | | | | ___ `' / /'/ /___ ",
    r"| | | | | __/ _` | '_ ` _ \| | '_ \| | | |/ __|  / /  | ___ \",
    r"\ \_/ / | || (_| | | | | | | | | | \ \_/ /\__ \./ /___| \_/ |",
    r" \___/|_|\__\__,_|_| |_| |_|_|_| |_|\___/ |___/\_____/\_____|",
];

/// Рисует баннер по центру чистого экрана. Аллокации не использует,
/// поэтому безопасно вызывать даже до инициализации кучи.
pub fn draw(writer: &mut Writer) {
    writer.set_color(COLOR_WHITE);
    writer.clear_screen();
    writer.set_color(COLOR_LIGHT_GREEN);
    let top_pad = 3;
    for _ in 0..top_pad {
        writer.write_string("\n");
    }
    for line in BANNER {
        centered(writer, line.trim_end());
    }
    writer.write_string("\n");
    writer.set_color(COLOR_WHITE);
    centered(writer, "booting subsystems...");
    writer.write_string("\n\n");
}

/// Строка статуса подсистемы: зелёный `[ OK ]` + имя, затем короткая пауза.
pub fn step(writer: &mut Writer, name: &str) {
    writer.set_color(COLOR_LIGHT_GREEN);
    writer.write_string("  [ OK ] ");
    writer.set_color(COLOR_WHITE);
    writer.write_string(name);
    writer.write_string("\n");
    pause();
}

/// Строка неудачи (красный `[ FAIL ]`) без паузы.
pub fn fail(writer: &mut Writer, name: &str) {
    const COLOR_LIGHT_RED: u8 = 0x0C;
    writer.set_color(COLOR_LIGHT_RED);
    writer.write_string("  [ FAIL ] ");
    writer.set_color(COLOR_WHITE);
    writer.write_string(name);
    writer.write_string("\n");
}

/// Финал: очищаем экран — дальше начинается интерактивный шелл.
pub fn finish(writer: &mut Writer) {
    writer.set_color(COLOR_WHITE);
    writer.clear_screen();
}

/// Ждёт нажатия Enter перед переходом к промпту. Сначала очищает буфер
/// сканкодов, чтобы нажатия во время загрузки не проскакивали мгновенно;
/// break-коды (>=0x80) и прочие клавиши игнорируются. Ждёт через hlt,
/// поэтому не грузит CPU (IF=1 — прерывания уже включены).
pub fn wait_enter(writer: &mut Writer) {
    crate::keyboard::flush();
    writer.write_string("\n");
    writer.set_color(COLOR_LIGHT_CYAN);
    centered(writer, "press Enter to continue...");
    writer.set_color(COLOR_WHITE);
    const ENTER_MAKE: u8 = 0x1C;
    loop {
        if crate::keyboard::kb_hit() {
            let sc = crate::keyboard::kb_read();
            if sc == ENTER_MAKE {
                break;
            }
        }
        x86_64::instructions::hlt();
    }
}

fn centered(writer: &mut Writer, s: &str) {
    let pad = SCREEN_WIDTH.saturating_sub(s.len()) / 2;
    for _ in 0..pad {
        writer.write_string(" ");
    }
    writer.write_string(s);
    writer.write_string("\n");
}

fn pit_count() -> u16 {
    unsafe {
        // Latch-команда канала 0: фиксирует текущий счётчик для чтения,
        // сам таймер продолжает считать.
        x86_64::instructions::port::Port::<u8>::new(0x43).write(0x00);
        let lo = x86_64::instructions::port::Port::<u8>::new(0x40).read();
        let hi = x86_64::instructions::port::Port::<u8>::new(0x40).read();
        ((hi as u16) << 8) | lo as u16
    }
}

/// Короткая пауза между строками статуса (~0.1 c до перепрограммирования PIT):
/// ждём несколько переходов счётчика таймера через ноль. IF=0, прерывания
/// не мешают; после переключения PIT на 100 Гц пауза просто становится короче.
fn pause() {
    let mut prev = pit_count();
    let mut wraps = 0u8;
    while wraps < 4 {
        let c = pit_count();
        if c > prev {
            wraps += 1;
        }
        prev = c;
    }
}
