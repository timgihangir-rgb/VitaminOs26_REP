//! Init-система (аналог sysinit/supervisord): задача-супервизор, которая при
//! загрузке читает /etc/rc.conf, запускает фоновые службы и перезапускает
//! упавшие (с лимитом рестартов и защитой от шторма).
//!
//! Таблица служб защищена спинлоком SERVICES. Правило порядка локов, чтобы
//! не было дедлока с VFS_LOCK:
//!   - никогда не держать SERVICES лок, выполняя VFS-операции (with_vfs),
//!   - никогда не вызывать with_vfs из кода шелла (шелл уже держит VFS_LOCK,
//!     а лок не реентерабельный) — вместо этого передавать &mut Vfs напрямую.
//!
//! Супервизор проверяет службы каждые SUPERVISE_INTERVAL тиков. Служба с
//! `respawn` перезапускается максимум MAX_RESTARTS раз за RESTART_WINDOW
//! тиков; если служба живёт дольше окна, счётчик сбрасывается. После
//! превышения лимита служба переходит в состояние failed и требует ручного
//! `init start`.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spinning_top::Spinlock;

use crate::scheduler::TaskState;
use crate::vfs::Vfs;
use crate::vga::Writer;

const MAX_SERVICES: usize = 16;
const MAX_RESTARTS: u32 = 5;
const RESTART_WINDOW: u64 = 2000;
const SUPERVISE_INTERVAL: u64 = 50;

const DEFAULT_RC_CONF: &[u8] = b"\
# VitaminOS26 init services\n\
clock bin\n\
ticker builtin respawn\n\
crashy builtin respawn\n";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Builtin,
    Bin,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum ServiceState {
    Stopped,
    Running,
    Failed,
}

#[derive(Clone)]
struct ServiceEntry {
    name: String,
    kind: Kind,
    args: Vec<u64>,
    respawn: bool,
    pid: usize,
    state: ServiceState,
    restarts: u32,
    window_start: u64,
}

struct ServiceTable {
    entries: Vec<ServiceEntry>,
}

static SERVICES: Spinlock<ServiceTable> = Spinlock::new(ServiceTable { entries: Vec::new() });

fn kind_str(k: Kind) -> &'static str {
    match k {
        Kind::Builtin => "builtin",
        Kind::Bin => "bin",
    }
}

/// Читает /etc/rc.conf и заполняет таблицу служб. Вызывается при загрузке.
fn load_config() {
    let text = scheduler_with_vfs(|vfs| {
        vfs.cat("/etc/rc.conf")
            .map(|d| String::from_utf8_lossy(d).into_owned())
    })
    .unwrap_or_default();

    let mut entries = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut toks = line.split_whitespace();
        let name = match toks.next() {
            Some(n) => n,
            None => continue,
        };
        let kind = match toks.next() {
            Some("builtin") => Kind::Builtin,
            Some("bin") => Kind::Bin,
            _ => continue,
        };
        let mut args = Vec::new();
        let mut respawn = false;
        for t in toks {
            if t == "respawn" {
                respawn = true;
            } else if let Ok(v) = t.parse::<u64>() {
                args.push(v);
            }
        }
        entries.push(ServiceEntry {
            name: name.to_string(),
            kind,
            args,
            respawn,
            pid: 0,
            state: ServiceState::Stopped,
            restarts: 0,
            window_start: 0,
        });
    }

    let mut g = SERVICES.lock();
    g.entries = entries;
    if g.entries.len() > MAX_SERVICES {
        g.entries.truncate(MAX_SERVICES);
    }
}

/// Старт init-системы: создаёт /var/log, дефолтный /etc/rc.conf (если нет),
/// запускает службы и задачу-супервизор. Вызывается из main перед run_shell.
pub fn boot(writer: &mut Writer) {
    scheduler_with_vfs(|vfs| {
        let _ = vfs.mkdir("/var");
        let _ = vfs.mkdir("/var/log");
        if vfs.cat("/etc/rc.conf").is_none() {
            let _ = vfs.write_file("/etc/rc.conf", DEFAULT_RC_CONF);
        }
    });

    crate::progabi::install();
    load_config();

    let n = SERVICES.lock().entries.len();
    for i in 0..n {
        if start_service(i, None, true) {
            let name = SERVICES.lock().entries[i].name.clone();
            log_line(None, &alloc::format!("[init] boot: {} started\n", name));
        }
    }

    if crate::scheduler::spawn("init", Box::new(|| supervisor_loop())).is_some() {
        log_line(None, "[init] boot: supervisor started\n");
        writer.write_string("[init] ");
        writer.write_string(&n.to_string());
        writer.write_string(" service(s) configured\n");
    } else {
        writer.write_string("[init] ERROR: no free task slot for supervisor\n");
    }
}

/// Цикл супервизора: раз в SUPERVISE_INTERVAL тиков проверяет службы.
fn supervisor_loop() -> ! {
    loop {
        let target = crate::scheduler::ticks() + SUPERVISE_INTERVAL;
        crate::scheduler::sleep_until(target);
        supervise_once();
    }
}

/// Один проход супервизора: находит умершие службы и запускает рестарт.
fn supervise_once() {
    let now = crate::scheduler::ticks();
    let mut respawn_indices: Vec<usize> = Vec::new();
    let mut logs: Vec<String> = Vec::new();

    {
        let mut g = SERVICES.lock();
        for (i, e) in g.entries.iter_mut().enumerate() {
            if e.state != ServiceState::Running || e.pid == 0 {
                continue;
            }
            let alive = crate::scheduler::task_state(e.pid)
                .map_or(false, |s| s != TaskState::Finished);
            if alive {
                if e.restarts > 0 && now.saturating_sub(e.window_start) > RESTART_WINDOW {
                    e.restarts = 0;
                }
                continue;
            }

            if e.respawn && e.restarts < MAX_RESTARTS {
                e.restarts += 1;
                if e.restarts == 1 {
                    e.window_start = now;
                }
                respawn_indices.push(i);
            } else if e.respawn {
                e.state = ServiceState::Failed;
                e.pid = 0;
                logs.push(alloc::format!(
                    "[init] {} exceeded restart limit, marked failed\n",
                    e.name
                ));
            } else {
                e.state = ServiceState::Stopped;
                e.pid = 0;
                logs.push(alloc::format!("[init] {} exited (no respawn)\n", e.name));
            }
        }
    }

    // VFS-операции выполняются только вне критической секции SERVICES, иначе
    // возможен дедлок: шелл держит VFS_LOCK, супервизор держит SERVICES.
    for l in &logs {
        log_line(None, l);
    }
    for i in respawn_indices {
        respawn_service(i);
    }
}

/// Перезапуск умершей службы: убивает старую задачу (освобождает слот) и
/// спавнит новую, сохраняя счётчик рестартов.
fn respawn_service(i: usize) {
    let entry = SERVICES.lock().entries[i].clone();
    if entry.state != ServiceState::Running || entry.pid == 0 {
        return;
    }
    if entry.pid != 0 {
        crate::scheduler::kill(entry.pid);
    }
    let pid = do_spawn(entry.kind, &entry.name, &entry.args, None);
    {
        let mut g = SERVICES.lock();
        let e = &mut g.entries[i];
        e.pid = pid.unwrap_or(0);
        if pid.is_none() {
            e.state = ServiceState::Failed;
        }
    }
    match pid {
        Some(pid) => log_line(None, &alloc::format!(
            "[init] respawn {} pid={} attempt={}\n",
            entry.name,
            pid,
            entry.restarts
        )),
        None => log_line(None, &alloc::format!(
            "[init] respawn {} FAILED (no free slot)\n",
            entry.name
        )),
    }
}

/// Запуск службы по индексу. `reset=true` обнуляет счётчик рестартов
/// (первоначальный запуск и ручной `init start`), `false` — рестарт супервизора.
fn start_service(i: usize, vfs: Option<&mut Vfs>, reset: bool) -> bool {
    let (name, kind, args) = {
        let mut g = SERVICES.lock();
        let e = &mut g.entries[i];
        if e.state == ServiceState::Running {
            return true;
        }
        e.state = ServiceState::Running;
        if reset {
            e.restarts = 0;
            e.window_start = crate::scheduler::ticks();
        }
        (e.name.clone(), e.kind, e.args.clone())
    };

    match do_spawn(kind, &name, &args, vfs) {
        Some(pid) => {
            SERVICES.lock().entries[i].pid = pid;
            true
        }
        None => {
            SERVICES.lock().entries[i].state = ServiceState::Failed;
            false
        }
    }
}

/// Запуск задачи под службу: builtin через `tasks::spawn`, bin — чтение
/// /bin/имя и запуск как сырого машинного кода (как `bg`).
fn do_spawn(kind: Kind, name: &str, args: &[u64], vfs: Option<&mut Vfs>) -> Option<usize> {
    match kind {
        Kind::Builtin => {
            let strs: Vec<String> = args.iter().map(|v| v.to_string()).collect();
            let refs: Vec<&str> = strs.iter().map(|s| s.as_str()).collect();
            crate::tasks::spawn(name, &refs)
        }
        Kind::Bin => start_bin_service(name, vfs),
    }
}

fn start_bin_service(name: &str, vfs: Option<&mut Vfs>) -> Option<usize> {
    crate::progabi::install();
    let path = alloc::format!("/bin/{}", name);
    let code = match vfs {
        Some(v) => v.cat(&path).map(|d| d.to_vec()),
        None => scheduler_with_vfs(|v| v.cat(&path).map(|d| d.to_vec())),
    }?;
    crate::exec::launch_user(name, &code, 0)
}

/// Оборачивает VFS-доступ общим локом (безопасно вне шелла и без вложения
/// в SERVICES лок).
fn scheduler_with_vfs<R>(f: impl FnOnce(&mut Vfs) -> R) -> R {
    crate::scheduler::with_vfs(f)
}

/// Дописывает строку в /var/log/init.log (с ограничением размера).
///
/// Из шелла передаётся `Some(vfs)` — шелл уже держит VFS_LOCK (лок не
/// реентерабельный), поэтому использовать `with_vfs` там нельзя. Из задач
/// супервизора/загрузки — `None` (берёт общий лок).
fn log_line(vfs: Option<&mut Vfs>, line: &str) {
    match vfs {
        Some(v) => append_log(v, line),
        None => scheduler_with_vfs(|v| append_log(v, line)),
    }
}

fn append_log(vfs: &mut Vfs, line: &str) {
    let mut out = vfs
        .cat("/var/log/init.log")
        .map(|d| d.to_vec())
        .unwrap_or_default();
    if out.len() + line.len() > 8192 {
        out = out[out.len().saturating_sub(4096)..].to_vec();
    }
    out.extend_from_slice(line.as_bytes());
    let _ = vfs.write_file("/var/log/init.log", &out);
}

/// Команда шелла `init ...`. Вызывается с уже удерживаемым шеллом VFS_LOCK,
/// поэтому bin-чтения идут через `vfs` напрямую.
pub fn cmd(writer: &mut Writer, vfs: &mut Vfs, args: &[&str]) {
    match args {
        [] | ["status"] => status(writer),
        ["list"] => list(writer),
        ["start", name] => {
            match find_index(name) {
                Some(i) => {
                    if start_service(i, Some(vfs), true) {
                        writer.write_string("init: started ");
                        writer.write_string(name);
                        writer.write_string("\n");
                    } else {
                        writer.write_string("init: failed to start ");
                        writer.write_string(name);
                        writer.write_string(" (no free task slot)\n");
                    }
                }
                None => unknown_service(writer, name),
            }
        }
        ["stop", name] => {
            match find_index(name) {
                Some(i) => {
                    let pid = {
                        let mut g = SERVICES.lock();
                        let e = &mut g.entries[i];
                        let pid = e.pid;
                        e.pid = 0;
                        e.state = ServiceState::Stopped;
                        pid
                    };
                    if pid != 0 {
                        crate::scheduler::kill(pid);
                    }
                    log_line(Some(vfs), &alloc::format!("[init] stop {} pid={}\n", name, pid));
                    writer.write_string("init: stopped ");
                    writer.write_string(name);
                    writer.write_string("\n");
                }
                None => unknown_service(writer, name),
            }
        }
        ["restart", name] => {
            match find_index(name) {
                Some(i) => {
                    let pid = {
                        let mut g = SERVICES.lock();
                        let e = &mut g.entries[i];
                        let pid = e.pid;
                        e.pid = 0;
                        e.state = ServiceState::Stopped;
                        pid
                    };
                    if pid != 0 {
                        crate::scheduler::kill(pid);
                    }
                    if start_service(i, Some(vfs), true) {
                        writer.write_string("init: restarted ");
                        writer.write_string(name);
                        writer.write_string("\n");
                    } else {
                        writer.write_string("init: failed to restart ");
                        writer.write_string(name);
                        writer.write_string("\n");
                    }
                }
                None => unknown_service(writer, name),
            }
        }
        _ => {
            writer.write_string("usage: init status|list|start NAME|stop NAME|restart NAME\n");
        }
    }
}

fn unknown_service(writer: &mut Writer, name: &str) {
    writer.write_string("init: unknown service: ");
    writer.write_string(name);
    writer.write_string("\n");
}

fn find_index(name: &str) -> Option<usize> {
    SERVICES.lock().entries.iter().position(|e| e.name == name)
}

fn display_state(e: &ServiceEntry) -> &'static str {
    match e.state {
        ServiceState::Stopped => "stopped",
        ServiceState::Failed => "failed",
        ServiceState::Running => {
            let alive = crate::scheduler::task_state(e.pid)
                .map_or(false, |s| s != TaskState::Finished);
            if alive {
                "running"
            } else {
                "restarting"
            }
        }
    }
}

fn status(writer: &mut Writer) {
    let g = SERVICES.lock();
    writer.write_string("  NAME       KIND     PID  STATE      RESTARTS  RESP\n");
    for e in &g.entries {
        let pid = if e.pid == 0 {
            String::from("-")
        } else {
            e.pid.to_string()
        };
        let resp = if e.respawn { "yes" } else { "no" };
        writer.write_string(&alloc::format!(
            "  {:<10} {:<8} {:>4} {:<10} {:>3}       {}\n",
            e.name,
            kind_str(e.kind),
            pid,
            display_state(e),
            e.restarts,
            resp
        ));
    }
}

fn list(writer: &mut Writer) {
    let g = SERVICES.lock();
    writer.write_string("  NAME       KIND     ARGS            RESP\n");
    for e in &g.entries {
        let args: Vec<String> = e.args.iter().map(|v| v.to_string()).collect();
        let resp = if e.respawn { "respawn" } else { "-" };
        writer.write_string(&alloc::format!(
            "  {:<10} {:<8} {:<15} {}\n",
            e.name,
            kind_str(e.kind),
            args.join(" "),
            resp
        ));
    }
}
