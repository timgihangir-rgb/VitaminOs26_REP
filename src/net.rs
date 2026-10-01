//! Сетевой стек: polled-драйвер Realtek rtl8139 + микро-стек ARP/IPv4/ICMP
//! + TCP-клиент (один активный сокет, ECMA-стиль: строго in-order приём
//! с выбросом дубликатов/пропусков — пир ретрамитит сам, мы только ACKим).
//!
//! TCP живёт в статике и продвигается каждым вызовом ABI-функций
//! (net_connect/net_send/net_recv/net_close), которые делают rx_poll():
//! сисколы НЕ блокируют диспетчер, программа сама крутит цикл
//! connect/recv со `sleep`-отпуском CPU до получения EAGAIN-результата.
//! Результаты ABI: 0 = ок, NET_EAGAIN(-2) = ещё в процессе/нет данных,
//! NET_EOF(-1) = данные закончились (FIN получен), NET_ERR(-3) = ошибка/RST.
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
const ICMP_ID: u16 = 0x1234;
const ICMP_SEQ: u16 = 0x0001;

/// Наш адрес в user-net QEMU (10.0.2.0/24, шлюз 10.0.2.2).
pub const OWN_IP: [u8; 4] = [10, 0, 2, 15];

// ─── DMA-буферы в .bss ядра (физический адрес < 1 ГиБ, окно PHYS_MEM_OFFSET) ─
#[repr(align(16))]
struct RxBuf([u8; RX_RING_SIZE + 16]);

static mut RX_RING: RxBuf = RxBuf([0; RX_RING_SIZE + 16]);
static mut TX_BUF: [u8; 1600] = [0; 1600];
/// Сборка кадра, перенесённого чипом через конец кольца (см. rx_poll).
static mut RX_FRAME: [u8; 2048] = [0; 2048];

static mut IO_BASE: u16 = 0;
static mut MAC: [u8; 6] = [0; 6];
static mut RX_CUR: usize = 0;
static mut INITIALIZED: bool = false;
static mut NEXT_TX_DESC: u32 = 0;

// Флаги ответов ping-сессии (живут в shell-потоке, конкурентности нет)
static mut GOT_ICMP: bool = false;
static mut ICMP_SRC: [u8; 4] = [0; 4];

// ─── ARP-кэш и маршрутизация ────────────────────────────────────────────────
/// Шлюз user-net QEMU: весь трафик наружу (slirp) идёт через него.
const GATEWAY: [u8; 4] = [10, 0, 2, 2];
/// Сеть, в которой живут сам гость, шлюз и DNS — ARP отвечают напрямую.
const LOCAL_NET: [u8; 3] = [10, 0, 2];
const ARP_SLOTS: usize = 4;
const ARP_TTL: u64 = 500; // 5 c при 100 Гц PIT
static mut ARP_IP: [[u8; 4]; ARP_SLOTS] = [[0; 4]; ARP_SLOTS];
static mut ARP_MAC: [[u8; 6]; ARP_SLOTS] = [[0; 6]; ARP_SLOTS];
static mut ARP_TICK: [u64; ARP_SLOTS] = [0; ARP_SLOTS];
/// Кто владеет mac для ip: сам ip в нашей сети либо шлюз.
fn arp_next_hop(ip: &[u8; 4]) -> [u8; 4] {
    if ip[..3] == LOCAL_NET {
        *ip
    } else {
        GATEWAY
    }
}

/// Свежая запись кэша для ip (0 = записи нет).
unsafe fn arp_lookup(ip: &[u8; 4]) -> [u8; 6] {
    let now = crate::scheduler::ticks();
    for i in 0..ARP_SLOTS {
        if ARP_IP[i] == *ip && now.saturating_sub(ARP_TICK[i]) < ARP_TTL {
            return ARP_MAC[i];
        }
    }
    [0; 6]
}

/// Запомнить развязку из ARP-ответа (перезаписывая слот с тем же ip).
unsafe fn arp_learn(ip: &[u8; 4], mac: &[u8; 6]) {
    let now = crate::scheduler::ticks();
    for i in 0..ARP_SLOTS {
        if ARP_IP[i] == *ip && now.saturating_sub(ARP_TICK[i]) < ARP_TTL {
            ARP_MAC[i].copy_from_slice(mac);
            ARP_TICK[i] = now;
            return;
        }
    }
    for i in 0..ARP_SLOTS {
        if ARP_IP[i] != [0; 4] && now.saturating_sub(ARP_TICK[i]) < ARP_TTL {
            continue; // занята живая запись — вытесняем только устаревшую
        }
        ARP_IP[i].copy_from_slice(ip);
        ARP_MAC[i].copy_from_slice(mac);
        ARP_TICK[i] = now;
        return;
    }
    // Кэш полон живых записей: затираем самую старую.
    let mut oldest = 0;
    for i in 1..ARP_SLOTS {
        if ARP_TICK[i] < ARP_TICK[oldest] {
            oldest = i;
        }
    }
    ARP_IP[oldest].copy_from_slice(ip);
    ARP_MAC[oldest].copy_from_slice(mac);
    ARP_TICK[oldest] = now;
}

/// Развязка для ip: из кэша либо ARP-запрос и ожидание ответа.
unsafe fn arp_resolve(ip: &[u8; 4], timeout: u64) -> Option<[u8; 6]> {
    let mac = arp_lookup(ip);
    if mac != [0; 6] {
        return Some(mac);
    }
    if !arp_request(ip) {
        return None;
    }
    let deadline = crate::scheduler::ticks() + timeout;
    while crate::scheduler::ticks() < deadline {
        rx_poll();
        let mac = arp_lookup(ip);
        if mac != [0; 6] {
            return Some(mac);
        }
        crate::scheduler::sleep_until(crate::scheduler::ticks() + 1);
    }
    None
}

// ─── TCP-клиент: единственное активное соединение ───────────────────────────
/// Коды результатов ABI (см. tcp_connect/tcp_recv).
pub const NET_EAGAIN: i32 = -2; // ещё идёт рукопожатие / данных пока нет
pub const NET_EOF: i32 = -1; // конец данных (FIN получен, всё прочитано)
pub const NET_ERR: i32 = -3; // фатальная ошибка (нет карты/RST/не установлено)

/// Буфер принятых данных. Реальные страницы (example.com ~1 КБ, iana.org —
/// десятки КБ) не влезали в прежние 16 КиБ: переполнение молча обрезало хвост
/// документа. 96 КиБ — с запасом на типичную HTML-страницу целиком.
const TCP_RX_CAP: usize = 98304;
const TCP_HS_TIMEOUT: u64 = 400; // 4 c на ARP+SYN-фазу рукопожатия

/// Фазы соединения: 0 — ARP, 1 — SYN послан, 2 — ESTABLISHED, 4 — RST/ошибка.
static mut TCP_ACTIVE: bool = false;
static mut TCP_PHASE: u8 = 0;
static mut TCP_START_TICK: u64 = 0;
static mut TCP_ISN: u32 = 0; // seq нашего SYN
static mut TCP_SEND_SEQ: u32 = 0; // seq следующего отправляемого сегмента
static mut TCP_RECV_NEXT: u32 = 0; // следующий ожидаемый от пира seq (= наш ack)
static mut TCP_DST_IP: [u8; 4] = [0; 4];
static mut TCP_DST_MAC: [u8; 6] = [0; 6];
static mut TCP_DST_PORT: u16 = 0;
static mut TCP_SRC_PORT: u16 = 0;
static mut TCP_RST: bool = false;
static mut TCP_GOT_FIN: bool = false;
static mut TCP_RX: [u8; TCP_RX_CAP] = [0; TCP_RX_CAP];
static mut TCP_RX_LEN: usize = 0; // всего накоплено
static mut TCP_RX_READ: usize = 0; // прочитано приложением

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
/// Возвращает длину кадра. Кадр НЕ паддится до минимума Ethernet: emulated
/// slirp считает длину TCP-данных по длине кадра (а не по полю IP total-length),
/// и байты паддинга читаются ему как «данные» потоков (они двигают rcv_nxt и
/// приводят к отбрасыванию настоящих данных как out-of-order).
unsafe fn build_eth(dst: &[u8; 6], ethertype: u16, payload: &[u8]) -> usize {
    let n = payload.len() + 14;
    TX_BUF[..6].copy_from_slice(dst);
    TX_BUF[6..12].copy_from_slice(&MAC);
    TX_BUF[12..14].copy_from_slice(&ethertype.to_be_bytes());
    TX_BUF[14..n].copy_from_slice(payload);
    n
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
        let mut count = 0;
        loop {
            // Позицию записи чипа перечитываем НА КАЖДОЙ итерации, а не один
            // раз: запись в CAPR вызывает в QEMU `qemu_flush_queued_packets()`,
            // и чип дописывает в кольцо прямо посреди нашего обхода. Со снятым
            // однажды значением мы уезжали за реальную границу и читали остатки
            // прошлой «революции» кольца.
            // CBR (RxBufAddr) НЕ «off by 16» — это позиция записи чипа.
            let cbr = inw(REG_CBR) as usize;
            let new = (cbr as isize - RX_CUR as isize).rem_euclid(RX_RING_SIZE as isize) as usize;
            if new == 0 {
                break; // свежих пакетов больше нет
            }
            // Инвалидируем кэш кольца: данные писал эмулятор (DMA).
            clflush_range(RX_RING.0.as_ptr() as usize, RX_RING_SIZE + 16);

            let off = RX_CUR;
            let hdr = u32::from_le_bytes(RX_RING.0[off..off + 4].try_into().unwrap());
            let size_field = ((hdr >> 16) & 0x3FFF) as usize;
            // Дескриптор без флага «готов», битая длина или кадр длиннее
            // оставшегося места — рассинхронизация с чипом. Раньше здесь был
            // `break` без продвижения RX_CUR, и это убивало сеть НАВСЕГДА:
            // RxBufPtr уезжал вперёд RxBufAddr, `rtl8139_can_receive()` возвращал
            // `avail < 1514` → false навсегда, кольцо больше не принимало пакетов
            // («cannot resolve host» на втором и всех следующих запусках).
            // Лечение: прыгаем указателем чтения на позицию записи чипа.
            if (hdr & RX_STATUS_OK) == 0
                || !(4..=2048).contains(&size_field)
                || size_field + 4 > new
            {
                RX_CUR = cbr;
                outw(REG_CAPR, (RX_CUR as u16).wrapping_sub(16));
                break;
            }
            let payload_len = size_field - 4; // минус CRC, который доливает чип
            if off + 4 + payload_len <= RX_RING_SIZE {
                handle_frame(&RX_RING.0[off + 4..off + 4 + payload_len]);
            } else {
                // Кадр перенесён через конец кольца: чип записал голову в хвост
                // кольца, а остаток — с нуля, но в дескрипторе указана полная
                // длина (см. rtl8139_write_buffer в hw/net/rtl8139.c). Собираем
                // кадр из двух кусков, иначе в стек уйдёт мусор.
                let head = RX_RING_SIZE - (off + 4);
                let tail = payload_len - head;
                if payload_len <= RX_FRAME.len() && tail <= RX_RING_SIZE {
                    RX_FRAME[..head]
                        .copy_from_slice(&RX_RING.0[off + 4..off + 4 + head]);
                    RX_FRAME[head..payload_len].copy_from_slice(&RX_RING.0[..tail]);
                    handle_frame(&RX_FRAME[..payload_len]);
                }
            }
            // Чип выравнивает указатель записи по 4 и приводит по модулю кольца.
            // Занимает кадр 4 (заголовок) + size_field (данные с CRC).
            RX_CUR = (off + 4 + size_field + 3) & !3;
            RX_CUR %= RX_RING_SIZE;
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
        // sha отправителя — его MAC, spa (14..18) — его IP. 18..24 — это tha
        // (наш MAC), туда IP отправителя не лежит.
        let spa: [u8; 4] = pkt[14..18].try_into().unwrap();
        let sha: [u8; 6] = pkt[8..14].try_into().unwrap();
        arp_learn(&spa, &sha);
    }
}

/// IPv4: ICMP echo-reply нам навстречу или TCP для активного соединения.
fn handle_ipv4(pkt: &[u8]) {
    unsafe {
        if pkt.len() < 20 {
            return;
        }
        if pkt[0] >> 4 != 4 {
            return;
        }
        let ihl = (pkt[0] & 0x0F) as usize * 4;
        if pkt.len() < ihl + 8 {
            return;
        }
        if pkt[16..20] != OWN_IP {
            return; // не нам
        }
        // Режем «хвост» кадра по длине IP-датаграммы из заголовка: короткие
        // кадры (< 60 байт) Ethernet дополняет паддингом до минимума, и этот
        // паддинг не должен читаться как данные TCP/ICMP (иначе каждый чистый
        // ACK пира «отдаёт» нам 6 фантомных байт).
        let iplen = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
        if iplen < ihl + 8 {
            return;
        }
        let pkt = if iplen < pkt.len() { &pkt[..iplen] } else { pkt };
        if pkt[9] == 1 {
            let icmp = &pkt[ihl..];
            if icmp[0] != 0 {
                return; // не echo-reply
            }
            if icmp[4..6] != ICMP_ID.to_be_bytes() {
                return;
            }
            GOT_ICMP = true;
            ICMP_SRC.copy_from_slice(&pkt[12..16]);
        } else if pkt[9] == 6 {
            handle_tcp(pkt, ihl);
        } else if pkt[9] == 17 {
            handle_udp(pkt, ihl);
        }
    }
}

/// UDP к нашему DNS-порту: сохраняем датаграмму для резолвера.
fn handle_udp(pkt: &[u8], ihl: usize) {
    unsafe {
        if pkt.len() < ihl + 8 {
            return;
        }
        let udp = &pkt[ihl..];
        let sport = u16::from_be_bytes([udp[0], udp[1]]);
        let dport = u16::from_be_bytes([udp[2], udp[3]]);
        let ulen = u16::from_be_bytes([udp[4], udp[5]]) as usize;
        if dport != DNS_SRC_PORT || sport != DNS_PORT {
            return; // не наш DNS-обмен
        }
        let data = &udp[8..ulen.min(udp.len())];
        let n = data.len().min(DNS_RX_CAP);
        DNS_REPLY[..n].copy_from_slice(&data[..n]);
        DNS_REPLY_LEN = n;
    }
}

// ─── TCP: сборка/отправка сегментов и разбор входящих ───────────────────────

/// Собирает и отправляет Ethernet+IPv4+TCP-сегмент (data — в `body`, которое
/// кладётся сразу после TCP-заголовка: для SYN это опции, для данных —
/// payload). `data_off` — длина TCP-заголовка в 4-байтовых словах
/// (SYN c MSS = 6, обычные сегменты = 5).
unsafe fn tcp_seg(flags: u8, data_off: u8, seq: u32, ack: u32, body: &[u8]) -> bool {
    let hdr = (data_off as usize) * 4;
    let tcp_len = hdr + body.len();
    if tcp_len > 1400 {
        return false;
    }
    let mut seg = [0u8; 1400];
    seg[0..2].copy_from_slice(&TCP_SRC_PORT.to_be_bytes());
    seg[2..4].copy_from_slice(&TCP_DST_PORT.to_be_bytes());
    seg[4..8].copy_from_slice(&seq.to_be_bytes());
    seg[8..12].copy_from_slice(&ack.to_be_bytes());
    seg[12] = data_off << 4;
    seg[13] = flags;
    seg[14..16].copy_from_slice(&0xFFFFu16.to_be_bytes()); // window
    // checksum (16..18) и urgent (18..20) остаются нулевыми до подсчёта.
    // Опции живут ВНУТРИ TCP-заголовка (hdr = 4*data_off). Для SYN c MSS
    // (data_off == 6) пишем опцию в seg[20..24]; иначе заголовок 20 байт.
    if data_off == 6 {
        seg[20..24].copy_from_slice(&[2, 4, 0x05, 0xB4]); // MSS 1460
    }
    seg[hdr..tcp_len].copy_from_slice(body);

    // TCP checksum: псевдозаголовок (src, dst, 0, 6, tcp_len) + сегмент.
    let mut c = [0u8; 12 + 1400];
    c[0..4].copy_from_slice(&OWN_IP);
    c[4..8].copy_from_slice(&TCP_DST_IP);
    c[8] = 0;
    c[9] = 6;
    c[10..12].copy_from_slice(&(tcp_len as u16).to_be_bytes());
    c[12..12 + tcp_len].copy_from_slice(&seg[..tcp_len]);
    let sum = checksum(&c[..12 + tcp_len]);
    seg[16..18].copy_from_slice(&sum.to_be_bytes());

    // IPv4-заголовок.
    let mut ip = [0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&(20u16 + tcp_len as u16).to_be_bytes());
    ip[4..6].copy_from_slice(&0x0002u16.to_be_bytes()); // id
    ip[8] = 64; // TTL
    ip[9] = 6; // TCP
    ip[12..16].copy_from_slice(&OWN_IP);
    ip[16..20].copy_from_slice(&TCP_DST_IP);
    let ip_sum = checksum(&ip);
    ip[10..12].copy_from_slice(&ip_sum.to_be_bytes());

    let mut frame = [0u8; 1420];
    frame[..20].copy_from_slice(&ip);
    frame[20..20 + tcp_len].copy_from_slice(&seg[..tcp_len]);
    let len = build_eth(&TCP_DST_MAC, 0x0800, &frame[..20 + tcp_len]);
    let ok = tx_send(len);

    ok
}

/// Шлёт SYN с опцией MSS 1460 внутри TCP-заголовка (seq = TCP_ISN).
unsafe fn tcp_send_syn() {
    let _ = tcp_seg(0x02, 6, TCP_ISN, 0, &[]);
}

/// Разбор входящего TCP-сегмента единственного активного соединения.
/// Принимаем строго in-order данные (выброс дубликатов/пропусков заставляет
/// пира ретрамитить), ACKим всё принятое; FIN закрывает поток (EOF для
/// приложения).
fn handle_tcp(pkt: &[u8], ihl: usize) {
    unsafe {
        if !TCP_ACTIVE || TCP_PHASE == 4 || pkt.len() < ihl + 20 {
            return;
        }
        let seg = &pkt[ihl..];
        let src_port = u16::from_be_bytes([seg[0], seg[1]]);
        let dst_port = u16::from_be_bytes([seg[2], seg[3]]);
        if src_port != TCP_DST_PORT || dst_port != TCP_SRC_PORT {
            return; // не наш поток
        }
        let seq = u32::from_be_bytes([seg[4], seg[5], seg[6], seg[7]]);
        let ack = u32::from_be_bytes([seg[8], seg[9], seg[10], seg[11]]);
        let off = ((seg[12] >> 4) as usize) * 4;
        if off < 20 || off > seg.len() {
            return;
        }
        let flags = seg[13];
        let syn = flags & 0x02 != 0;
        let ackf = flags & 0x10 != 0;
        let fin = flags & 0x01 != 0;
        let rst = flags & 0x04 != 0;
        let payload = &seg[off..];

        if rst {
            TCP_PHASE = 4; // соединение разорвано пиром (порт закрыт и т.п.)
            return;
        }
        // SYN+ACK на наш SYN: их ack обязан быть ISN+1.
        if syn && TCP_PHASE == 1 {
            if ackf && ack == TCP_ISN.wrapping_add(1) {
                TCP_RECV_NEXT = seq.wrapping_add(1);
                TCP_PHASE = 2;
                let _ = tcp_seg(0x10, 5, TCP_SEND_SEQ, TCP_RECV_NEXT, &[]); // ACK
            }
            return; // SYN-сегменты данных не несут
        }
        if TCP_PHASE != 2 {
            return;
        }

        // Данные строго по порядку. Переполнение приёмного буфера отбрасываем
        // (ACKим пропущенное — пир не будет ретрамитить зря).
        if !payload.is_empty() && seq == TCP_RECV_NEXT {
            let space = TCP_RX_CAP - TCP_RX_LEN;
            let n = payload.len().min(space);
            TCP_RX[TCP_RX_LEN..TCP_RX_LEN + n].copy_from_slice(&payload[..n]);
            TCP_RX_LEN += n;
            TCP_RECV_NEXT = seq.wrapping_add(payload.len() as u32);
            let _ = tcp_seg(0x10, 5, TCP_SEND_SEQ, TCP_RECV_NEXT, &[]); // ACK
        }
        // FIN занимает собственный номер seq (после данных того же сегмента).
        if fin && seq.wrapping_add(payload.len() as u32) == TCP_RECV_NEXT {
            TCP_RECV_NEXT = TCP_RECV_NEXT.wrapping_add(1);
            TCP_GOT_FIN = true;
            let _ = tcp_seg(0x10, 5, TCP_SEND_SEQ, TCP_RECV_NEXT, &[]); // ACK FIN
        }
    }
}

// ─── TCP: ABI-функции (вызываются из syscall.rs, не блокируют CPU) ──────────

/// Инициирует/продвигает соединение с `ip:port`. Возвращает 0 (ESTABLISHED),
/// NET_EAGAIN (звать ещё раз), NET_ERR (таймаут/нет карты/ошибка).
/// Первый вызов стартует сессию (ARP + SYN); повторные обрабатывают кольцо
/// RX и продвигают фазы до SYN+ACK.
pub fn tcp_connect(ip: &[u8; 4], port: u16) -> i32 {
    unsafe {
        if !init() {
            return NET_ERR;
        }
        let now = crate::scheduler::ticks();
        if !TCP_ACTIVE || TCP_PHASE == 4 || TCP_GOT_FIN {
            // Новая сессия (в т.ч. восстановление после RST и полного EOF).
            TCP_ACTIVE = true;
            TCP_START_TICK = now;
            TCP_PHASE = 0;
            TCP_RST = false;
            TCP_GOT_FIN = false;
            TCP_RX_LEN = 0;
            TCP_RX_READ = 0;
            TCP_DST_IP = *ip;
            TCP_DST_PORT = port;
            TCP_SRC_PORT = 0xC000 + (now % 0x3FFF) as u16; // эфемерный 49152+
            TCP_ISN = (now as u32).wrapping_mul(0x9E37_79B9) | 0x0001_0000;
            // Публичные адреса живут за шлюзом: ARP спрашиваем у него, а не
            // у самого адреса (slirp на ARP для публичных IP не отвечает).
            if arp_resolve(&arp_next_hop(ip), 200).is_none() {
                TCP_ACTIVE = false;
                return NET_ERR;
            }
        }
        rx_poll();
        let dst_mac = arp_lookup(&arp_next_hop(ip));
        if TCP_PHASE == 0 && dst_mac != [0; 6] {
            TCP_DST_MAC = dst_mac;
            TCP_PHASE = 1;
            tcp_send_syn();
            TCP_SEND_SEQ = TCP_ISN.wrapping_add(1);
        }
        if TCP_PHASE == 2 {
            0
        } else if TCP_PHASE == 4 || now > TCP_START_TICK + TCP_HS_TIMEOUT {
            TCP_ACTIVE = false; // следующий connect начнёт заново
            NET_ERR
        } else {
            NET_EAGAIN
        }
    }
}

/// Отправляет данные по установленному соединению (PSH+ACK). Возвращает
/// число отправленных байт (равно `data.len()`) или NET_ERR.
pub fn tcp_send(data: &[u8]) -> i32 {
    unsafe {
        if !TCP_ACTIVE || TCP_PHASE != 2 || data.len() > 1400 {
            return NET_ERR;
        }
        if !tcp_seg(0x18, 5, TCP_SEND_SEQ, TCP_RECV_NEXT, data) {
            return NET_ERR;
        }
        TCP_SEND_SEQ = TCP_SEND_SEQ.wrapping_add(data.len() as u32);
        data.len() as i32
    }
}

/// Читает принятые данные из буфера. >0 — скопировано байт; NET_EAGAIN —
/// данных пока нет (или соединение ещё не установлено); NET_EOF — конец
/// потока (FIN получен, всё прочитано); NET_ERR — RST/не активен.
pub fn tcp_recv(buf: &mut [u8]) -> i32 {
    unsafe {
        if !TCP_ACTIVE {
            return NET_ERR;
        }
        rx_poll();
        if TCP_PHASE == 4 || TCP_RST {
            return NET_ERR;
        }
        if TCP_PHASE != 2 {
            return NET_EAGAIN;
        }
        let avail = TCP_RX_LEN - TCP_RX_READ;
        if avail > 0 {
            let n = avail.min(buf.len());
            buf[..n].copy_from_slice(&TCP_RX[TCP_RX_READ..TCP_RX_READ + n]);
            TCP_RX_READ += n;
            n as i32
        } else if TCP_GOT_FIN {
            NET_EOF
        } else {
            NET_EAGAIN
        }
    }
}

/// Закрывает соединение и сбрасывает состояние (после EOF сервера FIN не
/// шлём — пир уже закрыл поток; при живом соединении просто рвём локально).
pub fn tcp_close() -> i32 {
    unsafe {
        TCP_ACTIVE = false;
        TCP_PHASE = 0;
        TCP_RST = false;
        TCP_GOT_FIN = false;
        TCP_RX_LEN = 0;
        TCP_RX_READ = 0;
        0
    }
}

/// Полный сброс TCP-состояния (ядро зовёт перед запуском очередной
/// программы — сокет не должен «протекать» между запусками).
pub fn tcp_reset() {
    unsafe {
        TCP_ACTIVE = false;
        TCP_PHASE = 0;
        TCP_RST = false;
        TCP_GOT_FIN = false;
        TCP_RX_LEN = 0;
        TCP_RX_READ = 0;
    }
}

/// «a.b.c.d» -> 4 байта.
pub fn parse_ip(s: &str) -> Option<[u8; 4]> {
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

    // Этап 1: ARP-развязка — из кэша либо запрос и ожидание ответа. Публичные
    // адреса идут через шлюз, ARP спрашиваем у него.
    let dst_mac = match unsafe { arp_resolve(&arp_next_hop(&ip), 200) } {
        Some(m) => m,
        None => {
            writer.write_string("ping: ARP timeout for ");
            writer.write_string(addr);
            writer.write_string("\n");
            return;
        }
    };
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

// ─── DNS-резолвер (UDP поверх того же rtl8139) ──────────────────────────────

/// DNS-сервер в user-net QEMU (slirp слушает 10.0.2.3:53 и проксирует хост).
const DNS_SERVER: [u8; 4] = [10, 0, 2, 3];
const DNS_PORT: u16 = 53;
/// Порт источника: фиксированный, чтобы ответ демартизировать по нему.
const DNS_SRC_PORT: u16 = 0x3E00;
/// Ответ DNS помещается в один UDP-датаграммный буфер.
const DNS_RX_CAP: usize = 1024;
/// 3 c на ARP + запрос + ответ (300 тиков PIT 100 Гц).
const DNS_TIMEOUT: u64 = 300;
/// Сколько раз повторяем запрос, прежде чем сдаться.
const DNS_ATTEMPTS: u32 = 3;
/// Максимум A-записей, которые разбираем из ответа.
const DNS_MAX_ANS: usize = 8;

#[derive(Clone, Copy)]
struct DnsAnswer {
    ip: [u8; 4],
    ttl: [u8; 4],
}

static mut DNS_REPLY: [u8; DNS_RX_CAP] = [0; DNS_RX_CAP];
static mut DNS_REPLY_LEN: usize = 0;
static mut DNS_QUERY_ID: u16 = 0x4D53; // "MS"

/// UDP-датаграмма: Ethernet + IPv4 + UDP + полезная нагрузка.
unsafe fn udp_send(
    dst_mac: &[u8; 6],
    dst_ip: &[u8; 4],
    src_port: u16,
    dst_port: u16,
    payload: &[u8],
) -> bool {
    if payload.len() > 1400 {
        return false;
    }
    let udp_len = 8 + payload.len();

    // UDP-заголовок (checksum = 0 — для IPv4 это допустимо).
    let mut udp = [0u8; 1408];
    udp[0..2].copy_from_slice(&src_port.to_be_bytes());
    udp[2..4].copy_from_slice(&dst_port.to_be_bytes());
    udp[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
    udp[6..8].copy_from_slice(&0u16.to_be_bytes());
    udp[8..8 + payload.len()].copy_from_slice(payload);

    // IPv4 (proto = 17 UDP).
    let mut ip = [0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&(20u16 + udp_len as u16).to_be_bytes());
    ip[4..6].copy_from_slice(&0x0003u16.to_be_bytes()); // id
    ip[8] = 64; // TTL
    ip[9] = 17; // UDP
    ip[12..16].copy_from_slice(&OWN_IP);
    ip[16..20].copy_from_slice(dst_ip);
    let ip_sum = checksum(&ip).to_be_bytes();
    ip[10..12].copy_from_slice(&ip_sum);

    let mut frame = [0u8; 1428];
    frame[..20].copy_from_slice(&ip);
    frame[20..20 + udp_len].copy_from_slice(&udp[..udp_len]);
    let len = build_eth(dst_mac, 0x0800, &frame[..20 + udp_len]);
    tx_send(len)
}

/// Собирает DNS-запрос A для `name` (без точечной формы): header + QNAME +
/// QTYPE=A + QCLASS=IN.
fn dns_build_query(name: &str, id: u16) -> alloc::vec::Vec<u8> {
    let mut q = alloc::vec::Vec::with_capacity(12 + name.len() + 8);
    q.extend_from_slice(&id.to_be_bytes()); // id
    q.extend_from_slice(&0x0100u16.to_be_bytes()); // flags: recursion desired
    q.extend_from_slice(&1u16.to_be_bytes()); // qdcount
    q.extend_from_slice(&0u16.to_be_bytes()); // ancount
    q.extend_from_slice(&0u16.to_be_bytes()); // nscount
    q.extend_from_slice(&0u16.to_be_bytes()); // arcount
    for label in name.split('.').filter(|s| !s.is_empty()) {
        let len = label.len().min(63);
        q.push(len as u8);
        q.extend_from_slice(&label.as_bytes()[..len]);
    }
    q.push(0); // терминатор QNAME
    q.extend_from_slice(&1u16.to_be_bytes()); // QTYPE = A
    q.extend_from_slice(&1u16.to_be_bytes()); // QCLASS = IN
    q
}

/// Разбирает A-записи из DNS-ответа. Возвращает число найденных записей.
fn dns_parse_answer(reply: &[u8], qid: u16, out: &mut [DnsAnswer]) -> usize {
    if reply.len() < 12 {
        return 0;
    }
    let rid = u16::from_be_bytes([reply[0], reply[1]]);
    if rid != qid {
        return 0; // не наш запрос
    }
    let rcode = reply[3] & 0x0F;
    if rcode != 0 {
        return 0; // NXDOMAIN/SERVFAIL/REFUSED
    }
    let qd = u16::from_be_bytes([reply[4], reply[5]]) as usize;
    let an = u16::from_be_bytes([reply[6], reply[7]]) as usize;

    // Пропускаем вопрос, чтобы встать на секцию ответов.
    let mut p = 12usize;
    for _ in 0..qd {
        // QNAME: последовательность меток до нулевого байта.
        while p < reply.len() && reply[p] != 0 {
            let l = reply[p] as usize;
            p += 1 + l;
        }
        p += 1; // нулевой терминатор
        p += 4; // QTYPE + QCLASS
        if p > reply.len() {
            return 0;
        }
    }

    let mut n = 0usize;
    for _ in 0..an {
        if p + 12 > reply.len() {
            break;
        }
        // Имя может быть сжатым (0xC0) — для A-записи нам важен только указатель,
        // чтобы пропустить его; сами IP лежат после.
        if reply[p] & 0xC0 == 0xC0 {
            p += 2;
        } else {
            while p < reply.len() && reply[p] != 0 {
                let l = reply[p] as usize;
                p += 1 + l;
            }
            p += 1;
        }
        if p + 10 > reply.len() {
            break;
        }
        let rtype = u16::from_be_bytes([reply[p], reply[p + 1]]);
        let _rclass = u16::from_be_bytes([reply[p + 2], reply[p + 3]]);
        let ttl = [
            reply[p + 4],
            reply[p + 5],
            reply[p + 6],
            reply[p + 7],
        ];
        let rdlen = u16::from_be_bytes([reply[p + 8], reply[p + 9]]) as usize;
        p += 10;
        if p + rdlen > reply.len() {
            break;
        }
        if rtype == 1 && rdlen == 4 && n < out.len() {
            out[n].ip.copy_from_slice(&reply[p..p + 4]);
            out[n].ttl = ttl;
            n += 1;
        }
        p += rdlen;
    }
    n
}

/// Резолвит `name` в набор A-адресов, записывая их в `out`. Возвращает их
/// количество либо 0 при ошибке/таймауте. Каждый вызов делает ARP + запрос.
pub fn dns_resolve(name: &str, out: &mut [[u8; 4]]) -> usize {
    if !init() || name.is_empty() || name.len() > 240 || out.is_empty() {
        return 0;
    }
    let mut answers = [DnsAnswer {
        ip: [0; 4],
        ttl: [0; 4],
    }; DNS_MAX_ANS];

    // Этап 1: ARP-развязка с DNS-сервером (берётся из общего кэша).
    let dns_mac = match unsafe { arp_resolve(&DNS_SERVER, 200) } {
        Some(m) => m,
        None => return 0,
    };
    unsafe {
        // Этап 2: отправляем запрос и ждём ответа с нашим ID. UDP не имеет
        // ретрасмитов — потерянная датаграмма молча ломала весь резолв, поэтому
        // повторяем запрос с новым ID (старый ответ отфильтруется по ID).
        for _ in 0..DNS_ATTEMPTS {
            DNS_QUERY_ID = DNS_QUERY_ID.wrapping_add(1);
            let q = dns_build_query(name, DNS_QUERY_ID);
            if !udp_send(&dns_mac, &DNS_SERVER, DNS_SRC_PORT, DNS_PORT, &q) {
                return 0;
            }
            DNS_REPLY_LEN = 0;
            let deadline = crate::scheduler::ticks() + DNS_TIMEOUT;
            while crate::scheduler::ticks() < deadline {
                rx_poll();
                if DNS_REPLY_LEN > 0 {
                    break;
                }
                crate::scheduler::sleep_until(crate::scheduler::ticks() + 1);
            }
            if DNS_REPLY_LEN == 0 {
                continue;
            }
            let n = dns_parse_answer(
                &DNS_REPLY[..DNS_REPLY_LEN],
                DNS_QUERY_ID,
                &mut answers[..],
            );
            if n > 0 {
                for i in 0..n.min(out.len()) {
                    out[i] = answers[i].ip;
                }
                return n.min(out.len());
            }
            // Ответ пришёл, но A-записей нет (NXDOMAIN/прокси-пустышка):
            // повтор не поможет.
            return 0;
        }
        0
    }
}
