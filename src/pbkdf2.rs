// src/pbkdf2.rs
//
// PBKDF2-HMAC-SHA256 (RFC 2898).
//
//   DK = F(P, S, c, dkLen)
//   F(P, S, c, i) = U_1 XOR U_2 XOR ... XOR U_c
//   U_1 = HMAC(P, S || INT(i))
//   U_j = HMAC(P, U_{j-1})
//
// Для dkLen = 32 (один блок SHA-256) достаточно одного блока F.
// Для большей длины — F вызывается для i = 1..ceil(dkLen / 32).

use crate::hmac::hmac_sha256;

/// Число итераций по умолчанию.
///
/// На скорости этой системы одно 64-байтовое сжатие ~единицы мкс, так что
/// 100 000 итераций дают десятки-сотни миллисекунд на проверку пароля.
/// На QEMU может оказаться медленнее — есть iteration_count_for_cpu.
pub const DEFAULT_ITERATIONS: u32 = 20_000;
/// Минимум, ниже которого не опускаемся ни при каких условиях.
pub const MIN_ITERATIONS: u32 = 2_000;
/// Верхняя граница — от DoS-атаки через очень большое число итераций
/// в /etc/shadow (файл принадлежит root, но всё же).
pub const MAX_ITERATIONS: u32 = 10_000_000;

/// PBKDF2-HMAC-SHA256, один блок. Возвращает 32 байта.
///
/// ВАЖНО: НЕ ограничивает итерации. Clamp только в iteration_count_for_cpu
/// и в make_shadow_line — иначе RFC 6070 тест-векторы (1, 2, 4096 итераций)
/// и внутренний бенчмарк ломаются.
pub fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    assert!(iterations > 0, "PBKDF2 iterations must be >= 1");
    let iters = iterations;

    // U_1 = HMAC(P, S || 0x00000001)
    let mut first = alloc::vec::Vec::with_capacity(salt.len() + 4);
    first.extend_from_slice(salt);
    first.extend_from_slice(&1u32.to_be_bytes());

    let mut u = hmac_sha256(password, &first);
    let mut t = u;

    for _ in 1..iters {
        u = hmac_sha256(password, &u);
        for i in 0..32 {
            t[i] ^= u[i];
        }
    }
    t
}

/// Сколько итераций использовать на этой машине.
///
/// Простой критерий: измеряем время 100 итераций (по тикам PIT) и подбираем
/// число так, чтобы проверка пароля занимала ~200 мс.
pub fn iteration_count_for_cpu() -> u32 {
    // Прогреваем и кэшируем — не пересчитываем на каждый логин.
    static CACHED: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
    let cached = CACHED.load(core::sync::atomic::Ordering::Relaxed);
    if cached != 0 {
        return cached;
    }

    let start = crate::scheduler::ticks();
    let _ = pbkdf2_sha256(b"benchmark", b"benchmark-salt", 100);
    let elapsed = crate::scheduler::ticks().saturating_sub(start);

    // Один тик = 10 мс. Цель — 200 мс = 20 тиков на проверку.
    // elapsed измерен для 100 итераций, значит на одну итерацию
    // уходит elapsed/100 тиков, а на 20 тиков влезает 100*20/elapsed итераций.
    let target = if elapsed == 0 {
        // Слишком быстро — берём нижнюю границу и не гадаем.
        MIN_ITERATIONS as u64
    } else {
        100u64 * 20 / elapsed as u64
    };
    let result = (target as u32).clamp(MIN_ITERATIONS, MAX_ITERATIONS);
    CACHED.store(result, core::sync::atomic::Ordering::Relaxed);
    result
}
