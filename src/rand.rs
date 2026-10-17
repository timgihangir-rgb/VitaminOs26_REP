// src/rand.rs
//
// Криптостойкий генератор случайных чисел на основе SHA-256.
//
// Архитектура:
//   - Пул 32 байта — состояние DRBG.
//   - Пул инициализируется при первой загрузке: RTC + jitter rdtsc +
//     счётчик прерываний клавиатуры + (если есть) rdrand.
//   - Каждый вызов random_bytes():
//       1) берёт свежий снимок TSC и счётчика вызовов;
//       2) смешивает их с пулом через SHA-256;
//       3) из полученного хэша берёт выход, а сам хэш становится
//          новым состоянием пула.
//
// Свойства:
//   - forward secrecy: из выхода нельзя восстановить предыдущий пул
//     (SHA-256 — односторонняя);
//   - нет повторений: даже два вызова в одном rdtsc-тике дадут разные
//     результаты, потому что счётчик вызовов увеличивается;
//   - нет блокировки: в отличие от /dev/random, мы не ждём «достаточно
//     энтропии». На старте пул заполняется тем, что есть, и этого
//     достаточно для генерации солей и сессионных ключей.
//
// Чего НЕ делаем:
//   - не оцениваем энтропию (нет формулы Шеннона);
//   - не блокируемся, если энтропии «мало»;
//   - не храним пул между перезагрузками (нет seed file, как в Linux).
//
// Для соли пароля и сессионного ключа этого достаточно. Если понадобится
// долгоживущий ключ шифрования — нужно добавить seed file и запись его
// на диск перед первой генерацией.

use crate::sha256;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const POOL_BYTES: usize = 32;

static mut POOL: [u8; POOL_BYTES] = [0; POOL_BYTES];
static SEEDED: AtomicBool = AtomicBool::new(false);
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Инициализация пула. Идемпотентна, безопасна к повторному вызову.
/// Вызывать один раз при загрузке — до первого random_bytes.
pub fn init() {
    if SEEDED.swap(true, Ordering::SeqCst) {
        return;
    }
    let mut pool = [0u8; POOL_BYTES];

    // 1) RTC-время загрузки. Уникально с точностью до секунды, но не секрет.
    let now = crate::rtc::now_unix();
    pool[0..8].copy_from_slice(&now.to_le_bytes());

    // 2) Jitter rdtsc: 64 пары замеров. Разница между последовательными
    //    чтениями TSC зависит от кэшей, прерываний, DMA, планировщика.
    //    На QEMU jitter меньше, чем на железе, но всё же есть: между
    //    двумя rdtsc проходит разное число инструкций эмуляции.
    let mut jit = 0u64;
    for _ in 0..64 {
        let a = rdtsc();
        // Немного работы между замерами.
        for _ in 0..16 { core::hint::spin_loop(); }
        let b = rdtsc();
        jit = jit.wrapping_add(b.wrapping_sub(a));
    }
    pool[8..16].copy_from_slice(&jit.to_le_bytes());

    // 3) Счётчик прерываний клавиатуры: пользователь обычно что-то нажал
    //    до первого запроса на случайные байты (установщик, логин).
    let kbd = crate::keyboard::kbd_irq_count();
    pool[16..24].copy_from_slice(&kbd.to_le_bytes());

    // 4) Тики планировщика — не секрет, но меняются.
    let ticks = crate::scheduler::ticks();
    pool[24..32].copy_from_slice(&ticks.to_le_bytes());

    // Первый SHA-256 — сжать всё вместе.
    let mut h = sha256::hash(&pool);

    // 5) rdrand, если поддерживается. Это лучший источник из доступных:
    //    аппаратный CSPRNG.
    if has_rdrand() {
        for _ in 0..8 {
            let r = rdrand64();
            for i in 0..8 {
                h[i] ^= (r >> (i * 8)) as u8;
            }
            // После каждого rdrand — SHA-256, чтобы один раунд не давал
            // линейную комбинацию h и r.
            h = sha256::hash(&h);
        }
    }

    unsafe {
        POOL.copy_from_slice(&h);
    }
}

/// Дополнительная энтропия от внешнего события (клавиатура, прерывание
/// сети, диск). Дешёвая, можно звать часто. Не блокируется.
pub fn add_entropy(bytes: &[u8]) {
    init();
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let mut buf = [0u8; POOL_BYTES + 64 + 16];
        buf[..POOL_BYTES].copy_from_slice(&POOL);
        let n = bytes.len().min(64);
        buf[POOL_BYTES..POOL_BYTES + n].copy_from_slice(&bytes[..n]);
        buf[POOL_BYTES + n..POOL_BYTES + n + 8].copy_from_slice(&rdtsc().to_le_bytes());
        let new = sha256::hash(&buf[..POOL_BYTES + n + 8]);
        POOL.copy_from_slice(&new);
    });
}

/// Криптостойкие случайные байты. Безопасна к вызову из любого контекста
/// (включая прерывания — внутренне глушит IF на короткий участок).
pub fn random_bytes(out: &mut [u8]) {
    init();
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let mut off = 0usize;
        while off < out.len() {
            // Свежая энтропия: TSC + счётчик. Даже если TSC не сдвинулся
            // (QEMU), счётчик уникален для каждого вызова.
            let mut extra = [0u8; 16];
            extra[0..8].copy_from_slice(&rdtsc().to_le_bytes());
            let c = COUNTER.fetch_add(1, Ordering::Relaxed);
            extra[8..16].copy_from_slice(&c.to_le_bytes());

            // h = SHA256(POOL || extra); POOL' = h; output = h[..n]
            let mut buf = [0u8; POOL_BYTES + 16];
            buf[..POOL_BYTES].copy_from_slice(&POOL);
            buf[POOL_BYTES..].copy_from_slice(&extra);
            let h = sha256::hash(&buf);

            POOL.copy_from_slice(&h);

            let n = core::cmp::min(32, out.len() - off);
            out[off..off + n].copy_from_slice(&h[..n]);
            off += n;
        }
    });
}

/// u32 для API, которым нужен «случайный идентификатор».
pub fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    random_bytes(&mut b);
    u32::from_le_bytes(b)
}

/// u64 — тот же приём.
pub fn random_u64() -> u64 {
    let mut b = [0u8; 8];
    random_bytes(&mut b);
    u64::from_le_bytes(b)
}

#[inline]
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi,
                         options(nomem, nostack));
    }
    ((hi as u64) << 32) | lo as u64
}

fn has_rdrand() -> bool {
    // CPUID.01H:ECX.RDRAND[bit 30].
    // На «старом железе» до Ivy Bridge (2012) этого бита нет —
    // сработает fallback из SHA-256 + jitter.
    unsafe { core::arch::x86_64::__cpuid(1).ecx & (1 << 30) != 0 }
}

fn rdrand64() -> u64 {
    loop {
        let r: u64;
        let ok: u8;
        unsafe {
            core::arch::asm!(
                "rdrand {r}",
                "setc {ok}",
                r = out(reg) r,
                ok = out(reg_byte) ok,
                options(nomem, nostack)
            );
        }
        if ok != 0 {
            return r;
        }
        // Если CF = 0, аппаратный пул пуст. На практике это почти
        // невозможно после первого rdrand, но на всякий случай повторяем.
    }
}

#[cfg(debug_assertions)]
/// Вызывается из stage0_diag: отчёт «два подряд разные, распределение».
pub fn selftest() -> (bool, u32) {
    let mut a = [0u8; 32];
    let mut b = [0u8; 32];
    random_bytes(&mut a);
    random_bytes(&mut b);
    let repeat = a == b;
    let mut bytes = [0u8; 256];
    random_bytes(&mut bytes);
    let ones: u32 = bytes.iter().map(|b| b.count_ones()).sum();
    (!repeat, ones)
}
