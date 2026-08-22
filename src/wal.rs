// src/wal.rs
//
// Write-ahead log (WAL) для VITAFS - этап 3.2.
//
// Раскладка журнальной области (блоки JOURNAL_START..JOURNAL_START+64):
//   блок 1      - JSUPER_A (зеркало дескриптора журнала)
//   блок 2      - JSUPER_B (зеркало; валидность по magic+контрольной сумме,
//                 при двух валидных побеждает старший next_seq)
//   блоки 3..64 - 62 слота кольцевого лога: полные копии блоков ФС.
//
// Дескриптор (jsuper):
//   0..8   magic "VITAWL1\0"
//   8..12  next_seq     - номер следующей транзакции
//   12..16 applied_seq  - все транзакции с seq <= applied_seq применены
//                         к реальным блокам
//   16..20 slot_count   - число слотов (62)
//   20..24 cksum        - сумма u32-слов дескриптора с нулевым полем суммы
//   24..   descs[SLOTS] { block_no: u32, seq: u32 }
//
// Состояние слота ВЫВОДИТСЯ: seq > applied_seq => LOGGED (копия в слоте
// относится к транзакции seq и, возможно, ещё не применена). Иначе FREE.
//
// Протокол транзакции:
//   begin() -> мутации через vitafs::fs_write_block попадают в staging
//              (дедуп по номеру блока; чтения видят staging через
//              fs_read_block/read_through);
//   commit():
//     1) грязные bcache-буферы и inode захватываются в staging;
//     2) порции по <=MAX_STAGING блоков фиксируются по очереди:
//        a) payload'ы - в слоты кольца (напрямую на диск);
//        b) jsuper с LOGGED-дескрипторами в ОБА зеркала - ТОЧКА ФИКСАЦИИ;
//        c) применение в bcache (грязные до flush_all);
//        d) jsuper с applied_seq=seq - закрытие (слоты снова свободны).
//   abort(): staging сбрасывается, диск не трогается.
//
// Отказоустойчивость:
//   - падение до точки фиксации: слоты мусорные, дескриптор их не признаёт;
//   - падение между (b) и (d): recover() при монтировании переигрывает
//     LOGGED-слоты по возрастанию seq (применение полного блока
//     идемпотентно);
//   - слот переиспользуется только после закрытия его транзакции, поэтому
//     мусор в слоте никогда не получает чужой LOGGED-дескриптор;
//   - журнал конечен: длинная транзакция режется на порции, каждая
//     фиксируется отдельно (crash посреди оставляет консистентный префикс).
//
// ВАЖНО О ПРЕРЫВАНИЯХ И КУЧЕ: куча ядра под спинлоком, и правило ядра
// ("аллокации только при включённых прерываниях", см. scheduler::spawn)
// обязывает НЕ аллоцировать внутри without_interrupts: задача, вытесненная
// посреди аллокации, держит лок кучи, а крутящийся с IF=0 не уступит ей
// процессор никогда. Поэтому: ёмкость staging резервируется в begin()/в
// начале commit() (IF=1), весь путь под without_interrupts работает только
// с уже выделенной памятью.

use crate::blockdev::{self, BLOCK_SIZE};
use alloc::vec::Vec;

pub const JSUPER_A: u64 = 1;
pub const JSUPER_B: u64 = 2;
pub const SLOT_START: u64 = 3;
pub const SLOTS: usize = 62;

const MAGIC: &[u8; 8] = b"VITAWL1\0";
const OFF_NEXT_SEQ: usize = 8;
const OFF_APPLIED_SEQ: usize = 12;
const OFF_SLOT_COUNT: usize = 16;
const OFF_CKSUM: usize = 20;
const DESC_OFF: usize = 24;
const DESC_SIZE: usize = 8; // block_no: u32, seq: u32
const DESC_END: usize = DESC_OFF + SLOTS * DESC_SIZE;

/// Порция транзакции: максимум блоков в одной точке фиксации.
pub const MAX_STAGING: usize = 16;

/// Резерв под захват кэшей поверх порции: до 16 bcache-буферов и до 8
/// инодных блоков на одну фиксацию. Держим скромно - куча всего 1 МиБ.
const CAPTURE_RESERVE: usize = 24;

struct Wal {
    depth: u32,
    next_seq: u32,
    applied_seq: u32,
    ring_pos: usize,
    staging: Vec<(u64, [u8; BLOCK_SIZE])>,
}

static mut WAL: Option<Wal> = None;

fn with_wal<R>(f: impl FnOnce(&mut Wal) -> R) -> Option<R> {
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let w = WAL.as_mut()?;
        Some(f(w))
    })
}

// --- сериализация дескриптора ----------------------------------------------

fn g32(buf: &[u8; BLOCK_SIZE], o: usize) -> u32 {
    u32::from_le_bytes(buf[o..o + 4].try_into().unwrap_or([0; 4]))
}

fn cksum(buf: &[u8; BLOCK_SIZE]) -> u32 {
    let mut sum: u32 = 0;
    let mut o = 0;
    while o + 4 <= DESC_END {
        if o != OFF_CKSUM {
            sum = sum.wrapping_add(g32(buf, o));
        }
        o += 4;
    }
    sum
}

fn encode_jsuper(
    out: &mut [u8; BLOCK_SIZE],
    next_seq: u32,
    applied_seq: u32,
    descs: &[(usize, u32, u32)],
) {
    *out = [0u8; BLOCK_SIZE];
    out[0..8].copy_from_slice(MAGIC);
    out[OFF_NEXT_SEQ..OFF_NEXT_SEQ + 4].copy_from_slice(&next_seq.to_le_bytes());
    out[OFF_APPLIED_SEQ..OFF_APPLIED_SEQ + 4].copy_from_slice(&applied_seq.to_le_bytes());
    out[OFF_SLOT_COUNT..OFF_SLOT_COUNT + 4].copy_from_slice(&(SLOTS as u32).to_le_bytes());
    for (i, bno, seq) in descs.iter().copied() {
        if i >= SLOTS {
            continue;
        }
        let o = DESC_OFF + i * DESC_SIZE;
        out[o..o + 4].copy_from_slice(&bno.to_le_bytes());
        out[o + 4..o + 8].copy_from_slice(&seq.to_le_bytes());
    }
    let c = cksum(out);
    out[OFF_CKSUM..OFF_CKSUM + 4].copy_from_slice(&c.to_le_bytes());
}

fn decode_jsuper(buf: &[u8; BLOCK_SIZE]) -> Option<(u32, u32, [(u32, u32); SLOTS])> {
    if &buf[0..8] != MAGIC || g32(buf, OFF_SLOT_COUNT) as usize != SLOTS {
        return None;
    }
    if g32(buf, OFF_CKSUM) != cksum(buf) {
        return None;
    }
    let mut descs = [(0u32, 0u32); SLOTS];
    for i in 0..SLOTS {
        let o = DESC_OFF + i * DESC_SIZE;
        descs[i] = (g32(buf, o), g32(buf, o + 4));
    }
    Some((g32(buf, OFF_NEXT_SEQ), g32(buf, OFF_APPLIED_SEQ), descs))
}

/// Пишет оба зеркала дескриптора. descs - только затронутые слоты.
/// Вызывается с IF=1: blockev::write_block сам гасит прерывания на PIO.
fn write_jsuper(next_seq: u32, applied_seq: u32, descs: &[(usize, u32, u32)]) -> bool {
    // Статический буфер: экономим 4 КиБ стека в глубокой цепочке вызовов
    // (однопроцессорное ядро, реентерабельности нет).
    static mut JSUPER_BUF: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
    let buf: &mut [u8; BLOCK_SIZE] = unsafe { &mut JSUPER_BUF };
    encode_jsuper(buf, next_seq, applied_seq, descs);
    // Оба зеркала идентичны; падение посреди записи оставляет хотя бы одно
    // валидное (recover выбирает по magic+cksum, затем по старшему next_seq).
    let ra = blockdev::write_block(JSUPER_A, buf);
    let rb = blockdev::write_block(JSUPER_B, buf);
    ra.is_ok() && rb.is_ok()
}

// --- инициализация и восстановление -----------------------------------------

/// Читает дескриптор журнала с диска (при монтировании, до заполнения bcache).
/// Вызывается с IF=1.
pub fn init() {
    let mut a = [0u8; BLOCK_SIZE];
    let mut b = [0u8; BLOCK_SIZE];
    let da = blockdev::read_block(JSUPER_A, &mut a)
        .ok()
        .and_then(|_| decode_jsuper(&a));
    let db = blockdev::read_block(JSUPER_B, &mut b)
        .ok()
        .and_then(|_| decode_jsuper(&b));

    let (next_seq, applied_seq) = match (da, db) {
        (Some((na, aa, _)), Some((nb, ab, _))) => {
            if na >= nb {
                (na, aa)
            } else {
                (nb, ab)
            }
        }
        (Some((n, ap, _)), None) => (n, ap),
        (None, Some((n, ap, _))) => (n, ap),
        (None, None) => (1, 0),
    };

    unsafe {
        WAL = Some(Wal {
            depth: 0,
            next_seq,
            applied_seq,
            ring_pos: 0,
            staging: Vec::new(),
        });
    }
}

/// Свежий пустой журнал (после format_image).
pub fn reset() {
    init();
    let _ = write_jsuper(1, 0, &[]);
}

/// Восстановление при монтировании: переигрывает зафиксированные, но не
/// закрытые транзакции (seq > applied_seq) по возрастанию seq, затем
/// очищает журнал. Возвращает число переигранных блоков.
pub fn recover() -> usize {
    init();
    let (next_seq, applied_seq) = match with_wal(|w| (w.next_seq, w.applied_seq)) {
        Some(v) => v,
        None => return 0,
    };

    // Список слотов берём из валидного зеркала (то же правило выбора).
    let mut js = [0u8; BLOCK_SIZE];
    let descs = {
        let da = blockdev::read_block(JSUPER_A, &mut js)
            .ok()
            .and_then(|_| decode_jsuper(&js));
        match da {
            Some((_, _, d)) => d,
            None => {
                let db = blockdev::read_block(JSUPER_B, &mut js)
                    .ok()
                    .and_then(|_| decode_jsuper(&js));
                match db {
                    Some((_, _, d)) => d,
                    None => {
                        reset();
                        return 0;
                    }
                }
            }
        }
    };

    // LOGGED-слоты по возрастанию seq (сортировка вставками в стековый
    // массив - SLOTS крошечный, аллокаций нет).
    let mut order: [usize; SLOTS] = [0; SLOTS];
    let mut n_order = 0usize;
    for (i, (_, seq)) in descs.iter().enumerate() {
        if n_order >= SLOTS {
            break; // страховка от переполнения order[]
        }
        if *seq > applied_seq && *seq < next_seq && *seq != 0 {
            let mut k = 0;
            while k < n_order && descs[order[k]].1 <= *seq {
                k += 1;
            }
            // сдвиг хвоста
            let mut j = n_order;
            while j > k {
                order[j] = order[j - 1];
                j -= 1;
            }
            order[k] = i;
            n_order += 1;
        }
    }

    let mut replayed = 0usize;
    for k in 0..n_order {
        let i = order[k];
        let (bno, _) = descs[i];
        if bno == 0 {
            continue;
        }
        // Читаем прямо в статический буфер (экономия стека), сразу
        // отдаём в bcache::write.
        static mut PAYLOAD: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];
        let payload: &mut [u8; BLOCK_SIZE] = unsafe { &mut PAYLOAD };
        if blockdev::read_block(SLOT_START + i as u64, payload).is_err() {
            continue;
        }
        // Через bcache: последующие чтения увидят данные, flush_all()
        // догонит диск. Повторное применение безопасно - пишется блок целиком.
        if crate::bcache::write(bno as u64, payload).is_ok() {
            replayed += 1;
        }
    }

    // Журнал чист: всё применено, все слоты свободны.
    let _ = write_jsuper(next_seq, next_seq.wrapping_sub(1), &[]);
    with_wal(|w| {
        w.applied_seq = next_seq.wrapping_sub(1);
        w.ring_pos = 0;
    });

    if replayed > 0 {
        crate::vga::serial_write_atomic("[wal] recover: replayed blocks=");
        crate::vga::serial_u64(replayed as u64);
        crate::vga::serial_write_atomic("\n");
    }
    replayed
}

// --- транзакции -------------------------------------------------------------

/// Активна ли транзакция (для маршрутизации fs_write_block).
pub fn txn_active() -> bool {
    with_wal(|w| w.depth > 0).unwrap_or(false)
}

/// Открывает транзакцию (вложенность допустима, фиксация - на внешней).
/// Резервирует ёмкость staging ЗАРАНЕЕ: внутри without_interrupts аллокаций
/// не будет (правило кучи ядра). Вызывается с IF=1.
pub fn begin() {
    with_wal(|w| {
        w.depth += 1;
        let need = MAX_STAGING + CAPTURE_RESERVE;
        if w.staging.capacity() < need {
            // try_reserve: без паники при нехватке кучи. Если резерв не
            // удался, stage() откажет по страховочному проверку и запись
            // уйдёт в bcache напрямую (как до WAL).
            let _ = w.staging.try_reserve(need - w.staging.capacity());
        }
    });
}

/// Закрывает успешную транзакцию: journal write + apply.
pub fn commit() -> bool {
    let last = match with_wal(|w| {
        if w.depth == 0 {
            return false;
        }
        w.depth -= 1;
        w.depth == 0
    }) {
        Some(v) => v,
        None => return false,
    };
    if !last {
        return true;
    }
    commit_all()
}

/// Откатывает текущую внешнюю транзакцию без записи на диск.
pub fn abort() {
    with_wal(|w| {
        if w.depth > 0 {
            w.depth -= 1;
        }
        if w.depth == 0 {
            w.staging.clear();
        }
    });
}

/// Ставит блок в очередь транзакции (дедуп по номеру). Вызывается из
/// vitafs::fs_write_block при активной транзакции и из capture-функций.
/// БЕЗ аллокаций: ёмкость зарезервирована в begin()/commit().
/// При переполнении - фиксация накопленного (порция) и продолжение набора.
pub fn stage(bno: u64, buf: &[u8; BLOCK_SIZE]) -> bool {
    // 1) Дедап: обновление существующей записи.
    let dup = with_wal(|w| {
        for (b, data) in w.staging.iter_mut() {
            if *b == bno {
                data.copy_from_slice(buf);
                return true;
            }
        }
        false
    })
    .unwrap_or(true);
    if dup {
        return true;
    }
    crate::vga::serial_write_atomic("[st:nodup]\n");
    let full_chk = with_wal(|w| w.staging.len()).unwrap_or(9999);
    crate::vga::serial_write_atomic("[st:len]=");
    crate::vga::serial_u64(full_chk as u64);
    crate::vga::serial_write_atomic("\n");

    // 2) Порция полна - фиксируем накопленное (без аллокаций).
    let full = with_wal(|w| w.staging.len() >= MAX_STAGING).unwrap_or(true);
    if full && !commit_portion() {
        return false;
    }

    // 3) Пуш. Гарантия ёмкости: begin() резервировал MAX_STAGING+CAPTURE_RESERVE,
    //    порции держат len <= MAX_STAGING, захват добавляет <= CAPTURE_RESERVE.
    let pushed = with_wal(|w| {
        if w.staging.len() >= w.staging.capacity() {
            return false; // страховка: не аллоцируем, отказываем
        }
        for (b, data) in w.staging.iter_mut() {
            if *b == bno {
                data.copy_from_slice(buf);
                return true;
            }
        }
        let l = w.staging.len();
        unsafe {
            let slot = w.staging.as_mut_ptr().add(l);
            (*slot).0 = bno;
            core::ptr::copy_nonoverlapping(buf.as_ptr(), (*slot).1.as_mut_ptr(), BLOCK_SIZE);
            w.staging.set_len(l + 1);
        }
        true
    });
    pushed.unwrap_or(false)
}

/// То же, что stage(), но принимает ссылку на чужой буфер (без копии на
/// стеке вызывающего). Используется capture-функциями кэшей.
pub fn stage_ref(bno: u64, buf: &[u8; BLOCK_SIZE]) -> bool {
    stage(bno, buf)
}

/// Читает блок сквозь staging (свежие данные текущей транзакции).
pub fn read_through(bno: u64, out: &mut [u8; BLOCK_SIZE]) -> bool {
    with_wal(|w| {
        for (b, data) in w.staging.iter().rev() {
            if *b == bno {
                out.copy_from_slice(data);
                return true;
            }
        }
        false
    })
    .unwrap_or(false)
}

/// Фиксирует ВСЁ накопленное: захват кэшей + порции по MAX_STAGING.
/// Вызывается с IF=1 (из wal_txn / fsck_lite).
fn commit_all() -> bool {
    // Резерв под захват кэшей (аллокация при IF=1, до безпрерывных секций).
    with_wal(|w| {
        let need = MAX_STAGING + CAPTURE_RESERVE;
        if w.staging.capacity() < need {
            let _ = w.staging.try_reserve(need - w.staging.capacity());
        }
    });

    // 1) Захват грязных кэшей, чтобы всё накопленное ушло через журнал.
    if !crate::bcache::capture_dirty_into_wal() {
        return false;
    }
    if false {
        if !crate::vitafs::capture_dirty_inodes_into_wal() {
            return false;
        }
    } // TEST-F3: захват инодов отключён
    // TEST-F4: порции отключены, staging сливаем
    with_wal(|w| w.staging.clear());
    true
}
#[allow(dead_code)]
fn commit_portion_disabled() -> bool {
    loop {
        let remaining = with_wal(|w| w.staging.len()).unwrap_or(0);
        if remaining == 0 {
            return true;
        }
        if !commit_portion() {
            return false;
        }
    }
}

/// Одна точка фиксация: до MAX_STAGING блоков из staging.
/// АТОМАРНОСТЬ: всё тело под without_interrupts. Иначе тик посреди
/// фиксации переключает CPU на другую задачу, чья транзакция мутирует
/// тот же staging (push/drain/take) - и снятые указатели/границыя
/// становятся невалидными (use-after-free, порча чужих данных).
/// Дисковый PIO поллинговый, работа с IF=0 допустима (как в flush_all).
/// Аллокаций нет: указатели на буферы снимаются под guard'ом, порция
/// удаляется из staging через drain(0..take) со сдвигом на месте
/// (ёмкость зарезервирована, поэтому сдвиг не перевыделяет память).
fn commit_portion() -> bool {
    // Замораживаем ПЛАНИРОВАНИЕ (не прерывания!): staging недоступен другим
    // задачам, тики и клавиатура продолжают обрабатываться немедленно -
    // длинные дисковые записи не глушат ввод.
    crate::scheduler::SCHED_FROZEN.store(1, core::sync::atomic::Ordering::SeqCst);
    let r = commit_portion_inner();
    crate::scheduler::SCHED_FROZEN.store(0, core::sync::atomic::Ordering::SeqCst);
    r
}

fn commit_portion_inner() -> bool {
    crate::vga::serial_write_atomic("[cp:a]\n");
    // Стековый список затронутых слотов: <= MAX_STAGING записей.
    let mut touched: [(usize, u32, u32); MAX_STAGING] = [(0, 0, 0); MAX_STAGING];
    let mut n_touched = 0usize;

    let (seq, applied_old, rp, n_stage) = match with_wal(|w| {
        if w.staging.is_empty() {
            None
        } else {
            Some((w.next_seq, w.applied_seq, w.ring_pos, w.staging.len()))
        }
    }) {
        Some(Some(v)) => v,
        _ => return true,
    };
    let take = core::cmp::min(MAX_STAGING, n_stage);

    // Снимаем указатели на буферы staging (память стабильна: до drain()
    // никто staging не мутирует - одноядерность + наша дисциплина вызовов).
    let mut ptrs: [*const [u8; BLOCK_SIZE]; MAX_STAGING] =
        [core::ptr::null(); MAX_STAGING];
    let mut bnos: [u64; MAX_STAGING] = [0; MAX_STAGING];
    with_wal(|w| {
        for k in 0..take {
            bnos[k] = w.staging[k].0;
            ptrs[k] = &w.staging[k].1;
        }
    });

    crate::vga::serial_write_atomic("[cp:b]\n");
    // a) Payload'ы в слоты кольца (напрямую на диск, мимо bcache).
    for k in 0..take {
        let idx = (rp + k) % SLOTS;
        let buf: &[u8; BLOCK_SIZE] = unsafe { &*ptrs[k] };
        if blockdev::write_block(SLOT_START + idx as u64, buf).is_err() {
            // Не зафиксировано: staging не трогаем, транзакция останется
            // незафиксированной и будет повторена следующим commit().
            return false;
        }
        touched[n_touched] = (idx, bnos[k] as u32, seq);
        n_touched += 1;
    }

    crate::vga::serial_write_atomic("[cp:c]\n");
    // b) ТОЧКА ФИКСАЦИИ: LOGGED-дескрипторы в оба зеркала.
    if !write_jsuper(seq + 1, applied_old, &touched[..n_touched]) {
        return false;
    }

    // c) Применяем к реальным блокам: bcache (для чтений) И СРАЗУ НА ДИСК.
    // Запись на диск обязана быть синхронной: после закрытия (d) журнал
    // больше не защищает эти блоки, поэтому к моменту закрытия они должны
    // быть уже на носителе. Иначе kill посреди отложенного flush рвёт блоки
    // при пустом журнале - восстановить нечего.
    for k in 0..take {
        let buf: &[u8; BLOCK_SIZE] = unsafe { &*ptrs[k] };
        let _ = crate::bcache::write(bnos[k], buf);
        // BISECT-B: синхронная дисковая запись отключена
    }

    crate::vga::serial_write_atomic("[cp:d]\\n");
    // d) Закрытие: applied_seq = seq, дескрипторы слотов обнуляются -
    //    слоты снова можно переиспользовать.
    let _ = write_jsuper(seq + 1, seq, &[]);

    with_wal(|w| {
        w.next_seq = seq + 1;
        w.applied_seq = seq;
        w.ring_pos = (rp + take) % SLOTS;
        w.staging.drain(0..take);
    });
    true
}
