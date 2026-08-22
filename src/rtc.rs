// src/rtc.rs
//
// Драйвер часов реального времени MC146818 (CMOS) - источник времени
// для atime/mtime в новой ФС. QEMU эмулирует RTC, по умолчанию UTC.
//
// Чтение устойчивое: снимаем все поля дважды и сравниваем; если между
// чтениями тикнул секундный апдейт - повторяем. Это проще и надёжнее
// танцев с флагом UIP для наших целей.

use x86_64::instructions::port::Port;

const CMOS_ADDR: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;

const REG_SEC: u8 = 0x00;
const REG_MIN: u8 = 0x02;
const REG_HOUR: u8 = 0x04;
const REG_DAY: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_CENTURY: u8 = 0x32;
const REG_STATUS_A: u8 = 0x0A;
const REG_STATUS_B: u8 = 0x0B;

fn read_reg(reg: u8) -> u8 {
    unsafe {
        let mut a: Port<u8> = Port::new(CMOS_ADDR);
        let mut d: Port<u8> = Port::new(CMOS_DATA);
        // NMI не трогаем: старший бит адреса оставляем как есть (0).
        a.write(reg & 0x7F);
        d.read()
    }
}

fn update_in_progress() -> bool {
    read_reg(REG_STATUS_A) & 0x80 != 0
}

fn bcd_or_bin(v: u8, bcd: bool) -> u8 {
    if bcd {
        (v & 0x0F) + (v >> 4) * 10
    } else {
        v
    }
}

#[derive(Clone, Copy)]
struct DateTime {
    year: u64,
    month: u64,
    day: u64,
    hour: u64,
    min: u64,
    sec: u64,
}

/// Один согласованный снимок часов; None при таймауте UIP.
fn read_once() -> Option<DateTime> {
    // Ждём окончания апдейта, максимум ~1 сек (порядка 2^24 оборотов).
    let mut spins = 0usize;
    while update_in_progress() {
        spins += 1;
        if spins > 20_000_000 {
            return None;
        }
        core::hint::spin_loop();
    }
    let status_b = read_reg(REG_STATUS_B);
    let bcd = status_b & 0x04 == 0;
    let h24 = status_b & 0x02 != 0;

    let sec = bcd_or_bin(read_reg(REG_SEC), bcd) as u64;
    let min = bcd_or_bin(read_reg(REG_MIN), bcd) as u64;
    let day = bcd_or_bin(read_reg(REG_DAY), bcd) as u64;
    let month = bcd_or_bin(read_reg(REG_MONTH), bcd) as u64;
    let mut year = bcd_or_bin(read_reg(REG_YEAR), bcd) as u64;

    // Час: в 12-часовом формате бит 7 - флаг PM.
    let hour_raw = read_reg(REG_HOUR);
    let hour = if h24 {
        bcd_or_bin(hour_raw, bcd) as u64
    } else {
        let pm = hour_raw & 0x80 != 0;
        let h12 = bcd_or_bin(hour_raw & 0x7F, bcd) as u64 % 12;
        if pm {
            h12 + 12
        } else {
            h12
        }
    };

    let century = if read_reg(REG_CENTURY) != 0 {
        bcd_or_bin(read_reg(REG_CENTURY), bcd) as u64 * 100
    } else {
        // Без регистра века считаем, что это 2000-е (для QEMU верно).
        2000
    };
    year += century;

    // Защита от мусора.
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 61 {
        return None;
    }
    Some(DateTime {
        year,
        month,
        day,
        hour,
        min,
        sec,
    })
}

/// Согласованное время: два снимка подряд должны совпасть.
fn read_stable() -> Option<DateTime> {
    for _ in 0..8 {
        let a = read_once()?;
        let b = read_once()?;
        if a.year == b.year
            && a.month == b.month
            && a.day == b.day
            && a.hour == b.hour
            && a.min == b.min
            && a.sec == b.sec
        {
            return Some(a);
        }
    }
    None
}

/// Unix-timestamp из календарной даты (Howard Hinnant, days_from_civil).
fn days_from_civil(y: u64, m: u64, d: u64) -> i64 {
    let y = y as i64 - if m <= 2 { 1 } else { 0 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m as i64 + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d as i64 - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Текущее unix-время в секундах. При недоступности RTC - 0.
pub fn now_unix() -> u64 {
    match read_stable() {
        Some(dt) => {
            let days = days_from_civil(dt.year, dt.month, dt.day);
            let secs = days * 86400
                + (dt.hour as i64) * 3600
                + (dt.min as i64) * 60
                + dt.sec as i64;
            if secs < 0 {
                0
            } else {
                secs as u64
            }
        }
        None => 0,
    }
}
