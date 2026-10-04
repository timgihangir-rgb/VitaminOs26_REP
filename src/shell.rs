use crate::exec;
use crate::fs;
use crate::keyboard;
use crate::sysinfo;
use crate::vfs::Vfs;
use crate::vga::{COLOR_DARK_GREY, COLOR_LIGHT_CYAN, COLOR_LIGHT_GREEN, COLOR_WHITE, Writer};
use alloc::vec::Vec;
/// Восьмеричное число из строки ("755" -> 0o755). None, если в строке есть
/// что-то кроме цифр 0..7.
fn parse_octal(s: &str) -> Option<u16> {
    if s.is_empty() || s.len() > 6 {
        return None;
    }
    let mut v: u16 = 0;
    for b in s.bytes() {
        if !(b'0'..=b'7').contains(&b) {
            return None;
        }
        v = v.checked_mul(8)?.checked_add((b - b'0') as u16)?;
    }
    Some(v)
}

/// Отказ команды. Отказ по правам отличаем от прочих ошибок: «permission
/// denied» прямо подсказывает, что поможет sudo, а «no such file» - про
/// другое, и смешивать их в одну формулировку незачем.
fn refused(writer: &mut crate::vga::Writer, cmd: &str, action: &str, detail: &str) {
    if crate::vfs::permission_denied() {
        writer.write_string(cmd);
        writer.write_string(": permission denied: ");
        writer.write_string(detail);
        writer.write_string("\n");
    } else {
        writer.write_string(cmd);
        writer.write_string(": cannot ");
        writer.write_string(action);
        writer.write_string(" '");
        writer.write_string(detail);
        writer.write_string("'\n");
    }
}

pub fn run_shell(
    writer: &mut Writer,
    mem: sysinfo::MemInfo,
    vfs: &mut Vfs,
    desk_index: u8,
) -> ! {
    let mut history = keyboard::History::new();
    loop {
        // Номер рабочего стола — чтобы было видно, какой из четырёх
        // терминалов активен (переключение: Ctrl+Shift+1..4).
        writer.set_color(COLOR_DARK_GREY);
        writer.write_string("[");
        writer.set_color(COLOR_LIGHT_GREEN);
        writer.write_string(alloc::format!("{}", desk_index + 1).as_str());
        writer.set_color(COLOR_DARK_GREY);
        writer.write_string("] ");
        writer.set_color(COLOR_LIGHT_CYAN);
        writer.write_string("vitamin_os26");
        writer.set_color(COLOR_WHITE);
        writer.write_string(":");
        // Кто вошёл: иначе в приглашении не видно, под кем идёт работа.
        writer.set_color(COLOR_LIGHT_GREEN);
        writer.write_string(crate::user::current_name().as_str());
        writer.set_color(COLOR_WHITE);
        writer.write_string("@");
        writer.set_color(COLOR_LIGHT_CYAN);
        writer.write_string(vfs.pwd().as_str());
        writer.set_color(COLOR_WHITE);
        writer.write_string("$ ");
        writer.set_color(COLOR_WHITE);

        let input = keyboard::read_line(writer, &mut history);
        if input.is_empty() {
            // Пустая строка — и нажатие Ctrl+C: ничего не выполняем, но
            // и историю не портим.
            continue;
        }
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

    // sudo <команда>. Пароль проверяет настоящая программа /bin/sudo (она
    // одна умеет спросить пароль и не может быть подменена: /bin принадлежит
    // root), а сама команда выполняется уже с правами root.
    //
    // Права поднимаются на время одной команды и возвращаются обратно. Так
    // работают и встроенные команды шелла (ls, rm, cp), которых в /bin нет,
    // и настоящие программы (/bin/adduser) — последние дополнительно
    // запускаются от uid 0 через exec::run_program.
    if let ["sudo", rest @ ..] = args.as_slice() {
        if rest.is_empty() {
            writer.write_string("usage: sudo <command> [arguments...]\n");
            return false;
        }
        // Редиректы разобраны выше; собираем внутреннюю команду заново,
        // операторы можно поставить в конец - порядок для них безразличен.
        let mut inner: Vec<&str> = rest.to_vec();
        if let Some(f) = out_overwrite {
            inner.push(">");
            inner.push(f);
        }
        if let Some(f) = out_append {
            inner.push(">>");
            inner.push(f);
        }
        if let Some(f) = in_file {
            inner.push("<");
            inner.push(f);
        }
        // -v: только спросить пароль (sudo -v), без запуска команды.
        let asked = exec::run_program(writer, &["sudo", "-v"], mem);
        if !asked {
            writer.write_string("sudo: cannot run /bin/sudo\n");
            return false;
        }
        if exec::last_exit_code() != 0 {
            return false;
        }
        let saved = crate::user::current_uid();
        crate::user::set_current_uid(crate::user::ROOT_UID);
        let ok = run_one(inner.as_slice(), writer, mem, vfs);
        crate::user::set_current_uid(saved);
        let _g = crate::scheduler::vfs_lock();
        crate::vitafs::sync_all();
        return ok;
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

    // `ping` — сетевой стек: живёт на портах и в статической памяти ядра,
    // VFS не трогает. Выполняем БЕЗ VFS_LOCK (как `kill`): запрос может
    // ждать ARP/ICMP до пары секунд, и фоновые задачи с ФС не должны
    // крутиться на локе в это время.
    if let ["ping", addr] = args.as_slice() {
        crate::net::ping(writer, addr);
        let _g = crate::scheduler::vfs_lock();
        crate::vitafs::sync_all();
        return true;
    }

    // `top` VFS не трогает, а держит терминал минутами — значит, под
    // VFS_LOCK он столько же блокировал бы весь файловый доступ. Хуже: при
    // переключении рабочего стола top приостанавливается, его guard остаётся
    // лежать на стеке спящей задачи, и лок уже никто не снимет — все
    // файловые команды зависнут навсегда. Выполняем БЕЗ лока.
    if let ["top"] = args.as_slice() {
        crate::top::run(writer);
        let _g = crate::scheduler::vfs_lock();
        crate::vitafs::sync_all();
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
                if fs::make_directory(vfs, dir).is_err() {
                    refused(writer, "mkdir", "create directory", dir);
                }
            }
        }
        ["rmdir", dirs @ ..] => {
            for dir in dirs {
                if fs::remove_directory(vfs, dir).is_err() {
                    refused(writer, "rmdir", "remove directory", dir);
                }
            }
        }
        ["touch", files @ ..] => {
            for file in files {
                if fs::create_file(vfs, file).is_err() {
                    refused(writer, "touch", "touch", file);
                }
            }
        }
        ["rm", files @ ..] => {
            for file in files {
                if fs::remove_file(vfs, file).is_err() {
                    refused(writer, "rm", "remove", file);
                }
            }
        }
        ["cp", src, dst] => {
            if vfs.copy(src, dst).is_err() {
                if crate::vfs::permission_denied() {
                    writer.write_string("cp: permission denied: ");
                    writer.write_string(src);
                    writer.write_string("\n");
                } else {
                    writer.write_string("cp: cannot copy '");
                    writer.write_string(src);
                    writer.write_string("' to '");
                    writer.write_string(dst);
                    writer.write_string("'\n");
                }
            }
        }
        ["mv", src, dst] => {
            if vfs.mv(src, dst).is_err() {
                if crate::vfs::permission_denied() {
                    writer.write_string("mv: permission denied: ");
                    writer.write_string(src);
                    writer.write_string("\n");
                } else {
                    writer.write_string("mv: cannot move '");
                    writer.write_string(src);
                    writer.write_string("' to '");
                    writer.write_string(dst);
                    writer.write_string("'\n");
                }
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
                    refused(writer, "echo", "write to", file);
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
                        refused(writer, "cat", "write to", out_path);
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
            if vfs.symlink(target, link).is_err() {
                refused(writer, "ln", "create symlink", link);
            }
        }
        ["whoami"] => {
            writer.write_string(crate::user::current_name().as_str());
            writer.write_string("\n");
        }
        ["id"] => {
            let uid = crate::user::current_uid();
            writer.write_string(&alloc::format!(
                "uid={} ({})\n",
                uid,
                crate::user::current_name()
            ));
        }
        ["chown", path, uid] => match uid.parse::<u16>() {
            Ok(u) => {
                if vfs.chown(path, u).is_err() {
                    refused(writer, "chown", "change owner of", path);
                }
            }
            Err(_) => writer.write_string("chown: uid must be a number\n"),
        },
        ["chmod", path, mode] => match parse_octal(mode) {
            Some(m) => {
                if vfs.chmod(path, m).is_err() {
                    refused(writer, "chmod", "change mode of", path);
                }
            }
            None => writer.write_string("chmod: mode must be octal, e.g. 755\n"),
        },
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
        // Выключение и перезагрузка. Синхронизируем ФС под локом (он у нас
        // уже взят), а потом передаём управление в power: обе функции не
        // возвращаются, и после sync_all() писать в ФС уже нельзя.
        ["shutdown"] | ["poweroff"] => {
            crate::vitafs::sync_all();
            drop(_vfs_guard);
            crate::power::shutdown(writer);
        }
        ["reboot"] => {
            crate::vitafs::sync_all();
            drop(_vfs_guard);
            crate::power::reboot(writer);
        }
        ["disks"] => {
            let list = crate::blockdev::scan();
            if list.is_empty() {
                writer.write_string("no block devices\n");
            }
            let active = crate::blockdev::active();
            for d in &list {
                let mark = if d.id == active { "*" } else { " " };
                let desc = d.describe();
                writer.write_string(
                    alloc::format!(
                        "{} {}: {} MiB ({} sectors){}\n",
                        mark,
                        d.name(),
                        d.sectors / 2048,
                        d.sectors,
                        if desc.is_empty() {
                            alloc::string::String::new()
                        } else {
                            alloc::format!(" — {}", desc)
                        }
                    )
                    .as_str(),
                );
            }
        }
        ["install"] => {
            // Интерактивный установщик. Мы держим VFS_LOCK: во время установки
            // blockdev переключается между устройствами, и параллельный доступ
            // фоновых задач к ФС испортил бы кэш.
            crate::install::run(writer, vfs);
        }
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
        crate::net::tcp_reset();
        crate::progabi::install();
        let argv = [binpath.as_str()];
        let uid = crate::user::current_uid();
        match crate::exec::launch_user(name, &code, &argv, 0, uid) {
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
        // Приостановленные задачи (неактивные рабочие столы) показываем
        // отдельным состоянием: их `ready` вводит в заблуждение — квантов
        // CPU они не получают вовсе.
        let state = if p.suspended { "paused" } else { p.state };
        writer.write_string(&alloc::format!(
            "{:>5}  {:<20}  {:<10}  {}\n",
            p.pid,
            name_str,
            state,
            p.ticks
        ));
    }
}