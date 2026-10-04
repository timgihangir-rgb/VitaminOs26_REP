// src/install.rs
//
// Установщик VitaminOS26 на диск (MBR + собственный загрузчик + VITAFS).
//
// Раскладка на цели повторяет src/bootldr/layout.inc:
//   LBA 0            stage1 + таблица разделов + 0x55AA
//   LBA 1..16        stage2
//   LBA 256..        сырой ELF ядра (читается из /boot установочной ФС)
//   LBA 32768..      раздел 1: VITAFS (клон рабочей ФС)
//
// Рабочая (live) ФС и цель - разные устройства, поэтому при копировании
// приходится переключать blockdev::select. Пока выбрана цель, к live-ФС
// обращаться нельзя (кэш bcache/inode привязан к активному устройству);
// поэтому порядок всегда такой: читаем с live -> select(target) -> пишем ->
// select(live).

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::blockdev::{self, DeviceId, BLOCK_SIZE, SECTORS_PER_BLOCK};
use crate::keyboard::{self, Key};
use crate::part::{self, Entry, ENTRY_COUNT, TYPE_VITAFS};
use crate::vga::{COLOR_LIGHT_CYAN, COLOR_LIGHT_GREEN, COLOR_LIGHT_RED, COLOR_WHITE, Writer};
use crate::vfs::Vfs;

const STAGE2_LBA: u32 = 1;
const KERNEL_LBA: u32 = 256;
const PART1_LBA: u32 = 32768;
const KERNEL_NAME: &str = "vitamin_os26.elf";
const MARKER_PATH: &str = "/etc/installer";

static STAGE1: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/stage1.bin"));
static STAGE2: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/stage2.bin"));

/// Секторный буфер копирования живёт в .bss: на стеке 4 КиБ в глубокой
/// цепочке вызовов нежелательны.
static mut COPY_BUF: [u8; BLOCK_SIZE] = [0; BLOCK_SIZE];

/// Диагностика установщика в COM1: на экране меню, а ход установки виден
/// в serial (и при отладке в QEMU).
fn ser(msg: &str) {
    crate::vga::serial_write_atomic("[I] ");
    crate::vga::serial_write_atomic(msg);
    crate::vga::serial_putchar(b'\n');
}

/// Есть ли на смонтированной ФС маркер установщика.
pub fn marker_present(vfs: &Vfs) -> bool {
    vfs.cat(MARKER_PATH).is_some()
}

/// Пишет произвольный буфер с LBA, добивая последний сектор нулями.
fn write_at(lba: u32, data: &[u8]) -> Result<(), &'static str> {
    let sectors = (data.len() + 511) / 512;
    for i in 0..sectors {
        let mut sec = [0u8; 512];
        let start = i * 512;
        let end = core::cmp::min(start + 512, data.len());
        sec[..end - start].copy_from_slice(&data[start..end]);
        blockdev::write_sectors_pub(lba + i as u32, 1, &sec).map_err(|_| "write failed")?;
    }
    Ok(())
}

/// Основная операция: клонирует live-ФС на раздел цели, пишет загрузчик и
/// ядро. `drop_programs` - имена в /bin, которые на цель НЕ ставить.
pub fn install_to(
    target: DeviceId,
    drop_programs: &[String],
    account_name: &str,
    account_password: &str,
) -> Result<(), &'static str> {
    let live = blockdev::active();
    if live == target {
        return Err("target is the live device");
    }
    // Смещение live-ФС внутри устройства (0 для live-образа, LBA/8 для
    // установленной системы). blockdev::select() сбрасывает его, поэтому
    // после каждого выбора live его нужно вернуть.
    let live_offset = blockdev::block_offset();
    let live_on = || {
        blockdev::select(live);
        blockdev::set_block_offset(live_offset);
    };
    ser("install: start");
    ser(&alloc::format!("live={} target={}", blockdev::device_name(live), blockdev::device_name(target)));

    // Синхронизируем live-ФС и сбрасываем кэши: смена устройства делает их
    // невалидными.
    crate::vitafs::sync_all();
    crate::bcache::drop_all();

    let sb = *crate::vitafs::mounted_sb().ok_or("no mounted filesystem")?;
    let live_blocks = sb.total_blocks as u64;
    let live_sectors = live_blocks * SECTORS_PER_BLOCK as u64;
    ser(&alloc::format!("live blocks={} sectors={}", live_blocks, live_sectors));

    // Ёмкость цели.
    if !blockdev::select(target) {
        live_on();
        return Err("cannot select target device");
    }
    let target_blocks = blockdev::capacity_blocks().ok_or("cannot read target capacity")?;
    let target_sectors = target_blocks as u64 * SECTORS_PER_BLOCK as u64;
    if target_sectors < PART1_LBA as u64 + live_sectors {
        live_on();
        crate::vitafs::try_mount();
        return Err("target is too small");
    }
    ser(&alloc::format!("target blocks={} ok", target_blocks));

    // Ядро на live-ФС (/boot/vitamin_os26.elf).
    live_on();
    let boot_ino = crate::vitafs::dir_lookup(&sb, crate::vitafs::ROOT_INODE, "boot")
        .ok_or("live filesystem has no /boot")?;
    let kernel_ino = crate::vitafs::dir_lookup(&sb, boot_ino, KERNEL_NAME)
        .ok_or("live filesystem has no kernel")?;
    let knode = crate::vitafs::iget(&sb, kernel_ino).ok_or("bad kernel inode")?;
    let kernel_blocks = ((knode.size as usize + BLOCK_SIZE - 1) / BLOCK_SIZE) as u32;
    let kernel_sectors = (knode.size as u64 + 511) / 512;
    if KERNEL_LBA as u64 + kernel_sectors > PART1_LBA as u64 {
        return Err("kernel does not fit before the partition");
    }
    ser(&alloc::format!("kernel bytes={} blocks={}", knode.size, kernel_blocks));

    // MBR: stage1 + таблица разделов. stage1 сам обнуляет хвост, поэтому
    // таблицу накладываем после него.
    let part_sectors = live_sectors as u32;
    let mut entries = [Entry::empty(); ENTRY_COUNT];
    entries[0] = Entry {
        bootable: true,
        ptype: TYPE_VITAFS,
        start_lba: PART1_LBA,
        sectors: part_sectors,
    };
    let mbr = part::build_mbr(STAGE1, &entries);
    // Дальше пишем на цель: сначала MBR и stage2.
    blockdev::select(target);
    if blockdev::write_sectors_pub(0, 1, &mbr).is_err() {
        return Err("cannot write MBR");
    }

    // stage2.
    write_at(STAGE2_LBA, STAGE2)?;

    // Ядро: блок за блоком с live, сразу на цель.
    let mut kbuf = [0u8; BLOCK_SIZE];
    for bi in 0..kernel_blocks {
        live_on();
        if crate::vitafs::file_read_block(&sb, kernel_ino, bi, &mut kbuf).is_none() {
            return Err("cannot read kernel");
        }
        blockdev::select(target);
        let lba = KERNEL_LBA + bi * SECTORS_PER_BLOCK as u32;
        let ok = blockdev::write_sectors_pub(lba, SECTORS_PER_BLOCK as u8, &kbuf).is_ok();
        if !ok {
            return Err("cannot write kernel");
        }
    }
    ser("kernel written");

    // Клон ФС: блок за блоком. Сырые секторные вызовы абсолютны, поэтому
    // к их адресу добавляем базу live-ФС внутри устройства (не 0, если live
    // сам установлен в разделе).
    let live_base = (live_offset * SECTORS_PER_BLOCK as u64) as u32;
    let buf = unsafe { &mut *core::ptr::addr_of_mut!(COPY_BUF) };
    let mut b: u64 = 0;
    while b < live_blocks {
        live_on();
        let src_lba = live_base + (b * SECTORS_PER_BLOCK as u64) as u32;
        if blockdev::read_sectors_pub(src_lba, SECTORS_PER_BLOCK as u8, buf).is_err() {
            live_on();
            return Err("cannot read live filesystem");
        }
        blockdev::select(target);
        let dst_lba = PART1_LBA + (b * SECTORS_PER_BLOCK as u64) as u32;
        let ok = blockdev::write_sectors_pub(dst_lba, SECTORS_PER_BLOCK as u8, buf).is_ok();
        if !ok {
            return Err("cannot write target filesystem");
        }
        b += 1;
    }
    ser("filesystem cloned");

    // Целевая ФС: обязательно убираем маркер установщика (иначе система
    // будет при каждой загрузке снова запускать установщик), а заодно
    // снимаем программы, которые пользователь отключил в меню.
    crate::bcache::drop_all();
    crate::vitafs::idrop_all();
    blockdev::select(target);
    if crate::vitafs::try_mount() {
        let tsb = *crate::vitafs::mounted_sb().unwrap();
        if let Some(etc) = crate::vitafs::dir_lookup(&tsb, crate::vitafs::ROOT_INODE, "etc") {
            crate::vitafs::destroy_node(&tsb, etc, "installer", crate::vitafs::TYPE_FILE);
        }
        if !drop_programs.is_empty() {
            if let Some(bin) = crate::vitafs::dir_lookup(&tsb, crate::vitafs::ROOT_INODE, "bin") {
                for name in drop_programs {
                    crate::vitafs::destroy_node(&tsb, bin, name, crate::vitafs::TYPE_FILE);
                }
            }
        }
        // Учётная запись администратора: без неё на цели никто не войдёт.
        // Пишем прямо в иноды целевой ФС, пока смонтирована цель: VFS
        // сейчас смотрит на live-образ, а смены смонтированной ФС тут нет.
        crate::vitafs::idrop_all();
        setup_account(&tsb, account_name, account_password);
        crate::vitafs::sync_all();
    } else {
        ser("warn: cannot mount target to finalize");
    }
    ser("target finalized");
    live_on();

    // Возвращаемся на live-ФС, чтобы система осталась работоспособной.
    crate::bcache::drop_all();
    crate::vitafs::idrop_all();
    blockdev::select(live);
    crate::vitafs::try_mount();
    Ok(())
}

// ─── Интерактивный интерфейс ─────────

struct Menu {
    /// Индексы дисков-кандидатов (все, кроме активного) + имена.
    disks: Vec<(DeviceId, String)>,
    programs: Vec<String>,
    keep: Vec<bool>,
}

fn draw(writer: &mut Writer, menu: &Menu, disk_idx: usize, prog_idx: usize, focus: usize) {
    writer.clear_screen();
    writer.set_color(COLOR_LIGHT_GREEN);
    writer.write_string("VitaminOS26 Installer\n");
    writer.set_color(COLOR_WHITE);
    writer.write_string("Установка системы на выбранный диск. Ctrl+C не прерывает.\n\n");

    writer.set_color(if focus == 0 { COLOR_LIGHT_CYAN } else { COLOR_WHITE });
    writer.write_string("  Целевой диск (Tab/стрелки):\n");
    for (i, (_, name)) in menu.disks.iter().enumerate() {
        let mark = if i == disk_idx { ">" } else { " " };
        writer.write_string(&alloc::format!("   {} {}\n", mark, name));
    }

    writer.set_color(if focus == 1 { COLOR_LIGHT_CYAN } else { COLOR_WHITE });
    writer.write_string("\n  Программы в /bin (Space - вкл/выкл):\n");
    let per_col = 12usize;
    let cols = 3usize;
    for row in 0..per_col {
        let mut line = String::from("   ");
        for c in 0..cols {
            let i = row + c * per_col;
            if i >= menu.programs.len() {
                continue;
            }
            let cursor = if focus == 1 && i == prog_idx { ">" } else { " " };
            let box_ = if menu.keep[i] { "[x]" } else { "[ ]" };
            line.push_str(&alloc::format!("{}{}{:<10} ", cursor, box_, menu.programs[i]));
        }
        writer.write_string(&line);
        writer.write_string("\n");
    }

    writer.set_color(COLOR_WHITE);
    writer.write_string("\n  Enter - установить, Esc - отмена\n");
}

/// Ждёт подтверждения Enter (да) или Esc (нет).
fn confirm(writer: &mut Writer, question: &str) -> bool {
    writer.write_string("\n");
    writer.write_string(question);
    writer.write_string(" [Enter=да, Esc=нет] ");
    loop {
        match keyboard::read_key() {
            Key::Enter => return true,
            Key::Esc => return false,
            _ => {}
        }
    }
}

/// Учётная запись, которую заводит установщик.
pub struct NewAccount {
    pub name: String,
    pub password: String,
}

/// Спрашивает имя и пароль администратора. None - пользователь нажал Esc и
/// установку отменил.
///
/// Имя и пароль набираются латиницей: раскладка клавиатуры в этой системе одна
/// (US), а учётные записи должны переживать копирование образа на другой
/// жесткий диск, где нет ни /etc/shadow, ни /etc/passwd.
fn ask_account(writer: &mut Writer) -> Option<NewAccount> {
    loop {
        writer.clear_screen();
        writer.set_color(COLOR_LIGHT_GREEN);
        writer.write_string("VitaminOS26 Installer\n");
        writer.set_color(COLOR_WHITE);
        writer.write_string("\nУчётная запись администратора.\n\n");
        writer.write_string("  Имя пользователя: ");
        // Esc на пустом поле - отмена установки (диск ещё не тронут).
        let Some(name) = crate::linein::read_line_cancelable(writer, true) else {
            writer.write_string("\nУстановка отменена.\n");
            ser("account: cancelled by user");
            return None;
        };
        if !crate::linein::valid_user_name(&name) {
            writer.set_color(COLOR_LIGHT_RED);
            writer.write_string(
                "Не подходит: латинская буква, дальше буквы, цифры, дефис,\n\
                 подчёркивание; до 31 символа. Имя \"root\" занято.\n\n",
            );
            writer.set_color(COLOR_WHITE);
            continue;
        }

        writer.write_string("\n  Пароль: ");
        let p1 = crate::linein::read_line(writer, false);
        writer.write_string("\n  Пароль ещё раз: ");
        let p2 = crate::linein::read_line(writer, false);
        if p1 != p2 {
            writer.set_color(COLOR_LIGHT_RED);
            writer.write_string("Пароли не совпадают.\n\n");
            writer.set_color(COLOR_WHITE);
            continue;
        }
        if !crate::user::password_acceptable(&p1) {
            writer.set_color(COLOR_LIGHT_RED);
            writer.write_string("Пароль короче 4 символов.\n\n");
            writer.set_color(COLOR_WHITE);
            continue;
        }

        writer.set_color(COLOR_LIGHT_GREEN);
        writer.write_string(&alloc::format!(
            "Готово: будет создан пользователь {} с домашним каталогом /home/{}.\n",
            name, name
        ));
        writer.set_color(COLOR_WHITE);
        ser(&alloc::format!("account: name={} (password hidden)", name));
        return Some(NewAccount {
            name,
            password: p1,
        });
    }
}

/// Создаёт учётную запись и домашний каталог на смонтированной целевой ФС.
/// Вызывается из `install_to`, пока цель смонтирована и текущий uid - root.
fn setup_account(_sb: &crate::vitafs::Superblock, name: &str, password: &str) {
    match crate::user::add_with_home(name, password) {
        Ok(acc) => ser(&alloc::format!("account: created {} uid={}", acc.name, acc.uid)),
        Err(e) => ser(&alloc::format!("account: ERR {}", e)),
    }
}

/// Интерактивный сеанс установщика. true - установка выполнена.
pub fn run(writer: &mut Writer, vfs: &mut Vfs) -> bool {
    let active = blockdev::active();
    let disks: Vec<(DeviceId, String)> = blockdev::scan()
        .into_iter()
        .filter(|d| d.id != active)
        .map(|d| {
            let desc = d.describe();
            let name = if desc.is_empty() {
                alloc::format!("{} ({} МиБ)", d.name(), d.sectors / 2048)
            } else {
                alloc::format!("{} ({} МиБ, {})", d.name(), d.sectors / 2048, desc)
            };
            (d.id, name)
        })
        .collect();

    if disks.is_empty() {
        writer.clear_screen();
        writer.write_string("Установщик: не найдено второго диска для установки.\n");
        writer.write_string("Подключите целевой диск и перезапустите установщик.\n");
        return false;
    }

    let mut programs: Vec<String> = vfs
        .ls("/bin")
        .map(|v| v.into_iter().map(|(n, _, _)| n).collect())
        .unwrap_or_default();
    programs.sort();

    let mut menu = Menu {
        disks,
        keep: vec![true; programs.len()],
        programs,
    };

    let mut disk_idx = 0usize;
    let mut prog_idx = 0usize;
    let mut focus = 0usize;

    loop {
        draw(writer, &menu, disk_idx, prog_idx, focus);
        match keyboard::read_key() {
            Key::Esc => {
                if confirm(writer, "Отменить установку?") {
                    return false;
                }
            }
            Key::Tab => focus ^= 1,
            Key::Up => {
                if focus == 0 {
                    disk_idx = disk_idx.saturating_sub(1);
                } else {
                    prog_idx = prog_idx.saturating_sub(1);
                }
            }
            Key::Down => {
                if focus == 0 {
                    if disk_idx + 1 < menu.disks.len() {
                        disk_idx += 1;
                    }
                } else if prog_idx + 1 < menu.programs.len() {
                    prog_idx += 1;
                }
            }
            Key::Space if focus == 1 => {
                menu.keep[prog_idx] = !menu.keep[prog_idx];
            }
            Key::Enter => {
                let target = menu.disks[disk_idx].0;
                let name = menu.disks[disk_idx].1.clone();
                // Учётную запись спрашиваем до стирания диска: если пароль
                // не совпадёт, данные на цели останутся нетронутыми.
                let Some(account) = ask_account(writer) else {
                    return false;
                };
                if !confirm(
                    writer,
                    &alloc::format!("Стереть {} и установить VitaminOS26?", name),
                ) {
                    continue;
                }
                writer.write_string("\nУстановка... не выключайте питание.\n");
                let drop: Vec<String> = menu
                    .programs
                    .iter()
                    .zip(menu.keep.iter())
                    .filter(|(_, k)| !**k)
                    .map(|(n, _)| n.clone())
                    .collect();
                ser(&alloc::format!(
                    "menu: install target={} drop={}",
                    name,
                    drop.len()
                ));
                match install_to(target, &drop, &account.name, &account.password) {
                    Ok(()) => {
                        ser("install: OK");
                        writer.set_color(COLOR_LIGHT_GREEN);
                        writer.write_string("Готово. Перезагрузка...\n");
                        writer.set_color(COLOR_WHITE);
                        return true;
                    }
                    Err(e) => {
                        ser(&alloc::format!("install: ERR {}", e));
                        writer.write_string("Ошибка установки: ");
                        writer.write_string(e);
                        writer.write_string("\n");
                        let _ = confirm(writer, "Продолжить?");
                    }
                }
            }
            _ => {}
        }
    }
}

/// Разбор списка программ из аргументов (для неинтерактивного режима).
#[allow(dead_code)]
pub fn all_programs(vfs: &Vfs) -> Vec<String> {
    vfs.ls("/bin")
        .map(|v| v.into_iter().map(|(n, _, _)| n.to_string()).collect())
        .unwrap_or_default()
}
