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
}

fn str_to_fixed<const N: usize>(s: &str) -> [u8; N] {
    let mut buf = [0u8; N];
    let bytes = s.as_bytes();
    let len = bytes.len().min(N - 1);
    buf[..len].copy_from_slice(&bytes[..len]);
    buf
}

impl SysInfo {
    pub fn fill(cpu: &CpuInfo, mem: MemInfo) -> Self {
        let mut flags = 0u8;
        if cpu.has_sse { flags |= 1 << 0; }
        if cpu.has_sse2 { flags |= 1 << 1; }
        if cpu.has_avx { flags |= 1 << 2; }
        if cpu.has_rdrand { flags |= 1 << 3; }

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
            resolution: str_to_fixed("80x30 VGA"),
            terminal: str_to_fixed("VGA text mode"),
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
