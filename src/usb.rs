// src/usb.rs
//
// USB-клавиатура: контроллер UHCI + HID boot protocol.
//
// Задача узкая — получить клавиатуру и отдавать её в тот же поток сканкодов,
// что и PS/2, чтобы шелл и все программы не различали источник ввода.
//
// Что реализовано:
//   * поиск UHCI на PCI (класс 0x0C, подкласс 0x03, progIF 0x00/0x01);
//   * программирование контроллера: сброс, frame list, SOF, включение портов;
//   * перечисление по стандартной процедуре USB: SET_ADDRESS,
//     GET_DESCRIPTOR(device/config), разбор HID-интерфейса;
//   * HID boot protocol: SET_PROTOCOL(0) и чтение 8-байтных отчётов
//     с interrupt-IN точки; отчёты переводятся в сканкоды набора 1.
//
// Режим — опросный, как AHCI в ahci.rs и rtl8139 в net.rs: прерывания USB
// не поднимаем, точку IN дёргаем из задачи раз в пару тиков. USB-контроллер
// на старых платах один, а лишний irq в 8259 без надобности.
//
// ── Про link pointer ──────────────────────────────────────────────────────
// Здесь есть расхождение между спецификацией UHCI 1.1 и моделью QEMU, и
// приходится поддерживать оба варианта.
//
// По спецификации (раздел 4.3.2) признак «элемент готов к обработке» лежит в
// бите 13 (V) link pointer'а, а сам link pointer'у предшествует 10-битное
// смещение: адрес занимает биты 31..10. QEMU разбирает link иначе: признак
// конца цепочки у него в бите 0, признак QH — в бите 1, а адрес он
// вырезает маской `& ~0xF`, то есть бит 13 попадает в адрес. Кроме того,
// QEMU считает активным TD бит 23 поля Control/Status (это соглашение
// EHCI), хотя по спецификации бит 23 — признак валидности полей.
//
// Из-за этого бит V в TD link'ах ставить нельзя: QEMU прочитает TD по
// адресу `phys | 0x2000`. Поэтому драйвер держит два варианта разметки и
// подбирает рабочий по первому же обмену: сначала пробуем «как в QEMU»,
// при неудаче переключаемся на «как в спецификации».
//
// Регистры и битовые поля сверены с hw/usb/hcd-uhci.c, hw/usb/uhci-regs.h
// (QEMU) и разделом 4.3 спецификации UHCI 1.1.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::memory::PHYS_MEM_OFFSET;

// ─── Регистры UHCI (I/O-пространство, BAR4) ──────────────────────────────
const REG_USBCMD: u16 = 0x00;
const REG_USBSTS: u16 = 0x02;
const REG_USBINTR: u16 = 0x04;
const REG_USBFRNUM: u16 = 0x06;
const REG_FLBASEADD: u16 = 0x08;
const REG_SOFMOD: u16 = 0x0c;
const REG_PORTSC: u16 = 0x10;

/// USBCMD
const CMD_RS: u16 = 1 << 0; // Run/Stop: 1 = контроллер крутит расписание
const CMD_HCRESET: u16 = 1 << 1; // Host Controller Reset (сам сбрасывается)
/// USBSTS
const STS_HCHALTED: u16 = 1 << 5;
/// PORTSC
const PORT_CCS: u16 = 1 << 0; // Current Connect Status: устройство воткнуто
const PORT_EN: u16 = 1 << 2; // Port Enable
const PORT_RESET: u16 = 1 << 9;

/// У UHCI два порта.
const PORTS: u16 = 2;

// ─── Поля TD и QH ────────────────────────────────────────────────────────
/// Link pointer в «спецификационной» разметке: адрес в битах 31..10, бит 11 —
/// Terminate, бит 13 — Valid.
const TD_LINK_T: u32 = 1 << 11;
const TD_LINK_V: u32 = 1 << 13;
/// Link pointer в разметке QEMU: бит 0 — Terminate, бит 1 — «это QH».
const QEMU_LINK_T: u32 = 1 << 0;
const QEMU_LINK_Q: u32 = 1 << 1;

const TD_CTRL_ACTIVE: u32 = 1 << 23;
/// Признак сорвавшейся передачи. Позиции полей C_ERR/E/H между спецификацией
/// и QEMU различаются, поэтому берём объединение: нам важно лишь «обмен
/// сорвался». NAK (бит 19 у QEMU) сюда не входит: им помечается штатная
/// неготовность устройства, а после успеха он сбрасывается.
const TD_CTRL_ERROR: u32 = (1 << 16) | (1 << 17) | (7 << 11) | (1 << 18) | (1 << 20) | (1 << 22);

const PID_SETUP: u8 = 0x2d;
const PID_IN: u8 = 0x69;
const PID_OUT: u8 = 0xe1;

/// Сколько кадров (по 1 мс) ждём завершения обмена. Запас большой: обмен
/// должен уложиться в первые же кадры расписания, а лишние миллисекунды
/// видны только в диагностике.
const FRAMES_WAIT: u64 = 30;

// ─── Разметка ссылок ─────────────────────────────────────────────────────
#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    /// Как в спецификации UHCI 1.1: признак активности — бит 13.
    Spec,
    /// Как в модели QEMU: активность — бит 23 поля ctrl, конец цепочки — бит 0.
    Qemu,
}

static mut STYLE: Style = Style::Qemu;

impl Style {
    /// Link pointer элемента, который контроллер должен обработать.
    #[inline]
    fn active(self, addr: u32) -> u32 {
        match self {
            Style::Spec => addr | TD_LINK_V,
            Style::Qemu => addr,
        }
    }

    /// Link pointer, которым цепочка закрывается.
    #[inline]
    fn term(self) -> u32 {
        match self {
            Style::Spec => TD_LINK_T,
            Style::Qemu => QEMU_LINK_T,
        }
    }

    /// Link pointer на элемент QH в списке кадров: обе разметки совпадают.
    #[inline]
    fn frame_entry(self, addr: u32) -> u32 {
        (addr & !0xF) | QEMU_LINK_Q
    }

    /// Ссылка считается свободной, когда элемент не готов к обработке.
    #[inline]
    fn is_idle(self, link: u32) -> bool {
        match self {
            Style::Spec => link & TD_LINK_V == 0,
            Style::Qemu => link & QEMU_LINK_T != 0,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Style::Spec => "spec",
            Style::Qemu => "qemu",
        }
    }
}

// ─── DMA-структуры ───────────────────────────────────────────────────────
#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Td {
    link: u32,
    ctrl: u32,
    token: u32,
    data: u32,
}

#[repr(C, align(16))]
#[derive(Clone, Copy)]
struct Qh {
    link: u32,
    el_link: u32,
}

/// Всё, что контроллер читает и пишет сам. Страница выровнена: буферы не
/// должны пересекать границу 4 КиБ, а они копятся подряд.
///
/// Буферы обязаны лежать здесь, а не на стеке или в куче: физический адрес
/// кучи (2 ГиБ, см. memory::HEAP_START) не входит в 32-битное адресное
/// пространство PCI, и контроллер туда не достучится. Тот же приём, что в
/// ahci.rs, только там структуры и вовсе не менялись.
#[repr(C, align(4096))]
struct Dma {
    qh_ctrl: Qh,
    td_setup: Td,
    td_data: Td,
    td_status: Td,
    qh_in: Qh,
    td_in: Td,
    setup: [u8; 8],
    desc: [u8; 256],
    report: [u8; 8],
}

static mut DMA: Dma = Dma {
    qh_ctrl: Qh { link: 0, el_link: 0 },
    td_setup: Td { link: 0, ctrl: 0, token: 0, data: 0 },
    td_data: Td { link: 0, ctrl: 0, token: 0, data: 0 },
    td_status: Td { link: 0, ctrl: 0, token: 0, data: 0 },
    qh_in: Qh { link: 0, el_link: 0 },
    td_in: Td { link: 0, ctrl: 0, token: 0, data: 0 },
    setup: [0; 8],
    desc: [0; 256],
    report: [0; 8],
};

/// Список кадров: 1024 указателя по 4 байта, страница выровнена.
#[repr(C, align(4096))]
struct Frames([u32; 1024]);
static mut FRAME_LIST: Frames = Frames([0; 1024]);

// ─── Состояние ───────────────────────────────────────────────────────────
static mut IO_BASE: u16 = 0;
/// Адрес HID-клавиатуры на шине USB.
static mut DEV_ADDR: u8 = 0;
static mut EP_IN: u8 = 0;
/// Клавиатура найдена и обслуживается: 8042 в этом случае игнорируется
/// (в BIOS с legacy-эмуляцией USB-клавиатура приходит в оба канала).
static OWNED: AtomicBool = AtomicBool::new(false);
/// Счётчик принятых HID-отчётов.
static mut REPORTS: u64 = 0;
/// Клавиши, нажатые по последнему отчёту: HID присылает весь текущий набор,
/// а не поток событий, поэтому разности и дают make/break.
static mut HELD: [u8; 6] = [0; 6];
static mut MODS: u8 = 0;

fn diag(msg: &str) {
    crate::vga::serial_write_atomic("[U] ");
    crate::vga::serial_write_atomic(msg);
    crate::vga::serial_putchar(b'\n');
}

fn diag_u(label: &str, v: u64) {
    crate::vga::serial_write_atomic("[U] ");
    crate::vga::serial_write_atomic(label);
    crate::vga::serial_u64(v);
    crate::vga::serial_putchar(b'\n');
}

// ─── Регистры ────────────────────────────────────────────────────────────
#[inline]
fn rd_reg(off: u16) -> u16 {
    use x86_64::instructions::port::Port;
    unsafe { Port::<u16>::new(IO_BASE + off).read() }
}

#[inline]
fn wr_reg(off: u16, v: u16) {
    use x86_64::instructions::port::Port;
    unsafe { Port::<u16>::new(IO_BASE + off).write(v) }
}

/// Физический адрес DMA-структуры: ядро отображено со сдвигом
/// PHYS_MEM_OFFSET (ровно как в ahci.rs и net.rs).
#[inline]
fn phys(p: *const u8) -> u32 {
    (p as u64 - PHYS_MEM_OFFSET) as u32
}

/// Физический адрес поля внутри DMA-блока: поля лежат в нём подряд.
#[inline]
fn phys_field<T>(base: *const u8, field: *const T) -> u32 {
    (field as *const u8 as u64 - base as u64) as u32 | phys(base)
}

/// Поле Maximum Length в TD token: значение = длина минус один, а 0x3FF
/// означает нулевую длину.
#[inline]
fn maxlen(len: usize) -> u32 {
    if len == 0 {
        0x3ff
    } else {
        (len - 1) as u32
    }
}

/// Token пакета: MaxLen в 31..21, Endpoint в 18..15, Device Address в 14..8,
/// PID в 7..0 (раздел 4.3.3 спецификации UHCI 1.1).
#[inline]
fn token(pid: u8, dev: u8, ep: u8, len: usize) -> u32 {
    (maxlen(len) << 21) | (((ep as u32) & 0xf) << 15) | (((dev as u32) & 0x7f) << 8) | pid as u32
}

// ─── Поиск контроллера ───────────────────────────────────────────────────
/// Ищет UHCI: класс 0x0C (Serial bus), подкласс 0x03 (USB), progIF 0x00
/// (UHCI 1.0) или 0x01 (UHCI 1.1). Регистры лежат в I/O-BAR4.
fn find_uhci() -> Option<u16> {
    for slot in 0..32u8 {
        let vendor0 = crate::pci::read_u16(0, slot, 0, 0);
        if vendor0 == 0xFFFF {
            continue;
        }
        let multifunction = crate::pci::read_u8(0, slot, 0, 0x0E) & 0x80 != 0;
        let funcs = if multifunction { 8 } else { 1 };
        for func in 0..funcs {
            let vendor = if func == 0 {
                vendor0
            } else {
                crate::pci::read_u16(0, slot, func, 0)
            };
            if vendor == 0xFFFF {
                continue;
            }
            if crate::pci::read_u8(0, slot, func, 0x0B) != 0x0C
                || crate::pci::read_u8(0, slot, func, 0x0A) != 0x03
            {
                continue;
            }
            let prog_if = crate::pci::read_u8(0, slot, func, 0x09);
            if prog_if > 0x01 {
                continue; // 0x02 — EHCI, у него другой контроллер
            }
            let bar = crate::pci::read_u32(0, slot, func, 0x20);
            if bar & 1 == 0 {
                continue; // у UHCI BAR4 — это I/O
            }
            let io = (bar & 0xFFFC) as u16;
            if io == 0 {
                continue; // BAR не назначен BIOS
            }
            // Включаем декодирование I/O и bus master: без DMA-движка
            // контроллер молча ничего не передаёт.
            let cmd = crate::pci::read_u16(0, slot, func, 0x04);
            crate::pci::write_u16(0, slot, func, 0x04, cmd | 0x0001 | 0x0004);
            return Some(io);
        }
    }
    None
}

// ─── Управление контроллером ─────────────────────────────────────────────
unsafe fn controller_start() -> bool {
    // Сброс. После HCRESET контроллер обязан выставить HCHALTED.
    wr_reg(REG_USBCMD, CMD_HCRESET);
    for _ in 0..100_000 {
        if rd_reg(REG_USBSTS) & STS_HCHALTED != 0 {
            break;
        }
        core::hint::spin_loop();
    }
    if rd_reg(REG_USBSTS) & STS_HCHALTED == 0 {
        diag("controller reset did not halt");
        return false;
    }
    wr_reg(REG_USBCMD, 0);
    // Прерываниями не пользуемся — сбрасываем и маску, и висящие флаги.
    wr_reg(REG_USBINTR, 0);
    wr_reg(REG_USBSTS, 0xFFFF);

    // FRNUM пишется только в остановленном состоянии.
    wr_reg(REG_USBFRNUM, 0);
    wr_reg(REG_SOFMOD, 64);

    let base = core::ptr::addr_of!(DMA) as *const u8;
    let fl = phys(core::ptr::addr_of!(FRAME_LIST) as *const u8);
    wr_reg(REG_FLBASEADD, (fl & 0xFFFF) as u16);
    wr_reg(REG_FLBASEADD + 2, (fl >> 16) as u16);

    // Список кадров: все 1024 кадра указывают на управляющий QH.
    let entry = STYLE.frame_entry(phys_field(base, core::ptr::addr_of!(DMA.qh_ctrl)));
    let fl_ptr = core::ptr::addr_of_mut!(FRAME_LIST.0) as *mut u32;
    for i in 0..1024 {
        core::ptr::write_volatile(fl_ptr.add(i), entry);
    }

    // Управляющий QH пуст: элементов нет. Он ссылается на QH приёма отчётов
    // HID, поэтому interrupt-IN точка тоже попадает в расписание.
    let qh = core::ptr::addr_of_mut!(DMA.qh_ctrl);
    let qh_in_p = phys_field(base, core::ptr::addr_of!(DMA.qh_in));
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*qh).link),
        (qh_in_p & !0xF) | QEMU_LINK_Q,
    );
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*qh).el_link), STYLE.term());
    let qh_in = core::ptr::addr_of_mut!(DMA.qh_in);
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*qh_in).link), QEMU_LINK_T);
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*qh_in).el_link), STYLE.term());

    // Запускаем расписание — после этого контроллер пошёл по списку кадров.
    wr_reg(REG_USBCMD, CMD_RS);
    true
}

/// Сброс порта и включение его. Возвращает false, если устройство не ответило.
unsafe fn port_reset(port: u16) -> bool {
    let off = REG_PORTSC + port * 2;
    if rd_reg(off) & PORT_CCS == 0 {
        return false;
    }
    wr_reg(off, PORT_RESET);
    // По спецификации полный сброс занимает не меньше 10 мс; ждём с запасом.
    for _ in 0..20 {
        crate::scheduler::sleep_until(crate::scheduler::ticks() + 1);
        if rd_reg(off) & PORT_CCS == 0 {
            return false;
        }
    }
    // Снимаем reset и включаем порт: без этого контроллер не ищет устройство.
    wr_reg(off, PORT_EN);
    true
}

// ─── Control transfer ────────────────────────────────────────────────────
/// Передача SETUP (+ необязательные DATA и STATUS) на адрес `dev`, ep 0.
///
/// `data_in` — направление DATA-стадии; при `false` это OUT (или вовсе без
/// DATA-стадии, если `data` пуст).
unsafe fn control(dev: u8, setup: [u8; 8], data: &mut [u8], data_in: bool) -> bool {
    let base = core::ptr::addr_of!(DMA) as *const u8;
    let style = STYLE;
    let qh = core::ptr::addr_of_mut!(DMA.qh_ctrl);
    let td_setup = core::ptr::addr_of_mut!(DMA.td_setup);
    let td_data = core::ptr::addr_of_mut!(DMA.td_data);
    let td_status = core::ptr::addr_of_mut!(DMA.td_status);
    let setup_buf = core::ptr::addr_of_mut!(DMA.setup) as *mut u8;

    for i in 0..8 {
        core::ptr::write_volatile(setup_buf.add(i), setup[i]);
    }

    let td_setup_p = phys_field(base, td_setup);
    let td_data_p = phys_field(base, td_data);
    let td_status_p = phys_field(base, td_status);

    // SETUP-стадия.
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*td_setup).ctrl), TD_CTRL_ACTIVE);
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*td_setup).token),
        token(PID_SETUP, dev, 0, 8),
    );
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*td_setup).data),
        phys(setup_buf as *const u8),
    );

    // STATUS-стадия: направление противоположно DATA-стадии, а без DATA
    // (управляющая передача вроде SET_ADDRESS) это всегда IN.
    let has_data = !data.is_empty();
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*td_status).link), style.term());
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*td_status).ctrl), TD_CTRL_ACTIVE);
    let status_pid = if has_data && data_in { PID_OUT } else { PID_IN };
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*td_status).token),
        token(status_pid, dev, 0, 0),
    );
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*td_status).data), 0);

    // DATA-стадия (необязательная). Замыкаем цепочку справа налево:
    // SETUP -> DATA -> STATUS, иначе последний TD до контроллера не дойдёт.
    if has_data {
        let pid = if data_in { PID_IN } else { PID_OUT };
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*td_data).link),
            style.active(td_status_p),
        );
        core::ptr::write_volatile(core::ptr::addr_of_mut!((*td_data).ctrl), TD_CTRL_ACTIVE);
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*td_data).token),
            token(pid, dev, 0, data.len()),
        );
        core::ptr::write_volatile(
            core::ptr::addr_of_mut!((*td_data).data),
            phys(data.as_mut_ptr() as *const u8),
        );
    }
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*td_setup).link),
        style.active(if has_data { td_data_p } else { td_status_p }),
    );

    // Запускаем цепочку: элемент QH указывает на первый TD.
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*qh).el_link),
        style.active(td_setup_p),
    );

    // Ждём, пока QH опустеет.
    let mut waited = 0u64;
    loop {
        let el = core::ptr::read_volatile(core::ptr::addr_of!((*qh).el_link));
        if style.is_idle(el) {
            break;
        }
        if waited >= FRAMES_WAIT {
            diag("control transfer timeout");
            diag_u("  el_link", el as u64);
            diag_u("  setup.ctrl", core::ptr::read_volatile(
                core::ptr::addr_of!((*td_setup).ctrl)) as u64);
            diag_u("  data.ctrl", core::ptr::read_volatile(
                core::ptr::addr_of!((*td_data).ctrl)) as u64);
            diag_u("  status.ctrl", core::ptr::read_volatile(
                core::ptr::addr_of!((*td_status).ctrl)) as u64);
            diag_u("  sts", rd_reg(REG_USBSTS) as u64);
            let mut hex = [0u8; 24];
            let sb = core::ptr::addr_of!(DMA.setup) as *const u8;
            for i in 0..8 {
                hex[i * 3] = b"0123456789abcdef"[(*sb.add(i) >> 4) as usize];
                hex[i * 3 + 1] = b"0123456789abcdef"[(*sb.add(i) & 0xf) as usize];
                hex[i * 3 + 2] = b' ';
            }
            crate::vga::serial_write_atomic("[U]   setup=");
            crate::vga::serial_write_atomic(core::str::from_utf8(&hex).unwrap_or("?"));
            crate::vga::serial_putchar(b'\n');
            return false;
        }
        crate::scheduler::sleep_until(crate::scheduler::ticks() + 1);
        waited += 1;
    }

    let mut ok = core::ptr::read_volatile(core::ptr::addr_of!((*td_setup).ctrl)) & TD_CTRL_ERROR == 0;
    if has_data && ok {
        ok = core::ptr::read_volatile(core::ptr::addr_of!((*td_data).ctrl)) & TD_CTRL_ERROR == 0;
    }
    if !ok {
        diag_u("  setup.ctrl", core::ptr::read_volatile(
            core::ptr::addr_of!((*td_setup).ctrl)) as u64);
        diag_u("  data.ctrl", core::ptr::read_volatile(
            core::ptr::addr_of!((*td_data).ctrl)) as u64);
        diag_u("  status.ctrl", core::ptr::read_volatile(
            core::ptr::addr_of!((*td_status).ctrl)) as u64);
    }
    // Сбрасываем QH в исходное состояние: он всё это время в списке кадров.
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*qh).el_link), style.term());
    ok
}

// ─── Перечисление ────────────────────────────────────────────────────────
/// GET_DESCRIPTOR. `offset` — смещение внутри дескриптора: длинный
/// конфигурационный дескриптор по спецификации читается несколькими
/// запросами, и смещение идёт в младшем байте wValue.
///
/// Данные кладутся в DMA-буфер, а не в `out`: физический адрес строки задачи
/// контроллеру недоступен (см. комментарий к Dma). Копию делает вызывающий.
unsafe fn read_descriptor(dev: u8, kind: u8, offset: u8, len: usize) -> bool {
    let mut setup = [0u8; 8];
    setup[0] = 0x80; // bmRequestType: IN, стандартный, устройство
    setup[1] = 0x06; // GET_DESCRIPTOR
    // 16-битные поля SETUP-пакета идут от младшего байта к старшему:
    //   wValue  = тип дескриптора << 8 | смещение  -> setup[3], setup[2]
    //   wIndex  = язык (0x0409)                      -> setup[5], setup[4]
    //   wLength = длина запроса                      -> setup[7], setup[6]
    setup[2] = offset;
    setup[3] = kind;
    setup[4] = 0x09; // language ID: US English. Для строковых дескрипторов он
    setup[5] = 0x04; // обязателен, остальные устройство просто игнорируют.
    setup[6] = len as u8;
    let buf = &mut DMA.desc[..len];
    control(dev, setup, buf, true)
}

fn set_address(dev: u8, addr: u8) -> bool {
    let mut setup = [0u8; 8];
    setup[0] = 0x00; // OUT, стандартный, устройство
    setup[1] = 0x05; // SET_ADDRESS
    setup[2] = addr; // wValue: адрес в младшем байте
    unsafe { control(dev, setup, &mut [], false) }
}

/// Копия DMA-буфера: дескрипторы наружу отдаём по месту, из стека.
fn take_desc() -> [u8; 256] {
    unsafe { DMA.desc }
}

/// Полный device descriptor. По спецификации его запрашивают в два приёма:
/// сначала 8 байт (там bMaxPacketSize0), потом сразу остаток.
fn read_device_descriptor(dev: u8, out: &mut [u8; 18]) -> bool {
    if !unsafe { read_descriptor(dev, 1, 0, 8) } {
        return false;
    }
    out[..8].copy_from_slice(&take_desc()[..8]);
    let total = out[0] as usize;
    if !(8..=18).contains(&total) {
        return false;
    }
    if total == 8 {
        return true;
    }
    if !unsafe { read_descriptor(dev, 1, 8, total - 8) } {
        return false;
    }
    out[8..total].copy_from_slice(&take_desc()[..total - 8]);
    true
}

/// Конфигурационный descriptor: сначала заголовок (9 байт, откуда берём
/// wTotalLength), потом целиком одним запросом. Младший байт wValue у
/// GET_DESCRIPTOR — это номер конфигурации, а не смещение, так что читать
/// конфигурацию «кусками со смещением» нельзя: устройство такой запрос
/// отвергает.
fn read_config_descriptor(dev: u8, out: &mut [u8; 256]) -> bool {
    if !unsafe { read_descriptor(dev, 2, 0, 9) } {
        return false;
    }
    out[..9].copy_from_slice(&take_desc()[..9]);
    let total = u16::from_le_bytes([out[2], out[3]]) as usize;
    if !(9..=out.len()).contains(&total) {
        return false;
    }
    if total > 255 {
        // Не помещается в однобайтовый wLength. Для HID-клавиатур так не
        // бывает, но на всякий случай идём дальше с одним заголовком.
        return true;
    }
    if !unsafe { read_descriptor(dev, 2, 0, total) } {
        return false;
    }
    out[..total].copy_from_slice(&take_desc()[..total]);
    true
}

/// Ищет в конфигурационном дескрипторе интерфейс HID-boot и его
/// interrupt-IN точку. Возвращает (номер интерфейса, адрес точки).
fn find_kbd_iface(cfg: &[u8]) -> Option<(u8, u8)> {
    let mut off = 0usize;
    let mut iface: Option<u8> = None; // номер текущего HID-boot интерфейса
    while off + 2 <= cfg.len() {
        let len = cfg[off] as usize;
        let kind = cfg[off + 1];
        if len < 2 || off + len > cfg.len() {
            break;
        }
        match kind {
            0x04 => {
                // Interface descriptor: класс 3 (HID), подкласс 1 (boot).
                iface = if len >= 9 && cfg[off + 5] == 0x03 && cfg[off + 6] == 0x01 {
                    Some(cfg[off + 2])
                } else {
                    None
                };
            }
            0x05 => {
                // Endpoint descriptor: ищем первую interrupt-IN.
                if let Some(num) = iface {
                    if len >= 7 && cfg[off + 2] & 0x80 != 0 && cfg[off + 3] & 0x03 == 0x03 {
                        return Some((num, cfg[off + 2]));
                    }
                }
            }
            _ => {}
        }
        off += len;
    }
    None
}

// ─── HID: отчёты и перевод в сканкоды ────────────────────────────────────
/// Соответствие HID usage (страница Keyboard 0x07) → сканкод набора 1.
const HID_TO_SCAN: [(u8, u8); 53] = [
    (0x04, 0x1E), // a
    (0x05, 0x30), // b
    (0x06, 0x2E), // c
    (0x07, 0x20), // d
    (0x08, 0x12), // e
    (0x09, 0x21), // f
    (0x0A, 0x22), // g
    (0x0B, 0x23), // h
    (0x0C, 0x17), // i
    (0x0D, 0x24), // j
    (0x0E, 0x25), // k
    (0x0F, 0x26), // l
    (0x10, 0x32), // m
    (0x11, 0x31), // n
    (0x12, 0x18), // o
    (0x13, 0x19), // p
    (0x14, 0x10), // q
    (0x15, 0x13), // r
    (0x16, 0x1F), // s
    (0x17, 0x14), // t
    (0x18, 0x16), // u
    (0x19, 0x2F), // v
    (0x1A, 0x11), // w
    (0x1B, 0x2D), // x
    (0x1C, 0x15), // y
    (0x1D, 0x2C), // z
    (0x1E, 0x02), // 1
    (0x1F, 0x03), // 2
    (0x20, 0x04), // 3
    (0x21, 0x05), // 4
    (0x22, 0x06), // 5
    (0x23, 0x07), // 6
    (0x24, 0x08), // 7
    (0x25, 0x09), // 8
    (0x26, 0x0A), // 9
    (0x27, 0x0B), // 0
    (0x28, 0x1C), // Enter
    (0x29, 0x01), // Esc
    (0x2A, 0x0E), // Backspace
    (0x2B, 0x0F), // Tab
    (0x2C, 0x39), // Space
    (0x2D, 0x0C), // -
    (0x2E, 0x0D), // =
    (0x2F, 0x1A), // [
    (0x30, 0x1B), // ]
    (0x31, 0x2B), // \
    (0x33, 0x27), // ;
    (0x34, 0x28), // '
    (0x35, 0x29), // `
    (0x36, 0x33), // ,
    (0x37, 0x34), // .
    (0x38, 0x35), // /
    (0x39, 0x3A), // CapsLock
];

/// Клавиши вне основной таблицы: F1..F12, цифровой блок, стрелки, Edit.
/// Второй элемент — нужен ли префикс E0.
fn extended_scan(usage: u8) -> Option<(u8, bool)> {
    let v = match usage {
        0x3A..=0x45 => (0x3B + (usage - 0x3A), false), // F1..F12
        0x46 => (0x37, false),                         // PrintScreen
        0x47 => (0x46, false),                         // ScrollLock
        0x48 => (0x45, false),                         // Pause
        0x49 => (0x52, true),                          // Insert
        0x4A => (0x47, true),                          // Home
        0x4B => (0x49, true),                          // PageUp
        0x4C => (0x53, true),                          // Delete
        0x4D => (0x4F, true),                          // End
        0x4E => (0x51, true),                          // PageDown
        0x4F => (0x4D, true),                          // Right
        0x50 => (0x4B, true),                          // Left
        0x51 => (0x50, true),                          // Down
        0x52 => (0x48, true),                          // Up
        0x53 => (0x45, false),                         // NumLock
        0x54 => (0x35, true),                          // KP /
        0x55 => (0x37, false),                         // KP *
        0x56 => (0x4A, false),                         // KP -
        0x57 => (0x4E, false),                         // KP +
        0x58 => (0x1C, true),                          // KP Enter
        0x59..=0x61 => (0x4F + (usage - 0x59), false), // KP 1..9
        0x62 => (0x52, false),                         // KP 0
        0x63 => (0x53, false),                         // KP .
        _ => return None,
    };
    Some(v)
}

fn scan_for(usage: u8) -> Option<(u8, bool)> {
    for &(u, sc) in HID_TO_SCAN.iter() {
        if u == usage {
            return Some((sc, false));
        }
    }
    extended_scan(usage)
}

fn push_key(sc: u8, e0: bool) {
    if e0 {
        crate::keyboard::push_scancode_usb(0xE0);
    }
    crate::keyboard::push_scancode_usb(sc);
}

/// Разбирает очередной отчёт: нажатые клавиши шлём как make, исчезнувшие — как
/// break. Отчёт без изменений игнорируем (HID шлёт их и по таймеру).
fn handle_report(rep: &[u8; 8]) {
    // Модификаторы: HID отдаёт их битовой маской, PS/2 — отдельными
    // сканкодами. Разворачиваем в make/break, чтобы шелл видел привычное.
    const MOD_BITS: [(u8, u8, bool); 8] = [
        (1 << 0, 0x1D, false), // Left Ctrl
        (1 << 1, 0x2A, false), // Left Shift
        (1 << 2, 0x38, false), // Left Alt
        (1 << 3, 0x5B, true),  // Left GUI
        (1 << 4, 0x1D, true),  // Right Ctrl
        (1 << 5, 0x36, false), // Right Shift
        (1 << 6, 0x38, true),  // Right Alt
        (1 << 7, 0x5B, true),  // Right GUI
    ];
    let mods = rep[0];
    unsafe {
        for &(bit, sc, e0) in MOD_BITS.iter() {
            let was = MODS & bit != 0;
            let now = mods & bit != 0;
            if was == now {
                continue;
            }
            push_key(sc, e0);
            if !now {
                crate::keyboard::push_scancode_usb(sc | 0x80);
            }
        }
        MODS = mods;

        for i in 0..6 {
            let cur = rep[i + 2];
            let prev = HELD[i];
            if cur == prev {
                continue;
            }
            // Слот сменился: если клавиша исчезла — это отпускание (break),
            // если появилась — нажатие (make). Если в слоте сразу другая
            // клавиша, сначала отпускаем старую, потом нажимаем новую.
            if prev != 0 {
                if let Some((sc, _)) = scan_for(prev) {
                    crate::keyboard::push_scancode_usb(sc | 0x80);
                }
            }
            if cur != 0 {
                if let Some((sc, e0)) = scan_for(cur) {
                    push_key(sc, e0);
                }
            }
            HELD[i] = cur;
        }
        REPORTS += 1;
    }
}

/// Готовит interrupt-IN точку на приём отчётов.
unsafe fn arm_in() {
    let base = core::ptr::addr_of!(DMA) as *const u8;
    let style = STYLE;
    let qh = core::ptr::addr_of_mut!(DMA.qh_in);
    let td = core::ptr::addr_of_mut!(DMA.td_in);
    let rep = core::ptr::addr_of_mut!(DMA.report) as *mut u8;
    for i in 0..8 {
        core::ptr::write_volatile(rep.add(i), 0);
    }
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*td).link), style.term());
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*td).ctrl), TD_CTRL_ACTIVE);
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*td).token),
        token(PID_IN, DEV_ADDR, EP_IN & 0x0F, 8),
    );
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*td).data), phys(rep as *const u8));
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*qh).link), QEMU_LINK_T);
    core::ptr::write_volatile(
        core::ptr::addr_of_mut!((*qh).el_link),
        style.active(phys_field(base, td)),
    );
}

/// Забирает один отчёт, если он есть. Не блокирует.
unsafe fn poll_once() -> bool {
    let style = STYLE;
    let qh = core::ptr::addr_of!(DMA.qh_in);
    let el = core::ptr::read_volatile(core::ptr::addr_of!((*qh).el_link));
    if !style.is_idle(el) {
        return false; // точка ещё занята: отчёта нет
    }
    let td = core::ptr::addr_of!(DMA.td_in);
    let failed = core::ptr::read_volatile(core::ptr::addr_of!((*td).ctrl)) & TD_CTRL_ERROR != 0;
    let rep_buf = core::ptr::addr_of!(DMA.report) as *mut u8;
    let mut rep = [0u8; 8];
    for i in 0..8 {
        rep[i] = core::ptr::read_volatile(rep_buf.add(i));
    }
    arm_in();
    if failed {
        diag("hid interrupt endpoint reported an error");
        return true;
    }
    handle_report(&rep);
    true
}

// ─── Внешний интерфейс ───────────────────────────────────────────────────
/// USB-клавиатура найдена и забирает ввод на себя.
pub fn owns_keyboard() -> bool {
    OWNED.load(Ordering::Relaxed)
}

/// Сколько HID-отчётов принято (для диагностики: 0 = USB-клавиатуры нет).
pub fn reports() -> u64 {
    unsafe { REPORTS }
}

/// Обслуживает один порт: сброс, адрес, дескрипторы, HID boot protocol.
/// Возвращает адрес interrupt-IN точки при успехе.
unsafe fn probe_port(port: u16) -> Option<u8> {
    if !port_reset(port) {
        return None;
    }
    // Первым запросом по спецификации идёт SET_ADDRESS: после сброса порта
    // устройство слушает только адрес 0, и всё остальное до присвоения
    // адреса оно игнорирует (а многие просто зависают на этом).
    DEV_ADDR = 1;
    if !set_address(0, DEV_ADDR) {
        diag("SET_ADDRESS failed");
        return None;
    }
    // После SET_ADDRESS устройство сбрасывает адрес на 0 и минуту «думает»:
    // все запросы в это время не проходят.
    crate::scheduler::sleep_until(crate::scheduler::ticks() + 30);

    let mut dev_desc = [0u8; 18];
    if !read_device_descriptor(DEV_ADDR, &mut dev_desc) {
        diag("GET_DESCRIPTOR device failed");
        return None;
    }
    if dev_desc[7] as usize != 8 {
        return None; // у полноскоростной клавиатуры ep0 всегда 8 байт
    }

    let mut cfg = [0u8; 256];
    if !read_config_descriptor(DEV_ADDR, &mut cfg) {
        diag("GET_DESCRIPTOR config failed");
        return None;
    }
    let (iface, ep) = match find_kbd_iface(&cfg) {
        Some(v) => v,
        None => {
            diag_u("  cfg.len", cfg[0] as u64);
            return None;
        }
    };

    // Boot protocol: без него устройство шлёт отчёты по HID-протоколу с
    // использованием report ID, и разбор становится другим.
    let mut setup = [0u8; 8];
    setup[0] = 0x21; // OUT, класс, интерфейс
    setup[1] = 0x09; // SET_PROTOCOL
    setup[2] = 0; // protocol = boot (0)
    setup[4] = iface;
    if !control(DEV_ADDR, setup, &mut [], false) {
        diag("SET_PROTOCOL failed");
        return None;
    }
    EP_IN = ep;
    Some(ep)
}

unsafe fn init() -> bool {
    IO_BASE = match find_uhci() {
        Some(io) => io,
        None => {
            diag("no UHCI controller");
            return false;
        }
    };
    diag_u("uhci io", IO_BASE as u64);
    if !controller_start() {
        return false;
    }

    // Подбор разметки ссылок: сначала пробуем «как в QEMU», при неудаче
    // переходим на «как в спецификации». Перебор виден в диагностике.
    let mut found = None;
    for style in [Style::Qemu, Style::Spec] {
        STYLE = style;
        for p in 0..PORTS {
            if let Some(ep) = probe_port(p) {
                found = Some(ep);
                break;
            }
        }
        if found.is_some() {
            crate::vga::serial_write_atomic("[U] link style=");
            crate::vga::serial_write_atomic(style.name());
            crate::vga::serial_putchar(b'\n');
            break;
        }
        diag("style did not work, retrying");
    }

    let ep = match found {
        Some(e) => e,
        None => {
            diag("no HID boot keyboard on UHCI");
            return false;
        }
    };
    crate::vga::serial_write_atomic("[U] hid boot keyboard ep=");
    crate::vga::serial_u64(ep as u64);
    crate::vga::serial_putchar(b'\n');

    arm_in();
    OWNED.store(true, Ordering::SeqCst);
    true
}

/// Задача USB-клавиатуры: подъём контроллера и опрос точки IN.
pub fn run() {
    unsafe {
        if init() {
            let mut last = 0u64;
            loop {
                crate::scheduler::sleep_until(crate::scheduler::ticks() + 2);
                poll_once();
                // В отладочной сборке — раз в секунду: на настоящем железе
                // это единственный способ понять, доходят ли отчёты.
                #[cfg(debug_assertions)]
                if REPORTS != last {
                    last = REPORTS;
                    diag_u("reports", REPORTS);
                }
            }
        } else {
            // Контроллер не нашёлся или клавиатуры нет: задача спит, чтобы
            // не крутить планировщик вхолостую. 8042 продолжает работать.
            loop {
                crate::scheduler::sleep_until(crate::scheduler::ticks() + 100);
            }
        }
    }
}