use x86_64::{
    structures::paging::{
        FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable,
        PageTableFlags, PhysFrame, Size4KiB,
    },
    PhysAddr, VirtAddr,
};

// High-half window set up by boot.asm (pml4[256]): physical X is mapped at
// PHYS_MEM_OFFSET + X. Shared by every address space.
pub const PHYS_MEM_OFFSET: u64 = 0xFFFF800000000000;

// Heap lives in the high half at pml4[257] (0xFFFF808000000000), so it is
// visible in every address space (kernel tables are mirrored into each process
// PML4). Note: pml4[256] cannot be used — boot.asm maps the whole pdp_high
// table with 2 MiB huge pages, so every 1 GiB slot under pml4[256] is taken.
pub const HEAP_START: usize = 0xFFFF8080_00000000;
/// 4 МиБ: 1 МиБ не хватало - staging WAL (164КБ куском) + стеки задач по
/// 32КБ + ELF-загрузки фрагментировали кучу, и linked_list_allocator падал
/// в split_current на обмельчавших дырах.
pub const HEAP_SIZE: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
#[repr(C, packed)]
pub struct MemRegion {
    pub start: u64,
    pub length: u64,
    pub kind: u8,
}

pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    let level_4_table = active_level_4_table(physical_memory_offset);
    OffsetPageTable::new(level_4_table, physical_memory_offset)
}

unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;
    let (level_4_table_frame, _) = Cr3::read();
    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();
    &mut *page_table_ptr
}

const FREE_LIST_CAP: usize = 4096;

static mut REGIONS: &'static [MemRegion] = &[];
static mut FREE_FRAMES: [u64; FREE_LIST_CAP] = [0; FREE_LIST_CAP];
static mut FREE_COUNT: usize = 0;

// Physical ranges owned by the boot trampoline and the kernel image. GRUB's
// multiboot memory map reports these as "available", so the allocator must
// exclude them explicitly or it would hand out frames holding the live page
// tables / GDT / stack / kernel text.
extern "C" {
    static _boot_start: u8;
    static _boot_end: u8;
    static _kernel_phys: u8;
    static _kernel_lma_size: u8;
}

fn kernel_reserved_ranges() -> [(u64, u64); 2] {
    let b_start = unsafe { &_boot_start as *const u8 as u64 };
    let b_end = unsafe { &_boot_end as *const u8 as u64 };
    let k_start = unsafe { &_kernel_phys as *const u8 as u64 };
    let k_len = unsafe { &_kernel_lma_size as *const u8 as u64 };
    [(b_start, b_end), (k_start, k_start + k_len)]
}

fn frame_is_reserved(addr: u64, reserved: &[(u64, u64); 2]) -> bool {
    for (s, e) in reserved {
        if addr >= *s && addr < *e {
            return true;
        }
    }
    false
}

// Global physical frame allocator used by the heap, the paging code and any
// future process setup. Freed frames (e.g. when an address space is destroyed
// or a page unmapped) are recycled through a small free list before the linear
// sweep over the multiboot regions continues.
//
// The sweep keeps a persistent cursor (region + offset) so each allocation is
// O(1) amortized. The old `usable_frames().nth(next)` rebuilt the region
// iterator and re-walked it from frame 0 on every call, which made spawning a
// task quadratic in the number of frames handed out.
pub struct FrameAlloc {
    region_idx: usize,
    frame_offset: u64,
}

impl FrameAlloc {
    pub const fn new() -> Self {
        FrameAlloc {
            region_idx: 0,
            frame_offset: 0,
        }
    }

    pub unsafe fn init(&mut self, regions: &'static [MemRegion]) {
        REGIONS = regions;
        self.region_idx = 0;
        self.frame_offset = 0;
        FREE_COUNT = 0;
    }

    pub fn free_count(&self) -> usize {
        unsafe { FREE_COUNT }
    }

    /// Следующий свободный кадр из линейного прохода по регионам, начиная с
    /// позиции курсора. O(1) амортизированно: курсор двигается только вперёд.
    unsafe fn sweep_next(&mut self) -> Option<PhysFrame> {
        // Пропускаем всё ниже 4 МиБ: диапазон 0..2 МиБ виден user-задачам
        // через identity-map (VGA/ABI), а 2..4 МиБ - это физическая память
        // ядра. Фреймы из этой зоны в руках ring 3 - прямой путь к порче.
        const LOW_MEMORY_END: u64 = 0x40_0000;
        let reserved = kernel_reserved_ranges();
        while self.region_idx < REGIONS.len() {
            let r = &REGIONS[self.region_idx];
            if r.kind != 1 {
                self.region_idx += 1;
                self.frame_offset = 0;
                continue;
            }
            let first_addr = r.start + self.frame_offset * 4096;
            if first_addr + 4096 > r.start + r.length {
                self.region_idx += 1;
                self.frame_offset = 0;
                continue;
            }
            let addr = first_addr;
            self.frame_offset += 1;
            if addr >= LOW_MEMORY_END && !frame_is_reserved(addr, &reserved) {
                return Some(PhysFrame::containing_address(PhysAddr::new(addr)));
            }
        }
        None
    }
}

unsafe impl FrameAllocator<Size4KiB> for FrameAlloc {
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        // Атомарность против тика: курсор sweep и free-list - static mut,
        // без защиты две задачи (например, супервизор и exec) получают
        // ОДИН И ТОТ ЖЕ фрейм, и zero() страниц затирает чужие данные.
        x86_64::instructions::interrupts::without_interrupts(|| unsafe {
            while FREE_COUNT > 0 {
                FREE_COUNT -= 1;
                let addr = FREE_FRAMES[FREE_COUNT];
                // Валидация записи free-list: мусор здесь = чья-то порча.
                if addr >= 0x40_0000 && addr < 0x80_0000 && addr % 4096 == 0 {
                    ALLOCATED_FRAMES.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
                    return Some(PhysFrame::containing_address(PhysAddr::new(addr)));
                }
                crate::vga::serial_write_atomic("[frames] BAD entry addr=");
                crate::vga::serial_u64(addr as u64);
                crate::vga::serial_write_atomic(" idx=");
                crate::vga::serial_u64(FREE_COUNT as u64);
                crate::vga::serial_write_atomic("\n");
            }
            let f = self.sweep_next();
            if f.is_some() {
                ALLOCATED_FRAMES.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
            }
            f
        })
    }
}

impl FrameDeallocator<Size4KiB> for FrameAlloc {
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame) {
        x86_64::instructions::interrupts::without_interrupts(|| unsafe {
            let addr = frame.start_address().as_u64();
            // Детектор двойного освобождения: фрейм, уже висящий в списке,
            // будет выдан второму владельцу, и тот затрёт память первого
            // (нулями таблиц/страниц). Не пушим дубликат - кричим в serial.
            for i in 0..FREE_COUNT {
                if FREE_FRAMES[i] == addr {
                    crate::vga::serial_write_atomic("[frames] DOUBLE FREE addr=");
                    crate::vga::serial_u64(addr);
                    crate::vga::serial_write_atomic("\n");
                    return;
                }
            }
            if FREE_COUNT < FREE_LIST_CAP {
                FREE_FRAMES[FREE_COUNT] = addr;
                FREE_COUNT += 1;
            } else {
                crate::vga::serial_write_atomic("[frames] free-list FULL, leak\n");
            }
            // И в free-list, и в «утечке» фрейм больше не занят владельцем.
            ALLOCATED_FRAMES.fetch_sub(1, core::sync::atomic::Ordering::SeqCst);
        })
    }
}

pub static mut FRAME_ALLOC: FrameAlloc = FrameAlloc::new();

/// Счётчик выданных и не освобождённых фреймов (рамка для `meminfo`:
/// «занято/свободно»). Не точный аудит, а оценка: кадры ещё не отданные
/// аллокатором в подсчёт не входят.
pub static ALLOCATED_FRAMES: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

pub fn init_allocator(regions: &'static [MemRegion]) {
    unsafe {
        FRAME_ALLOC.init(regions);
    }
}

#[derive(Debug)]
pub enum HeapInitError {
    Map,
}

pub fn init_heap(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
) -> Result<(), HeapInitError> {
    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + HEAP_SIZE as u64 - 1u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    for page in page_range {
        let frame = frame_allocator
            .allocate_frame()
            .ok_or(HeapInitError::Map)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe {
            mapper
                .map_to(page, frame, flags, frame_allocator)
                .map_err(|_| HeapInitError::Map)?
                .flush();
        }
    }

    Ok(())
}

const MAX_REGIONS: usize = 64;

#[repr(C, align(8))]
struct RegionBuf([u8; MAX_REGIONS * 24]);

static mut REGION_STORAGE: RegionBuf = RegionBuf([0u8; MAX_REGIONS * 24]);
static mut REGION_COUNT: usize = 0;

unsafe fn parse_multiboot2_into_buf(mb_info_ptr: u64) -> &'static [MemRegion] {
    let ptr = mb_info_ptr as *const u8;
    let total_size = core::ptr::read_unaligned(ptr as *const u32) as usize;

    let buf = &mut REGION_STORAGE.0;
    let regions = core::slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut MemRegion, MAX_REGIONS);

    let mut count = 0usize;
    let mut offset = 8usize;

    while offset + 8 <= total_size {
        let tag_type = core::ptr::read_unaligned(ptr.add(offset) as *const u32);
        let tag_size = core::ptr::read_unaligned(ptr.add(offset + 4) as *const u32) as usize;

        if tag_type == 0 {
            break;
        }

        if tag_type == 6 {
            let entry_size = core::ptr::read_unaligned(ptr.add(offset + 8) as *const u32) as usize;
            let entries_start = offset + 16;
            let entries_end = offset + tag_size;
            let mut entry_off = entries_start;

            while entry_off + entry_size <= entries_end && count < MAX_REGIONS {
                let addr = core::ptr::read_unaligned(ptr.add(entry_off) as *const u64);
                let len = core::ptr::read_unaligned(ptr.add(entry_off + 8) as *const u64);
                let kind_u32 = core::ptr::read_unaligned(ptr.add(entry_off + 16) as *const u32);

                regions[count] = MemRegion {
                    start: addr,
                    length: len,
                    kind: kind_u32 as u8,
                };
                count += 1;
                entry_off += entry_size;
            }
        }

        offset += tag_size;
        offset = (offset + 7) & !7;
    }

    REGION_COUNT = count;
    core::slice::from_raw_parts(buf.as_ptr() as *const MemRegion, count)
}

pub unsafe fn boot_params(mb_info_ptr: u64) -> (VirtAddr, &'static [MemRegion]) {
    // High-half physical memory window set up by boot.asm (pml4[256]):
    // physical address X is mapped at 0xFFFF800000000000 + X.
    let phys_mem_offset = VirtAddr::new(PHYS_MEM_OFFSET);
    let regions = parse_multiboot2_into_buf(mb_info_ptr);
    (phys_mem_offset, regions)
}
