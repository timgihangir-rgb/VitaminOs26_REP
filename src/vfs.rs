// src/vfs.rs
//
// ПЕРЕПИСАНО ЦЕЛИКОМ. Старая версия хранила дерево как Vec<FsEntry> с полем
// `parent: *mut FsEntry` - "сырым" указателем на родителя. Проблема: как
// только `Vec::push` вызывает реаллокацию (а это происходит при почти любом
// mkdir/touch), ВСЕ элементы этого Vec физически переезжают в новую область
// памяти - и все указатели, которые куда-то в них указывали (в т.ч. parent
// указатели детей и `current`, т.е. текущая директория), становятся
// висячими (dangling). Плюс в Vfs::new() был classic self-referential-struct
// баг: `root.parent = &mut root` брало адрес локальной переменной ДО того,
// как эта переменная переехала (была перемещена) в поле структуры.
//
// Итог - неопределённое поведение при обычной работе с файлами: иногда
// файлы "не создавались", иногда падало, воспроизвести стабильно было
// невозможно - именно то, что вы описали.
//
// Новая версия хранит все узлы плоско в `Vec<FsEntry>` (арену) и ссылается
// на них через usize-индексы вместо указателей. Индексы НЕ инвалидируются
// реаллокацией Vec (в отличие от указателей/ссылок) - это и есть исправление.
// rmdir/rm не сдвигают чужие индексы: они просто убирают ссылку на ребёнка
// из списка `children` родителя, а сам узел остаётся в арене "осиротевшим".
// Для игрушечной in-memory ФС это нормальная и безопасная цена простоты.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EntryType {
    File,
    Directory,
}

#[derive(Debug, Clone)]
struct FsEntry {
    name: String,
    entry_type: EntryType,
    content: Vec<u8>,
    parent: Option<usize>,
    children: Vec<usize>,
}

impl FsEntry {
    fn new_file(name: &str, parent: usize) -> Self {
        Self {
            name: name.to_string(),
            entry_type: EntryType::File,
            content: Vec::new(),
            parent: Some(parent),
            children: Vec::new(),
        }
    }

    fn new_dir(name: &str, parent: Option<usize>) -> Self {
        Self {
            name: name.to_string(),
            entry_type: EntryType::Directory,
            content: Vec::new(),
            parent,
            children: Vec::new(),
        }
    }
}

const ROOT: usize = 0;

pub struct Vfs {
    nodes: Vec<FsEntry>,
    current: usize,
}

impl Vfs {
    pub fn new() -> Self {
        let root = FsEntry::new_dir("/", None);
        Vfs {
            nodes: alloc::vec![root],
            current: ROOT,
        }
    }

    pub fn init(&mut self) {
        let _ = self.mkdir("/bin");
        let _ = self.mkdir("/etc");
        let _ = self.mkdir("/home");
        let _ = self.mkdir("/tmp");
        let _ = self.cd("/home");
        let _ = self.touch("/etc/passwd");
        let _ = self.touch("/etc/hostname");
        let _ = self.write_file("/etc/hostname", b"VitaminOS26\n");

        self.install_bin();
    }

    /// Записывает встроенные программы ядра в /bin (перезаписывая одноимённые
    /// файлы). Используется при создании начальной ФС и после загрузки слепка
    /// с диска, чтобы бинарники всегда были актуальными.
    pub fn install_bin(&mut self) {
        for prog in crate::programs::PROGRAMS {
            let path = alloc::format!("/bin/{}", prog.name);
            let _ = self.write_file(&path, prog.data);
        }
    }

    /// Сериализует живое дерево (только узлы, достижимые из корня) в бинарный
    /// буфер для записи на диск.
    ///
    /// Обход — BFS от корня: родитель всегда сериализуется раньше ребёнка,
    /// поэтому порядок не зависит от индексов арены (после mv они нарушаются).
    /// Вместо индексов арены у каждого узла хранится серийный индекс родителя.
    ///
    /// Формат: u32 count, затем на каждый узел:
    ///   u32 len_имени | имя | u8 тип (0 файл, 1 каталог) | u32 серийный_родителя
    ///   (0xFFFFFFFF у корня) | u32 len_контента | контент.
    pub fn serialize(&self) -> Vec<u8> {
        let mut frontier = Vec::new();
        frontier.push(ROOT);
        let mut order = Vec::new();
        let mut i = 0;
        while i < frontier.len() {
            let idx = frontier[i];
            i += 1;
            order.push(idx);
            for &c in &self.nodes[idx].children {
                frontier.push(c);
            }
        }

        let mut serial = alloc::vec![0usize; self.nodes.len()];
        for (pos, &idx) in order.iter().enumerate() {
            serial[idx] = pos;
        }

        let mut out = Vec::new();
        out.extend_from_slice(&(order.len() as u32).to_le_bytes());
        for &idx in &order {
            let n = &self.nodes[idx];
            out.extend_from_slice(&(n.name.len() as u32).to_le_bytes());
            out.extend_from_slice(n.name.as_bytes());
            out.push(match n.entry_type {
                EntryType::File => 0,
                EntryType::Directory => 1,
            });
            let parent_serial = match n.parent {
                Some(p) => serial[p] as u32,
                None => 0xFFFF_FFFF,
            };
            out.extend_from_slice(&parent_serial.to_le_bytes());
            out.extend_from_slice(&(n.content.len() as u32).to_le_bytes());
            out.extend_from_slice(&n.content);
        }
        out
    }

    /// Полностью пересобирает дерево из слепка, полученного с диска.
    /// Возвращает false при повреждённых данных (тогда дерево оставляется
    /// в исходном состоянии — Vfs::new).
    pub fn rebuild_from(&mut self, data: &[u8]) -> bool {
        let mut pos = 0usize;
        let take_u32 = |data: &[u8], pos: &mut usize| -> Option<u32> {
            if *pos + 4 > data.len() {
                return None;
            }
            let v = u32::from_le_bytes(data[*pos..*pos + 4].try_into().ok()?);
            *pos += 4;
            Some(v)
        };

        let count = match take_u32(data, &mut pos) {
            Some(c) => c as usize,
            None => return false,
        };
        if count == 0 || count > 1_000_000 {
            return false;
        }

        self.nodes.clear();
        self.nodes.push(FsEntry::new_dir("/", None));
        self.current = ROOT;
        let mut serial_to_node = Vec::with_capacity(count);
        serial_to_node.push(ROOT);

        for _ in 0..count {
            let name_len = match take_u32(data, &mut pos) {
                Some(l) => l as usize,
                None => return false,
            };
            if pos + name_len > data.len() {
                return false;
            }
            let name = match core::str::from_utf8(&data[pos..pos + name_len]) {
                Ok(s) => s.to_string(),
                Err(_) => return false,
            };
            pos += name_len;
            if pos >= data.len() {
                return false;
            }
            let etype = data[pos];
            pos += 1;
            let parent_serial = match take_u32(data, &mut pos) {
                Some(p) => p as usize,
                None => return false,
            };
            let is_root = parent_serial == 0xFFFF_FFFF;
            if !is_root && parent_serial >= serial_to_node.len() {
                return false;
            }
            let content_len = match take_u32(data, &mut pos) {
                Some(l) => l as usize,
                None => return false,
            };
            if pos + content_len > data.len() {
                return false;
            }
            let content = data[pos..pos + content_len].to_vec();
            pos += content_len;

            // Корень уже находится в арене под индексом ROOT - узел из слепка
            // не создаём, иначе получится два корня и все индексы съедут.
            if is_root {
                continue;
            }

            let parent = serial_to_node[parent_serial];
            if self.nodes[parent]
                .children
                .iter()
                .any(|&c| self.nodes[c].name == name)
            {
                return false;
            }

            let node_idx = match etype {
                0 => {
                    let mut f = FsEntry::new_file(&name, parent);
                    f.content = content;
                    let idx = self.nodes.len();
                    self.nodes.push(f);
                    idx
                }
                1 => {
                    let idx = self.nodes.len();
                    self.nodes.push(FsEntry::new_dir(&name, Some(parent)));
                    idx
                }
                _ => return false,
            };
            self.nodes[parent].children.push(node_idx);
            serial_to_node.push(node_idx);
        }

        self.current = ROOT;
        true
    }

    /// Резолвит путь в индекс узла арены, начиная от `start` (для
    /// относительных путей) либо от корня (для абсолютных, начинающихся с '/').
    ///
    /// Важно: директорией обязана быть только НЕПОСЛЕДНЯЯ часть пути.
    /// Последняя часть может оказаться и файлом - раньше это было не так,
    /// из-за чего cat() на любой существующий файл ошибочно возвращал None.
    fn resolve_from(&self, start: usize, path: &str) -> Option<usize> {
        let path = path.trim();
        let mut idx = if path.starts_with('/') { ROOT } else { start };
        if path.is_empty() {
            return Some(idx);
        }

        for part in path.split('/').filter(|s| !s.is_empty()) {
            if part == "." {
                continue;
            }
            if part == ".." {
                idx = self.nodes[idx].parent.unwrap_or(ROOT);
                continue;
            }
            if self.nodes[idx].entry_type != EntryType::Directory {
                return None;
            }
            idx = self.nodes[idx]
                .children
                .iter()
                .copied()
                .find(|&c| self.nodes[c].name == part)?;
        }
        Some(idx)
    }

    pub fn resolve(&self, path: &str) -> Option<usize> {
        self.resolve_from(self.current, path)
    }

    fn node_path(&self, idx: usize) -> String {
        if idx == ROOT {
            return "/".to_string();
        }
        let mut parts = Vec::new();
        let mut i = idx;
        while i != ROOT {
            parts.push(self.nodes[i].name.clone());
            i = self.nodes[i].parent.unwrap_or(ROOT);
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    fn current_path(&self) -> String {
        self.node_path(self.current)
    }

    pub fn pwd(&self) -> String {
        self.current_path()
    }

    /// True, если `idx` находится внутри поддерева `ancestor`
    /// (ancestor != ROOT). Используется для запрета cp/mv директории в себя.
    fn is_descendant(&self, mut idx: usize, ancestor: usize) -> bool {
        while idx != ROOT {
            if idx == ancestor {
                return true;
            }
            idx = self.nodes[idx].parent.unwrap_or(ROOT);
        }
        false
    }

    pub fn ls(&self, path: &str) -> Option<Vec<(String, EntryType, usize)>> {
        let idx = self.resolve(path)?;
        if self.nodes[idx].entry_type != EntryType::Directory {
            return None;
        }
        Some(
            self.nodes[idx]
                .children
                .iter()
                .map(|&c| {
                    let n = &self.nodes[c];
                    (n.name.clone(), n.entry_type, n.content.len())
                })
                .collect(),
        )
    }

    pub fn cd(&mut self, path: &str) -> bool {
        let path = path.trim();
        if path.is_empty() || path == "/" {
            self.current = ROOT;
            return true;
        }
        match self.resolve(path) {
            Some(idx) if self.nodes[idx].entry_type == EntryType::Directory => {
                self.current = idx;
                true
            }
            _ => false,
        }
    }

    /// Разбивает путь на (индекс родительской директории, имя последнего компонента).
    fn split_parent(&self, path: &str) -> Option<(usize, String)> {
        let path = path.trim();
        if path.is_empty() {
            return None;
        }
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let name = (*parts.last()?).to_string();

        let parent_idx = if parts.len() > 1 {
            let parent_rel = parts[..parts.len() - 1].join("/");
            let parent_path = if path.starts_with('/') {
                format!("/{}", parent_rel)
            } else {
                parent_rel
            };
            self.resolve(&parent_path)?
        } else if path.starts_with('/') {
            ROOT
        } else {
            self.current
        };

        if self.nodes[parent_idx].entry_type != EntryType::Directory {
            return None;
        }
        Some((parent_idx, name))
    }

    pub fn mkdir(&mut self, path: &str) -> Result<(), ()> {
        let (parent_idx, name) = self.split_parent(path).ok_or(())?;
        if self.nodes[parent_idx]
            .children
            .iter()
            .any(|&c| self.nodes[c].name == name)
        {
            return Err(());
        }
        let new_idx = self.nodes.len();
        self.nodes.push(FsEntry::new_dir(&name, Some(parent_idx)));
        self.nodes[parent_idx].children.push(new_idx);
        Ok(())
    }

    pub fn rmdir(&mut self, path: &str) -> Result<(), ()> {
        let (parent_idx, name) = self.split_parent(path).ok_or(())?;
        let pos = self.nodes[parent_idx]
            .children
            .iter()
            .position(|&c| self.nodes[c].name == name)
            .ok_or(())?;
        let child_idx = self.nodes[parent_idx].children[pos];
        if self.nodes[child_idx].entry_type != EntryType::Directory {
            return Err(());
        }
        if !self.nodes[child_idx].children.is_empty() {
            return Err(());
        }
        self.nodes[parent_idx].children.remove(pos);
        Ok(())
    }

    pub fn touch(&mut self, path: &str) -> Result<(), ()> {
        let (parent_idx, name) = self.split_parent(path).ok_or(())?;
        if let Some(&existing) = self.nodes[parent_idx]
            .children
            .iter()
            .find(|&&c| self.nodes[c].name == name)
        {
            // Настоящий touch не стирает содержимое существующего файла.
            return if self.nodes[existing].entry_type == EntryType::File {
                Ok(())
            } else {
                Err(())
            };
        }
        let new_idx = self.nodes.len();
        self.nodes.push(FsEntry::new_file(&name, parent_idx));
        self.nodes[parent_idx].children.push(new_idx);
        Ok(())
    }

    pub fn rm(&mut self, path: &str) -> Result<(), ()> {
        let (parent_idx, name) = self.split_parent(path).ok_or(())?;
        let pos = self.nodes[parent_idx]
            .children
            .iter()
            .position(|&c| self.nodes[c].name == name)
            .ok_or(())?;
        let child_idx = self.nodes[parent_idx].children[pos];
        if self.nodes[child_idx].entry_type != EntryType::File {
            return Err(());
        }
        self.nodes[parent_idx].children.remove(pos);
        Ok(())
    }

    pub fn cat(&self, path: &str) -> Option<&[u8]> {
        let idx = self.resolve(path)?;
        if self.nodes[idx].entry_type != EntryType::File {
            return None;
        }
        Some(&self.nodes[idx].content)
    }

    pub fn write_file(&mut self, path: &str, data: &[u8]) -> Result<(), ()> {
        let (parent_idx, name) = self.split_parent(path).ok_or(())?;
        if let Some(&existing) = self.nodes[parent_idx]
            .children
            .iter()
            .find(|&&c| self.nodes[c].name == name)
        {
            return if self.nodes[existing].entry_type == EntryType::File {
                self.nodes[existing].content = data.to_vec();
                Ok(())
            } else {
                Err(())
            };
        }
        let mut f = FsEntry::new_file(&name, parent_idx);
        f.content = data.to_vec();
        let new_idx = self.nodes.len();
        self.nodes.push(f);
        self.nodes[parent_idx].children.push(new_idx);
        Ok(())
    }

    pub fn echo(&mut self, args: &[&str]) -> Result<(), ()> {
        if args.is_empty() {
            return Ok(());
        }
        if let Some(gt_pos) = args.iter().position(|&a| a == ">") {
            let message = args[..gt_pos].join(" ");
            let path = *args.get(gt_pos + 1).ok_or(())?;
            self.write_file(path, message.as_bytes())
        } else {
            let data = args.join(" ") + "\n";
            self.write_file("/tmp/last_echo.txt", data.as_bytes())
        }
    }

    /// Копирует файл или директорию (рекурсивно).
    ///
    /// Если `dst` — существующая директория, источник копируется в неё
    /// с сохранением своего имени. Если по целевому пути уже лежит файл —
    /// он перезаписывается (только файл поверх файла).
    pub fn copy(&mut self, src: &str, dst: &str) -> Result<(), ()> {
        let src_idx = self.resolve(src).ok_or(())?;
        let (mut dst_parent, mut dst_name) = self.split_parent(dst).ok_or(())?;

        if let Some(dst_idx) = self.resolve(dst) {
            if self.nodes[dst_idx].entry_type == EntryType::Directory {
                dst_parent = dst_idx;
                dst_name = self.nodes[src_idx].name.clone();
            }
        }

        // Запрет копировать директорию саму в себя.
        if self.nodes[src_idx].entry_type == EntryType::Directory
            && self.is_descendant(dst_parent, src_idx)
        {
            return Err(());
        }

        // Если целевой узел уже существует, разрешена только замена файла файлом.
        if let Some(pos) = self.nodes[dst_parent]
            .children
            .iter()
            .position(|&c| self.nodes[c].name == dst_name)
        {
            let existing = self.nodes[dst_parent].children[pos];
            if self.nodes[existing].entry_type != EntryType::File
                || self.nodes[src_idx].entry_type != EntryType::File
            {
                return Err(());
            }
            let data = self.nodes[src_idx].content.clone();
            self.nodes[existing].content = data;
            return Ok(());
        }

        let new_idx = self.copy_node(src_idx, dst_parent, &dst_name)?;
        self.nodes[dst_parent].children.push(new_idx);
        Ok(())
    }

    fn copy_node(
        &mut self,
        src_idx: usize,
        dst_parent: usize,
        dst_name: &str,
    ) -> Result<usize, ()> {
        if self.nodes[src_idx].entry_type == EntryType::Directory {
            let new_idx = self.nodes.len();
            self.nodes
                .push(FsEntry::new_dir(dst_name, Some(dst_parent)));
            let children: Vec<usize> = self.nodes[src_idx].children.clone();
            for &c in &children {
                let child_name = self.nodes[c].name.clone();
                let child_idx = self.copy_node(c, new_idx, &child_name)?;
                self.nodes[new_idx].children.push(child_idx);
            }
            Ok(new_idx)
        } else {
            let mut f = FsEntry::new_file(dst_name, dst_parent);
            f.content = self.nodes[src_idx].content.clone();
            let new_idx = self.nodes.len();
            self.nodes.push(f);
            Ok(new_idx)
        }
    }

    /// Перемещает/переименовывает файл или директорию.
    ///
    /// Если `dst` — существующая директория, источник переносится в неё
    /// с сохранением своего имени. Файл-цель перезаписывается, директория-цель
    /// не трогается.
    pub fn mv(&mut self, src: &str, dst: &str) -> Result<(), ()> {
        let src_idx = self.resolve(src).ok_or(())?;
        let src_parent = self.nodes[src_idx].parent.ok_or(())?;

        let (mut dst_parent, mut dst_name) = self.split_parent(dst).ok_or(())?;
        if let Some(dst_idx) = self.resolve(dst) {
            if self.nodes[dst_idx].entry_type == EntryType::Directory {
                dst_parent = dst_idx;
                dst_name = self.nodes[src_idx].name.clone();
            }
        }

        // Запрет перемещать директорию саму в себя.
        if self.nodes[src_idx].entry_type == EntryType::Directory
            && self.is_descendant(dst_parent, src_idx)
        {
            return Err(());
        }

        if let Some(pos) = self.nodes[dst_parent]
            .children
            .iter()
            .position(|&c| self.nodes[c].name == dst_name)
        {
            let existing = self.nodes[dst_parent].children[pos];
            if existing == src_idx {
                return Ok(()); // mv a a — бездействие
            }
            if self.nodes[existing].entry_type != EntryType::File {
                return Err(());
            }
            self.nodes[dst_parent].children.remove(pos);
        }

        let src_pos = self
            .nodes[src_parent]
            .children
            .iter()
            .position(|&c| c == src_idx)
            .ok_or(())?;
        self.nodes[src_parent].children.remove(src_pos);

        self.nodes[src_idx].name = dst_name;
        self.nodes[src_idx].parent = Some(dst_parent);
        self.nodes[dst_parent].children.push(src_idx);
        Ok(())
    }

    /// Ищет по всему диску (рекурсивно от корня) все файлы и директории
    /// с именем `name` и возвращает их полные пути.
    pub fn find(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        self.find_from(ROOT, name, &mut out);
        out
    }

    fn find_from(&self, idx: usize, name: &str, out: &mut Vec<String>) {
        for &c in &self.nodes[idx].children {
            let child = &self.nodes[c];
            if child.name == name {
                out.push(self.node_path(c));
            }
            if child.entry_type == EntryType::Directory {
                self.find_from(c, name, out);
            }
        }
    }
}