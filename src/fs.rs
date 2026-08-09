// src/fs.rs
use crate::vga::Writer;
use crate::vfs::Vfs;
use alloc::format;

pub fn init_filesystem() {}

pub fn list_files(writer: &mut Writer, vfs: &Vfs, path: &str) {
    match vfs.ls(path) {
        Some(entries) if !entries.is_empty() => {
            for (name, entry_type, size) in entries {
                let kind = if entry_type == crate::vfs::EntryType::Directory {
                    "DIR"
                } else {
                    "FILE"
                };
                writer.write_string(&format!("{:<8} {:>6}  {}\n", kind, size, name));
            }
        }
        Some(_) => {
            writer.write_string("(empty)\n");
        }
        None => {
            writer.write_string("No such directory: ");
            writer.write_string(path);
            writer.write_string("\n");
        }
    }
}

pub fn read_file(writer: &mut Writer, vfs: &Vfs, filename: &str) {
    match vfs.cat(filename) {
        Some(data) => {
            for byte in data {
                writer.write_byte(*byte);
            }
        }
        None => {
            writer.write_string("File not found: ");
            writer.write_string(filename);
            writer.write_string("\n");
        }
    }
}

pub fn change_directory(vfs: &mut Vfs, path: &str) -> bool {
    vfs.cd(path)
}



#[allow(dead_code)]

pub fn make_directory(vfs: &mut Vfs, path: &str) -> Result<(), ()> {
    vfs.mkdir(path)
}

pub fn remove_directory(vfs: &mut Vfs, path: &str) -> Result<(), ()> {
    vfs.rmdir(path)
}

pub fn create_file(vfs: &mut Vfs, path: &str) -> Result<(), ()> {
    vfs.touch(path)
}

pub fn remove_file(vfs: &mut Vfs, path: &str) -> Result<(), ()> {
    vfs.rm(path)
}

pub fn write_to_file(vfs: &mut Vfs, args: &[&str]) -> Result<(), ()> {
    vfs.echo(args)
}
