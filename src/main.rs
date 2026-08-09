#![no_std]
#![no_main]
#![feature(default_alloc_error_handler)]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::panic::PanicInfo;

mod disk;
mod exec;
mod fs;
mod init;
mod interrupts;
mod keyboard;
mod memory;
mod paging;
mod progabi;
mod programs;
mod scheduler;
mod shell;
mod syscall;
mod sysinfo;
mod tasks;
mod vfs;
mod vga;

use linked_list_allocator::LockedHeap;

#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

#[no_mangle]
pub extern "C" fn kernel_main(_magic: u64, mb_info_ptr: u64) -> ! {
    let mut writer = vga::Writer::new();
    vga::set_80x30();
    vga::clear_screen_com1();
    writer.clear_screen();

    {
        use x86_64::instructions::port::Port;
        let mut crt_idx: Port<u8> = Port::new(0x3D4);
        let mut crt_dat: Port<u8> = Port::new(0x3D5);
        let mut rd = |reg: u8| -> u8 {
            unsafe {
                crt_idx.write(reg);
                crt_dat.read()
            }
        };
        let _ = rd(0);
        vga::serial_write("[CRTC]");
        for reg in [0x0Au8, 0x0B, 0x0E, 0x0F, 0x09] {
            let v = rd(reg);
            let hx = b"0123456789ABCDEF";
            vga::serial_putchar(hx[(v >> 4) as usize]);
            vga::serial_putchar(hx[(v & 0xF) as usize]);
            vga::serial_putchar(b' ');
        }
        vga::serial_write("\n");
    }

    writer.write_string("=================================\n");
    writer.write_string("  VitaminOS26 - Welcome!\n");
    writer.write_string("=================================\n");

    let (phys_mem_offset, memory_regions) = unsafe { memory::boot_params(mb_info_ptr) };

    memory::init_allocator(memory_regions);
    let mut mapper = unsafe { memory::init(phys_mem_offset) };

    match memory::init_heap(&mut mapper, unsafe { &mut memory::FRAME_ALLOC }) {
        Ok(()) => unsafe {
            ALLOCATOR
                .lock()
                .init(memory::HEAP_START as *mut u8, memory::HEAP_SIZE);
        },
        Err(_) => {
            writer.write_string("KERNEL PANIC: heap initialization failed\n");
            halt_loop();
        }
    }

    paging::self_test(&mut writer);

    fs::init_filesystem();

    let mem = sysinfo::MemInfo::from_memory_map(memory_regions);
    let mut vfs = vfs::Vfs::new();

    if let Some(data) = disk::load() {
        if vfs.rebuild_from(&data) {
            vfs.install_bin();
            writer.write_string("Loaded filesystem from disk.\n");
        } else {
            vfs.init();
            writer.write_string("Disk snapshot invalid, rebuilt initial filesystem.\n");
        }
    } else {
        vfs.init();
        if disk::present() {
            let _ = disk::flush(&vfs);
            writer.write_string("Initialized empty disk.\n");
        }
    }

    scheduler::init();
    scheduler::set_vfs(&mut vfs);
    interrupts::init();
    writer.write_string("Multitasking ready (PIT 100 Hz).\n");

    init::boot(&mut writer);

    shell::run_shell(&mut writer, mem, &mut vfs);
}

fn halt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut writer = vga::Writer::new();
    writer.write_string("KERNEL PANIC: ");

    if let Some(location) = info.location() {
        writer.write_string(location.file());
    } else {
        writer.write_string("Kernel panic occurred");
    }
    writer.write_string("\n");

    halt_loop()
}
