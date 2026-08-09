//! Встроенные фоновые программы для команды `bg`.

use alloc::boxed::Box;
use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::scheduler;

/// Запускает встроенную фоновую программу по имени. Возвращает PID или None.
pub fn spawn(name: &str, args: &[&str]) -> Option<usize> {
    let f: Box<dyn FnOnce()> = match name {
        "prime" => {
            let limit = parse_u64(args.get(0).copied()).unwrap_or(100_000);
            Box::new(move || prime(limit))
        }
        "fib" => {
            let n = parse_u64(args.get(0).copied()).unwrap_or(32);
            Box::new(move || fib(n))
        }
        "parallel" => {
            let limit = parse_u64(args.get(0).copied()).unwrap_or(200_000);
            let nw = parse_u64(args.get(1).copied()).unwrap_or(4) as usize;
            Box::new(move || parallel(limit, nw))
        }
        "ticker" => {
            let period = parse_u64(args.get(0).copied()).unwrap_or(100);
            Box::new(move || ticker(period))
        }
        "crashy" => {
            let survival = parse_u64(args.get(0).copied()).unwrap_or(0);
            Box::new(move || crashy(survival))
        }
        _ => return None,
    };
    scheduler::spawn(name, f)
}

pub fn parse_u64(s: Option<&str>) -> Option<u64> {
    let s = s?;
    let mut v: u64 = 0;
    for c in s.bytes() {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

/// CPU-bound задача: ищет простые числа до `limit`, пишет результат в /tmp/prime.log.
fn prime(limit: u64) {
    let mut count: u64 = 0;
    let mut last: u64 = 0;
    let mut n: u64 = 2;
    while n < limit {
        let mut is_prime = true;
        let mut d: u64 = 2;
        while d * d <= n {
            if n % d == 0 {
                is_prime = false;
                break;
            }
            d += 1;
        }
        if is_prime {
            count += 1;
            last = n;
        }
        n += 1;
    }
    let msg = alloc::format!("primes<{} count={} last={}\n", limit, count, last);
    scheduler::with_vfs(|vfs| {
        let _ = vfs.write_file("/tmp/prime.log", msg.as_bytes());
    });
}

/// Рекурсивный фибоначчи: заметная CPU-нагрузка, результат в /tmp/fib.log.
fn fib(n: u64) {
    let res = fib_rec(n);
    let msg = alloc::format!("fib({})={}\n", n, res);
    scheduler::with_vfs(|vfs| {
        let _ = vfs.write_file("/tmp/fib.log", msg.as_bytes());
    });
}

fn fib_rec(n: u64) -> u64 {
    if n < 2 {
        n
    } else {
        fib_rec(n - 1) + fib_rec(n - 2)
    }
}

const MAX_WORKERS: usize = 8;

/// Тик, с которого воркеры должны начать вычисление (общий старт-барьер).
static START_TICK: AtomicU64 = AtomicU64::new(0);
static W_START: [AtomicU64; MAX_WORKERS] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];
static W_FINISH: [AtomicU64; MAX_WORKERS] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];
static W_COUNT: [AtomicU64; MAX_WORKERS] = [
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
    AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0),
];

/// Демонстрация многозадачности: координатор разбивает диапазон на N
/// воркеров, запускает их через `scheduler::spawn`, ждёт завершения
/// (разделяемые атомики) и пишет отчёт в /tmp/parallel.log.
///
/// Воркеры ждут общего старт-тик (барьер), так что их start-тики совпадают,
/// а finish-тики близки — это доказывает вытесняющую конкурентность.
pub fn parallel(limit: u64, nworkers: usize) {
    let nw = if nworkers == 0 { 1 } else { nworkers.min(MAX_WORKERS) };
    let start = scheduler::ticks() + 300;
    START_TICK.store(start, Ordering::SeqCst);
    for i in 0..nw {
        W_START[i].store(0, Ordering::SeqCst);
        W_FINISH[i].store(0, Ordering::SeqCst);
        W_COUNT[i].store(0, Ordering::SeqCst);
    }

    for i in 0..nw {
        let entry = Box::new(move || {
            let lo = i as u64 * limit / nw as u64;
            let hi = (i as u64 + 1) * limit / nw as u64;
            let barrier = START_TICK.load(Ordering::SeqCst);
            scheduler::sleep_until(barrier);
            W_START[i].store(scheduler::ticks(), Ordering::SeqCst);
            let c = count_primes(lo, hi);
            W_COUNT[i].store(c, Ordering::SeqCst);
            W_FINISH[i].store(scheduler::ticks(), Ordering::SeqCst);
        });
        let name = alloc::format!("w{}", i);
        if scheduler::spawn(&name, entry).is_none() {
            W_START[i].store(scheduler::ticks(), Ordering::SeqCst);
            W_FINISH[i].store(scheduler::ticks(), Ordering::SeqCst);
            W_COUNT[i].store(0, Ordering::SeqCst);
        }
    }

    while (0..nw).any(|i| W_FINISH[i].load(Ordering::SeqCst) == 0) {
        scheduler::sleep_until(scheduler::ticks() + 1);
    }

    let mut total: u64 = 0;
    let mut report = String::new();
    for i in 0..nw {
        let c = W_COUNT[i].load(Ordering::SeqCst);
        total += c;
        let lo = i as u64 * limit / nw as u64;
        let hi = (i as u64 + 1) * limit / nw as u64;
        report.push_str(&alloc::format!(
            "w{} range={}-{} count={} start={} finish={}\n",
            i,
            lo,
            hi,
            c,
            W_START[i].load(Ordering::SeqCst),
            W_FINISH[i].load(Ordering::SeqCst),
        ));
    }
    report.push_str(&alloc::format!(
        "parallel limit={} workers={} total={}\n",
        limit,
        nw,
        total
    ));
    scheduler::with_vfs(|vfs| {
        let _ = vfs.write_file("/tmp/parallel.log", report.as_bytes());
    });
}

/// Считает простые числа в диапазоне [lo, hi).
fn count_primes(lo: u64, hi: u64) -> u64 {
    let mut count: u64 = 0;
    let mut n: u64 = if lo < 2 { 2 } else { lo };
    while n < hi {
        let mut is_prime = true;
        let mut d: u64 = 2;
        while d * d <= n {
            if n % d == 0 {
                is_prime = false;
                break;
            }
            d += 1;
        }
        if is_prime {
            count += 1;
        }
        n += 1;
    }
    count
}

/// Сердцебиение init-демона: каждые `period` тиков перезаписывает
/// /tmp/ticker.log текущим счётчиком. Живёт вечно (respawn-демонстрация).
fn ticker(period: u64) {
    let period = if period == 0 { 100 } else { period };
    let mut count: u64 = 0;
    loop {
        let msg = alloc::format!(
            "ticker alive count={} ticks={}\n",
            count,
            scheduler::ticks()
        );
        scheduler::with_vfs(|vfs| {
            let _ = vfs.write_file("/tmp/ticker.log", msg.as_bytes());
        });
        count += 1;
        let target = scheduler::ticks() + period;
        scheduler::sleep_until(target);
    }
}

/// Демонстрация рестарта: живёт `survival` тиков (по умолчанию мгновенно
/// падает), пишет отчёт в /tmp/crashy.log и завершается.
fn crashy(survival: u64) {
    let start = scheduler::ticks();
    let msg = alloc::format!(
        "crashy started at {} surviving {} ticks\n",
        start,
        survival
    );
    scheduler::with_vfs(|vfs| {
        let _ = vfs.write_file("/tmp/crashy.log", msg.as_bytes());
    });
    let target = start + survival;
    scheduler::sleep_until(target);
}
