use crate::scheduler;
use crate::sysinfo;
use crate::vfs::EntryType;
use crate::vga::Writer;
use core::cmp;

#[cfg(debug_assertions)]
fn dbg(msg: &str) {
    let ticks = crate::scheduler::ticks();
    for b in msg.bytes() {
        crate::vga::serial_putchar(b);
    }
    for b in alloc::format!(" [{:?}]\n", ticks).bytes() {
        crate::vga::serial_putchar(b);
    }
}

/// Опрос клавиатуры, пока шелл ждёт foreground-программу.
///
/// Возвращает true — пора убить программу. Сейчас это только Ctrl+C:
/// программы типа snake/web/vita Ctrl+C не понимают, поэтому перехватываем
/// его в IRQ-обработчике клавиатуры (см. keyboard::HOTKEY_CTRL_C).
///
/// Переключение рабочих столов (Ctrl+Shift+1..4) здесь не прерывает
/// ожидание — стол меняется, а программа просто засыпает вместе с ним.
fn foreground_poll(_pid: usize) -> bool {
    if crate::keyboard::take_hotkey(crate::keyboard::HOTKEY_CTRL_C) {
        return true;
    }
    if crate::keyboard::take_hotkey(crate::keyboard::HOTKEY_DESK) {
        let target = crate::keyboard::desk_target();
        crate::desk::activate(target);
    }
    false
}

/// Готовит адресное пространство программы и запускает её в ring 3.
/// ELF-файл (магия \x7fELF) грузится сегментами PT_LOAD по e_entry; иначе
/// данные считаются legacy flat .bin и копируются целиком на USER_START.
///
/// Регистры при входе:
///   ELF : rdi=argc, rsi=&argv[0], rdx=vga_offset
///   .bin: rdi=vga_offset, rsi=rdx=0 (старая конвенция)
///
/// Возвращает PID или None (нет свободного слота / битый образ / нет памяти).
pub fn launch_user(name: &str, data: &[u8], argv: &[&str], vga_offset: u64) -> Option<usize> {
    if !crate::scheduler::slot_free() {
        return None;
    }
    let mut space = crate::paging::create_address_space()?;
    let elf = crate::elf::is_elf(data);
    let entry = if elf {
        match crate::elf::load_elf(&mut space, data) {
            Ok(e) => e,
            Err(_) => {
                crate::paging::destroy_address_space(&mut space);
                return None;
            }
        }
    } else if crate::paging::map_user_image(&mut space, data).is_err() {
        crate::paging::destroy_address_space(&mut space);
        return None;
    } else {
        crate::paging::USER_START
    };
    let stack = match crate::paging::setup_user_stack(&mut space, argv) {
        Ok(s) => s,
        Err(_) => {
            crate::paging::destroy_address_space(&mut space);
            return None;
        }
    };
    let (rdi, rsi, rdx) = if elf {
        (stack.argc, stack.argv, vga_offset)
    } else {
        (vga_offset, 0, 0)
    };
    crate::scheduler::spawn_user(name, space, entry, stack.rsp, rdi, rsi, rdx)
}

/// Запускает .bin-программу в foreground (ring 3, собственное адресное
/// пространство).
///
/// VFS_LOCK берётся только на время собственных операций ядра (чтение файла,
/// подготовка shared memory для vita/help, сохранение vita). Сама программа
/// исполняется БЕЗ лока: она может обращаться к VFS через ABI-функции
/// (with_vfs), а лок нереентерабельный. Вызывается из шелла без удержания
/// VFS_LOCK (шелль освобождает его перед запуском программы).
pub fn run_program(writer: &mut Writer, args: &[&str], mem: sysinfo::MemInfo) -> bool {
    if args.is_empty() {
        return false;
    }

    let path = args[0];
    let name = path.rsplit('/').next().unwrap_or(path);
    let binpath = if path.starts_with('/') {
        alloc::string::String::from(path)
    } else {
        alloc::format!("/bin/{}", path)
    };

    let data = scheduler::with_vfs(|vfs| vfs.cat(&binpath).map(|d| d.to_vec()));
    if let Some(data) = data {
        let cpu = sysinfo::CpuInfo::detect();
        sysinfo::SysInfo::write_to_memory(&cpu, mem);

        if name == "vita" {
            let filename = args.get(1).copied().unwrap_or("untitled");
            let file_data: alloc::vec::Vec<u8> =
                scheduler::with_vfs(|vfs| vfs.cat(filename).map(|d| d.to_vec()))
                    .unwrap_or_default();
            let len = cmp::min(file_data.len(), 4096);
            unsafe {
                core::ptr::write_volatile(0x6000 as *mut u32, 0u32);
                core::ptr::write_volatile(0x6004 as *mut u32, len as u32);
                core::ptr::write_volatile(0x6008 as *mut u32, 4096u32);
                let fn_bytes = filename.as_bytes();
                let fn_max = 128;
                for i in 0..cmp::min(fn_bytes.len(), fn_max) {
                    core::ptr::write_volatile((0x600C + i) as *mut u8, fn_bytes[i]);
                }
                if fn_bytes.len() < fn_max {
                    core::ptr::write_volatile((0x600C + fn_bytes.len()) as *mut u8, 0u8);
                }
                core::ptr::copy_nonoverlapping(file_data.as_ptr(), 0x608C as *mut u8, len);
            }
        }

        if name == "help" {
            // Передаём в help список установленных программ через shared memory.
            let mut names: alloc::vec::Vec<alloc::string::String> = scheduler::with_vfs(|vfs| {
                let mut v: alloc::vec::Vec<alloc::string::String> = alloc::vec::Vec::new();
                if let Some(entries) = vfs.ls("/bin") {
                    for (prog_name, entry_type, _) in entries {
                        if entry_type == EntryType::File {
                            v.push(prog_name);
                        }
                    }
                }
                v
            });
            names.sort();
            let mut list: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
            for n in &names {
                list.extend_from_slice(n.as_bytes());
                list.push(b'\n');
            }
            list.push(0);
            let len = cmp::min(list.len(), 4095);
            unsafe {
                core::ptr::copy_nonoverlapping(list.as_ptr(), 0x608C as *mut u8, len);
                core::ptr::write_volatile((0x608C + len) as *mut u8, 0u8);
            }
        }

        // Общий протокол: программа может сообщить ядру, куда поставить курсор
        // после выхода (EXIT_ROW < 0 - не трогать позицию).
        unsafe {
            core::ptr::write_volatile(0x708C as *mut i32, -1);
            core::ptr::write_volatile(0x7090 as *mut i32, 0);
        }

        // Клавиатура: сбрасываем буфер сканкодов, чтобы foreground-программа не
        // получила остатки (break-коды Enter и т.п.) от командной строки шелла.
        crate::keyboard::flush();
        // TCP-сокет не должен «протекать» между запусками программ.
        crate::net::tcp_reset();

        crate::progabi::install();

        // argv[0] — полный путь, дальше — аргументы командной строки.
        let mut argv: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(args.len());
        argv.push(binpath.as_str());
        for a in &args[1..] {
            argv.push(a);
        }

        let vga_offset = writer.vga_offset() as u64;
        let pid = match launch_user(name, &data, &argv, vga_offset) {
            Some(pid) => pid,
            None => {
                writer.write_string("exec: cannot load (no free task slot or out of memory)\n");
                return true;
            }
        };

        dbg("exec: spawned, waiting");
        crate::desk::set_fg(pid);
        let interrupted = scheduler::wait_for_polled(pid, foreground_poll);
        crate::desk::clear_fg();
        dbg("exec: wait done");

        if interrupted {
            // Ctrl+C: kill() уже снял задачу, reap'уть нечего — слот и
            // адресное пространство освобождены внутри kill.
            writer.write_string(&alloc::format!("\n^C {} terminated\n", name));
            return true;
        }

        let exit_row = unsafe { core::ptr::read_volatile(0x708C as *const i32) };
        if exit_row >= 0 {
            let exit_col = unsafe { core::ptr::read_volatile(0x7090 as *const i32) };
            writer.set_cursor(exit_row as usize, exit_col as usize);
        }

        if name == "vita" {
            let dirty = unsafe { core::ptr::read_volatile(0x6000 as *mut u32) };
            if dirty != 0 {
                let new_size = unsafe { core::ptr::read_volatile(0x6004 as *mut u32) };
                let new_size = cmp::min(new_size, 4096) as usize;
                let fn_ptr = 0x600C as *const u8;
                let fn_len = (0..128)
                    .find(|&i| unsafe { core::ptr::read_volatile(fn_ptr.add(i)) } == 0)
                    .unwrap_or(128);
                let fn_slice = unsafe { core::slice::from_raw_parts(fn_ptr, fn_len) };
                let fname = core::str::from_utf8(fn_slice).unwrap_or("untitled");
                if new_size > 0 {
                    let mut new_data = alloc::vec![0u8; new_size];
                    unsafe {
                        core::ptr::copy_nonoverlapping(
                            0x608C as *const u8,
                            new_data.as_mut_ptr(),
                            new_size,
                        );
                    }
                    scheduler::with_vfs(|vfs| {
                        let _ = vfs.write_file(fname, &new_data);
                    });
                }
            }
        }

        scheduler::reap(pid);
        writer.write_string("\n");
        true
    } else {
        false
    }
}
