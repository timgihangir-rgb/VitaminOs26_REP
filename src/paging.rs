use alloc::vec;
use x86_64::{
    registers::control::{Cr3, Cr3Flags},
    structures::paging::{
        FrameAllocator, FrameDeallocator, MappedPageTable, Mapper, Page, PageTable,
        PageTableFlags, PhysFrame, Size4KiB, Translate,
    },
    structures::paging::{
        mapper::{MapToError, PageTableFrameMapping},
        OffsetPageTable,
    },
    PhysAddr, VirtAddr,
};

use crate::memory::{FRAME_ALLOC, PHYS_MEM_OFFSET};

// User pages live in the first pml4[0] subtree (below 512 GiB).
pub const USER_START: u64 = 0x400000;
pub const USER_END: u64 = PHYS_MEM_OFFSET;

// Стек user-задачи: 64 страницы (256 KiB), занимает диапазон
// [USER_STACK_BASE, USER_STACK_TOP) = [0x1FC0000, 0x2000000). Входной rsp
// user-задачи = USER_STACK_TOP - 8, а по адресу USER_STACK_TOP - 8 лежит адрес
// exit-стаба: программа, вернувшись из `_start` по `ret`, попадает на него и
// вызывает exit-сискол. Стек растёт вниз. Высоко, чтобы не мешать коду.
pub const USER_STACK_BASE: u64 = USER_STACK_TOP - USER_STACK_PAGES * 4096;
pub const USER_STACK_TOP: u64 = 0x2000000;
pub const USER_STACK_PAGES: u64 = 64;

/// Входной RSP user-задачи: уступаем 8 байт под адрес возврата в exit-стаб.
pub const USER_STACK_ENTRY_RSP: u64 = USER_STACK_TOP - 8;

fn table_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE
}

fn identity_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::HUGE_PAGE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceError {
    OutOfRange,
    MapError,
}

#[derive(Debug, Clone, Copy)]
pub struct AddressSpace {
    root: PhysFrame,
}

impl AddressSpace {
    #[allow(dead_code)]
    pub fn root_frame(&self) -> PhysFrame {
        self.root
    }
}

// All physical memory is reachable through the high-half window, so any frame
// can be treated as a page table pointer directly.
struct PhysWindow;

unsafe impl PageTableFrameMapping for PhysWindow {
    fn frame_to_pointer(&self, frame: PhysFrame) -> *mut PageTable {
        let va = VirtAddr::new(PHYS_MEM_OFFSET + frame.start_address().as_u64());
        va.as_mut_ptr()
    }
}

unsafe fn table_of(frame: PhysFrame) -> &'static mut PageTable {
    let va = VirtAddr::new(PHYS_MEM_OFFSET + frame.start_address().as_u64());
    &mut *va.as_mut_ptr()
}

fn active_level_4_table() -> &'static mut PageTable {
    let (frame, _) = Cr3::read();
    unsafe { table_of(frame) }
}

fn mapper(space: &mut AddressSpace) -> MappedPageTable<'_, PhysWindow> {
    let table = unsafe { table_of(space.root) };
    unsafe { MappedPageTable::new(table, PhysWindow) }
}

// Creates a new address space: a private pml4[0] subtree (identity 4 MiB for
// VGA/legacy access) plus the kernel high-half entries (pml4[256..512])
// mirrored from the active tables, so kernel code, the phys window and the heap
// stay reachable after a CR3 switch.
pub fn create_address_space() -> Option<AddressSpace> {
    // Атомарно против тика: три allocate_frame подряд и zero() таблиц не
    // должны перемежаться со спавном другой задачи.
    x86_64::instructions::interrupts::without_interrupts(|| unsafe {
        let alloc = &mut FRAME_ALLOC;
        let root_frame = alloc.allocate_frame()?;
        let pdp0_frame = alloc.allocate_frame()?;
        let pd0_frame = alloc.allocate_frame()?;

        let root = table_of(root_frame);
        let pdp0 = table_of(pdp0_frame);
        let pd0 = table_of(pd0_frame);
        root.zero();
        pdp0.zero();
        pd0.zero();

        // Identity map the first 4 MiB as two 2 MiB huge pages. Доступен из
        // ring 3 ТОЛЬКО диапазон 0..2 МиБ: VGA, shared memory и ABI-трамплины.
        // Вторая страница (2..4 МиБ) - физическая память САМОГО ЯДРА
        // (.text/.data/.bss, boot-стек): она видна по low-alias, но запись
        // из ring 3 запрещена. Иначе любая сбойная user-программа (crashy!)
        // затирает ядро нулями и указателями - случайная порча статики.
        pd0[0].set_addr(PhysAddr::new(0x0), identity_flags() | PageTableFlags::USER_ACCESSIBLE);
        pd0[1].set_addr(PhysAddr::new(0x200000), identity_flags());
        pdp0[0].set_frame(pd0_frame, table_flags());
        root[0].set_frame(pdp0_frame, table_flags());

        // Mirror kernel + heap high-half entries from the active tables.
        let active = active_level_4_table();
        for i in 256..512 {
            let e = active[i].clone();
            root[i].set_addr(e.addr(), e.flags());
        }

        Some(AddressSpace { root: root_frame })
    })
}

// Releases every frame owned by the space: the pml4[0] subtree (page tables
// plus all mapped data pages, skipping the shared identity huge pages) and the
// root table frame. Kernel high-half frames are shared and never freed here.
pub fn destroy_address_space(space: &mut AddressSpace) {
    unsafe {
        let alloc = &mut FRAME_ALLOC;
        let root = table_of(space.root);

        if let Some(pdp0) = root[0].frame().ok() {
            let pdp0_t = table_of(pdp0);
            if let Some(pd) = pdp0_t[0].frame().ok() {
                let pd_t = table_of(pd);
                for i in 0..512 {
                    let e = pd_t[i].clone();
                    if !e.flags().contains(PageTableFlags::PRESENT) {
                        continue;
                    }
                    if e.flags().contains(PageTableFlags::HUGE_PAGE) {
                        continue; // shared identity mapping, not owned
                    }
                    if let Some(pt) = e.frame().ok() {
                        let pt_t = table_of(pt);
                        for j in 0..512 {
                            let fe = pt_t[j].clone();
                            if fe.flags().contains(PageTableFlags::PRESENT) {
                                if let Ok(f) = fe.frame() {
                                    alloc.deallocate_frame(f);
                                }
                            }
                        }
                        alloc.deallocate_frame(pt);
                    }
                }
                alloc.deallocate_frame(pd);
            }
            alloc.deallocate_frame(pdp0);
        }

        root.zero();
        alloc.deallocate_frame(space.root);
    }
}

// Fork-style deep copy: identity huge pages are shared by reference, every
// mapped 4 KiB user page is copied into a fresh frame so the child gets its
// own writable snapshot.
pub fn copy_address_space(src: &AddressSpace) -> Option<AddressSpace> {
    unsafe {
        let alloc = &mut FRAME_ALLOC;
        let src_root = table_of(src.root);

        let dst = create_address_space()?;
        let dst_root = table_of(dst.root);

        let src_pdp0 = src_root[0].frame().ok()?;
        let dst_pdp0 = dst_root[0].frame().ok()?;
        let src_pd = table_of(src_pdp0)[0].frame().ok()?;
        let dst_pd = table_of(dst_pdp0)[0].frame().ok()?;

        let src_pd_t = table_of(src_pd);
        let dst_pd_t = table_of(dst_pd);

        for pd_idx in 0..512 {
            let s_pd_e = src_pd_t[pd_idx].clone();
            if !s_pd_e.flags().contains(PageTableFlags::PRESENT) {
                continue;
            }
            if s_pd_e.flags().contains(PageTableFlags::HUGE_PAGE) {
                // Shared identity mapping, reference it unchanged.
                dst_pd_t[pd_idx].set_addr(s_pd_e.addr(), s_pd_e.flags());
                continue;
            }
            let src_pt = s_pd_e.frame().ok()?;
            let dst_pt = alloc.allocate_frame()?;
            let src_pt_t = table_of(src_pt);
            let dst_pt_t = table_of(dst_pt);
            dst_pt_t.zero();
            for pt_idx in 0..512 {
                let s_pt_e = src_pt_t[pt_idx].clone();
                if !s_pt_e.flags().contains(PageTableFlags::PRESENT) {
                    continue;
                }
                let s_frame = s_pt_e.frame().ok()?;
                let d_frame = alloc.allocate_frame()?;
                copy_frame(s_frame, d_frame);
                dst_pt_t[pt_idx].set_frame(d_frame, s_pt_e.flags());
            }
            dst_pd_t[pd_idx].set_frame(dst_pt, s_pd_e.flags());
        }

        Some(dst)
    }
}

unsafe fn copy_frame(src: PhysFrame, dst: PhysFrame) {
    let s = VirtAddr::new(PHYS_MEM_OFFSET + src.start_address().as_u64()).as_ptr::<u8>();
    let d = VirtAddr::new(PHYS_MEM_OFFSET + dst.start_address().as_u64()).as_mut_ptr();
    core::ptr::copy_nonoverlapping(s, d, 4096);
}

/// Окно MMIO в верхней половине адресного пространства ядра: физический BAR
/// контроллера с адресом `p` отображается сюда по адресу
/// `KERNEL_MMIO_BASE + p`. Нужно контроллерам с memory-BAR: ABAR SATA-контроллера
/// стоит около 0xFEBF_1000, а это PCI-дыра вне RAM, и GRUB первые 4 ГиБ
/// разворачивает huge-страницами строго по карте памяти - под MMIO там ничего
/// нет, отображать его в PHYS_MEM_OFFSET нельзя.
///
/// Страницы помечаются NO_CACHE - содержимое MMIO не должно попадать в кэш CPU.
///
/// Окно должно быть отображено до создания адресных пространств задач: они
/// зеркалят верхнюю половину ядра из активных таблиц на момент создания.
pub const KERNEL_MMIO_BASE: u64 = 0xFFFF9000_00000000;

/// Размер окна MMIO: 4 ГиБ физических адресов, столько нужно PCI- BAR'ам.
pub const KERNEL_MMIO_SIZE: u64 = 0x1_0000_0000;

/// Отображает MMIO-диапазон физической памяти в окно KERNEL_MMIO_BASE.
/// Возвращает виртуальный адрес того же физического диапазона в ядре.
/// Диапазон выравнивается по границе страницы вниз: адрес ABAR может быть
/// не кратен 4 КиБ.
pub unsafe fn map_phys_in_kernel(phys: u64, len: u64) -> Option<u64> {
    let end = phys.checked_add(len)?;
    if end > KERNEL_MMIO_SIZE {
        dmap(&alloc::format!("mmio out of window: phys={:x} len={:x}", phys, len));
        return None;
    }
    let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE;
    let mut pt = OffsetPageTable::new(active_level_4_table(), VirtAddr::new(PHYS_MEM_OFFSET));
    let mut addr = phys & !0xFFF;
    while addr < end {
        // Адреса в нашем окне до этого ничем не заняты: отображаем без проверки.
        let virt = VirtAddr::new(KERNEL_MMIO_BASE + addr);
        let page: Page<Size4KiB> = match Page::from_start_address(virt) {
            Ok(p) => p,
            Err(_) => return None,
        };
        let frame: PhysFrame<Size4KiB> = PhysFrame::containing_address(PhysAddr::new(addr));
        if let Err(e) = OffsetPageTable::map_to(&mut pt, page, frame, flags, &mut FRAME_ALLOC) {
            dmap(&alloc::format!(
                "mmio map phys={:x} free={} err={}",
                addr,
                FRAME_ALLOC.free_count(),
                match e {
                    MapToError::FrameAllocationFailed => "no-frame",
                    MapToError::ParentEntryHugePage => "huge-page",
                    MapToError::PageAlreadyMapped(_) => "already-mapped",
                }
            ));
            return None;
        }
        addr += 0x1000;
    }
    Some(KERNEL_MMIO_BASE + phys)
}

fn dmap(msg: &str) {
    crate::vga::serial_write_atomic("[P] ");
    crate::vga::serial_write_atomic(msg);
    crate::vga::serial_putchar(b'\n');
}

pub fn map_page(
    space: &mut AddressSpace,
    page: Page<Size4KiB>,
    frame: PhysFrame<Size4KiB>,
    flags: PageTableFlags,
) -> Result<(), SpaceError> {
    let va = page.start_address().as_u64();
    if va < USER_START || va >= USER_END {
        return Err(SpaceError::OutOfRange);
    }
    let mut m = mapper(space);
    unsafe {
        m.map_to(page, frame, flags, &mut FRAME_ALLOC)
            .map_err(|_| SpaceError::MapError)?
            .flush();
    }
    Ok(())
}

#[allow(dead_code)]
pub fn unmap_page(space: &mut AddressSpace, page: Page<Size4KiB>) -> Result<(), SpaceError> {
    let va = page.start_address().as_u64();
    if va < USER_START || va >= USER_END {
        return Err(SpaceError::OutOfRange);
    }
    let mut m = mapper(space);
    unsafe {
        if let Ok((frame, flush)) = m.unmap(page) {
            flush.flush();
            FRAME_ALLOC.deallocate_frame(frame);
        }
    }
    Ok(())
}

fn user_flags() -> PageTableFlags {
    PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE
}

/// Копирует образ .bin-программы (плоский бинарник, секции не выровнены) в
/// страницы по адресу USER_START: выделяет фрейм на каждые 4 KiB кода,
/// нулит его, копирует байты и маппит в user-пространство.
pub fn map_user_image(space: &mut AddressSpace, data: &[u8]) -> Result<(), SpaceError> {
    unsafe {
        let len = data.len();
        let pages = (len + 4095) / 4096;
        for i in 0..pages {
            let frame = FRAME_ALLOC.allocate_frame().ok_or(SpaceError::MapError)?;
            let dst = (PHYS_MEM_OFFSET + frame.start_address().as_u64()) as *mut u8;
            core::ptr::write_bytes(dst, 0, 4096);
            let n = core::cmp::min(4096, len - i * 4096);
            core::ptr::copy_nonoverlapping(data.as_ptr().add(i * 4096), dst, n);
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(
                USER_START + (i as u64) * 4096,
            ));
            map_page(space, page, frame, user_flags())?;
        }
        Ok(())
    }
}

/// Копирует байты в user-память (все страницы обязаны быть уже замаплены).
/// Диапазон может пересекать границы страниц — пишется по кускам.
pub fn write_user_bytes(space: &AddressSpace, mut vaddr: u64, mut bytes: &[u8]) {
    while !bytes.is_empty() {
        let page_rem = 4096 - (vaddr % 4096) as usize;
        let n = page_rem.min(bytes.len());
        let pa = match translate_addr(space, VirtAddr::new(vaddr)) {
            Some(pa) => pa,
            None => return,
        };
        unsafe {
            let dst = (PHYS_MEM_OFFSET + pa.as_u64()) as *mut u8;
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, n);
        }
        vaddr += n as u64;
        bytes = &bytes[n..];
    }
}

/// Блок аргументов user-задачи на стеке.
pub struct UserStackArgs {
    /// Входной RSP: указывает на слот exit-стаба (`ret` из `_start` уходит
    /// туда, если программа не завершилась сисколлом явно).
    pub rsp: u64,
    /// Число аргументов (дублируется в rdi при входе).
    pub argc: u64,
    /// User-адрес массива указателей argv[0..argc] (дублируется в rsi).
    pub argv: u64,
}

/// Зазор между входным rsp и блоком аргументов: пушы и локалы программы
/// растут ВНИЗ от входного rsp и не должны затирать argc/argv.
const STACK_ARGS_GAP: u64 = 4096;

/// Маппит стек user-задачи ([USER_STACK_BASE, USER_STACK_TOP)) и строит на
/// нём блок аргументов. Раскладка сверху вниз:
/// ```text
/// TOP-8  : argc                        (информационно)
/// ниже   : argv[0..n], NULL (конец argv), NULL (конец envp)
/// ниже   : строки аргументов (NUL-terminated)
/// зазор STACK_ARGS_GAP байт — свободное место под пуши/локалы программы
/// rsp    : адрес exit-стаба (входной rsp указывает сюда; `ret` из `_start`
///          попадает на него)
/// ```
/// Регистры при входе задаёт ядро: rdi=argc, rsi=&argv[0] (для ELF; для
/// legacy .bin — rdi=vga_offset). Блок на стеке дублирует их по образцу SysV.
pub fn setup_user_stack(
    space: &mut AddressSpace,
    argv: &[&str],
) -> Result<UserStackArgs, SpaceError> {
    unsafe {
        for i in 0..USER_STACK_PAGES {
            let frame = FRAME_ALLOC.allocate_frame().ok_or(SpaceError::MapError)?;
            let dst = (PHYS_MEM_OFFSET + frame.start_address().as_u64()) as *mut u8;
            core::ptr::write_bytes(dst, 0, 4096);
            let page = Page::<Size4KiB>::containing_address(VirtAddr::new(
                USER_STACK_BASE + i * 4096,
            ));
            map_page(space, page, frame, user_flags())?;
        }
    }

    // Блок аргументов собираем в буфере ядра и пишем одним куском.
    let n = argv.len();
    // Размер строкового блока округляется до 8, чтобы strings_bottom был
    // выровнен (строки начинаются с выровненного адреса).
    let strings_size: usize = (argv.iter().map(|s| s.len() + 1).sum::<usize>() + 7) & !7;
    // argc + указатели argv + NULL argv + NULL envp + строки
    let block_size = 8u64 + 8 * (n as u64 + 2) + strings_size as u64;
    if block_size + STACK_ARGS_GAP > USER_STACK_PAGES * 4096 / 2 {
        return Err(SpaceError::OutOfRange);
    }

    let top = USER_STACK_TOP;
    let strings_top = top - 8 - 8 * (n as u64 + 2);
    let strings_bottom = strings_top - strings_size as u64;
    let entry_rsp = strings_bottom - STACK_ARGS_GAP;

    let mut buf = vec![0u8; (top - 8 - strings_bottom) as usize];
    let mut off = 0usize;
    for s in argv {
        buf[off..off + s.len()].copy_from_slice(s.as_bytes());
        off += s.len() + 1;
    }
    let mut p = (strings_top - strings_bottom) as usize;
    for i in 0..n {
        let ptr = strings_bottom + strings_offsets(argv, i);
        buf[p..p + 8].copy_from_slice(&ptr.to_le_bytes());
        p += 8;
    }
    // NULL конца argv и NULL конца envp уже нули в буфере.
    let argc_off = (top - 16 - strings_bottom) as usize;
    buf[argc_off..argc_off + 8].copy_from_slice(&(n as u64).to_le_bytes());

    write_user_bytes(space, strings_bottom, &buf);

    // Адрес exit-стаба по входному rsp: `ret` из `_start` уходит туда.
    let stub_pa =
        translate_addr(space, VirtAddr::new(entry_rsp)).ok_or(SpaceError::MapError)?;
    unsafe {
        ((PHYS_MEM_OFFSET + stub_pa.as_u64()) as *mut u64)
            .write(crate::progabi::EXIT_STUB as u64);
    }

    Ok(UserStackArgs {
        rsp: entry_rsp,
        argc: n as u64,
        argv: strings_top,
    })
}

/// Смещение строки аргумента `i` от начала строкового блока.
fn strings_offsets(argv: &[&str], i: usize) -> u64 {
    argv[..i].iter().map(|s| s.len() as u64 + 1).sum()
}

#[allow(dead_code)]
pub fn translate(space: &mut AddressSpace, addr: VirtAddr) -> Option<PhysAddr> {
    let m = mapper(space);
    m.translate_addr(addr)
}

pub fn translate_addr(space: &AddressSpace, addr: VirtAddr) -> Option<PhysAddr> {
    unsafe {
        let m = MappedPageTable::new(&mut *table_of(space.root), PhysWindow);
        m.translate_addr(addr)
    }
}

pub fn switch_to(space: &AddressSpace) {
    switch_to_root(space.root);
}

pub fn switch_to_root(root: PhysFrame) {
    unsafe { Cr3::write(root, Cr3Flags::empty()) };
}

// Boot-time sanity check: create/fork a space, copy content, switch to it and
// back, then destroy everything. Runs before interrupts are enabled.
pub fn self_test(writer: &mut crate::vga::Writer) {
    use core::ptr;
    use x86_64::structures::paging::PageTableFlags as F;

    writer.write_string("paging self-test: ");

    let result = (|| -> Result<(), ()> {
        unsafe {
            // 1. Create a space and map two user pages.
            let mut a = create_address_space().ok_or(())?;
            let p1 = Page::<Size4KiB>::containing_address(VirtAddr::new(USER_START));
            let p2 = Page::<Size4KiB>::containing_address(VirtAddr::new(USER_START + 0x1000));
            let f1 = FRAME_ALLOC.allocate_frame().ok_or(())?;
            let f2 = FRAME_ALLOC.allocate_frame().ok_or(())?;
            map_page(&mut a, p1, f1, F::PRESENT | F::WRITABLE).map_err(|_| ())?;
            map_page(&mut a, p2, f2, F::PRESENT | F::WRITABLE).map_err(|_| ())?;

            // 2. Verify translate() resolves to the mapped frames.
            if translate_addr(&a, p1.start_address()) != Some(f1.start_address()) {
                return Err(());
            }

            // 3. Write a marker through the physical window.
            let w1 = (PHYS_MEM_OFFSET + f1.start_address().as_u64()) as *mut u8;
            ptr::write_bytes(w1, 0xAB, 16);

            // 4. Fork: the child must have an independent copy.
            let mut b = copy_address_space(&a).ok_or(())?;
            let b_frame = translate_addr(&b, p1.start_address()).ok_or(())?;
            let rb = (PHYS_MEM_OFFSET + b_frame.as_u64()) as *const u8;
            if ptr::read(rb) != 0xAB {
                return Err(());
            }

            // 5. Mutating the parent must not affect the child.
            ptr::write_bytes(w1, 0xCD, 16);
            if ptr::read(rb) != 0xAB {
                return Err(());
            }

            // 6. Switch to the child and read through the virtual address.
            let kernel_root = Cr3::read().0;
            switch_to(&b);
            let v1 = p1.start_address().as_u64() as *const u8;
            let ok = ptr::read(v1) == 0xAB;
            switch_to_root(kernel_root);
            if !ok {
                return Err(());
            }

            // 7. Cleanup: all frames must come back to the allocator.
            let freed_before = FRAME_ALLOC.free_count();
            destroy_address_space(&mut b);
            destroy_address_space(&mut a);
            let freed_after = FRAME_ALLOC.free_count();
            if freed_after < freed_before + 2 {
                return Err(()); // at least the two mapped data frames returned
            }
        }
        Ok(())
    })();

    if result.is_ok() {
        writer.write_string("OK\n");
    } else {
        writer.write_string("FAILED\n");
    }
}
