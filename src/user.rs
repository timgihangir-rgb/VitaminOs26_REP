// src/user.rs
//
// Учётные записи и пароли.
//
// Хранилище два файла в корне ФС, оба текстовые и по строке на запись:
//
//   /etc/passwd    имя:uid:домашний_каталог
//   /etc/shadow    имя:алгоритм:итерации:соль:хеш
//
// root всегда есть (uid 0) и пароля не имеет: он заводится установщиком при
// первой установке. Обычные учётки начинаются с uid 1000.
//
// Пароль не хранится: в shadow лежит PBKDF2-HMAC-SHA256 от соли и пароля.
// Соль — 16 случайных байт, поэтому два одинаковых пароля дают разные хеши
// и по содержимому файла нельзя сказать, где пароли совпадают.
//
// Учётные записи читаются и пиш��ются через scheduler::with_vfs: VFS живёт за
// общим локом, и шелл его держит, пока не отдаст.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::string::ToString;
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
//
// Соль берётся из криптостойкого DRBG (src/rand.rs). Старую
// ad-hoc функцию с xorshift-подобным смешиванием убрали: у неё
// предсказуемое состояние и детерминированный выход на QEMU.

/// Соль новой записи: 32 шестнадцатеричных символа.
pub fn make_salt() -> String {
    let mut raw = [0u8; SALT_BYTES];
    crate::rand::random_bytes(&mut raw);
    sha256::hex(&raw)
}

// ─── Хеширование ──────────────────────────────────────────────────────────

/// Хеш пароля в конкретном алгоритме. `algo` сейчас только `pbkdf2-sha256`
/// и legacy `sha256` (один раунд SHA-256 от соли+пароля).
fn hash_with(algo: &str, iterations: u32, salt: &str, password: &str) -> String {
    match algo {
        "pbkdf2-sha256" => sha256::hex(&crate::pbkdf2::pbkdf2_sha256(
            password.as_bytes(),
            salt.as_bytes(),
            iterations,
        )),
        _ => {
            // Legacy: один раунд SHA-256.
            let mut st = sha256::Sha256State::new();
            st.update(salt.as_bytes());
            st.update(password.as_bytes());
            sha256::hex(&st.finalize())
        }
    }
}

/// Новая shadow-строка для пароля в текущем (самом стойком) формате.
fn make_shadow_line(name: &str, password: &str) -> String {
    let salt = make_salt();
    let iters = crate::pbkdf2::iteration_count_for_cpu();
    let hash = hash_with("pbkdf2-sha256", iters, &salt, password);
    alloc::format!("{}:pbkdf2-sha256:{}:{}:{}", name, iters, salt, hash)
}

/// Разбирает строку shadow. Поддерживает и новый формат
/// `name:algo:iterations:salt:hash`, и старый `name:salt:hash`
/// (читается как algo=sha256, iterations=1).
fn parse_shadow_line(line: &str) -> Option<(String, String, u32, String, String)> {
    let parts: Vec<&str> = line.splitn(5, ':').collect();
    match parts.as_slice() {
        [name, salt, hash] => Some((
            name.trim().to_string(),
            "sha256".to_string(),
            1,
            salt.trim().to_string(),
            hash.trim().to_string(),
        )),
        [name, algo, iters, salt, hash] => Some((
            name.trim().to_string(),
            algo.trim().to_string(),
            iters.trim().parse::<u32>().ok()?,
            salt.trim().to_string(),
            hash.trim().to_string(),
        )),
        _ => None,
    }
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
    crate::scheduler::with_vfs(|v| all_vfs(v))
}

/// Вариант с явным &mut Vfs (на случай, когда VFS_LOCK уже удержан).
pub fn all_vfs(vfs: &mut crate::vfs::Vfs) -> Vec<Account> {
    let text = vfs.cat(PASSWD_PATH).unwrap_or_default();
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
    crate::scheduler::with_vfs(|v| lookup_vfs(v, name))
}

/// Вариант с явным &mut Vfs.
pub fn lookup_vfs(vfs: &mut crate::vfs::Vfs, name: &str) -> Option<Account> {
    all_vfs(vfs).into_iter().find(|a| a.name == name)
}

/// Поиск по uid.
pub fn lookup_uid(uid: u16) -> Option<Account> {
    crate::scheduler::with_vfs(|v| lookup_uid_vfs(v, uid))
}

/// Вариант с явным &mut Vfs.
pub fn lookup_uid_vfs(vfs: &mut crate::vfs::Vfs, uid: u16) -> Option<Account> {
    all_vfs(vfs).into_iter().find(|a| a.uid == uid)
}

/// Свободный uid для новой записи.
pub fn next_uid() -> u16 {
    crate::scheduler::with_vfs(|v| next_uid_vfs(v))
}

/// Вариант с явным &mut Vfs.
pub fn next_uid_vfs(vfs: &mut crate::vfs::Vfs) -> u16 {
    let used = all_vfs(vfs);
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
    let mut winner: Option<(String, u32, String, String)> = None;
    for line in text.split(|&b| b == b'\n') {
        let line = core::str::from_utf8(line).unwrap_or("").trim_end();
        if line.is_empty() {
            continue;
        }
        let Some((n, algo, iters, salt, want)) = parse_shadow_line(line) else {
            continue;
        };
        if n != name {
            continue;
        }
        // Не return на первом совпадении — берём последнюю запись для имени:
        // любой дубль в shadow отменяет старую, и иначе verify проверил бы
        // устаревший хеш.
        winner = Some((algo, iters, salt, want));
    }
    let Some((algo, iters, salt, want)) = winner else {
        return false;
    };
    if want.is_empty() {
        // Запись без пароля (root): вход только по имени.
        return password.is_empty();
    }
    let got = hash_with(&algo, iters, &salt, password);
    const_eq(got.as_bytes(), want.as_bytes())
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

fn rewrite_passwd(vfs: &mut crate::vfs::Vfs, accounts: &[Account]) -> Result<(), &'static str> {
    let mut text = String::new();
    for a in accounts {
        text.push_str(&alloc::format!("{}:{}:{}\n", a.name, a.uid, a.home));
    }
    vfs.write_file(PASSWD_PATH, text.as_bytes())
        .map_err(|_| "cannot write /etc/passwd")
}

fn rewrite_shadow(vfs: &mut crate::vfs::Vfs, lines: &[String]) -> Result<(), &'static str> {
    // Схлопываем по имени: побеждает ПОСЛЕДНЯЯ строка. Иначе в файле могли
    // остаться дубликаты, и verify с «последним выигрывает» мог бы путаться.
    let mut seen: BTreeMap<String, &str> = BTreeMap::new();
    for l in lines {
        if let Some(n) = l.splitn(2, ':').next() {
            seen.insert(String::from(n), l.as_str());
        }
    }
    let mut text = String::new();
    for (_, l) in seen {
        text.push_str(l);
        text.push('\n');
    }
    vfs.write_file(SHADOW_PATH, text.as_bytes())
        .map_err(|_| "cannot write /etc/shadow")
}

fn read_shadow(vfs: &mut crate::vfs::Vfs) -> Vec<String> {
    let text = vfs.cat(SHADOW_PATH).unwrap_or_default();
    let mut out = Vec::new();
    for line in text.split(|&b| b == b'\n') {
        let line = core::str::from_utf8(line).unwrap_or("").trim_end();
        if line.is_empty() {
            continue;
        }
        out.push(String::from(line));
    }
    out
}

/// Имя из shadow-строки (первое поле перед ':'), для фильтрации.
fn shadow_name(line: &str) -> &str {
    line.splitn(2, ':').next().unwrap_or("").trim()
}

/// Создаёт учётную запись с новой солью и хешем. Пароль кладёт и в shadow.
/// Lock taking done by caller if needed.
pub fn add_with_vfs(vfs: &mut crate::vfs::Vfs, name: &str, password: &str, home: &str) -> Result<Account, &'static str> {
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
    if lookup_vfs(vfs, name).is_some() {
        return Err("user already exists");
    }
    let uid = next_uid_vfs(vfs);
    let account = Account {
        name: String::from(name),
        uid,
        home: String::from(home),
    };
    let mut accounts = all_vfs(vfs);
    accounts.push(account.clone());
    rewrite_passwd(vfs, &accounts)?;

    let mut shadow = read_shadow(vfs);
    shadow.push(make_shadow_line(&account.name, password));
    rewrite_shadow(vfs, &shadow)?;
    Ok(account)
}

/// Backward-compatible wrapper that acquires the VFS_LOCK for the full operation.
pub fn add(name: &str, password: &str, home: &str) -> Result<Account, &'static str> {
    crate::scheduler::with_vfs(|v| add_with_vfs(v, name, password, home))
}

/// Создаёт учётную запись вместе с домашним каталогом. Каталог делается с
/// нужным владельцем сразу, иначе он остался бы принадлежать root и обычный
/// пользователь не смог бы в нём ничего создать.
///
/// Вызывается от root (установщик, `sudo adduser`).
pub fn add_with_home(name: &str, password: &str) -> Result<Account, &'static str> {
    crate::scheduler::with_vfs(|v| add_with_home_vfs(v, name, password))
}

/// Вариант с явным &mut Vfs — для контекстов, где VFS_LOCK уже держится
/// (установщик) и повторный захват привёл бы к дедлоку.
pub fn add_with_home_vfs(vfs: &mut crate::vfs::Vfs, name: &str, password: &str) -> Result<Account, &'static str> {
    let name = name.trim();
    let home = alloc::format!("/home/{}", name);
    let account = add_with_vfs(vfs, name, password, &home)?;
    let uid = account.uid;
    // Создаём /home, если его ещё нет: иначе mkdir(/home/имя) проваливается.
    let _ = vfs.mkdir("/home");
    // Каталог может уже существовать — это не ошибка, но любая другая
    // ошибка (диск полон, права, ФС) обязана всплыть явно.
    match vfs.mkdir(&home) {
        Ok(()) => {}
        Err(()) => {
            let exists = vfs.ls(&home).is_some();
            if !exists {
                return Err("cannot create home directory");
            }
        }
    }
    if vfs.chown(&home, uid).is_err() {
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
    crate::scheduler::with_vfs(|v| {
        rewrite_passwd(v, &accounts)?;
        let mut shadow = read_shadow(v);
        shadow.retain(|l| shadow_name(l) != name);
        rewrite_shadow(v, &shadow)
    })?;
    Ok(())
}

/// Меняет пароль: новая соль каждый раз, поэтому старый хеш не восстановить.
pub fn set_password(name: &str, password: &str) -> Result<(), &'static str> {
    if lookup(name).is_none() {
        return Err("no such user");
    }
    crate::scheduler::with_vfs(|v| {
        let mut shadow = read_shadow(v);
        let mut found = false;
        let new_line = make_shadow_line(name, password);
        for e in shadow.iter_mut() {
            if shadow_name(e) == name {
                *e = new_line.clone();
                found = true;
            }
        }
        if !found {
            shadow.push(new_line);
        }
        rewrite_shadow(v, &shadow)
    })?;
    Ok(())
}

/// Служебная проверка для установщика: пароль не короче 4 символов.
/// Настоящей стойкости это не даёт, но отсекает пустой и односимвольный ввод.
pub fn password_acceptable(p: &str) -> bool {
    p.chars().count() >= 4
}