//! Сетевой стек: polled-драйвер Realtek rtl8139 + микро-стек ARP/IPv4/ICMP.
//!
//! Первый милстоун — `ping 10.0.2.2` под QEMU user-net (`-netdev user,id=n0
//! -device rtl8139,netdev=n0`). Регистры, биты и семантика TX/RX выверены по
//! исходнику эмуляции QEMU (hw/net/rtl8139.c):
//!
//! - BAR0 (I/O space) берём из PCI-конфигурации (см. `crate::pci`);
//! - RX: кольцо 8 КиБ с запасом 16 байт, RBSTART = физический адрес буфера.
//!   CAPR у QEMU «off by 16»: пишем (rx_cur - 16), чтобы внутренний указатель
//!   совпадал с позицией записи чипа (тогда avail == 0 — кольцо «пусто», и
//!   эмуляция не считает первое принятое кольцо переполненным);
//! - TX: пишем TxAddr[i], затем TxStatus[i] = длина — запись в TSAD служит
//!   триггером передачи (QEMU передаёт синхронно). Бит 13 (TxHostOwns =
//!   0x2000) ставить НЕЛЬЗЯ: с ним эмуляция отказывается передавать.
//!   Завершение — когда чип выставит TxHostOwns;
//! - прерывания не используем (IMR = 0), приём опросом из shell-команды
//!   `ping`. Реентерабельности нет: единственный потребитель — шелл.

use crate::memory::PHYS_MEM_OFFSET;
use x86_64::instructions::port::Port;

// ─── Смещения регистров от I/O base ─────────────────────────────────────────
const REG_IDR0: u16 = 0x00; // MAC, 6 байт
const REG_TX_STATUS0: u16 = 0x10;
const REG_TX_ADDR0: u16 = 0x20;
const REG_RBSTART: u16 = 0x30;
const REG_CHIP_CMD: u16 = 0x37;
const REG_CAPR: u16 = 0x38; // RxBufPtr: «off by 16»
const REG_CBR: u16 = 0x3A; // RxBufAddr: позиция записи чипа
const REG_IMR: u16 = 0x3C;
const REG_ISR: u16 = 0x3E;
const REG_RX_CONFIG: u16 = 0x44;

const CMD_RESET: u8 = 0x10;
const CMD_RX_ENB: u8 = 0x08;
const CMD_TX_ENB: u8 = 0x04;

// RxConfig (биты из QEMU enum rx_mode_bits)
const RCR_ACCEPT_MY_PHYS: u32 = 0x02;
const RCR_ACCEPT_MULTICAST: u32 = 0x04;
const RCR_ACCEPT_BROADCAST: u32 = 0x08;

// TxStatus: эмуляция ставит TxHostOwns, когда забрала дескриптор
const TX_HOST_OWNS: u32 = 0x2000;
const TX_STAT_OK: u32 = 0x8000;

// Поле статуса в заголовке кадра RX-кольца (младшее слово)
const RX_STATUS_OK: u32 = 0x0001;

const RX_RING_SIZE: usize = 8192;
const ETH_MIN: usize = 60;
const ICMP_ID: u16 = 0x1234;
const ICMP_SEQ: u16 = 0x0001;

/// Наш адрес в user-net QEMU (10.0.2.0/24, шлюз 10.0.2.2).
pub const OWN_IP: [u8; 4] = [10, 0, 2, 15];

// ─── DMA-буферы в .bss ядра (физический адрес < 1 ГиБ, окно PHYS_MEM_OFFSET) ─
#[repr(align(16))]
struct RxBuf([u8; RX_RING_SIZE + 16]);

static mut RX_RING: RxBuf = RxBuf([0; RX_RING_SIZE + 16]);
static mut TX_BUF: [u8; 1600] = [0; 1600];

static mut IO_BASE: u16 = 0;
static mut MAC: [u8; 6] = [0; 6];
static mut RX_CUR: usize = 0;
static mut INITIALIZED: bool = false;
static mut NEXT_TX_DESC: u32 = 0;

// Флаги ответов ping-сессии (живут в shell-потоке, конкурентности нет)
static mut GOT_ARP: bool = false;
static mut GOT_ARP_MAC: [u8; 6] = [0; 6];
static mut GOT_ICMP: bool = false;
static mut ICMP_SRC: [u8; 4] = [0; 4];

// ─── Доступ к портам чипа ───────────────────────────────────────────────────
unsafe fn inb(reg: u16) -> u8 {
    Port::<u8>::new(IO_BASE + reg).read()
}
unsafe fn inw(reg: u16) -> u16 {
    Port::<u16>::new(IO_BASE + reg).read()
}
unsafe fn inl(reg: u16) -> u32 {
    Port::<u32>::new(IO_BASE + reg).read()
}
unsafe fn outb(reg: u16, val: u8) {
    Port::<u8>::new(IO_BASE + reg).write(val);
}
unsafe fn outw(reg: u16, val: u16) {
    Port::<u16>::new(IO_BASE + reg).write(val);
}
unsafe fn outl(reg: u16, val: u32) {
    Port::<u32>::new(IO_BASE + reg).write(val);
}

/// Сбрасывает строки кэша в память: публикует TX-буфер перед «DMA-чтением»
/// эмулятора и инвалидирует RX-кольцо перед чтением данных, записанных QEMU
/// (под KVM запись эмулятора в guest-RAM не трогает кэш vCPU).
fn clflush_range(addr: usize, len: usize) {
    let mut a = addr;
    let end = addr.wrapping_add(len);
    while a < end {
        unsafe {
            core::arch::asm!("clflush [{}]", in(reg) a, options(nostack, preserves_flags));
        }
        a += 64;
    }
}

/// RFC 1071: ones-complement контрольная сумма (IP/ICMP).
fn checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Ищет rtl8139 на PCI (vendor 0x10EC / device 0x8139) и настраивает чип.
/// Повторные вызовы — no-op; если карты нет (или BAR0 не I/O) — false.
pub fn init() -> bool {
    unsafe {
        if INITIALIZED {
            return IO_BASE != 0;
        }
        let Some(dev) = crate::pci::find_device(0x10EC, 0x8139) else {
            return false;
        };
        if dev.bar0 & 1 == 0 {
            return false; // BAR0 — не I/O space, этот драйвер такое не берёт
        }
        // Bus master обязателен: QEMU 11.x проводит PCI DMA (и TX, и RX) через
        // пер-устройственное адресное пространство, чей алиас на системную
        // память включается ТОЛЬКО битом 2 (PCI_COMMAND_MASTER) в COMMAND.
        // Без него все DMA-чтения возвращают MEMTX_DECODE_ERROR, и кадры
        // уходят нулями. Ставим I/O | Memory | BusMaster (0x07).
        let cmd = crate::pci::read_u16(dev.bus, dev.slot, dev.func, 0x04);
        crate::pci::write_u16(dev.bus, dev.slot, dev.func, 0x04, cmd | 0x0007);
        IO_BASE = (dev.bar0 & 0xFFFC) as u16;
        INITIALIZED = true;
        for i in 0..6 {
            MAC[i] = inb(REG_IDR0 + i as u16);
        }
        rtl_hw_init();
        true
    }
}

/// Последовательность конфигурации чипа (выверена по hw/net/rtl8139.c).
unsafe fn rtl_hw_init() {
    // 1) Аппаратный сброс.
    outb(REG_CHIP_CMD, CMD_RESET);
    let mut spins = 0;
    while (inb(REG_CHIP_CMD) & CMD_RESET) != 0 && spins < 1000 {
        spins += 1;
    }

    // 2) Кольцо RX: физический адрес буфера.
    RX_CUR = 0;
    let rx_phys = (RX_RING.0.as_ptr() as u64 - PHYS_MEM_OFFSET) as u32;
    outl(REG_RBSTART, rx_phys);

    // 3) Приём: свой unicast + broadcast + multicast, кольцо 8 КиБ.
    outl(
        REG_RX_CONFIG,
        RCR_ACCEPT_MY_PHYS | RCR_ACCEPT_BROADCAST | RCR_ACCEPT_MULTICAST,
    );

    // 4) CAPR «off by 16»: пишем -16, чтобы внутренний указатель стал 0 и
    //    совпал с позицией записи чипа (avail == 0 = кольцо пусто).
    outw(REG_CAPR, 0xFFF0);

    // 5) Прерывания не используем (polled): маска 0, статус сброшен.
    outw(REG_ISR, 0xFFFF);
    outw(REG_IMR, 0x0000);

    // 6) Включить приём и передачу.
    outb(REG_CHIP_CMD, CMD_RX_ENB | CMD_TX_ENB);
}

/// Собирает Ethernet-кадр в TX_BUF: dst MAC + наш MAC + ethertype + payload.
/// Паддит до минимума 60 байт. Возвращает длину кадра.
unsafe fn build_eth(dst: &[u8; 6], ethertype: u16, payload: &[u8]) -> usize {
    let n = payload.len() + 14;
    let padded = n.max(ETH_MIN);
    TX_BUF[..6].copy_from_slice(dst);
    TX_BUF[6..12].copy_from_slice(&MAC);
    TX_BUF[12..14].copy_from_slice(&ethertype.to_be_bytes());
    TX_BUF[14..n].copy_from_slice(payload);
    padded
}

/// Отправляет кадр TX_BUF[..len] через «текущий» дескриптор. QEMU (в отличие от
/// железа) передаёт ТОЛЬКО `currTxDesc` и после удачной передачи переходит к
/// следующему дескриптору — поэтому драйвер ведёт свой счётчик и пишет в свой
/// TSAD. Ждём TxHostOwns именно на своём дескрипторе.
unsafe fn tx_send(len: usize) -> bool {
    // Публикуем данные в гостевой памяти перед «DMA-чтением» эмулятора.
    let buf = TX_BUF.as_ptr();
    clflush_range(buf as usize, len);
    let desc = NEXT_TX_DESC % 4;
    let phys = (buf as u64 - PHYS_MEM_OFFSET) as u32;
    // TxAddr-регистр для этого дескриптора.
    outl(REG_TX_ADDR0 + 4 * desc as u16, phys);
    // Запись длины в свой TSAD — триггер передачи (QEMU передаёт синхронно).
    outl(REG_TX_STATUS0 + 4 * desc as u16, len as u32);
    let mut spins = 0;
    while (inl(REG_TX_STATUS0 + 4 * desc as u16) & TX_HOST_OWNS) == 0 {
        spins += 1;
        if spins > 1_000_000 {
            return false;
        }
    }
    let ts = inl(REG_TX_STATUS0 + 4 * desc as u16);
    if (ts & TX_STAT_OK) != 0 {
        NEXT_TX_DESC += 1;
        true
    } else {
        false
    }
}

/// ARP-запрос «кто имеет target_ip?» (broadcast).
fn arp_request(target_ip: &[u8; 4]) -> bool {
    unsafe {
        let mut pkt = [0u8; 28];
        pkt[0..2].copy_from_slice(&1u16.to_be_bytes()); // htype = Ethernet
        pkt[2..4].copy_from_slice(&0x0800u16.to_be_bytes()); // ptype = IPv4
        pkt[4] = 6; // hlen
        pkt[5] = 4; // plen
        pkt[6..8].copy_from_slice(&1u16.to_be_bytes()); // oper = request
        pkt[8..14].copy_from_slice(&MAC);
        pkt[14..18].copy_from_slice(&OWN_IP);
        pkt[24..28].copy_from_slice(target_ip);
        let len = build_eth(&[0xFF; 6], 0x0806, &pkt);
        tx_send(len)
    }
}

/// ICMP echo-request на target_mac / target_ip.
fn icmp_echo(target_mac: &[u8; 6], target_ip: &[u8; 4]) -> bool {
    unsafe {
        // ICMP: type=8, code=0, checksum, id, seq, payload 4 байта
        let mut icmp = [0u8; 12];
        icmp[0] = 8; // echo request
        icmp[4..6].copy_from_slice(&ICMP_ID.to_be_bytes());
        icmp[6..8].copy_from_slice(&ICMP_SEQ.to_be_bytes());
        icmp[8..12].copy_from_slice(&[0x56, 0x49, 0x54, 0x41]); // "VITA"
        let icmp_sum = checksum(&icmp).to_be_bytes();
        icmp[2..4].copy_from_slice(&icmp_sum);

        // IPv4: 45 00 len id 00 00 40 01 cksum src dst
        let mut ip = [0u8; 20];
        ip[0] = 0x45;
        ip[2..4].copy_from_slice(&(20u16 + icmp.len() as u16).to_be_bytes());
        ip[4..6].copy_from_slice(&0x0001u16.to_be_bytes()); // id
        ip[8] = 64; // TTL
        ip[9] = 1; // ICMP
        ip[12..16].copy_from_slice(&OWN_IP);
        ip[16..20].copy_from_slice(target_ip);
        let ip_sum = checksum(&ip).to_be_bytes();
        ip[10..12].copy_from_slice(&ip_sum);

        let mut payload = [0u8; 20 + 12];
        payload[..20].copy_from_slice(&ip);
        payload[20..].copy_from_slice(&icmp);
        let len = build_eth(target_mac, 0x0800, &payload);
        tx_send(len)
    }
}

/// Опрашивает кольцо RX, диспетчеризует новые кадры. Возвращает их число.
fn rx_poll() -> u32 {
    unsafe {
        // Позиция записи чипа — через порт (кэш не участвует). CBR НЕ «off by 16».
        let cbr = inw(REG_CBR) as usize;
        let new = (cbr as isize - RX_CUR as isize).rem_euclid(RX_RING_SIZE as isize) as usize;
        if new == 0 {
            return 0; // новых данных нет
        }
        // Инвалидируем кэш кольца: данные писал эмулятор (DMA).
        clflush_range(RX_RING.0.as_ptr() as usize, RX_RING_SIZE + 16);

        let mut count = 0;
        loop {
            let off = RX_CUR;
            let hdr = u32::from_le_bytes(RX_RING.0[off..off + 4].try_into().unwrap());
            if (hdr & RX_STATUS_OK) == 0 {
                break; // свежих пакетов больше нет
            }
            let size_field = ((hdr >> 16) & 0x3FFF) as usize;
            if !(4..=2048).contains(&size_field) {
                break; // битый заголовок: длину не знаем — не продвигаемся
            }
            let payload_len = size_field - 4; // минус CRC, который доливает чип
            let frame = &RX_RING.0[off + 4..off + 4 + payload_len];
            handle_frame(frame);
            // 4 (заголовок) + payload + 4 (CRC) = size_field + 4, выравнено по 4
            RX_CUR = (off + size_field + 4 + 3) & !3;
            if RX_CUR >= RX_RING_SIZE + 16 {
                RX_CUR -= RX_RING_SIZE;
            }
            // CAPR «off by 16»: внутренний указатель чипа = RX_CUR.
            outw(REG_CAPR, (RX_CUR as u16).wrapping_sub(16));
            count += 1;
            if count > 64 {
                break;
            }
        }
        count
    }
}

/// Разбирает Ethernet-кадр и диспетчеризует по ethertype.
fn handle_frame(frame: &[u8]) {
    if frame.len() < 14 {
        return;
    }
    let etype = u16::from_be_bytes([frame[12], frame[13]]);
    match etype {
        0x0806 => handle_arp(&frame[14..]),
        0x0800 => handle_ipv4(&frame[14..]),
        _ => {}
    }
}

/// ARP-ответ: фиксируем MAC отправителя (sha) цели.
fn handle_arp(pkt: &[u8]) {
    unsafe {
        if pkt.len() < 28 {
            return;
        }
        let oper = u16::from_be_bytes([pkt[6], pkt[7]]);
        if oper != 2 {
            return; // не reply
        }
        // Отвечают именно на наш запрос: tha == наш MAC, tpa == наш IP.
        if pkt[18..24] != MAC {
            return;
        }
        if pkt[24..28] != OWN_IP {
            return;
        }
        GOT_ARP = true;
        GOT_ARP_MAC.copy_from_slice(&pkt[8..14]); // sha
    }
}

/// IPv4 + ICMP echo-reply нам навстречу.
fn handle_ipv4(pkt: &[u8]) {
    unsafe {
        if pkt.len() < 20 {
            return;
        }
        if pkt[0] >> 4 != 4 {
            return;
        }
        let ihl = (pkt[0] & 0x0F) as usize * 4;
        if pkt.len() < ihl + 8 || pkt[9] != 1 {
            return; // не ICMP
        }
        if pkt[16..20] != OWN_IP {
            return; // не нам
        }
        let icmp = &pkt[ihl..];
        if icmp[0] != 0 {
            return; // не echo-reply
        }
        if icmp[4..6] != ICMP_ID.to_be_bytes() {
            return;
        }
        GOT_ICMP = true;
        ICMP_SRC.copy_from_slice(&pkt[12..16]);
    }
}

/// «a.b.c.d» -> 4 байта.
fn parse_ip(s: &str) -> Option<[u8; 4]> {
    let mut octets = [0u8; 4];
    let mut idx = 0usize;
    for part in s.split('.') {
        if idx >= 4 {
            return None;
        }
        let v: u16 = part.parse().ok()?;
        if v > 255 {
            return None;
        }
        octets[idx] = v as u8;
        idx += 1;
    }
    if idx == 4 {
        Some(octets)
    } else {
        None
    }
}

/// «ping <a.b.c.d>»: ARP-резолв цели, затем ICMP echo. На каждом этапе ждём
/// до 2 c (200 тиков PIT 100 Гц), между опросами кольца отдаём CPU.
pub fn ping(writer: &mut crate::vga::Writer, addr: &str) {
    let Some(ip) = parse_ip(addr) else {
        writer.write_string("ping: bad address: ");
        writer.write_string(addr);
        writer.write_string("\n");
        return;
    };
    if !init() {
        writer.write_string("ping: no network card (rtl8139 not found)\n");
        return;
    }

    writer.write_string("ping ");
    writer.write_string(addr);
    writer.write_string("\n");

    let t0 = crate::scheduler::ticks();

    // Этап 1: ARP-запрос и ожидание ответа.
    unsafe {
        GOT_ARP = false;
    }
    if !arp_request(&ip) {
        writer.write_string("ping: transmit failed\n");
        return;
    }
    let deadline = crate::scheduler::ticks() + 200;
    while crate::scheduler::ticks() < deadline {
        rx_poll();
        if unsafe { GOT_ARP } {
            break;
        }
        crate::scheduler::sleep_until(crate::scheduler::ticks() + 1);
    }
    if !unsafe { GOT_ARP } {
        writer.write_string("ping: ARP timeout for ");
        writer.write_string(addr);
        writer.write_string("\n");
        return;
    }
    let dst_mac = unsafe { GOT_ARP_MAC };
    let mac_msg = alloc::format!(
        "  arp: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n",
        dst_mac[0], dst_mac[1], dst_mac[2], dst_mac[3], dst_mac[4], dst_mac[5]
    );
    writer.write_string(&mac_msg);

    // Этап 2: ICMP echo-request и ожидание reply.
    unsafe {
        GOT_ICMP = false;
    }
    if !icmp_echo(&dst_mac, &ip) {
        writer.write_string("ping: transmit failed\n");
        return;
    }
    let deadline = crate::scheduler::ticks() + 200;
    while crate::scheduler::ticks() < deadline {
        rx_poll();
        if unsafe { GOT_ICMP } {
            break;
        }
        crate::scheduler::sleep_until(crate::scheduler::ticks() + 1);
    }
    if !unsafe { GOT_ICMP } {
        writer.write_string("ping: no ICMP reply from ");
        writer.write_string(addr);
        writer.write_string("\n");
        return;
    }
    let src = unsafe { ICMP_SRC };
    let dt = crate::scheduler::ticks().saturating_sub(t0);
    let reply_msg = alloc::format!(
        "reply from {}.{}.{}.{} ({} ticks)\n",
        src[0],
        src[1],
        src[2],
        src[3],
        dt
    );
    writer.write_string(&reply_msg);
}