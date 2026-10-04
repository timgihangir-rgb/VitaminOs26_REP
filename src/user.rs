// src/user.rs
//
// Учётные записи и пароли.
//
// Хранилище два файла в корне ФС, оба текстовые и по строке на запись:
//
//   /etc/passwd    имя:uid:домашний_каталог
//   /etc/shadow    имя:соль:хеш_пароля
//
// root всегда есть (uid 0) и пароля не имеет: он заводится установщиком при
// первой установке. Обычные учётки начинаются с uid 1000.
//
// Пароль не хранится: в shadow лежит SHA-256 от соли и пароля. Соль — 16
// случайных байт, поэтому два одинаковых пароля дают разные хеши и по
// содержимому файла нельзя сказать, где пароли совпадают. Это один раунд
// SHA-256, а не полноценный KDF: перебор на быстрой машине возможен, и если
// пароли станут чем-то серьёзным, шаг вперёд - повторять хеш тысячи раз
// (как PBKDF2) либо перейти на полноценный алгоритм.
//
// Учётные записи читаются и пиш��ются через scheduler::with_vfs: VFS живёт за
// общим локом, и шелл его держит, пока не отдаст.

use alloc::string::String;
use alloc::vec::Vec;

use crate::sha256;

pub const PASSWD_PATH: &str = "/etc/passwd";
pub const SHADOW_PATH: &str = "/etc/shadow";

/// Администратор. Права uid 0 - полные, мимо проверок.
pub const ROOT_UID: u16 = 0;
/// С какого uid начинаются обычные учётные записи.
pub const FIRST_UID: u16 = 1000;

const SALT_BYTES: usize = 16;
/// Имя не длиннее 31 символа: строки файлов читаются в фиксированные буферы.
pub const NAME_MAX: usize = 31;

/// Учётная запись.
#[derive(Clone, PartialEq)]
pub struct Account {
    pub name: String,
    pub uid: u16,
    pub home: String,
}

impl Account {
    /// Является ли запись администратором.
    pub fn is_root(&self) -> bool {
        self.uid == ROOT_UID
    }
}

/// Кто сейчас работает. Учётные данные хранятся в задаче (см. Task::uid), а не
/// в одном глобальном числе: у каждой программы свои права, иначе `sudo`
/// повысил бы права всей системы до конца сеанса.
pub fn current_uid() -> u16 {
    crate::scheduler::current_uid()
}

/// uid 0 означает «владелец всех файлов сам» и дополнительной проверки не
/// требует, поэтому отдельного признака гостя не заводим.
pub fn set_current_uid(uid: u16) {
    crate::scheduler::set_current_uid(uid);
}

/// Имя текущего пользователя. Для root - всегда "root", даже если записи в
/// /etc/passwd нет (live-образ, свежий диск): "uid0" в приглашении выглядит
/// как поломка. Для несуществующего uid - "uid<N>".
pub fn current_name() -> String {
    if current_uid() == ROOT_UID {
        return String::from("root");
    }
    match lookup_uid(current_uid()) {
        Some(a) => a.name,
        None => alloc::format!("uid{}", current_uid()),
    }
}

// ─── Генерация соли ───────────────────────────────────────────────────────

/// 16 байт энтропии: время RTC (меняется раз в секунду), счётчик PIT
/// (меняется каждые 10 мс),TSC (наносекунды) и счётчик вызовов - на
/// виртуальной машине всё это предсказуемо, но против чтения файла из
/// образа диска достаточно.
fn random_bytes(out: &mut [u8]) {
    let mut seed: u64 = crate::rtc::now_unix() ^ 0x9E37_79B9_7F4A_7C15;
    let mut counter: u64 = 0;
    for chunk in out.chunks_mut(8) {
        counter = counter.wrapping_add(1);
        let lo: u32;
        let hi: u32;
        // rdtsc: наносекундная сетка, недоступная из гостя безrdtsc.
        unsafe {
            core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi, options(nomem, nostack));
        }
        seed ^= ((hi as u64) << 32 | lo as u64).rotate_left(counter as u32 & 63);
        seed = seed.wrapping_mul(0xBF58_476D_1CE4_E5B9);
        seed ^= seed >> 31;
        // Счётчик PIT: меняется быстрее, чем TSC успевает совпасть.
        let pit: u8 = unsafe { x86_64::instructions::port::Port::new(0x40).read() };
        seed = seed.rotate_left(9).wrapping_add(pit as u64);
        let b = seed.to_le_bytes();
        let n = core::cmp::min(8, chunk.len());
        chunk[..n].copy_from_slice(&b[..n]);
    }
}

/// Соль новой записи: 32 шестнадцатеричных символа.
pub fn make_salt() -> String {
    let mut raw = [0u8; SALT_BYTES];
    random_bytes(&mut raw);
    sha256::hex(&raw)
}

// ─── Хеширование ──────────────────────────────────────────────────────────

/// SHA-256 от (соль || пароль), 64 шестнадцатеричных символа.
pub fn hash_password(salt: &str, password: &str) -> String {
    let mut buf = Vec::with_capacity(salt.len() + password.len());
    buf.extend_from_slice(salt.as_bytes());
    buf.extend_from_slice(password.as_bytes());
    sha256::hex(&sha256::hash(&buf))
}

// ─── Разбор файлов ────────────────────────────────────────────────────────

fn split3(line: &str) -> Option<(&str, &str, &str)> {
    let mut it = line.splitn(3, ':');
    let a = it.next()?.trim();
    let b = it.next()?.trim();
    let c = it.next()?.trim();
    if a.is_empty() {
        return None;
    }
    Some((a, b, c))
}

/// Все записи из /etc/passwd, пропуская битые строки.
pub fn all() -> Vec<Account> {
    let text = crate::scheduler::with_vfs(|v| v.cat(PASSWD_PATH).unwrap_or_default());
    let mut out = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let line = core::str::from_utf8(line).unwrap_or("").trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, uid, home)) = split3(line) {
            if let Ok(uid) = uid.parse::<u16>() {
                out.push(Account {
                    name: String::from(name),
                    uid,
                    home: String::from(home),
                });
            }
        }
    }
    out
}

/// Поиск по имени.
pub fn lookup(name: &str) -> Option<Account> {
    all().into_iter().find(|a| a.name == name)
}

/// Поиск по uid.
pub fn lookup_uid(uid: u16) -> Option<Account> {
    all().into_iter().find(|a| a.uid == uid)
}

/// Свободный uid для новой записи.
pub fn next_uid() -> u16 {
    let used = all();
    let mut uid = FIRST_UID;
    while used.iter().any(|a| a.uid == uid) {
        uid = uid.saturating_add(1);
    }
    uid
}

/// Проверка пароля. Сопоставление хешей - за постоянное время по числу
/// символов, чтобы по времени ответа нельзя было перебирать хеш.
pub fn verify(name: &str, password: &str) -> bool {
    let text = crate::scheduler::with_vfs(|v| v.cat(SHADOW_PATH).unwrap_or_default());
    for line in text.split(|&b| b == b'\n') {
        let line = core::str::from_utf8(line).unwrap_or("").trim_end();
        if line.is_empty() {
            continue;
        }
        let Some((n, salt, want)) = split3(line) else {
            continue;
        };
        if n != name {
            continue;
        }
        if want.is_empty() {
            // Запись без пароля (root): вход только по имени.
            return password.is_empty();
        }
        let got = hash_password(salt, password);
        return const_eq(got.as_bytes(), want.as_bytes());
    }
    false
}

/// Побайтовое сравнение без раннего выхода.
fn const_eq(a: &[u8], b: &[u8]) -> bool {
    let mut diff = (a.len() ^ b.len()) as u8;
    let n = core::cmp::min(a.len(), b.len());
    for i in 0..n {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ─── Изменение учётных записей ────────────────────────────────────────────

fn rewrite_passwd(accounts: &[Account]) -> Result<(), &'static str> {
    let mut text = String::new();
    for a in accounts {
        text.push_str(&alloc::format!("{}:{}:{}\n", a.name, a.uid, a.home));
    }
    crate::scheduler::with_vfs(|v| v.write_file(PASSWD_PATH, text.as_bytes()))
        .map_err(|_| "cannot write /etc/passwd")
}

fn rewrite_shadow(shadow: &[(String, String, String)]) -> Result<(), &'static str> {
    let mut text = String::new();
    for (n, s, h) in shadow {
        text.push_str(&alloc::format!("{}:{}:{}\n", n, s, h));
    }
    crate::scheduler::with_vfs(|v| v.write_file(SHADOW_PATH, text.as_bytes()))
        .map_err(|_| "cannot write /etc/shadow")
}

fn read_shadow() -> Vec<(String, String, String)> {
    let text = crate::scheduler::with_vfs(|v| v.cat(SHADOW_PATH).unwrap_or_default());
    let mut out = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let line = core::str::from_utf8(line).unwrap_or("").trim_end();
        if line.is_empty() {
            continue;
        }
        if let Some((n, s, h)) = split3(line) {
            out.push((String::from(n), String::from(s), String::from(h)));
        }
    }
    out
}

/// Создаёт учётную запись с новой солью и хешем. Пароль кладёт и в shadow.
pub fn add(name: &str, password: &str, home: &str) -> Result<Account, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Err("empty user name");
    }
    if name.len() > NAME_MAX {
        return Err("user name too long");
    }
    if name.contains(':') {
        return Err("user name must not contain ':'");
    }
    if lookup(name).is_some() {
        return Err("user already exists");
    }
    let uid = next_uid();
    let account = Account {
        name: String::from(name),
        uid,
        home: String::from(home),
    };
    let mut accounts = all();
    accounts.push(account.clone());
    rewrite_passwd(&accounts)?;

    let mut shadow = read_shadow();
    let salt = make_salt();
    let hash = hash_password(&salt, password);
    shadow.push((account.name.clone(), salt, hash));
    rewrite_shadow(&shadow)?;
    Ok(account)
}

/// Создаёт учётную запись вместе с домашним каталогом. Каталог делается с
/// нужным владельцем сразу, иначе он остался бы принадлежать root и обычный
/// пользователь не смог бы в нём ничего создать.
///
/// Вызывается от root (установщик, `sudo adduser`).
pub fn add_with_home(name: &str, password: &str) -> Result<Account, &'static str> {
    let name = name.trim();
    let home = alloc::format!("/home/{}", name);
    let account = add(name, password, &home)?;
    let uid = account.uid;
    // Каталог может уже быть - это не ошибка, особенно при переустановке.
    let _ = crate::scheduler::with_vfs(|v| v.mkdir(&home));
    if crate::scheduler::with_vfs(|v| v.chown(&home, uid)).is_err() {
        return Err("cannot chown home directory");
    }
    Ok(account)
}

/// Удаляет учётную запись (домашний каталог трогать нельзя: на нём могут
/// лежать данные, которые ещё нужны; `rm -rf /home/<имя>` - осознанное
/// действие администратора).
pub fn remove(name: &str) -> Result<(), &'static str> {
    if name == "root" {
        return Err("cannot remove root");
    }
    let mut accounts = all();
    let before = accounts.len();
    accounts.retain(|a| a.name != name);
    if accounts.len() == before {
        return Err("no such user");
    }
    rewrite_passwd(&accounts)?;
    let mut shadow = read_shadow();
    shadow.retain(|(n, _, _)| n != name);
    rewrite_shadow(&shadow)?;
    Ok(())
}

/// Меняет пароль: новая соль каждый раз, поэтому старый хеш не восстановить.
pub fn set_password(name: &str, password: &str) -> Result<(), &'static str> {
    if lookup(name).is_none() {
        return Err("no such user");
    }
    let mut shadow = read_shadow();
    let salt = make_salt();
    let hash = hash_password(&salt, password);
    let mut found = false;
    for e in shadow.iter_mut() {
        if e.0 == name {
            e.1 = salt.clone();
            e.2 = hash.clone();
            found = true;
        }
    }
    if !found {
        shadow.push((String::from(name), salt, hash));
    }
    rewrite_shadow(&shadow)
}

/// Служебная проверка для установщика: пароль не короче 4 символов.
/// Настоящей стойкости это не даёт, но отсекает пустой и односимвольный ввод.
pub fn password_acceptable(p: &str) -> bool {
    p.chars().count() >= 4
}