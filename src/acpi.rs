//! src/acpi.rs
//!
//! Минимальная работа с ACPI - ровно столько, сколько нужно для `shutdown`
//! и `reboot`. Полного разбора таблиц здесь нет и не предполагается: находим
//! RSDP, по нему корневую таблицу, в ней FADT, а из FADT - два адреса.
//!
//!   PM1a_CNT - регистр управления питанием. Запись в него с битом SLP_EN
//!              (13) и SLP_TYP (10..12) = 5 («Soft Off») выключает машину.
//!              На настоящей плате это единственный способ; на QEMU
//!              SLP_TYP = 0.
//!   ResetReg - регистр сброса; запись в него перезагружает плату.
//!
//! Три отступления от буквы спецификации, каждое из-за реальных машин:
//!
//! 1. **RSDP лежит не там, где велит искать.** По спецификации он либо в
//!    первом килобайте EBDA, либо в 0xE0000..0xFFFFF. QEMU кладёт настоящую
//!    копию в 0x000E0000, а вторую - в обычную RAM (у нас 0x1FA278). Мы
//!    проверяем оба места плюс EBDA.
//! 2. **Запись корневой таблицы бывает короче 36 байт.** По спецификации
//!    каждая запись - это 36 байт (в XSDT - 40). QEMU раскладывает адреса
//!    плотно, по 4 байта, и объявляет длину таблицы accordingly, поэтому
//!    обход по 36 байтам не находит ничего. Перебираем все три варианта
//!    шага, а каждый кандидат проверяем контрольной суммой.
//! 3. **Поля FADT лежат не по спецификации.** По спецификации X_PM1aEventBlock
//!    находится по 0x100, а QEMU кладёт те же данные в свою внутреннюю
//!    раскладку. Поэтому читаем оба варианта и берём тот, который похож на
//!    настоящий Generic Address Structure.
//!
//! Пакет _S5 из DSDT мы не разбираем (это AML, нужен полноценный
//! интерпретатор), поэтому SLP_TYP берётся из спецификации: сначала
//! пробуем 0 - так выключает QEMU, затем 5 - так выключает железо.

use core::ptr::read_unaligned;

use x86_64::instructions::port::Port;

use crate::memory::PHYS_MEM_OFFSET;

/// Граница физической памяти, доступной нам через окно PHYS_MEM_OFFSET.
/// Ровно столько отображает трамполин из `src/boot.asm`: одна таблица PDP
///high на 512 записей по 2 МиБ. Всё, что выше, нам недоступно, и читать
/// оттуда нельзя - будет паника на странице.
const MAX_PHYS: u64 = 0x4000_0000;

/// Байты в little-endian u64 для сигнатуры "RSD PTR ".
const SIG_RSDP: u64 = 0x2052_5450_2044_5352;
/// Байты в little-endian u32 для сигнатуры "FACP" (FADT).
const SIG_FACP: u32 = 0x5043_4146;

/// Смещения в RSDP.
const RSDP_REVISION: u64 = 15;
const RSDP_RSDT32: u64 = 16;
const RSDP_XRSDT: u64 = 36;

/// Смещения в корневой таблице: длина таблицы и начало записей.
const ROOT_LENGTH: u64 = 4;
const ROOT_FIRST_ENTRY: u64 = 36;

/// Смещения в FADT. Спецификационные помечены комментарием, остальные - там,
/// где QEMU раскладывает те же данные у себя.
const FADT_LENGTH: u64 = 4;
const FADT_PM1A_QEMU: u64 = 0x40; // ACPI 1.0: адрес блока PM; QEMU: PM1a_CNT
const FADT_PM_BASE_QEMU: u64 = 0x38; // QEMU: база блока PM
const FADT_PM1A_GAS_70: u64 = 0x70; // часть машин пишет GAS здесь
const FADT_RESET_GAS_70: u64 = 0x74; // QEMU: GAS регистра сброса
const FADT_X_PM1A: u64 = 0x100; // spec ACPI 2.0+
const FADT_RESET_REG: u64 = 0xE0; // spec ACPI 2.0+
const FADT_RESET_VALUE: u64 = 0xEC; // spec ACPI 2.0+

/// Внутри Generic Address Structure: 4 байта заголовка, затем адрес.
///   0 - AddressSpaceId (0 = системная память, 1 = порты ввода-вывода)
///   1 - BitWidth
///   2 - BitOffset
///   3 - AccessSize
///   4..12 - Address (8 байт)
const GAS_SPACE: u64 = 0;
const GAS_ADDRESS: u64 = 4;
/// Самая большая таблица FADT, которую мы готовы принять (ACPI 6.x = 276).
const FADT_MAX_LEN: usize = 0x400;
/// Минимальная осмысленная длина: заголовок SDT плюс первые поля.
const FADT_MIN_LEN: usize = 0x30;

/// Что мы вытащили из FADT.
pub struct Fadt {
    /// Порт PM1a_CNT (0 - не нашли).
    pub pm1a_block: u16,
    /// Порт сброса (0 - нет).
    pub reset_register: u16,
    /// Значение, которое надо записать в reset_register.
    pub reset_value: u16,
}

static mut FADT: Option<Fadt> = None;

// ---------------------------------------------------------------------------
// Чтение физической памяти
//
// Ядро отображает физическую память в окно PHYS_MEM_OFFSET, поэтому любой
// физический адрес читается обычным указателем. Ни одна из адресов, что мы
// трогаем, не пересекается с нашими собственными структурами.
// ---------------------------------------------------------------------------

#[inline]
unsafe fn at(p: u64) -> *const u8 {
    (PHYS_MEM_OFFSET + p) as *const u8
}

unsafe fn rd8(p: u64) -> u8 {
    read_unaligned(at(p))
}

unsafe fn rd16(p: u64) -> u16 {
    read_unaligned(at(p) as *const u16)
}

unsafe fn rd32(p: u64) -> u32 {
    read_unaligned(at(p) as *const u32)
}

unsafe fn rd64(p: u64) -> u64 {
    read_unaligned(at(p) as *const u64)
}

/// Адрес в пределах доступного нам окна?
#[inline]
fn readable(p: u64, len: u64) -> bool {
    p != 0 && p < MAX_PHYS && p + len <= MAX_PHYS
}

/// Контрольная сумма таблицы ACPI - сумма всех байт по модулю 256 должна
/// давать ноль.
unsafe fn checksum_ok(p: u64, len: usize) -> bool {
    let mut sum: u8 = 0;
    for i in 0..len {
        sum = sum.wrapping_add(rd8(p + i as u64));
    }
    sum == 0
}

// ---------------------------------------------------------------------------
// Порты ввода-вывода
// ---------------------------------------------------------------------------

/// Запись 16-битного слова в порт. Регистры питания и сброса - это
/// всегда 16-битные порты.
unsafe fn out16(port: u16, value: u16) {
    Port::new(port).write(value);
}

/// Сколько крутимся вхолостую после записи в порт питания. Если выключение
/// сработало, процессор до сюда не дойдёт; если нет - мы просто потратим
/// несколько миллионов циклов на ожидание.
fn wait_power_off() {
    for _ in 0..2_000_000 {
        core::hint::spin_loop();
    }
}

// ---------------------------------------------------------------------------
// Поиск таблиц
// ---------------------------------------------------------------------------

/// Ищет RSDP: в EBDA, в 0x000E0000..0x000FFFFF и в первых 16 МиБ RAM.
///
/// Последнее место - уступка QEMU, который кладёт туда вторую копию RSDP.
/// На настоящем железе ACPI вне первого мегабайта не встречается, зато
/// встречается мусор, который отсеет контрольная сумма.
unsafe fn find_rsdp() -> Option<u64> {
    // EBDA: BIOS пишет в 0x40E начало как сегмент, поэтому умножаем на 16.
    // Сама EBDA может стоять где угодно в первом мегабайте (QEMU кладёт её
    // на 0x9FC00), поэтому проверяем только грубую осмысленность адреса.
    let ebda = (rd16(0x40E) as u64) << 4;
    if (0x400..0xA0000).contains(&ebda) {
        if let Some(p) = scan_for_rsdp(ebda, 0x400) {
            return Some(p);
        }
    }
    if let Some(p) = scan_for_rsdp(0xE0000, 0x20000) {
        return Some(p);
    }
    scan_for_rsdp(0x100000, 0x1000000)
}

unsafe fn scan_for_rsdp(start: u64, len: u64) -> Option<u64> {
    let mut p = start;
    let end = start + len;
    while p + 20 <= end {
        if rd64(p) == SIG_RSDP {
            let rev = rd8(p + RSDP_REVISION);
            // Для ревизии 2 и выше RSDP длиннее: там лежат расширенные поля
            // и 64-битные указатели, а контрольная сумма считается по всей
            // структуре.
            let size = if rev >= 2 { 36usize } else { 20 };
            if p + size as u64 <= end && checksum_ok(p, size) {
                return Some(p);
            }
        }
        p += 16;
    }
    None
}

/// Похоже ли это на настоящий FADT? Сигнатура плюс длина плюс сходящаяся
/// контрольная сумма. Эта же проверка отсеивает мусор, который даёт обход
/// корневой таблицы с неправильным шагом.
unsafe fn is_fadt(p: u64) -> bool {
    if !readable(p, FADT_MIN_LEN as u64) || rd32(p) != SIG_FACP {
        return false;
    }
    let len = rd32(p + FADT_LENGTH) as usize;
    (FADT_MIN_LEN..=FADT_MAX_LEN).contains(&len) && checksum_ok(p, len)
}

/// Читает адрес из Generic Address Structure по смещению `off`.
///
/// Возвращает адрес только если структура похожа на настоящую: пространство
/// адресов - системная память или порты, ширина непустая, сам адрес похож на
/// порт. Именно эта проверка позволяет читать поля FADT не по одному
/// смещению, а «где написано правдоподобное значение».
unsafe fn gas_address(base: u64, off: u64) -> Option<u64> {
    let space = rd8(base + off + GAS_SPACE);
    let width = rd8(base + off + GAS_SPACE + 1);
    if space > 1 || width == 0 || width > 64 {
        return None;
    }
    let addr = rd64(base + off + GAS_ADDRESS);
    // Регистры питания и сброса всегда 16-битные порты ввода-вывода.
    if space == 1 && addr != 0 && addr <= 0xFFFF {
        Some(addr)
    } else {
        None
    }
}

/// Ищет FADT среди записей корневой таблицы.
///
/// Шаг записи перебирается: по спецификации это 36 байт (в XSDT - 40),
/// но QEMU кладёт голые адреса по 4 байта и объявляет длину таблицы в
/// соответствии с этим. Кандидат, найденный неправильным шагом, не пройдёт
/// проверку контрольной суммой.
unsafe fn find_fadt_in_root(root: u64, len: u64) -> Option<u64> {
    // (шаг, размер адреса в записи)
    let strides: [(u64, u64); 3] = [(36, 4), (40, 8), (4, 4)];
    for (step, addr_size) in strides {
        let mut off = ROOT_FIRST_ENTRY;
        while off + step <= len {
            let entry = root + off;
            if !readable(entry, addr_size) {
                break;
            }
            let table = if addr_size == 8 {
                rd64(entry)
            } else {
                rd32(entry) as u64
            };
            if is_fadt(table) {
                return Some(table);
            }
            off += step;
        }
    }
    None
}

unsafe fn find_fadt() -> Option<u64> {
    let rsdp = find_rsdp()?;
    // Ревизия 2+ даёт 64-битный XSDT, ревизия 0 - 32-битный RSDT.
    let rev = rd8(rsdp + RSDP_REVISION);
    let root = if rev >= 2 {
        rd64(rsdp + RSDP_XRSDT)
    } else {
        rd32(rsdp + RSDP_RSDT32) as u64
    };
    if !readable(root, ROOT_FIRST_ENTRY) {
        return None;
    }

    let len = rd32(root + ROOT_LENGTH) as u64;
    if len < ROOT_FIRST_ENTRY || len > 0x10000 {
        return None;
    }
    find_fadt_in_root(root, len)
}

/// Достаёт из FADT адрес блока питания.
///
/// Сначала пробуем спецификационные поля, потом - раскладку QEMU. Отличие
/// от простого чтения по фиксированному смещению в том, что каждое поле
/// проверяется на правдоподобие: мусор в X_PM1aEventBlock у QEMU - это
/// буквально следующая за FADT таблица, и без проверки мы бы записали
/// в порт 0x7FE2213 мусор.
unsafe fn pm1a_from_fadt(p: u64) -> Option<u16> {
    if let Some(a) = gas_address(p, FADT_X_PM1A) {
        return Some(a as u16);
    }
    if let Some(a) = gas_address(p, FADT_PM1A_GAS_70) {
        return Some(a as u16);
    }
    // ACPI 1.0 и раскладка QEMU: голое 32-битное число. Отличаем от
    // настоящего поля «RSDT Pointer» (которое по спецификации тоже живёт
    // по 0x40) тем, что перед ним обязана стоять база блока питания,
    // а PM1a_CNT = база + 4.
    let cnt = rd32(p + FADT_PM1A_QEMU) as u64;
    let base = rd32(p + FADT_PM_BASE_QEMU) as u64;
    if cnt == base + 4 && base >= 0x300 && base <= 0xFFF0 {
        return Some(cnt as u16);
    }
    None
}

/// Достаёт из FADT порт сброса. Если FADT его не содержит, оставляем 0:
/// выключение через 8042 и порт 0xCF9 и так есть в `power.rs`.
unsafe fn reset_from_fadt(p: u64) -> (u16, u16) {
    if let Some(reg) = gas_address(p, FADT_RESET_REG) {
        return (reg as u16, rd8(p + FADT_RESET_VALUE) as u16);
    }
    if let Some(reg) = gas_address(p, FADT_RESET_GAS_70) {
        return (reg as u16, 0);
    }
    (0, 0)
}

/// Находит FADT и запоминает разобранные поля. Возвращает false, если ACPI
/// нет или FADT не читается - тогда `shutdown` перейдёт к запасным способам.
pub fn init() -> bool {
    unsafe {
        let Some(p) = find_fadt() else {
            FADT = None;
            return false;
        };
        let pm1a = pm1a_from_fadt(p).unwrap_or(0);
        let (reset_reg, reset_val) = reset_from_fadt(p);
        FADT = Some(Fadt {
            pm1a_block: pm1a,
            reset_register: reset_reg,
            reset_value: reset_val,
        });
    }
    true
}

fn fadt() -> Option<&'static Fadt> {
    unsafe { (*core::ptr::addr_of!(FADT)).as_ref() }
}

/// Порт PM1a_CNT, если FADT его объявил.
fn pm1a_cnt() -> Option<u16> {
    let addr = fadt()?.pm1a_block;
    if addr == 0 {
        None
    } else {
        Some(addr)
    }
}

/// Выключает машину через ACPI. Возвращает true, если порт найден (это не
/// значит, что выключение сработало - если не сработало, управление просто
/// вернётся). Никогда не выключает машину, если FADT не найден.
pub fn power_off() -> bool {
    let Some(port) = pm1a_cnt() else {
        return false;
    };

    // SLP_TYP = 0 ("soft power off"), SLP_EN = 1. Так выключает QEMU.
    unsafe { out16(port, 0x2000) };
    wait_power_off();

    // SLP_TYP = 5 ("Soft Off") по спецификации ACPI, SLP_EN = 1. Так
    // выключает настоящее железо.
    unsafe { out16(port, 0x3400) };
    wait_power_off();

    true
}

/// Перезагружает машину через регистр сброса из FADT.
pub fn reset() -> bool {
    let Some(fadt) = fadt() else {
        return false;
    };
    let reg = fadt.reset_register;
    if reg == 0 {
        return false;
    }
    unsafe { out16(reg, fadt.reset_value) };
    wait_power_off();
    true
}

/// Короткая метка состояния ACPI для sysinfo/fetch: что именно нашлось и
/// доступно ли выключение питания. `describe()` даёт длинную строку для
/// splash, здесь нужен вариант, который влезает в поле SysInfo.
pub fn status() -> &'static str {
    match fadt() {
        Some(f) if f.pm1a_block != 0 => "FADT+PM1a",
        Some(_) => "FADT",
        None => "none",
    }
}

/// Диагностика для splash: короткая строка о том, что нашлось.
pub fn describe() -> &'static str {
    match fadt() {
        Some(f) if f.pm1a_block != 0 => "acpi: FADT found, power-off available",
        Some(_) => "acpi: FADT found, no PM1a block",
        None => "acpi: no FADT (legacy power-off only)",
    }
}