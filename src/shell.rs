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
        {
            // История — best effort: не блокируемся на VFS_LOCK, если его
            // держит фоновая задача. Иначе шелл не смог бы выполнить `kill`
            // для зависшего владельца лока и завис бы сам (CRITICAL-3).
            if let Some(_g) = crate::scheduler::try_vfs_lock() {
                append_history(vfs, input.as_str());
            }
        }
        handle_command(input.as_str(), writer, mem, vfs);
    }
}

/// Разбирает и исполняет командную строку. Поддерживает цепочки `&&`:
/// сегменты выполняются по порядку, первый неуспех останавливает цепочку.
fn handle_command(command: &str, writer: &mut Writer, mem: sysinfo::MemInfo, vfs: &mut Vfs) {
    let raw = tokenize(command.trim());
    if raw.is_empty() {
        return;
    }

    let mut start = 0;
    for i in 0..=raw.len() {
        if i == raw.len() || raw[i] == "&&" {
            let seg = &raw[start..i];
            start = i + 1;
            if seg.is_empty() {
                continue;
            }
            if !run_one(seg, writer, mem, vfs) {
                break;
            }
        }
    }
}

/// Исполняет один сегмент (без "&&"). false - команда не удалась.
fn run_one(raw: &[&str], writer: &mut Writer, mem: sysinfo::MemInfo, vfs: &mut Vfs) -> bool {
    // Перенаправления: `> file` (перезапись), `>> file` (дозапись),
    // `< file` (ввод из файла). Операторы вырезаются из args и обрабатываются
    // отдельно для команд, поддерживающих редирект (echo, cat).
    let mut out_overwrite: Option<&str> = None;
    let mut out_append: Option<&str> = None;
    let mut in_file: Option<&str> = None;
    let mut args: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            ">" => {
                out_overwrite = raw.get(i + 1).copied();
                i += 2;
            }
            ">>" => {
                out_append = raw.get(i + 1).copied();
                i += 2;
            }
            "<" => {
                in_file = raw.get(i + 1).copied();
                i += 2;
            }
            a => {
                args.push(a);
                i += 1;
            }
        }
    }

    // Foreground-программы (`run X` и неизвестные команды) исполняются БЕЗ
    // VFS_LOCK: лок нереентерабельный, а программа сама читает/пишет файлы
    // через ABI (with_vfs). Внутренние команды шелла — под локом.
    if let ["run", rest @ ..] = args.as_slice() {
        let run_args: Vec<&str> = rest.to_vec();
        let ok = !run_args.is_empty() && exec::run_program(writer, &run_args, mem);
        if !ok {
            writer.write_string("run: file not found\n");
        }
        let _g = crate::scheduler::vfs_lock();
        crate::vitafs::sync_all();
        return ok;
    }

    // `bg` тоже запускается БЕЗ VFS_LOCK: бинарник читается через with_vfs
    // (лок только на время чтения), а сам spawn (создание адресного
    // пространства) не трогает VFS. Иначе фоновая задача в этот момент
    // крутилась бы на VFS_LOCK и отъедала у шелла CPU.
    if let ["bg", name, args @ ..] = args.as_slice() {
        bg_task(writer, name, args);
        let _g = crate::scheduler::vfs_lock();
        crate::vitafs::sync_all();
        return true;
    }

    // `kill` исполняется БЕЗ VFS_LOCK: жертва может держать лок, а лок
    // нереентерабельный — под ним шелл не смог бы убить владельца и система
    // зависла бы навсегда (CRITICAL-3). kill() сам принудительно снимает лок
    // жертвы. Ветку размещаем ДО захвата _vfs_guard.
    if let ["kill", pid] = args.as_slice() {
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
        return true;
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
            let out = out_append.or(out_overwrite);
            if let Some(file) = out {
                let msg = rest.join(" ") + "\n";
                let res = if out_append.is_some() {
                    vfs.append(file, msg.as_bytes())
                } else {
                    vfs.write_file(file, msg.as_bytes())
                };
                if res.is_err() {
                    writer.write_string("echo: write failed\n");
                }
            } else {
                writer.write_string(&rest.join(" "));
                writer.write_string("\n");
            }
        }
        ["cat", files @ ..] => {
            let mut files: Vec<&str> = files.to_vec();
            if files.is_empty() {
                if let Some(f) = in_file {
                    files.push(f);
                }
            }
            let out = out_append.or(out_overwrite);
            if let Some(out_path) = out {
                let mut out_data: Vec<u8> = Vec::new();
                let mut ok = true;
                for file in &files {
                    match vfs.cat(file) {
                        Some(d) => out_data.extend_from_slice(&d),
                        None => {
                            writer.write_string("cat: no such file: ");
                            writer.write_string(file);
                            writer.write_string("\n");
                            ok = false;
                        }
                    }
                }
                if ok {
                    let res = if out_append.is_some() {
                        vfs.append(out_path, &out_data)
                    } else {
                        vfs.write_file(out_path, &out_data)
                    };
                    if res.is_err() {
                        writer.write_string("cat: write failed\n");
                    }
                }
            } else {
                for file in &files {
                    fs::read_file(writer, vfs, file);
                }
            }
        }
        ["history"] => {
            if let Some(data) = vfs.cat("/var/log/history.log") {
                let mut n = 1;
                let mut start = 0;
                for (i, b) in data.iter().enumerate() {
                    if *b == b'\n' {
                        writer.write_string(&alloc::format!("{:>3}  ", n));
                        writer.write_string(core::str::from_utf8(&data[start..i]).unwrap_or(""));
                        writer.write_string("\n");
                        n += 1;
                        start = i + 1;
                    }
                }
            }
        }
        ["ps"] => {
            ps(writer);
        }
        ["init", sub @ ..] => {
            crate::init::cmd(writer, vfs, sub);
        }
        ["ln", "-s", target, link] => {
            match vfs.symlink(target, link) {
                Ok(()) => {}
                Err(()) => writer.write_string("ln: cannot create symlink\n"),
            }
        }
        ["chroot", path] => {
            if vfs.chroot(path) {
                writer.write_string("chroot: new root is ");
                writer.write_string(path);
                writer.write_string("\n");
            } else {
                writer.write_string("chroot: not a directory\n");
            }
        }
        ["ioctl", sub, rest @ ..] => match *sub {
            // Управление терминалом. Те же номера команд доступны C-программам
            // через int 0x80 / трамплин ioctl() (см. src/syscall.rs).
            "cursor" => {
                let (row, col) = crate::vga::hw_cursor_pos();
                writer.write_string(&alloc::format!("cursor: row={} col={}\n", row, col));
            }
            "goto" => {
                let r: usize = rest.first().and_then(|s| s.parse().ok()).unwrap_or(0);
                let c: usize = rest.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
                crate::vga::set_hw_cursor(r, c);
            }
            "clear" => {
                writer.clear_screen();
            }
            "size" => {
                let (row, _) = crate::vga::hw_cursor_pos();
                let _ = row;
                writer.write_string(&alloc::format!(
                    "screen: {}x{}\n",
                    crate::vga::SCREEN_WIDTH,
                    crate::vga::SCREEN_HEIGHT
                ));
            }
            "echo" => match rest.first() {
                Some(&"on") => {
                    crate::keyboard::echo_set(true);
                    writer.write_string("echo: on\n");
                }
                Some(&"off") => {
                    // Подсказку пишем ДО отключения - дальше шелл молчит,
                    // пока не наберёшь вслепую: ioctl echo on
                    writer.write_string("echo: off (restore: ioctl echo on)\n");
                    crate::keyboard::echo_set(false);
                }
                _ => {
                    writer.write_string(&alloc::format!(
                        "echo: {}\n",
                        if crate::keyboard::echo_get() { "on" } else { "off" }
                    ));
                }
            },
            _ => {
                writer.write_string("ioctl: unknown op (cursor|goto|clear|size|echo)\n");
            }
        },
        _ => {
            // Неизвестная команда: пробуем .bin-программу в foreground.
            // VFS_LOCK освобождаем — программа сама работает с VFS через ABI.
            drop(_vfs_guard);
            let ok = exec::run_program(writer, &args, mem);
            if !ok {
                writer.write_string("Unknown command: ");
                writer.write_string(args[0]);
                writer.write_string("\nType 'help' for available commands.\n");
            }
            let _g = crate::scheduler::vfs_lock();
            crate::vitafs::sync_all();
            return ok;
        }
    }

    crate::vitafs::sync_all();
    true
}

fn tokenize(command: &str) -> Vec<&str> {
    command.split_whitespace().collect()
}

/// Дописывает команду в /var/log/history.log (с ограничением размера).
fn append_history(vfs: &mut Vfs, line: &str) {
    if line.trim().is_empty() {
        return;
    }
    let mut out = vfs
        .cat("/var/log/history.log")
        .map(|d| d.to_vec())
        .unwrap_or_default();
    if out.len() + line.len() + 1 > 8192 {
        out = out[out.len().saturating_sub(4096)..].to_vec();
    }
    out.extend_from_slice(line.as_bytes());
    out.push(b'\n');
    let _ = vfs.write_file("/var/log/history.log", &out);
}

fn bg_task(writer: &mut Writer, name: &str, args: &[&str]) {
    if let Some(pid) = crate::tasks::spawn(name, args) {
        writer.write_string("Started background task ");
        writer.write_string(name);
        writer.write_string(&alloc::format!(" (pid {})\n", pid));
        return;
    }
    // Программа из /bin (ELF или legacy raw machine code). Работает в фоне в
    // ring 3, в собственном адресном пространстве; может рисовать в VGA — с
    // одним экраном это пересекается с выводом шелла.
    let binpath = alloc::format!("/bin/{}", name);
    let code = crate::scheduler::with_vfs(|vfs| vfs.cat(&binpath).map(|d| d.to_vec()));
    if let Some(code) = code {
        crate::progabi::install();
        let argv = [binpath.as_str()];
        match crate::exec::launch_user(name, &code, &argv, 0) {
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