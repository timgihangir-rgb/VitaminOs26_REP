// src/ahci.rs
//
// Драйвер AHCI/SATA: контроллер ищется на PCI по классу 0x01/0x06, регистры
// лежат в memory-BAR (BAR5 = ABAR) и мапятся в ядро через
// paging::map_phys_in_kernel. Команды отправляются через FIS Host Bus Data,
// данные идут DMA в физическую память ядра.
//
// Режим - опросный: прерывания HBA выключены (PxIE=0, GHC.IE=0), завершение
// команды ловится по битам в PxIS. Тот же приём, что в net.rs для rtl8139, и
// он не требует поднимать MSI-X в bare-metal-ядре.
//
// Раскладка регистров и структур сверена с тремя независимыми реализациями:
// drivers/ata/libahci.c и include/linux/ata.h (Linux), hw/ide/ahci.c (QEMU),
// src/hw/ahci.c (SeaBIOS). Ссылки на конкретные поля - в комментариях.
//
// DMA-структуры живут в .bss и выровнены по требованиям спецификации (список
// команд - 1 КиБ, таблица команды и область приёма FIS - 256 б). Физический
// адрес берётся вычитанием PHYS_MEM_OFFSET, ровно как это делает net.rs.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::memory::PHYS_MEM_OFFSET;

/// Диагностика драйвера в COM1: по ней видно, на каком шаге подъёма контроллера
/// всё встало, если диск не появился.
fn diag(msg: &str) {
    crate::vga::serial_write_atomic("[A] ");
    crate::vga::serial_write_atomic(msg);
    crate::vga::serial_putchar(b'\n');
}

// ─── Глобальные регистры HBA (байтовые смещения) ─────────
const HBA_CAP: usize = 0x00;
const HBA_GHC: usize = 0x04;
const HBA_IS: usize = 0x08;
const HBA_PI: usize = 0x0C;

const GHC_HR: u32 = 1 << 0;   // Host Reset (самосброс)
const GHC_AHCI_EN: u32 = 1 << 31;

/// Начало блока регистров порта и шаг между портами (AHCI 1.0+).
const PORT_BASE: usize = 0x100;
const PORT_STRIDE: usize = 0x80;

// ─── Регистры порта ─────────
const P_CLB: usize = 0x00;
const P_CLBU: usize = 0x04;
const P_FB: usize = 0x08;
const P_FBU: usize = 0x0C;
const P_IS: usize = 0x10;
const P_IE: usize = 0x14;
const P_CMD: usize = 0x18;
const P_TFDATA: usize = 0x20; // Task File Data: статус/ошибка ATA в битах 7:0/15:8
const P_SSTS: usize = 0x28;   // SStatus: DET и состояние PHY
const P_SERR: usize = 0x30;
const P_CI: usize = 0x38;

const CMD_ST: u32 = 1 << 0;    // Start: портовые DMA-движки запущены
const CMD_SUD: u32 = 1 << 1;   // Spin Up Device
const CMD_POD: u32 = 1 << 2;   // Power On Device
const CMD_FRE: u32 = 1 << 4;   // FIS Receive Enable
const CMD_ICC_ACTIVE: u32 = 0x1 << 28;
const CMD_CR: u32 = 1 << 15;   // Command List Running (ставит сам HBA)

// ─── Биты PxIS (раскладка как в Linux ahci.h и SeaBIOS ahci.h) ─────────
const IS_D2H_REG: u32 = 1 << 0;   // Device-to-Host Register FIS: команда завершена
const IS_HB_DATA_ERR: u32 = 1 << 28;
const IS_HB_FATAL: u32 = 1 << 29;
const IS_TASK_ERR: u32 = 1 << 30;
const IS_FATAL: u32 = IS_TASK_ERR | IS_HB_DATA_ERR | IS_HB_FATAL;
/// Команда завершена: пришёл D2H Register FIS или ошибка task file. Setup FIS
/// (PIO/DMA) - промежуточное уведомление, а не конец команды: после него
/// устройство ещё может держать BSY, поэтому его не ждём.
/// Connect/PhyRdy/SG_DONE намеренно не ждём: они могут прилететь не по делу.
const IS_DONE: u32 = IS_D2H_REG | IS_FATAL;

// ─── Команды ATA ─────────
const ATA_READ_EXT: u8 = 0x25;
const ATA_WRITE_EXT: u8 = 0x35;
const ATA_IDENTIFY: u8 = 0xEC;
const ATA_FLUSH: u8 = 0xE7;

// ─── Статус ATA (биты 7:0 регистра PxTFDATA) ─────────
const ST_ERR: u8 = 0x01;
const ST_DRQ: u8 = 0x08;
const ST_DF: u8 = 0x20;
const ST_DRDY: u8 = 0x40;
const ST_BSY: u8 = 0x80;

// ─── Геометрия DMA-структур ─────────
const CMD_LIST_SIZE: usize = 1024;
const FIS_AREA_SIZE: usize = 256;
const CMD_TABLE_SIZE: usize = 256;
/// PRD-таблица начинается с 0x80: FIS (64 б) + ATAPI CDB (32 б) + резерв (32 б).
const PRDT_OFF: usize = 0x80;
/// Буфер DMA на 256 секторов. Блочный запрос - это 8 секторов, но публичный
/// секторный API берёт до 255, поэтому берём с запасом.
const DMA_SIZE: usize = 256 * 512;
/// Больше 255 секторов в одну команду не шлём: в поле Count значение 0 означает
/// 256, и это лишняя неоднозначность в коде.
const MAX_XFER_SECTORS: usize = 255;

/// Предел ожидания в итерациях poll. Каждая итерация - чтение MMIO-регистра,
/// а оно в эмуляторе стоит порядка микросекунды, поэтому несколько сотен тысяч
/// итераций - это уже единицы секунд ожидания. Диск не должен держать систему
/// дольше этого: прерывания на время команды выключены (bcache зовёт нас из-под
/// without_interrupts), и тик таймера не идёт.
const SPIN: usize = 200_000;

// ─── Структуры для DMA (выравнены по требованиям спецификации) ─────────
#[repr(C, align(1024))]
struct CmdList([u8; CMD_LIST_SIZE]);
static mut CMD_LIST: CmdList = CmdList([0; CMD_LIST_SIZE]);

#[repr(C, align(256))]
struct CmdTable([u8; CMD_TABLE_SIZE]);
static mut CMD_TABLE: CmdTable = CmdTable([0; CMD_TABLE_SIZE]);

#[repr(C, align(256))]
struct FisArea([u8; FIS_AREA_SIZE]);
static mut FIS_AREA: FisArea = FisArea([0; FIS_AREA_SIZE]);

#[repr(C, align(4096))]
struct DmaBuf([u8; DMA_SIZE]);
static mut DMA: DmaBuf = DmaBuf([0; DMA_SIZE]);

// ─── Состояние драйвера ─────────
static mut MMIO: usize = 0;
static mut PORT: usize = 0;
static mut CAPACITY: Option<u64> = None;
static mut READY: bool = false;
/// Сериализация команд на случай вызова без запрета прерываний.
static BUSY: AtomicBool = AtomicBool::new(false);

#[inline]
fn hba(off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((MMIO + off) as *const u32) }
}

#[inline]
fn hba_set(off: usize, v: u32) {
    unsafe { core::ptr::write_volatile((MMIO + off) as *mut u32, v) }
}

#[inline]
fn prt(off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((PORT + off) as *const u32) }
}

#[inline]
fn prt_set(off: usize, v: u32) {
    unsafe { core::ptr::write_volatile((PORT + off) as *mut u32, v) }
}

/// Физический адрес ядерной памяти: ядро отображено со сдвигом PHYS_MEM_OFFSET.
#[inline]
unsafe fn phys(p: *const u8) -> u64 {
    p as u64 - PHYS_MEM_OFFSET
}

/// Занимает контроллер на время команды.
struct Lock;

impl Lock {
    fn take() -> Lock {
        while BUSY.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }
        Lock
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        BUSY.store(false, Ordering::Release);
    }
}

/// Останавливает портовые DMA-движки и ждёт, когда HBA погасит CR.
/// Без этого следующий запуск ST может не дать CR (AXI-пример из спецификации).
unsafe fn port_stop() -> bool {
    let mut cmd = prt(P_CMD);
    cmd &= !(CMD_ST | CMD_FRE);
    prt_set(P_CMD, cmd);
    for _ in 0..SPIN {
        if prt(P_CMD) & CMD_CR == 0 {
            return true;
        }
        core::hint::spin_loop();
    }
    false
}

/// Поднимает порт: адреса структур, движки ST|FRE, раскрутка линка.
/// Порядок шагов - как в ahci_port_setup() у SeaBIOS.
unsafe fn port_start(index: usize) -> bool {
    PORT = MMIO + PORT_BASE + index * PORT_STRIDE;

    // BIOS мог уже что-то запустить - гасим его движениями.
    let _ = port_stop();

    // Сброс task file: запись в SStatus сбрасывает DET у устройства,
    // ждём пока оно погаснет.
    prt_set(P_SSTS, 0xFFFF_FFFF);
    for _ in 0..SPIN {
        if prt(P_SSTS) & 0x0F == 0 {
            break;
        }
        core::hint::spin_loop();
    }

    let (clb, fb) = (phys(CMD_LIST.0.as_ptr()), phys(FIS_AREA.0.as_ptr()));
    prt_set(P_CLBU, (clb >> 32) as u32);
    prt_set(P_CLB, clb as u32);
    prt_set(P_FBU, (fb >> 32) as u32);
    prt_set(P_FB, fb as u32);

    // Прерывания выключены: обработчика AHCI в ядре нет, лишний IRQ в никуда.
    prt_set(P_IE, 0);
    prt_set(P_IS, 0xFFFF_FFFF);

    // Раскрутка линка и включение движков приёма FIS.
    let mut cmd = prt(P_CMD) & !(0xF << 28);
    cmd |= CMD_FRE | CMD_SUD | CMD_POD | CMD_ICC_ACTIVE;
    prt_set(P_CMD, cmd);
    let mut link_up = false;
    for _ in 0..SPIN {
        // SStatus: 0b011 = Device PhyComm established, значит за портом есть диск.
        if prt(P_SSTS) & 0x07 == 0x03 {
            link_up = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !link_up {
        return false;
    }

    // Гасим ошибки PHY, оставшиеся от предыдущего запуска.
    let serr = prt(P_SERR);
    if serr != 0 {
        prt_set(P_SERR, serr);
    }

    // Ждём, пока диск перестанет быть занятым: иначе первая команда уйдёт в
    // устройство, которое ещё раскручивается после сброса.
    for _ in 0..SPIN {
        if prt(P_TFDATA) & ((ST_BSY | ST_DRQ) as u32) == 0 {
            break;
        }
        core::hint::spin_loop();
    }

    cmd = prt(P_CMD) & !(0xF << 28);
    cmd |= CMD_ST | CMD_ICC_ACTIVE;
    prt_set(P_CMD, cmd);
    for _ in 0..SPIN {
        if prt(P_CMD) & CMD_CR != 0 {
            // Первичный D2H FIS при старте порта - не результат нашей команды.
            prt_set(P_IS, 0xFFFF_FFFF);
            return true;
        }
        core::hint::spin_loop();
    }
    let _ = port_stop();
    false
}

/// Отправляет одну команду и ждёт её завершения. Данные (если data) кладутся
/// в DMA-буфер до вызова и читаются из него после.
unsafe fn run_cmd(command: u8, feature: u8, lba: u64, count: u16, data: bool) -> Result<(), ()> {
    CMD_TABLE.0.fill(0);

    // Host Bus Data FIS: task file ATA в порядке её регистров
    // (совпадает с struct sata_cmd_fis в SeaBIOS и разбором в QEMU).
    let f = &mut CMD_TABLE.0[..20];
    f[0] = 0x27; // Register FIS Host Bus -> Device
    f[1] = 0x80; // C: FIS обновляет task file
    f[2] = command;
    f[3] = feature;
    f[4] = lba as u8;
    f[5] = (lba >> 8) as u8;
    f[6] = (lba >> 16) as u8;
    f[7] = 0x40; // Device: режим LBA (обязателен для *_DMA_EXT)
    f[8] = (lba >> 24) as u8;
    f[9] = (lba >> 32) as u8;
    f[10] = (lba >> 40) as u8;
    f[12] = (count & 0xFF) as u8;
    f[13] = (count >> 8) as u8;
    f[15] = 0; // Control

    // PRD: одна запись с адресом DMA-буфера. Счётчик байт нумеруется с нуля и
    // лежит в младших битах DW3 - так пишут и SeaBIOS (prdt[0].flags = bsize - 1),
    // и Linux (ahci_sg[si].flags_size = sg_len - 1); QEMU читает ровно это поле.
    let mut prdtl = 0u16;
    if data {
        let p = phys(DMA.0.as_ptr());
        let bytes = (count as u32) * 512 - 1;
        let prd = &mut CMD_TABLE.0[PRDT_OFF..PRDT_OFF + 16];
        prd[0..4].copy_from_slice(&(p as u32).to_le_bytes());
        prd[4..8].copy_from_slice(&((p >> 32) as u32).to_le_bytes());
        prd[12..16].copy_from_slice(&bytes.to_le_bytes());
        prdtl = 1;
    }

    // Запись списка команд: DW0 - флаги и число PRD-записей, DW1 - счётчик
    // байт PRD (для AHCI не нужен, HBA берёт длину из самих PRD), DW2/DW3 -
    // физический адрес таблицы команды.
    let ct = phys(CMD_TABLE.0.as_ptr());
    let mut dw0 = (prdtl as u32) << 16 | 5; // младшие 4 бита: длина FIS в dword'ах
    if command == ATA_WRITE_EXT {
        dw0 |= 1 << 6; // W: запись
    }
    CMD_LIST.0.fill(0);
    let le = &mut CMD_LIST.0[..32];
    le[0..4].copy_from_slice(&dw0.to_le_bytes());
    le[8..12].copy_from_slice(&(ct as u32).to_le_bytes());
    le[12..16].copy_from_slice(&((ct >> 32) as u32).to_le_bytes());
    le[28..32].copy_from_slice(&(command as u32).to_le_bytes()); // DW7: код команды

    FIS_AREA.0.fill(0);
    prt_set(P_IS, 0xFFFF_FFFF);
    prt_set(P_CI, 1);

    // Ждём отработки: HBA ставит бит в PxIS, когда команда завершилась
    // (D2H Register FIS) или провалилась (Task File Error). Status FIS может
    // прийти с ещё выставленным BSY - тогда ждём следующего.
    let mut bits = 0;
    let mut st = 0u8;
    for _ in 0..SPIN {
        bits = prt(P_IS);
        if bits & IS_DONE == 0 {
            core::hint::spin_loop();
            continue;
        }
        prt_set(P_IS, 0xFFFF_FFFF);
        st = (prt(P_TFDATA) & 0xFF) as u8;
        if st & ST_BSY == 0 {
            break;
        }
    }
    prt_set(P_IS, 0xFFFF_FFFF);
    if bits & IS_DONE == 0 {
        diag(&alloc::format!(
            "cmd {:02x} lba={} cnt={} timeout (pxis={:08x})",
            command, lba, count, bits
        ));
        return Err(());
    }

    // Статус ATA лежит в битах 7:0 PxTFDATA, ошибка - в 15:8 (так же его читают
    // SeaBIOS и QEMU). Успех: BSY/DF/ERR сняты и DRDY выставлен.
    if bits & IS_FATAL != 0
        || st & (ST_BSY | ST_DF | ST_ERR) != 0
        || st & ST_DRDY == 0
    {
        diag(&alloc::format!(
            "cmd {:02x} lba={} cnt={} failed: pxis={:08x} st={:02x} err={:02x}",
            command,
            lba,
            count,
            bits,
            st,
            (prt(P_TFDATA) >> 8) as u8
        ));
        return Err(());
    }
    Ok(())
}

/// Перезапуск порта после ошибки команды: иначе следующая команда уйдёт в
/// движок, который HBA не смог довести до конца.
unsafe fn port_recover() {
    let index = (PORT - (MMIO + PORT_BASE)) / PORT_STRIDE;
    let _ = port_stop();
    let err = prt(P_SERR);
    if err != 0 {
        prt_set(P_SERR, err);
    }
    if !port_start(index) {
        READY = false;
    }
}

/// Ищет SATA-контроллер, поднимает первый порт с диском и снимает ёмкость.
/// Вызывается один раз при загрузке, до первого обращения к ФС.
pub fn init() -> bool {
    unsafe {
        let Some(ctrl) = crate::pci::find_sata() else {
            diag("no sata controller on pci bus 0");
            return false;
        };
        diag(&alloc::format!(
            "ctrl {:02x}:{:02x}:{:02x} abar={:08x}",
            ctrl.bus, ctrl.slot, ctrl.func, ctrl.abar
        ));
        // BAR5 + немного запаса: блоки портов начинаются с 0x100 и идут
        // по 0x80, 32 порта дают 0x1100 от базы.
        let Some(mmio) = crate::paging::map_phys_in_kernel(ctrl.abar, 0x2000) else {
            diag("cannot map abar into kernel space");
            return false;
        };
        MMIO = mmio as usize;

        diag(&alloc::format!("mmio mapped at {:x}", mmio));

        // Шине нужно разрешение на Bus Master, иначе DMA в наш буфер не пойдёт.
        let cmd = crate::pci::read_u16(ctrl.bus, ctrl.slot, ctrl.func, 0x04);
        crate::pci::write_u16(ctrl.bus, ctrl.slot, ctrl.func, 0x04, cmd | 0x0006);

        // Программный сброс контроллера: обнуляет состояние, оставленное BIOS.
        hba_set(HBA_GHC, hba(HBA_GHC) | GHC_HR);
        for i in 0..SPIN {
            if hba(HBA_GHC) & GHC_HR == 0 {
                diag("hba reset done");
                break;
            }
            if i == SPIN - 1 {
                diag("hba reset timeout");
            }
            core::hint::spin_loop();
        }
        hba_set(HBA_GHC, GHC_AHCI_EN);
        hba_set(HBA_IS, 0xFFFF_FFFF);

        // Порты реализованы: 0x1f в младших битах CAP, битовая карта в PI.
        let cap = hba(HBA_CAP);
        let max_ports = ((cap & 0x1F) + 1) as usize;
        let impl_mask = hba(HBA_PI);
        diag(&alloc::format!(
            "cap={:08x} pi={:08x} ports={} impl={:08x}",
            cap,
            impl_mask,
            max_ports,
            impl_mask
        ));
        for index in 0..max_ports.min(32) {
            if impl_mask & (1 << index) == 0 {
                continue;
            }
            if !port_start(index) {
                diag(&alloc::format!("port {} did not start", index));
                continue;
            }
            match identify_sectors() {
                Some(sectors) => {
                    CAPACITY = Some(sectors);
                    READY = true;
                    diag(&alloc::format!("port {} disk, {} sectors", index, sectors));
                    return true;
                }
                None => diag(&alloc::format!("port {} no disk (identify failed)", index)),
            }
            let _ = port_stop();
        }
        false
    }
}

/// IDENTIFY DEVICE: 256 слов. Ёмкость - LBA48 (слова 100-103), если диск его
/// держит, иначе LBA28 (слова 60-61).
unsafe fn identify_sectors() -> Option<u64> {
    if run_cmd(ATA_IDENTIFY, 0, 0, 1, true).is_err() {
        return None;
    }
    let word = |i: usize| -> u16 { u16::from_le_bytes([DMA.0[i * 2], DMA.0[i * 2 + 1]]) };
    let mut sectors = (word(60) as u64) | ((word(61) as u64) << 16);
    // Word 83: бит 10 - поддержка LBA48, бит 15 - данные 82/83 валидны.
    if word(83) & 0x4000 == 0x4000 && word(83) & (1 << 10) != 0 {
        let lba48 = (word(100) as u64)
            | ((word(101) as u64) << 16)
            | ((word(102) as u64) << 32)
            | ((word(103) as u64) << 48);
        if lba48 != 0 && lba48 != u64::MAX {
            sectors = lba48;
        }
    }
    if sectors == 0 {
        return None;
    }
    Some(sectors)
}

/// Есть ли диск за AHCI-контроллером.
pub fn present() -> bool {
    unsafe { READY }
}

/// Ёмкость диска в секторах по 512 байт.
pub fn capacity_sectors() -> Option<u64> {
    unsafe { CAPACITY }
}

/// Читает `count` секторов начиная с `lba` в buf. Длинные запросы режутся
/// на куски по 255 секторов.
pub fn read_sectors(lba: u64, count: u16, buf: &mut [u8]) -> Result<(), ()> {
    if !present() || buf.len() < count as usize * 512 {
        return Err(());
    }
    let _lock = Lock::take();
    let mut done = 0usize;
    while done < count as usize {
        let n = core::cmp::min(count as usize - done, MAX_XFER_SECTORS);
        unsafe {
            if run_cmd(ATA_READ_EXT, 1, lba + done as u64, n as u16, true).is_err() {
                port_recover();
                return Err(());
            }
        }
        let off = done * 512;
        unsafe {
            buf[off..off + n * 512].copy_from_slice(&DMA.0[..n * 512]);
        }
        done += n;
    }
    Ok(())
}

/// Пишет `count` секторов из buf, затем сбрасывает кэш диска - чтобы данные
/// реально легли на носитель (то же поведение, что у ATA PIO в blockdev).
pub fn write_sectors(lba: u64, count: u16, buf: &[u8]) -> Result<(), ()> {
    if !present() || buf.len() < count as usize * 512 {
        return Err(());
    }
    let _lock = Lock::take();
    let mut done = 0usize;
    while done < count as usize {
        let n = core::cmp::min(count as usize - done, MAX_XFER_SECTORS);
        unsafe {
            DMA.0[..n * 512].copy_from_slice(&buf[done * 512..done * 512 + n * 512]);
        }
        unsafe {
            if run_cmd(ATA_WRITE_EXT, 1, lba + done as u64, n as u16, true).is_err() {
                port_recover();
                return Err(());
            }
        }
        done += n;
    }
    unsafe {
        if run_cmd(ATA_FLUSH, 0, 0, 0, false).is_err() {
            port_recover();
            return Err(());
        }
    }
    Ok(())
}