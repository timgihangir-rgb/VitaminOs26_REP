// src/ralloc.rs
//
// Канареечная обёртка над linked_list_allocator (диагностика zero-writer).
//
// Раскладка каждого выделенного блока:
//   [ Header {magic, size, cap, base} ][ данные ][ Tail(8 байт) ]
//
//   Header.magic - детект подделки/переиспользования заголовка;
//   Tail         - канарейка ПОСЛЕ данных: любое переполнение блока
//                  затирает её и ловится при освобождении;
//   Header.base  - реальное начало блока у внутреннего аллокатора
//                  (данные выравниваются, поэтому base != header).
//
// При порче - подробный отчёт в COM1 и останов. Все операции под
// without_interrupts: держатель лока кучи невытечем (см. bigtodo).

use core::alloc::{GlobalAlloc, Layout};
use linked_list_allocator::LockedHeap;
use x86_64::instructions::interrupts;

const MAGIC: u64 = 0xDEADBEEFCAFEBABE;
const TAIL_MAGIC: u64 = 0xCAFEF00DBEEFCAFE;

#[repr(C)]
struct Header {
    magic: u64,
    size: usize, // полезный размер данных
    cap: usize,  // полный размер блока у INNER (для dealloc)
    base: *mut u8,
}

const HDR_SIZE: usize = core::mem::size_of::<Header>();
const TAIL_SIZE: usize = 8;

static INNER: LockedHeap = LockedHeap::empty();

fn fail(msg: &str, ptr: usize, size: usize) -> ! {
    crate::vga::serial_write_atomic("\n[ralloc] ");
    crate::vga::serial_write_atomic(msg);
    crate::vga::serial_write_atomic(" ptr=");
    crate::vga::serial_u64(ptr as u64);
    crate::vga::serial_write_atomic(" size=");
    crate::vga::serial_u64(size as u64);
    crate::vga::serial_write_atomic("\n");
    loop {
        x86_64::instructions::hlt();
    }
}

pub struct Allocator;

impl Allocator {
    pub unsafe fn init(start: *mut u8, size: usize) {
        INNER.lock().init(start, size);
    }
}

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        interrupts::without_interrupts(|| {
            let align = layout.align().max(core::mem::align_of::<Header>());
            let need = HDR_SIZE + layout.size() + TAIL_SIZE + align;
            let raw = match INNER.alloc(Layout::from_size_align(need, 1).unwrap()) {
                p if !p.is_null() => p,
                _ => return core::ptr::null_mut(),
            };
            // Выравниваем данные, следя чтобы заголовок влез перед ними.
            let mut data = ((raw as usize + HDR_SIZE + align - 1) / align) * align;
            if data - HDR_SIZE < raw as usize {
                data += align;
            }
            let hdr = (data - HDR_SIZE) as *mut Header;
            (*hdr).magic = MAGIC;
            (*hdr).size = layout.size();
            (*hdr).cap = need;
            (*hdr).base = raw;
            let tail = (data + layout.size()) as *mut u64;
            *tail = TAIL_MAGIC;
            data as *mut u8
        })
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        interrupts::without_interrupts(|| {
            if ptr.is_null() {
                return;
            }
            let hdr = ptr.sub(HDR_SIZE) as *mut Header;
            if (*hdr).magic != MAGIC {
                fail("BAD HEADER MAGIC (underflow/чужой указатель)", ptr as usize, layout.size());
            }
            let tail = (ptr as usize + (*hdr).size) as *const u64;
            if *tail != TAIL_MAGIC {
                // ПЕРЕПОЛНЕНИЕ БЛОКА - наш главный подозреваемый (zero-writer).
                let mut extra = [0u8; 8];
                let tail_ptr = (ptr as usize + (*hdr).size) as *const u8;
                for k in 0..8 {
                    extra[k] = *tail_ptr.add(k);
                }
                crate::vga::serial_write_atomic("\n[ralloc] OVERFLOW detected! ptr=");
                crate::vga::serial_u64(ptr as usize as u64);
                crate::vga::serial_write_atomic(" size=");
                crate::vga::serial_u64((*hdr).size as u64);
                crate::vga::serial_write_atomic(" tail=");
                crate::vga::serial_u64(*tail);
                crate::vga::serial_write_atomic(" bytes=");
                for k in 0..8 {
                    crate::vga::serial_u64(extra[k] as u64);
                    crate::vga::serial_write_atomic(" ");
                }
                crate::vga::serial_write_atomic("\n");
                fail("OVERFLOW", ptr as usize, (*hdr).size);
            }
            INNER.dealloc(
                (*hdr).base,
                Layout::from_size_align((*hdr).cap, 1).unwrap(),
            );
        })
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ptr.is_null() {
            return self.alloc(Layout::from_size_align(new_size, layout.align()).unwrap());
        }
        // Проверяем старый блок (канарейки) ДО копирования.
        let hdr = ptr.sub(HDR_SIZE) as *mut Header;
        if (*hdr).magic != MAGIC {
            fail("BAD OLD HEADER MAGIC on realloc", ptr as usize, layout.size());
        }
        let new_align = layout.align();
        let new_ptr = self.alloc(Layout::from_size_align(new_size, new_align).unwrap());
        if new_ptr.is_null() {
            return core::ptr::null_mut();
        }
        let copy = core::cmp::min((*hdr).size, new_size);
        core::ptr::copy_nonoverlapping(ptr, new_ptr, copy);
        self.dealloc(ptr, layout);
        new_ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = self.alloc(layout);
        if !p.is_null() {
            core::ptr::write_bytes(p, 0, layout.size());
        }
        p
    }
}
