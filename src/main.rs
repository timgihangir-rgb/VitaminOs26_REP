#![no_std]
#![no_main]
#![feature(default_alloc_error_handler)]
#![feature(abi_x86_interrupt)]

extern crate alloc;

use core::panic::PanicInfo;

mod bcache;
mod blockdev;
mod cursor;
mod devices;
mod elf;
mod exec;
mod fs;
mod init;
mod interrupts;
mod keyboard;
mod memory;
mod net;
mod paging;
mod pci;
mod progabi;
mod ralloc;
mod rtc;
mod scheduler;
mod shell;
mod splash;
mod syscall;
mod sysinfo;
mod tasks;
mod vfs;
mod vga;
mod vitafs;
mod wal;

// Глобальный аллокатор - канареечная обёртка над linked_list_allocator
// (src/ralloc.rs): ловит переполнение блоков (zero-writer) и сохраняет
// атомарность операций по прерываниям.
use ralloc::Allocator;

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

#[no_mangle]
pub extern "C" fn kernel_main(_magic: u64, mb_info_ptr: u64) -> ! {
    let mut writer = vga::Writer::new();
    vga::set_80x30();
    vga::clear_screen_com1();
    writer.clear_screen();

    let (phys_mem_offset, memory_regions) = unsafe { memory::boot_params(mb_info_ptr) };

    memory::init_allocator(memory_regions);
    let mut mapper = unsafe { memory::init(phys_mem_offset) };

    match memory::init_heap(&mut mapper, unsafe { &mut memory::FRAME_ALLOC }) {
        Ok(()) => unsafe {
            Allocator::init(memory::HEAP_START as *mut u8, memory::HEAP_SIZE);
        },
        Err(_) => {
            writer.write_string("KERNEL PANIC: heap initialization failed\n");
            halt_loop();
        }
    }

    // Самотест paging печатает на экран до сплэша — draw() затрёт его.
    paging::self_test(&mut writer);

    fs::init_filesystem();

    // Загрузочный экран: баннер + статусы подсистем.
    splash::draw(&mut writer);

    let mem = sysinfo::MemInfo::from_memory_map(memory_regions);
    let mut vfs = vfs::Vfs::new();

    crate::vga::serial_write_atomic("[M] pre-mount\n");
    // Фаза 3: единственный источник истины - VITAFS на диске.
    if vitafs::mount_or_format() {
        splash::step(&mut writer, "vitafs filesystem mounted");
    } else {
        splash::fail(&mut writer, "vitafs: no disk - filesystem disabled");
    }

    scheduler::init();
    scheduler::set_vfs(&mut vfs);
    interrupts::init();
    splash::step(&mut writer, "multitasking ready (PIT 100 Hz)");
    crate::vga::serial_write_atomic("[M] pre-boot\n");

    // Диагностика этапа 0 (блочный слой, кэш, RTC) - в serial, экран не пачкаем.
    #[cfg(debug_assertions)]
    stage0_diag();

    init::boot(&mut writer);
    crate::vga::serial_write_atomic("[M] post-boot\n");
    splash::wait_enter(&mut writer);
    splash::finish(&mut writer);
    shell::run_shell(&mut writer, mem, &mut vfs);
    crate::vga::serial_write_atomic("[M] shell-exit\n");
}

fn halt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}

#[cfg(debug_assertions)]
fn stage0_diag() {
    use core::fmt::Write;
    // RTC: unix-time должен быть разумным (2020..2100).
    let t = rtc::now_unix();
    let mut msg = alloc::format!("stage0: rtc unix={} ", t);
    if (1_577_836_800..=4_102_444_800).contains(&t) {
        msg.push_str("OK");
    } else {
        msg.push_str("BAD");
    }
    {
        let imr = unsafe { x86_64::instructions::port::Port::<u8>::new(0x21).read() };
        msg.push_str(" | PIC IMR=");
        msg.push_str(&alloc::format!("{:02x}", imr));
        // Дисковые самотесты vitafs (bm/inode/icache/dir/fileops) УБРАНЫ из
    // загрузки: они пишут в реальную ФС и их "восстановление" неполно -
    // оставляли живой inode 1000 + блок 130 при откаченных битмапах, что
    // давало "[fsck] fixed" каждую загрузку и рассинхрон bcache/диска.
    // Запускать вручную при разработке ФС.
    msg.push_str(" | blockdev selftest=");
    msg.push_str(if blockdev::selftest() { "OK" } else { "FAIL" });
    msg.push_str(" bcache selftest=");
    msg.push_str(if bcache::selftest() { "OK" } else { "FAIL" });
    msg.push_str(" vitafs-sb selftest=");
    msg.push_str(if vitafs::selftest() { "OK" } else { "FAIL" });
    // Сетевой стек: карта (rtl8139) в этом прогоне может отсутствовать —
    // это диагностика, а не ошибка.
    msg.push_str(" | pci=");
    match crate::pci::find_device(0x10EC, 0x8139) {
        Some(d) => {
            msg.push_str(&alloc::format!(
                "rtl8139 {:02x}:{:02x}:{:02x} bar0={:08x}",
                d.bus, d.slot, d.func, d.bar0
            ));
            msg.push_str(" | nic=");
            msg.push_str(if crate::net::init() { "ready" } else { "init-fail" });
        }
        None => msg.push_str("no-rtl8139"),
    }
    }
    let st = blockdev::stats();
    let _ = write!(
        msg,
        " (rd={} wr={} err={})",
        st.reads, st.writes, st.errors
    );
    bcache::stats_line(&mut msg);
    for b in msg.bytes() {
        vga::serial_putchar(b);
    }
    vga::serial_putchar(b'\n');
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut writer = vga::Writer::new();
    writer.write_string("KERNEL PANIC: ");

    if let Some(location) = info.location() {
        writer.write_string(location.file());
        writer.write_string(":");
        writer.write_string(&alloc::format!("{}", location.line()));
    } else {
        writer.write_string("Kernel panic occurred");
    }
    writer.write_string("\n");

    // Дублируем на COM1: в headless-прогоне (-display none) VGA не виден.
    let mut msg = alloc::string::String::from("KERNEL PANIC\n");
    if let Some(location) = info.location() {
        msg.push_str(&alloc::format!("at {}:{}\n", location.file(), location.line()));
    }
    // Сообщение паники - главное для диагноза (что именно упало).
    let payload = info.payload();
    if let Some(s) = payload.downcast_ref::<&str>() {
        msg.push_str("msg: ");
        msg.push_str(s);
        msg.push_str("\n");
    } else if let Some(s) = payload.downcast_ref::<alloc::string::String>() {
        msg.push_str("msg: ");
        msg.push_str(s);
        msg.push_str("\n");
    }
    for b in msg.bytes() {
        vga::serial_putchar(b);
    }

    // Стоп в панике только для отладки: int3 без подключённого GDB —
    // молчаливое зависание. В debug-сборке сообщение уже напечатано
    // (VGA + COM1), дальше gdb получает полный контекст на int3.
    #[cfg(debug_assertions)]
    unsafe { core::arch::asm!("int3", options(nomem)) };

    halt_loop()
}
