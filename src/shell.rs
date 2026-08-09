use crate::exec;
use crate::fs;
use crate::keyboard;
use crate::sysinfo;
use crate::vfs::Vfs;
use crate::vga::{COLOR_LIGHT_CYAN, COLOR_WHITE, Writer};
use alloc::vec::Vec;
pub fn run_shell(writer: &mut Writer, mem: sysinfo::MemInfo, vfs: &mut Vfs) -> ! {
    let mut history = keyboard::History::new();
    loop {
        writer.set_color(COLOR_LIGHT_CYAN);
        writer.write_string("vitamin_os26");
        writer.set_color(COLOR_WHITE);
        writer.write_string(":");
        writer.set_color(COLOR_LIGHT_CYAN);
        writer.write_string(vfs.pwd().as_str());
        writer.set_color(COLOR_WHITE);
        writer.write_string("$ ");
        writer.set_color(COLOR_WHITE);

        let input = keyboard::read_line(writer, &mut history);
        #[cfg(debug_assertions)]
        {
            let msg = alloc::format!("shell got line: {:?}\n", input);
            for b in msg.bytes() {
                crate::vga::serial_putchar(b);
            }
        }
        handle_command(input.as_str(), writer, mem, vfs);
    }
}

fn handle_command(command: &str, writer: &mut Writer, mem: sysinfo::MemInfo, vfs: &mut Vfs) {
    let args = tokenize(command.trim());

    if args.is_empty() {
        return;
    }

    // Foreground-программы (`run X` и неизвестные команды) исполняются БЕЗ
    // VFS_LOCK: лок нереентерабельный, а программа сама читает/пишет файлы
    // через ABI (with_vfs). Внутренние команды шелла — под локом.
    if let ["run", path] = args.as_slice() {
        let run_args = [*path];
        if !exec::run_program(writer, &run_args, mem) {
            writer.write_string("run: file not found: ");
            writer.write_string(path);
            writer.write_string("\n");
        }
        let _g = crate::scheduler::vfs_lock();
        let _ = crate::disk::flush(vfs);
        return;
    }

    // `bg` тоже запускается БЕЗ VFS_LOCK: бинарник читается через with_vfs
    // (лок только на время чтения), а сам spawn (создание адресного
    // пространства) не трогает VFS. Иначе фоновая задача в этот момент
    // крутилась бы на VFS_LOCK и отъедала у шелла CPU.
    if let ["bg", name, args @ ..] = args.as_slice() {
        bg_task(writer, name, args);
        let _g = crate::scheduler::vfs_lock();
        let _ = crate::disk::flush(vfs);
        return;
    }

    // Весь доступ к VFS из шелла сериализуется с фоновыми задачами.
    let _vfs_guard = crate::scheduler::vfs_lock();

    match args.as_slice() {
        ["clear"] => {
            writer.clear_screen();
        }
        ["version"] => {
            writer.write_string("VitaminOS26 v0.1.0\n");
        }
        ["ls", path] => {
            fs::list_files(writer, vfs, path);
        }
        ["ls"] => {
            fs::list_files(writer, vfs, vfs.pwd().as_str());
        }
        ["pwd"] => {
            writer.write_string(vfs.pwd().as_str());
            writer.write_string("\n");
        }
        ["cd", path] => {
            if fs::change_directory(vfs, path) {
                writer.write_string("");
            } else {
                writer.write_string("cd: no such file or directory: ");
                writer.write_string(path);
                writer.write_string("\n");
            }
        }
        ["mkdir", dirs @ ..] => {
            for dir in dirs {
                if let Err(_) = fs::make_directory(vfs, dir) {
                    writer.write_string("mkdir: cannot create directory '");
                    writer.write_string(dir);
                    writer.write_string("'\n");
                }
            }
        }
        ["rmdir", dirs @ ..] => {
            for dir in dirs {
                if let Err(_) = fs::remove_directory(vfs, dir) {
                    writer.write_string("rmdir: failed to remove '");
                    writer.write_string(dir);
                    writer.write_string("'\n");
                }
            }
        }
        ["touch", files @ ..] => {
            for file in files {
                if let Err(_) = fs::create_file(vfs, file) {
                    writer.write_string("touch: cannot touch '");
                    writer.write_string(file);
                    writer.write_string("'\n");
                }
            }
        }
        ["rm", files @ ..] => {
            for file in files {
                if let Err(_) = fs::remove_file(vfs, file) {
                    writer.write_string("rm: cannot remove '");
                    writer.write_string(file);
                    writer.write_string("'\n");
                }
            }
        }
        ["cat", files @ ..] => {
            for file in files {
                fs::read_file(writer, vfs, file);
            }
        }
        ["cp", src, dst] => {
            if let Err(_) = vfs.copy(src, dst) {
                writer.write_string("cp: cannot copy '");
                writer.write_string(src);
                writer.write_string("' to '");
                writer.write_string(dst);
                writer.write_string("'\n");
            }
        }
        ["mv", src, dst] => {
            if let Err(_) = vfs.mv(src, dst) {
                writer.write_string("mv: cannot move '");
                writer.write_string(src);
                writer.write_string("' to '");
                writer.write_string(dst);
                writer.write_string("'\n");
            }
        }
        ["find", name] => {
            let results = vfs.find(name);
            if results.is_empty() {
                writer.write_string("find: no such file: ");
                writer.write_string(name);
                writer.write_string("\n");
            } else {
                for path in results {
                    writer.write_string(&path);
                    writer.write_string("\n");
                }
            }
        }
        ["echo", rest @ ..] => {
            if rest.is_empty() {
                writer.write_string("\n");
            } else if rest.contains(&">") {
                let echo_args: Vec<&str> = rest.to_vec();
                if let Err(_) = fs::write_to_file(vfs, &echo_args) {
                    writer.write_string("echo: write failed\n");
                }
            } else {
                writer.write_string(&rest.join(" "));
                writer.write_string("\n");
            }
        }
        ["ps"] => {
            ps(writer);
        }
        ["kill", pid] => {
            match crate::tasks::parse_u64(Some(pid)) {
                Some(p) if p > 0 => {
                    if crate::scheduler::kill(p as usize) {
                        writer.write_string("Killed task ");
                        writer.write_string(pid);
                        writer.write_string("\n");
                    } else {
                        writer.write_string("kill: no such task: ");
                        writer.write_string(pid);
                        writer.write_string("\n");
                    }
                }
                _ => {
                    writer.write_string("kill: bad pid: ");
                    writer.write_string(pid);
                    writer.write_string("\n");
                }
            }
        }
        ["init", sub @ ..] => {
            crate::init::cmd(writer, vfs, sub);
        }
        _ => {
            // Неизвестная команда: пробуем .bin-программу в foreground.
            // VFS_LOCK освобождаем — программа сама работает с VFS через ABI.
            drop(_vfs_guard);
            if !exec::run_program(writer, &args, mem) {
                writer.write_string("Unknown command: ");
                writer.write_string(args[0]);
                writer.write_string("\nType 'help' for available commands.\n");
            }
            let _g = crate::scheduler::vfs_lock();
            let _ = crate::disk::flush(vfs);
            return;
        }
    }

    let _ = crate::disk::flush(vfs);
}

fn tokenize(command: &str) -> Vec<&str> {
    command.split_whitespace().collect()
}

fn bg_task(writer: &mut Writer, name: &str, args: &[&str]) {
    if let Some(pid) = crate::tasks::spawn(name, args) {
        writer.write_string("Started background task ");
        writer.write_string(name);
        writer.write_string(&alloc::format!(" (pid {})\n", pid));
        return;
    }
    // Программа из /bin (raw machine code). Работает в фоне в ring 3, в
    // собственном адресном пространстве; может рисовать в VGA — с одним
    // экраном это пересекается с выводом шелла.
    let binpath = alloc::format!("/bin/{}", name);
    let code = crate::scheduler::with_vfs(|vfs| vfs.cat(&binpath).map(|d| d.to_vec()));
    if let Some(code) = code {
        crate::progabi::install();
        match crate::exec::launch_user(name, &code, 0) {
            Some(pid) => {
                writer.write_string("Started background task ");
                writer.write_string(name);
                writer.write_string(&alloc::format!(" (pid {})\n", pid));
            }
            None => writer.write_string("bg: no free task slots\n"),
        }
    } else {
        writer.write_string("bg: unknown program: ");
        writer.write_string(name);
        writer.write_string("\n");
    }
}

fn ps(writer: &mut Writer) {
    let mut procs = Vec::new();
    crate::scheduler::list(&mut procs);
    if procs.is_empty() {
        writer.write_string("no tasks\n");
        return;
    }
    writer.write_string("  PID  NAME                STATE       TICKS\n");
    for p in &procs {
        let name_str = core::str::from_utf8(&p.name).unwrap_or("?");
        let name_str = name_str.split('\0').next().unwrap_or("");
        writer.write_string(&alloc::format!(
            "{:>5}  {:<20}  {:<10}  {}\n",
            p.pid,
            name_str,
            p.state,
            p.ticks
        ));
    }
}