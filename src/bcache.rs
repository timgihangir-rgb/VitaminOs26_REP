// src/bcache.rs
//
// Write-back кэш 4-КиБ блоков поверх blockdev.
//
// Политика:
//   - фиксированное число буферов в .bss (без аллокаций);
//   - LRU по монотонному счётчику обращений;
//   - запись попадает только в кэш и помечает буфер грязным;
//     на диск грязные буферы выгоняет flush_all();
//   - вытеснение чистого буфера бесплатное, грязного требует записи.
//
// Реентерабельность: операции над метаданными и копированием выполняются
// с запрещёнными прерываниями (вложенно с blockdev::without_interrupts).

use crate::blockdev::{self, BLOCK_SIZE};

pub const NBUFS: usize = 32;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Free,
    Clean,
    Dirty,
}

#[derive(Clone, Copy)]
struct Entry {
    block: u64,
    state: State,
    stamp: u64,
    data: [u8; BLOCK_SIZE],
}

static mut CLOCK: u64 = 0;

static mut ENTRIES: [Entry; NBUFS] = [Entry {
    block: u64::MAX,
    state: State::Free,
    stamp: 0,
    data: [0; BLOCK_SIZE],
}; NBUFS];

pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub dirty_evictions: u64,
}

static mut CSTATS: CacheStats = CacheStats {
    hits: 0,
    misses: 0,
    dirty_evictions: 0,
};

fn stats() -> CacheStats {
    unsafe { core::ptr::read_volatile(&CSTATS) }
}

unsafe fn tick() -> u64 {
    let p = &mut CLOCK as *mut u64;
    let v = core::ptr::read_volatile(p) + 1;
    core::ptr::write_volatile(p, v);
    v
}

/// Индекс свободного или самого старого буфера для захвата.
/// Грязную жертву выгоняет на диск до возврата.
unsafe fn pick_victim(for_block: u64) -> Option<usize> {
    let mut free_idx = None;
    let mut lru_idx = None;
    let mut lru_stamp = u64::MAX;
    for i in 0..NBUFS {
        let e = &ENTRIES[i];
        match e.state {
            State::Free => {
                if free_idx.is_none() {
                    free_idx = Some(i);
                }
            }
            _ => {
                // Тот же блок всегда переиспользуем напрямую.
                if e.block == for_block {
                    return Some(i);
                }
                if e.stamp < lru_stamp {
                    lru_stamp = e.stamp;
                    lru_idx = Some(i);
                }
            }
        }
    }
    let idx = free_idx.or(lru_idx)?;
    if ENTRIES[idx].state == State::Dirty {
        let block = ENTRIES[idx].block;
        let ok = blockdev::write_block(block, &ENTRIES[idx].data);
        if ok.is_err() {
            return None;
        }
        let p = &mut CSTATS.dirty_evictions as *mut u64;
        let v = core::ptr::read_volatile(p);
        core::ptr::write_volatile(p, v.saturating_add(1));
    }
    Some(idx)
}

/// Читает блок через кэш: сначала ищет в кэше, при промахе тянет с диска.
pub fn read(block_no: u64, out: &mut [u8; BLOCK_SIZE]) -> Result<(), ()> {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let idx = match pick_victim(block_no) {
            Some(i) => i,
            None => return Err(()),
        };
        {
            let e = &mut ENTRIES[idx];
            // ВАЖНО: попадание засчитываем ТОЛЬКО если слот реально хранит
            // наш блок. pick_victim для отсутствующего блока возвращает
            // жертву (Clean/Dirty с чужим номером) - раньше это приводило к
            // ложному "попаданию" и возврату чужих данных.
            if e.state != State::Free && e.block == block_no {
                let hp = &mut CSTATS.hits as *mut u64;
                let hv = core::ptr::read_volatile(hp);
                core::ptr::write_volatile(hp, hv.saturating_add(1));
                e.stamp = tick();
                out.copy_from_slice(&e.data);
                return Ok(());
            }
        }
        // Промах: читаем с диска прямо в слот.
        let mp = &mut CSTATS.misses as *mut u64;
        let mv = core::ptr::read_volatile(mp);
        core::ptr::write_volatile(mp, mv.saturating_add(1));
        let e = &mut ENTRIES[idx];
        match blockdev::read_block(block_no, &mut e.data) {
            Ok(()) => {
                e.block = block_no;
                e.state = State::Clean;
                e.stamp = tick();
                out.copy_from_slice(&e.data);
                Ok(())
            }
            Err(_) => {
                e.state = State::Free;
                Err(())
            }
        }
    })
}

/// Пишет блок в кэш (грязный). Дисковая запись откладывается до flush_all().
pub fn write(block_no: u64, data: &[u8; BLOCK_SIZE]) -> Result<(), ()> {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let idx = match pick_victim(block_no) {
            Some(i) => i,
            None => return Err(()),
        };
        let e = &mut ENTRIES[idx];
        e.block = block_no;
        e.data.copy_from_slice(data);
        e.state = State::Dirty;
        e.stamp = tick();
        Ok(())
    })
}

/// Выгоняет все грязные буферы на диск.
pub fn flush_all() -> bool {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let mut ok = true;
        for i in 0..NBUFS {
            let e = &mut ENTRIES[i];
            if e.state == State::Dirty {
                if blockdev::write_block(e.block, &e.data).is_err() {
                    ok = false;
                } else {
                    e.state = State::Clean;
                }
            }
        }
        ok
    })
}

/// Сбрасывает содержимое кэша без записи грязных буферов (для тестов).
#[allow(dead_code)]
pub fn drop_all() {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        for e in ENTRIES.iter_mut() {
            e.state = State::Free;
            e.block = u64::MAX;
        }
    });
}

/// Захватывает грязные буферы в WAL-staging (вызывается из wal::commit_all).
/// Буфер помечается чистым: данные уходят в журнал, apply вернёт их грязными
/// с тем же содержимым. false - staging отказал (буфер остаётся грязным).
pub fn capture_dirty_into_wal() -> bool {
    // Копия буфера жертвы: статика вместо стека (экономия 4 КиБ).
    // ВАЖНО: ссылка на ENTRIES[i].data нельзя держать через stage() -
    // тот может зафиксировать порцию, чей apply вытеснит ЭТОТ же слот
    // кэша и перезапишет данные (алиасинг -> мусор в журнале).
    static mut CAP_BUF: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        for i in 0..NBUFS {
            let (b, data): (u64, &[u8; BLOCK_SIZE]) = {
                let e = &ENTRIES[i];
                if e.state != State::Dirty {
                    continue;
                }
                let buf: &mut [u8; BLOCK_SIZE] = &mut CAP_BUF;
                buf.copy_from_slice(&e.data);
                (e.block, buf)
            };
            // Блок, изменённый ТЕКУЩЕЙ транзакцией, в bcache лежит в СТАРОЙ
            // редакции (мутации идут в staging, минуя кэш). stage() дедуплицирует
            // по номеру и ЗАМЕНИЛ бы свежий staging-контент устаревшим из кэша —
            // тогда битмапы/данные «ревертились» бы между транзакциями и
            // alloc_inode повторно выдавал занятые иноды. Версия из staging всегда
            // новее: пропускаем её.
            if crate::wal::is_staged(b) {
                continue;
            }
            if !crate::wal::stage_ref(b, data) {
                return false;
            }
            ENTRIES[i].state = State::Clean;
        }
        true
    })
}

/// Строка статистики кэша для диагностики (serial).
pub fn stats_line(buf: &mut alloc::string::String) {
    let s = stats();
    use core::fmt::Write;
    let _ = write!(
        buf,
        "bcache: hits={} misses={} dirty_evict={}",
        s.hits, s.misses, s.dirty_evictions
    );
}

/// Самопроверка семантики write-back: запись видна через кэш сразу,
/// на диске - только после flush_all(). Тестовый блок в конце образа.
pub fn selftest() -> bool {
    const IMG_BLOCKS: u64 = (8 * 1024 * 1024 / BLOCK_SIZE) as u64;
    let b = IMG_BLOCKS - 2;
    let mut w = [0u8; BLOCK_SIZE];
    let mut out = [0u8; BLOCK_SIZE];
    let mut raw = [0u8; BLOCK_SIZE];
    for (i, x) in w.iter_mut().enumerate() {
        *x = (i as u8).wrapping_add(0xA5);
    }
    // 1) Пишем в кэш, читаем оттуда же - должно совпасть.
    if write(b, &w).is_err() {
        return false;
    }
    if read(b, &mut out).is_err() || out != w {
        return false;
    }
    // 2) На диске данных ещё быть не должно (write-back).
    if blockdev::read_block(b, &mut raw).is_err() {
        return false;
    }
    drop_all();
    if raw == w {
        return false;
    }
    // 3) После flush_all диск обязан совпадать.
    if write(b, &w).is_err() || !flush_all() {
        return false;
    }
    drop_all();
    if blockdev::read_block(b, &mut raw).is_err() {
        return false;
    }
    let ok = raw == w;
    // 4) Чистим за собой.
    let zeros = [0u8; BLOCK_SIZE];
    let _ = blockdev::write_block(b, &zeros);
    ok
}
