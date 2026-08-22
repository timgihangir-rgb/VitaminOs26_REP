//! Минимальный загрузчик статических ELF64-исполняемых файлов.
//!
//! Поддерживается то, что выдаёт сборка наших программ: ELF64 little-endian,
//! ET_EXEC для x86-64, сегменты PT_LOAD без релокаций и без динамики.
//! Файловый образ каждого сегмента копируется в freshly-выделенные страницы
//! по p_vaddr; хвост страницы и BSS (p_memsz > p_filesz) остаются нулями.
//!
//! Сегменты обязаны лежать в user-области [USER_START, USER_STACK_BASE) —
//! выше начинается стек задачи. Если страница уже была замаплена предыдущим
//! сегментом (сегменты делят страницу), она переиспользуется, а не выделяется
//! заново — так данные соседних сегментов в общей странице не затираются.

use x86_64::structures::paging::{
    FrameAllocator, Page, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use crate::memory::{FRAME_ALLOC, PHYS_MEM_OFFSET};
use crate::paging::{
    map_page, translate_addr, write_user_bytes, AddressSpace, SpaceError, USER_STACK_BASE,
    USER_START,
};

const PT_LOAD: u32 = 1;
const ET_EXEC: u16 = 2;
const EM_X86_64: u16 = 62;
const PAGE_SIZE: u64 = 4096;

/// Предел суммарного memsz образа: защита от мусорного файла с ELF-магией.
const MAX_IMAGE_MEMSZ: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfError {
    /// Не ELF64 little-endian ET_EXEC для x86-64.
    Unsupported,
    /// Заголовки повреждены (файл обрезан или содержит неверные смещения).
    Malformed,
    /// Сегмент не влезает в user-область.
    OutOfRange,
    /// Кончилась память при выделении фреймов.
    OutOfMemory,
}

impl From<SpaceError> for ElfError {
    fn from(e: SpaceError) -> Self {
        match e {
            SpaceError::OutOfRange => ElfError::OutOfRange,
            SpaceError::MapError => ElfError::OutOfMemory,
        }
    }
}

/// Быстрая проверка сигнатуры: годится ли файл для load_elf.
pub fn is_elf(data: &[u8]) -> bool {
    data.len() >= 64 && data[0..4] == [0x7f, b'E', b'L', b'F']
}

fn rd<'a>(data: &'a [u8], off: usize, len: usize) -> Result<&'a [u8], ElfError> {
    let end = off.checked_add(len).ok_or(ElfError::Malformed)?;
    data.get(off..end).ok_or(ElfError::Malformed)
}

fn u16le(data: &[u8], off: usize) -> Result<u16, ElfError> {
    Ok(u16::from_le_bytes(rd(data, off, 2)?.try_into().unwrap()))
}

fn u32le(data: &[u8], off: usize) -> Result<u32, ElfError> {
    Ok(u32::from_le_bytes(rd(data, off, 4)?.try_into().unwrap()))
}

fn u64le(data: &[u8], off: usize) -> Result<u64, ElfError> {
    Ok(u64::from_le_bytes(rd(data, off, 8)?.try_into().unwrap()))
}

/// Маппит все сегменты PT_LOAD в адресное пространство и возвращает точку
/// входа (e_entry).
pub fn load_elf(space: &mut AddressSpace, data: &[u8]) -> Result<u64, ElfError> {
    if !is_elf(data) || data[4] != 2 || data[5] != 1 || data[6] != 1 {
        return Err(ElfError::Unsupported);
    }
    if u16le(data, 16)? != ET_EXEC || u16le(data, 18)? != EM_X86_64 {
        return Err(ElfError::Unsupported);
    }

    let entry = u64le(data, 24)?;
    let phoff = u64le(data, 32)? as usize;
    let phentsize = u16le(data, 54)? as usize;
    let phnum = u16le(data, 56)? as usize;
    if phentsize < 56 {
        return Err(ElfError::Malformed);
    }

    for i in 0..phnum {
        let base = phoff
            .checked_add(i.checked_mul(phentsize).ok_or(ElfError::Malformed)?)
            .ok_or(ElfError::Malformed)?;
        let ph = rd(data, base, 56)?;
        if u32le(ph, 0)? != PT_LOAD {
            continue;
        }
        let p_offset = u64le(ph, 8)? as usize;
        let p_vaddr = u64le(ph, 16)?;
        let p_filesz = u64le(ph, 32)? as usize;
        let p_memsz = u64le(ph, 40)? as usize;
        map_segment(space, data, p_offset, p_vaddr, p_filesz, p_memsz)?;
    }

    Ok(entry)
}

fn map_segment(
    space: &mut AddressSpace,
    data: &[u8],
    offset: usize,
    vaddr: u64,
    filesz: usize,
    memsz: usize,
) -> Result<(), ElfError> {
    if memsz == 0 {
        return Ok(());
    }
    let seg_end = vaddr
        .checked_add(memsz as u64)
        .ok_or(ElfError::OutOfRange)?;
    if vaddr < USER_START || seg_end > USER_STACK_BASE {
        return Err(ElfError::OutOfRange);
    }
    if filesz > memsz
        || offset
            .checked_add(filesz)
            .ok_or(ElfError::Malformed)?
            > data.len()
    {
        return Err(ElfError::Malformed);
    }
    if memsz as u64 > MAX_IMAGE_MEMSZ {
        return Err(ElfError::OutOfRange);
    }

    let first = (vaddr / PAGE_SIZE) * PAGE_SIZE;
    let last = (seg_end + PAGE_SIZE - 1) / PAGE_SIZE * PAGE_SIZE;

    let mut page_va = first;
    while page_va < last {
        ensure_page_mapped(space, page_va)?;

        // Часть сегмента, попадающая в эту страницу: копируем файловые байты.
        let seg_file_end = vaddr + filesz as u64;
        let lo = page_va.max(vaddr);
        let hi = (page_va + PAGE_SIZE).min(seg_file_end);
        if lo < hi {
            let src = offset + (lo - vaddr) as usize;
            let n = (hi - lo) as usize;
            write_user_bytes(space, lo, &data[src..src + n]);
        }

        page_va += PAGE_SIZE;
    }
    Ok(())
}

/// Гарантирует, что страница замаплена: если нет — выделяет нулевой фрейм.
/// Свежие фреймы обнуляются, поэтому BSS (memsz > filesz) читается как нули.
fn ensure_page_mapped(space: &mut AddressSpace, page_va: u64) -> Result<(), ElfError> {
    if translate_addr(space, VirtAddr::new(page_va)).is_some() {
        return Ok(());
    }
    let frame = unsafe { FRAME_ALLOC.allocate_frame() }.ok_or(ElfError::OutOfMemory)?;
    unsafe {
        let dst = (PHYS_MEM_OFFSET + frame.start_address().as_u64()) as *mut u8;
        core::ptr::write_bytes(dst, 0, 4096);
    }
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(page_va));
    let frame = PhysFrame::<Size4KiB>::containing_address(PhysAddr::new(
        frame.start_address().as_u64(),
    ));
    map_page(
        space,
        page,
        frame,
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE,
    )?;
    Ok(())
}
