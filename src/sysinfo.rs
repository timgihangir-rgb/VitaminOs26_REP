use crate::memory::MemRegion;

const SYSINFO_ADDR: *mut SysInfo = 0x5000 as *mut SysInfo;

#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct SysInfo {
    pub total_kb: u32,
    pub vga_offset: u32,
    pub cpu_family: u32,
    pub cpu_model: u32,
    pub cpu_stepping: u32,
    pub cpu_flags: u8,
    _pad: [u8; 3],
    pub cpu_vendor: [u8; 12],
    pub cpu_brand: [u8; 48],
    pub os_name: [u8; 16],
    pub os_version: [u8; 8],
    pub kernel_version: [u8; 8],
    pub shell_name: [u8; 16],
    pub shell_version: [u8; 8],
    pub bootloader_version: [u8; 16],
    pub resolution: [u8; 16],
    pub terminal: [u8; 16],
    // Поля для программ (meminfo и пр.) — добавлены в конец, чтобы не менять
    // смещения прежних полей (fetch читает только старые).
    pub used_kb: u32,
    pub proc_count: u32,

    // ─── Драйверы и устройства ────────────────────────────────────────────
    // Добавлены 2026-10-05 вместе с переработкой fetch. Прежний fetch знал
    // только про CPU и память, хотя к этому моменту в ядре были сетевой
    // драйвер (rtl8139), USB-HID, AHCI/ATA, ACPI, RTC и композитор
    // линейного кадрового буфера. Порядок полей менять нельзя: адрес блока
    // 0x5000 общий для всех программ, и каждая ring3-программа (meminfo,
    // fetch) держит свою копию раскладки.
    pub fs_type: [u8; 16],
    pub net_driver: [u8; 16],
    pub net_mac: [u8; 6],
    pub net_ip: [u8; 4],
    pub net_up: u8,
    pub disk_dev: [u8; 12],
    pub disk_model: [u8; 48],
    pub disk_blocks: u32,
    pub block_backend: [u8; 12],
    pub fb_width: u32,
    pub fb_height: u32,
    pub fb_bpp: u8,
    pub fb_scale: u8,
    pub fb_on: u8,
    pub usb_kbd: u8,
    pub usb_reports: u32,
    pub input_ps2: u8,
    pub acpi: [u8; 16],
    pub rtc_unix: u32,
}

fn str_to_fixed<const N: usize>(s: &str) -> [u8; N] {
    let mut buf = [0u8; N];
    let bytes = s.as_bytes();
    let len = bytes.len().min(N - 1);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

/// Модель диска из IDENTIFY часто содержит непечатные байты и двойные
/// пробелы (спецификация ATA требует именно так их хранить). Для вывода
/// приводим к читаемому виду, иначе в поле SysInfo попадёт мусор.
fn clean_model(s: &str) -> alloc::string::String {
    let mut out = alloc::string::String::new();
    let mut space = false;
    for &b in s.as_bytes() {
        if (0x20..0x7F).contains(&b) {
            if space && !out.is_empty() {
                out.push(' ');
            }
            out.push(b as char);
            space = false;
        } else {
            space = true;
        }
    }
    out
}

/// Дисплей для sysinfo: с линейным кадровым буфером разрешение и терминал
/// разные вещи (на 1280x800 сетка текста становится 80x25 в масштабе 2x),
/// поэтому строка собирается из обоих, а не из Constants VGA. Обе строки
/// должны влезать в 16 байт поля SysInfo вместе с NUL.
fn display_strings(fb_w: u32, fb_h: u32, fb_bpp: u8, fb_scale: u8) -> ([u8; 16], [u8; 16]) {
    if fb_w == 0 {
        return (
            str_to_fixed(&alloc::format!(
                "{}x{} VGA",
                crate::vga::screen_cols(),
                crate::vga::screen_rows()
            )),
            str_to_fixed("VGA text mode"),
        );
    }
    let scale = if fb_scale < 1 { 1 } else { fb_scale };
    (
        str_to_fixed(&alloc::format!("{}x{} {}bpp", fb_w, fb_h, fb_bpp)),
        str_to_fixed(&alloc::format!(
            "text {}x{} @{}x",
            crate::vga::screen_cols(),
            crate::vga::screen_rows(),
            scale
        )),
    )
}

impl SysInfo {
    pub fn fill(cpu: &CpuInfo, mem: MemInfo) -> Self {
        let mut flags = 0u8;
        if cpu.has_sse { flags |= 1 << 0; }
        if cpu.has_sse2 { flags |= 1 << 1; }
        if cpu.has_avx { flags |= 1 << 2; }
        if cpu.has_rdrand { flags |= 1 << 3; }

        // Состояние драйверов на момент запуска программы. Всё это — простые
        // чтения глобалов и кэшей; единственное исключение — модель диска
        // (blockdev кэширует её после первого IDENTIFY) и кадровый буфер.
        let (fb_w, fb_h, fb_bpp, fb_scale, fb_on) = crate::fb::info();
        let (resolution, terminal) = display_strings(fb_w, fb_h, fb_bpp, fb_scale);
        let net_mac = crate::net::mac();
        let net_up = crate::net::ready();
        let disk_name = crate::blockdev::active_name();
        let disk_model = clean_model(&crate::blockdev::active_model());
        let usb_kbd = crate::usb::owns_keyboard();
        let acpi = crate::acpi::status();
        let disk_blocks = crate::blockdev::capacity_blocks().unwrap_or(0);

        SysInfo {
            total_kb: (mem.total_usable / 1024) as u32,
            vga_offset: 0xB8000,
            cpu_family: cpu.family,
            cpu_model: cpu.model,
            cpu_stepping: cpu.stepping,
            cpu_flags: flags,
            _pad: [0; 3],
            cpu_vendor: cpu.vendor,
            cpu_brand: cpu.brand,
            os_name: str_to_fixed("VitaminOS26"),
            os_version: str_to_fixed("0.1.0"),
            kernel_version: str_to_fixed("0.1.0"),
            shell_name: str_to_fixed("VitaminShell"),
            shell_version: str_to_fixed("0.1.0"),
            bootloader_version: str_to_fixed("GRUB"),
            resolution,
            terminal,
            used_kb: (crate::memory::ALLOCATED_FRAMES.load(
                core::sync::atomic::Ordering::SeqCst,
            ) * 4) as u32, // 1 фрейм = 4 КиБ -> КБ
            proc_count: crate::scheduler::task_count() as u32,

            fs_type: str_to_fixed("VITAFS+WAL"),
            net_driver: str_to_fixed(if net_up { "rtl8139" } else { "none" }),
            net_mac: {
                let mut m = [0u8; 6];
                if let Some(src) = net_mac {
                    m.copy_from_slice(&src);
                }
                m
            },
            net_ip: crate::net::OWN_IP,
            net_up: net_up as u8,
            disk_dev: str_to_fixed(&disk_name),
            disk_model: str_to_fixed(&disk_model),
            disk_blocks,
            block_backend: str_to_fixed(crate::blockdev::backend_name()),
            fb_width: fb_w,
            fb_height: fb_h,
            fb_bpp: fb_bpp,
            fb_scale: fb_scale,
            fb_on: fb_on as u8,
            usb_kbd: usb_kbd as u8,
            usb_reports: crate::usb::reports() as u32,
            input_ps2: (!usb_kbd) as u8,
            acpi: str_to_fixed(acpi),
            rtc_unix: crate::rtc::now_unix() as u32,
        }
    }

    pub fn write_to_memory(cpu: &CpuInfo, mem: MemInfo) {
        let info = Self::fill(cpu, mem);
        unsafe {
            core::ptr::write_volatile(SYSINFO_ADDR, info);
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct CpuInfo {
    pub vendor: [u8; 12],
    pub brand: [u8; 48],
    pub family: u32,
    pub model: u32,
    pub stepping: u32,
    pub has_sse: bool,
    pub has_sse2: bool,
    pub has_avx: bool,
    pub has_rdrand: bool,
}

impl CpuInfo {
    pub fn detect() -> Self {
        let cpuid = unsafe { core::arch::x86_64::__cpuid(0) };
        let vendor = [
            cpuid.ebx as u8,
            (cpuid.ebx >> 8) as u8,
            (cpuid.ebx >> 16) as u8,
            (cpuid.ebx >> 24) as u8,
            cpuid.edx as u8,
            (cpuid.edx >> 8) as u8,
            (cpuid.edx >> 16) as u8,
            (cpuid.edx >> 24) as u8,
            cpuid.ecx as u8,
            (cpuid.ecx >> 8) as u8,
            (cpuid.ecx >> 16) as u8,
            (cpuid.ecx >> 24) as u8,
        ];

        let mut brand = [0u8; 48];
        let max_extended = unsafe { core::arch::x86_64::__cpuid(0x80000000).eax };
        if max_extended >= 0x80000004 {
            let mut c = unsafe { core::arch::x86_64::__cpuid(0x80000002) };
            brand[0..16].copy_from_slice(&cpuid_raw(&c));
            c = unsafe { core::arch::x86_64::__cpuid(0x80000003) };
            brand[16..32].copy_from_slice(&cpuid_raw(&c));
            c = unsafe { core::arch::x86_64::__cpuid(0x80000004) };
            brand[32..48].copy_from_slice(&cpuid_raw(&c));
        }

        let cpuid = unsafe { core::arch::x86_64::__cpuid(0x1) };
        let stepping = cpuid.eax & 0xF;
        let model = (cpuid.eax >> 4) & 0xF;
        let family = (cpuid.eax >> 8) & 0xF;
        let has_sse = cpuid.edx & (1 << 25) != 0;
        let has_sse2 = cpuid.edx & (1 << 26) != 0;
        let has_avx = cpuid.ecx & (1 << 28) != 0;
        let has_rdrand = cpuid.ecx & (1 << 30) != 0;

        Self {
            vendor,
            brand,
            family,
            model,
            stepping,
            has_sse,
            has_sse2,
            has_avx,
            has_rdrand,
        }
    }
}

fn cpuid_raw(cpuid: &core::arch::x86_64::CpuidResult) -> [u8; 16] {
    [
        cpuid.eax as u8, (cpuid.eax >> 8) as u8, (cpuid.eax >> 16) as u8, (cpuid.eax >> 24) as u8,
        cpuid.ebx as u8, (cpuid.ebx >> 8) as u8, (cpuid.ebx >> 16) as u8, (cpuid.ebx >> 24) as u8,
        cpuid.ecx as u8, (cpuid.ecx >> 8) as u8, (cpuid.ecx >> 16) as u8, (cpuid.ecx >> 24) as u8,
        cpuid.edx as u8, (cpuid.edx >> 8) as u8, (cpuid.edx >> 16) as u8, (cpuid.edx >> 24) as u8,
    ]
}

#[derive(Debug, Clone, Copy)]
pub struct MemInfo {
    pub total_usable: u64,
    pub usable_regions: usize,
}

impl MemInfo {
    pub fn from_memory_map(regions: &[MemRegion]) -> Self {
        let mut total = 0u64;
        let mut usable = 0usize;
        for region in regions {
            if region.kind == 1 {
                usable += 1;
                total += region.length;
            }
        }
        Self {
            total_usable: total,
            usable_regions: usable,
        }
    }
}
