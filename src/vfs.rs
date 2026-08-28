// src/vfs.rs
//
// VFS поверх inode-ФС VITAFS (см. docs/fs.md и src/vitafs.rs). Фаза 3.
//
// Старая реализация держала всё дерево в памяти (арена Vec<FsEntry>) и целиком
// сериализовала его на диск слепком. Теперь источник истины - диск: каталоги и
// файлы живут в инодах, операции идут через bcache, а структура хранит только
// текущий каталог (канонический абсолютный путь строкой; инод для cwd не
// годится - у нас нет родительских указателей, путь восстанавливать нечем).
//
// cat() возвращает владеющий Vec<u8> - читать с диска "по ссылке" нельзя.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::vitafs;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EntryType {
    File,
    Directory,
    /// Символьное устройство (/dev/null и т.п.) - данные идут мимо инода.
    CharDev,
    /// Симлинк (fast-symlink: цель в теле инода).
    Symlink,
}

pub struct Vfs {
    cwd: String,
    /// Один-way chroot: все абсолютные пути канонизируются от этого префикса.
    /// Сбросить можно только перезагрузкой.
    root: String,
}

impl Vfs {
    pub fn new() -> Self {
        Vfs {
            cwd: "/".to_string(),
            root: "/".to_string(),
        }
    }

    /// Нормализует путь в список канонических компонент (без ".", "..", пустых).
    /// Относительные пути отсчитываются от cwd; абсолютные - от chroot-корня.
    /// None - если путь вылезает выше допустимого корня.
    fn canon_parts(&self, path: &str) -> Option<Vec<String>> {
        let path = path.trim();
        let mut out: Vec<String> = Vec::new();
        // Ниже этого уровня pop запрещён: для абсолютных путей это chroot-корень,
        // для относительных cwd всегда лежит внутри корня, так что 0.
        let floor;
        if !path.starts_with('/') {
            // cwd всегда каноничен ("/a/b"), так что просто добавляем его части.
            let base = self.cwd.trim_start_matches('/');
            if !base.is_empty() {
                for p in base.split('/') {
                    out.push(p.to_string());
                }
            }
            floor = 0;
        } else {
            let base = self.root.trim_start_matches('/');
            if !base.is_empty() {
                for p in base.split('/') {
                    out.push(p.to_string());
                }
            }
            floor = out.len();
        }
        for p in path.split('/') {
            match p {
                "" | "." => {}
                ".." => {
                    if out.len() <= floor {
                        return None;
                    }
                    out.pop();
                }
                name => {
                    if name.len() > vitafs::DIRENT_NAME_MAX {
                        return None;
                    }
                    out.push(name.to_string());
                }
            }
        }
        Some(out)
    }

    fn parts_to_path(parts: &[String]) -> String {
        if parts.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", parts.join("/"))
        }
    }

    /// Резолвит канонические компоненты в инод, попутно возвращая иноды всех
    /// промежуточных каталогов (включая корень и финальный узел).
    ///
    /// Симлинки раскручиваются здесь: при встрече TYPE_SYMLINK цель
    /// подставляется в очередь компонентов и обход перезапускается с корня.
    /// Относительные цели считаются от каталога, где лежит ссылка.
    /// Глубина ограничена 8 - иначе a->b->a зациклило бы навсегда.
    fn walk(parts: &[String]) -> Option<(u32, Vec<u32>)> {
        let sb = vitafs::mounted_sb()?;
        let mut queue: Vec<String> = parts.to_vec();
        let mut chain = Vec::with_capacity(queue.len() + 1);
        chain.push(vitafs::ROOT_INODE);
        let mut depth = 0usize;
        let mut i = 0usize;
        while i < queue.len() {
            let parent = *chain.last()?;
            let child = vitafs::dir_lookup(sb, parent, &queue[i])?;
            let node = vitafs::iget(sb, child)?;
            if node.itype == vitafs::TYPE_SYMLINK {
                depth += 1;
                if depth > 8 {
                    return None;
                }
                let target = node.target_str()?.to_string();
                let rest: Vec<String> = queue.drain(i + 1..).collect();
                // Новая полная очередь: префикс до ссылки (для относительных)
                // либо пусто (для абсолютных), затем компоненты цели, затем хвост.
                let mut np: Vec<String> = Vec::new();
                if !target.starts_with('/') {
                    np.extend(queue[..i].iter().cloned());
                }
                for p in target.split('/') {
                    match p {
                        "" | "." => {}
                        ".." => {
                            np.pop()?;
                        }
                        n => {
                            if n.len() > vitafs::DIRENT_NAME_MAX {
                                return None;
                            }
                            np.push(n.to_string());
                        }
                    }
                }
                np.extend(rest);
                queue = np;
                chain.clear();
                chain.push(vitafs::ROOT_INODE);
                i = 0;
                continue;
            }
            chain.push(child);
            i += 1;
        }
        Some((chain[chain.len() - 1], chain))
    }

    fn resolve(&self, path: &str) -> Option<u32> {
        Self::walk(&self.canon_parts(path)?).map(|(ino, _)| ino)
    }

    /// Резолв + имя последней компоненты (нужно устройствам: /dev/null ->
    /// inode CHARDEV плюс "null" для маршрутизации вызова).
    fn resolve_named(&self, path: &str) -> Option<(u32, String)> {
        let parts = self.canon_parts(path)?;
        let name = parts.last()?.clone();
        let ino = Self::walk(&parts).map(|(ino, _)| ino)?;
        Some((ino, name))
    }

    /// (инод родительского каталога, имя последней компоненты).
    fn split_parent(&self, path: &str) -> Option<(u32, String)> {
        let parts = self.canon_parts(path)?;
        let name = parts.last()?.clone();
        let parent_parts = &parts[..parts.len() - 1];
        let (parent, _) = Self::walk(parent_parts)?;
        Some((parent, name))
    }

    pub fn pwd(&self) -> String {
        self.cwd.clone()
    }

    /// Односторонний chroot: все абсолютные пути отныне канонизируются
    /// от нового корня. Возврат - только перезагрузкой.
    pub fn chroot(&mut self, path: &str) -> bool {
        let parts = match self.canon_parts(path) {
            Some(p) => p,
            None => return false,
        };
        let sb = match vitafs::mounted_sb() {
            Some(sb) => sb,
            None => return false,
        };
        if let Some((ino, _)) = Self::walk(&parts) {
            match vitafs::iget(sb, ino) {
                Some(n) if n.itype == vitafs::TYPE_DIR => {
                    self.root = Self::parts_to_path(&parts);
                    self.cwd = "/".to_string();
                    true
                }
                _ => false,
            }
        } else {
            false
        }
    }

    /// Создаёт спецфайл устройства (TYPE_CHARDEV). Имя последней компоненты
    /// обязано быть зарегистрированным устройством.
    pub fn mknod(&mut self, path: &str) -> Result<(), ()> {
        wal_txn(|| {
            let sb = vitafs::mounted_sb().ok_or(())?;
            let (parent, name) = self.split_parent(path).ok_or(())?;
            let id = crate::devices::dev_id(&name).ok_or(())?;
            if vitafs::dir_lookup(sb, parent, &name).is_some() {
                // Уже есть (перезагрузка после install) - считаем успехом.
                return Ok(());
            }
            let ino = vitafs::create_node(sb, parent, &name, vitafs::TYPE_CHARDEV).ok_or(())?;
            let mut node = vitafs::iget(sb, ino).ok_or(())?;
            node.device_id = id;
            if !vitafs::iput(sb, ino, &node) {
                return Err(());
            }
            Ok(())
        })
    }

    /// Создаёт симлинк link_path -> target (fast-symlink, цель в иноде).
    pub fn symlink(&mut self, target: &str, link_path: &str) -> Result<(), ()> {
        wal_txn(|| {
            let sb = vitafs::mounted_sb().ok_or(())?;
            let (parent, name) = self.split_parent(link_path).ok_or(())?;
            if vitafs::dir_lookup(sb, parent, &name).is_some() {
                return Err(());
            }
            let mut node = vitafs::DiskInode::zeroed(vitafs::TYPE_SYMLINK);
            if !node.set_target(target) {
                return Err(());
            }
            node.size = target.len() as u32;
            let ino = vitafs::alloc_inode(sb).ok_or(())?;
            if !vitafs::iput(sb, ino, &node) {
                vitafs::free_inode(sb, ino);
                return Err(());
            }
            if !vitafs::dir_add(sb, parent, &name, ino) {
                let zeroed = vitafs::DiskInode::zeroed(vitafs::TYPE_FREE);
                let _ = vitafs::iput(sb, ino, &zeroed);
                vitafs::free_inode(sb, ino);
                return Err(());
            }
            Ok(())
        })
    }

    pub fn ls(&self, path: &str) -> Option<Vec<(String, EntryType, usize)>> {
        let sb = vitafs::mounted_sb()?;
        let ino = self.resolve(path)?;
        let node = vitafs::iget(sb, ino)?;
        if node.itype != vitafs::TYPE_DIR {
            return None;
        }
        let entries = vitafs::dir_readdir(sb, ino)?;
        let mut out = Vec::with_capacity(entries.len());
        for e in entries {
            let child = vitafs::iget(sb, e.ino)?;
            let t = match child.itype {
                vitafs::TYPE_DIR => EntryType::Directory,
                vitafs::TYPE_CHARDEV => EntryType::CharDev,
                vitafs::TYPE_SYMLINK => EntryType::Symlink,
                _ => EntryType::File,
            };
            out.push((e.name, t, child.size as usize));
        }
        Some(out)
    }

    pub fn cd(&mut self, path: &str) -> bool {
        let parts = match self.canon_parts(path) {
            Some(p) => p,
            None => return false,
        };
        let sb = match vitafs::mounted_sb() {
            Some(sb) => sb,
            None => return false,
        };
        if let Some((ino, _)) = Self::walk(&parts) {
            match vitafs::iget(sb, ino) {
                Some(n) if n.itype == vitafs::TYPE_DIR => {
                    self.cwd = Self::parts_to_path(&parts);
                    return true;
                }
                _ => {}
            }
        }
        false
    }

    pub fn mkdir(&mut self, path: &str) -> Result<(), ()> {
        wal_txn(|| {
            let sb = vitafs::mounted_sb().ok_or(())?;
            let (parent, name) = self.split_parent(path).ok_or(())?;
            vitafs::create_node(sb, parent, &name, vitafs::TYPE_DIR).ok_or(())?;
            Ok(())
        })
    }

    pub fn rmdir(&mut self, path: &str) -> Result<(), ()> {
        wal_txn(|| {
            let sb = vitafs::mounted_sb().ok_or(())?;
            let (parent, name) = self.split_parent(path).ok_or(())?;
            if vitafs::destroy_node(sb, parent, &name, vitafs::TYPE_DIR) {
                Ok(())
            } else {
                Err(())
            }
        })
    }

    pub fn touch(&mut self, path: &str) -> Result<(), ()> {
        wal_txn(|| {
            let sb = vitafs::mounted_sb().ok_or(())?;
            let (parent, name) = self.split_parent(path).ok_or(())?;
            if let Some(existing) = vitafs::dir_lookup(sb, parent, &name) {
                // Настоящий touch не стирает содержимое существующего файла.
                return match vitafs::iget(sb, existing) {
                    Some(n) if n.itype == vitafs::TYPE_FILE => Ok(()),
                    _ => Err(()),
                };
            }
            vitafs::create_node(sb, parent, &name, vitafs::TYPE_FILE).ok_or(())?;
            Ok(())
        })
    }

    pub fn rm(&mut self, path: &str) -> Result<(), ()> {
        wal_txn(|| {
            let sb = vitafs::mounted_sb().ok_or(())?;
            let (parent, name) = self.split_parent(path).ok_or(())?;
            // Файл, спецфайл или симлинк - сносим любой не-каталог.
            if vitafs::destroy_node(sb, parent, &name, vitafs::TYPE_FILE)
                || vitafs::destroy_node(sb, parent, &name, vitafs::TYPE_CHARDEV)
                || vitafs::destroy_node(sb, parent, &name, vitafs::TYPE_SYMLINK)
            {
                Ok(())
            } else {
                Err(())
            }
        })
    }

    pub fn cat(&self, path: &str) -> Option<Vec<u8>> {
        let sb = vitafs::mounted_sb()?;
        let (ino, name) = self.resolve_named(path)?;
        let node = vitafs::iget(sb, ino)?;
        match node.itype {
            vitafs::TYPE_FILE => vitafs::file_read_all(sb, ino),
            // Спецфайлы: содержимое генерирует драйвер по имени устройства.
            vitafs::TYPE_CHARDEV => crate::devices::dev_read(&name),
            _ => None,
        }
    }

    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), ()> {
        wal_txn(|| {
            let sb = vitafs::mounted_sb().ok_or(())?;
            let (parent, name) = self.split_parent(path).ok_or(())?;
            match vitafs::dir_lookup(sb, parent, &name) {
                Some(existing) => {
                    let node = vitafs::iget(sb, existing).ok_or(())?;
                    match node.itype {
                        vitafs::TYPE_FILE => {
                            if !vitafs::file_write_all(sb, existing, data) {
                                return Err(());
                            }
                        }
                        // Запись в спецфайл уходит в драйвер (echo x > /dev/null).
                        vitafs::TYPE_CHARDEV => {
                            if !crate::devices::dev_write(&name, data) {
                                return Err(());
                            }
                        }
                        _ => return Err(()),
                    }
                }
                None => {
                    let ino = vitafs::create_node(sb, parent, &name, vitafs::TYPE_FILE).ok_or(())?;
                    if !vitafs::file_write_all(sb, ino, data) {
                        // Файл-пустышка лучше потерянной записи: оставляем.
                        return Err(());
                    }
                }
            }
            Ok(())
        })
    }

    /// Дозаписывает данные в конец файла (создаёт файл, если его нет).
    pub fn append(&mut self, path: &str, data: &[u8]) -> Result<(), ()> {
        wal_txn(|| {
            let mut existing = self.cat(path).unwrap_or_default();
            existing.extend_from_slice(data);
            self.write_file(path, &existing)
        })
    }

    /// Копирует файл или директорию (рекурсивно).
    ///
    /// Если `dst` — существующая директория, источник копируется в неё
    /// с сохранением своего имени. Если по целевому пути уже лежит файл —
    /// он перезаписывается (только файл поверх файла).
    pub fn copy(&mut self, src: &str, dst: &str) -> Result<(), ()> {
        wal_txn(|| self.copy_inner(src, dst))
    }

    fn copy_inner(&mut self, src: &str, dst: &str) -> Result<(), ()> {
        let sb = vitafs::mounted_sb().ok_or(())?;
        let src_parts = self.canon_parts(src).ok_or(())?;
        let src_ino = Self::walk(&src_parts).ok_or(())?.0;
        let src_node = vitafs::iget(sb, src_ino).ok_or(())?;

        // Куда именно копируем: либо dst как есть, либо внутрь каталога dst.
        let mut dst_parts = self.canon_parts(dst).ok_or(())?;
        if let Some((dst_ino, _)) = Self::walk(&dst_parts) {
            if let Some(n) = vitafs::iget(sb, dst_ino) {
                if n.itype == vitafs::TYPE_DIR {
                    dst_parts.push(src_parts.last().ok_or(())?.clone());
                }
            }
        }

        // Запрет копировать директорию саму в себя.
        if src_node.itype == vitafs::TYPE_DIR && is_prefix(&src_parts, &dst_parts) {
            return Err(());
        }

        let (dst_parent_ino, _) =
            Self::walk(&dst_parts[..dst_parts.len() - 1]).ok_or(())?;
        let dst_name = dst_parts.last().ok_or(())?.clone();

        if let Some(target) = vitafs::dir_lookup(sb, dst_parent_ino, &dst_name) {
            // Разрешена только замена файла файлом.
            let tnode = vitafs::iget(sb, target).ok_or(())?;
            if tnode.itype != vitafs::TYPE_FILE || src_node.itype != vitafs::TYPE_FILE {
                return Err(());
            }
            let data = vitafs::file_read_all(sb, src_ino).ok_or(())?;
            return if vitafs::file_write_all(sb, target, &data) {
                Ok(())
            } else {
                Err(())
            };
        }

        copy_tree(sb, src_ino, dst_parent_ino, &dst_name)
    }

    /// Перемещает/переименовывает файл или директорию.
    ///
    /// Если `dst` — существующая директория, источник переносится в неё
    /// с сохранением своего имени. Файл-цель перезаписывается, директория-цель
    /// не трогается.
    pub fn mv(&mut self, src: &str, dst: &str) -> Result<(), ()> {
        wal_txn(|| self.mv_inner(src, dst))
    }

    fn mv_inner(&mut self, src: &str, dst: &str) -> Result<(), ()> {
        let sb = vitafs::mounted_sb().ok_or(())?;
        let src_parts = self.canon_parts(src).ok_or(())?;
        let (src_ino, chain) = Self::walk(&src_parts).ok_or(())?;
        let src_node = vitafs::iget(sb, src_ino).ok_or(())?;
        let src_parent_ino = *chain.get(chain.len() - 2).ok_or(())?;

        let mut dst_parts = self.canon_parts(dst).ok_or(())?;
        if let Some((dst_ino, _)) = Self::walk(&dst_parts) {
            if let Some(n) = vitafs::iget(sb, dst_ino) {
                if n.itype == vitafs::TYPE_DIR {
                    dst_parts.push(src_parts.last().ok_or(())?.clone());
                }
            }
        }

        if src_parts == dst_parts {
            return Ok(()); // mv a a — бездействие
        }

        // Запрет перемещать директорию саму в себя.
        if src_node.itype == vitafs::TYPE_DIR && is_prefix(&src_parts, &dst_parts) {
            return Err(());
        }

        let (dst_parent_ino, _) =
            Self::walk(&dst_parts[..dst_parts.len() - 1]).ok_or(())?;
        let dst_name = dst_parts.last().ok_or(())?.clone();

        // Цель-файл перезаписывается, цель-каталог блокирует перемещение.
        if let Some(target) = vitafs::dir_lookup(sb, dst_parent_ino, &dst_name) {
            if target == src_ino {
                return Ok(());
            }
            let tnode = vitafs::iget(sb, target).ok_or(())?;
            if tnode.itype != vitafs::TYPE_FILE {
                return Err(());
            }
            if !vitafs::destroy_node(sb, dst_parent_ino, &dst_name, vitafs::TYPE_FILE) {
                return Err(());
            }
        }

        // Перенос записи: убрать из старого каталога, добавить в новый.
        if !vitafs::dir_remove(sb, src_parent_ino, src_parts.last().ok_or(())?) {
            return Err(());
        }
        if !vitafs::dir_add(sb, dst_parent_ino, &dst_name, src_ino) {
            // Откат: иначе узел осиротеет.
            let _ = vitafs::dir_add(
                sb,
                src_parent_ino,
                src_parts.last().ok_or(())?,
                src_ino,
            );
            return Err(());
        }
        Ok(())
    }

    /// Ищет по всему диску (рекурсивно от корня) все файлы и директории
    /// с именем `name` и возвращает их полные пути.
    pub fn find(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(sb) = vitafs::mounted_sb() {
            find_from(sb, vitafs::ROOT_INODE, "", name, &mut out);
        }
        out
    }
}

/// true, если `prefix` — префикс `parts` (включая равенство): parts лежит
/// внутри поддерева prefix.
fn is_prefix(prefix: &[String], parts: &[String]) -> bool {
    parts.len() >= prefix.len() && parts[..prefix.len()] == *prefix
}

/// Рекурсивное копирование дерева: файлы - через read/write_all,
/// каталоги - созданием и обходом записей.
fn copy_tree(sb: &vitafs::Superblock, src_ino: u32, dst_parent: u32, name: &str) -> Result<(), ()> {
    let src_node = vitafs::iget(sb, src_ino).ok_or(())?;
    let new_ino = vitafs::create_node(sb, dst_parent, name, src_node.itype).ok_or(())?;
    if src_node.itype == vitafs::TYPE_FILE {
        let data = vitafs::file_read_all(sb, src_ino).ok_or(())?;
        if !vitafs::file_write_all(sb, new_ino, &data) {
            return Err(());
        }
    } else if src_node.itype == vitafs::TYPE_DIR {
        for entry in vitafs::dir_readdir(sb, src_ino).ok_or(())? {
            copy_tree(sb, entry.ino, new_ino, &entry.name)?;
        }
    } else {
        return Err(());
    }
    Ok(())
}

fn find_from(sb: &vitafs::Superblock, dir_ino: u32, prefix: &str, name: &str, out: &mut Vec<String>) {
    let entries = match vitafs::dir_readdir(sb, dir_ino) {
        Some(e) => e,
        None => return,
    };
    for e in entries {
        let path = if prefix.is_empty() {
            format!("/{}", e.name)
        } else {
            format!("{}/{}", prefix, e.name)
        };
        if e.name == name {
            out.push(path.clone());
        }
        if let Some(node) = vitafs::iget(sb, e.ino) {
            if node.itype == vitafs::TYPE_DIR {
                find_from(sb, e.ino, &path, name, out);
            }
        }
    }
}

/// Обёртка транзакции WAL вокруг мутации: успех -> commit, ошибка -> abort.
fn wal_txn<T>(f: impl FnOnce() -> Result<T, ()>) -> Result<T, ()> {
    crate::wal::begin();
    match f() {
        Ok(v) => {
            crate::wal::commit();
            Ok(v)
        }
        Err(e) => {
            crate::wal::abort();
            Err(e)
        }
    }
}
