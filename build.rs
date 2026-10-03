use std::path::{Path, PathBuf};
use std::process::Command;

/// Собирает один исходник nasm в объект нужного формата.
fn nasm(src: &Path, format: &str, out: &Path, include_dir: &Path) {
    let status = Command::new("nasm")
        .arg("-I")
        .arg(include_dir)
        .args(["-f", format, &src.to_string_lossy(), "-o"])
        .arg(out)
        .status()
        .expect("nasm not found; install nasm (apt install nasm)");
    assert!(status.success(), "nasm failed on {}", src.display());
}

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // Загрузчик ядра: 32-битный трамполин, который GRUB поднимает в 64-битный
    // режим. Формат elf64 - его нужно линковать с Rust-объектами.
    nasm(
        Path::new("src/boot.asm"),
        "elf64",
        &out_dir.join("boot.o"),
        Path::new("src"),
    );

    // Собственный загрузчик установленной системы. Две стадии, обе в
    // реальном режиме, формат bin: их не с чем линковать, они идут на диск
    // как есть. stage1 обязан быть ровно 512 байт - это MBR целиком.
    let bootldr = Path::new("src/bootldr");
    nasm(
        &bootldr.join("stage1.asm"),
        "bin",
        &out_dir.join("stage1.bin"),
        bootldr,
    );
    nasm(
        &bootldr.join("stage2.asm"),
        "bin",
        &out_dir.join("stage2.bin"),
        bootldr,
    );

    // Проверяем размеры на этапе сборки, а не в рантайме установщика:
    // stage1 не влезает в MBR - это тихо ломает загрузку у клиента.
    let stage1 = std::fs::metadata(out_dir.join("stage1.bin"))
        .expect("stage1.bin missing")
        .len();
    assert!(
        stage1 == 512,
        "stage1 must be exactly 512 bytes (MBR), got {}",
        stage1
    );
    let stage2 = std::fs::metadata(out_dir.join("stage2.bin"))
        .expect("stage2.bin missing")
        .len();
    assert!(
        stage2 <= 16 * 512,
        "stage2 must fit in STAGE2_SECTORS=16 sectors, got {} bytes",
        stage2
    );

    let archive = out_dir.join("libboot.a");
    let status = Command::new("ar")
        .args(["crs", &archive.to_string_lossy()])
        .arg(out_dir.join("boot.o"))
        .status()
        .expect("ar not found");
    assert!(status.success(), "ar failed");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=boot");
    println!("cargo:rerun-if-changed=src/boot.asm");
    println!("cargo:rerun-if-changed=src/bootldr/stage1.asm");
    println!("cargo:rerun-if-changed=src/bootldr/stage2.asm");
    println!("cargo:rerun-if-changed=src/bootldr/layout.inc");
    println!("cargo:rerun-if-changed=src/bootldr/print.inc");
    println!("cargo:rerun-if-changed=src/bootldr/print32.inc");
    println!("cargo:rerun-if-changed=linker.ld");
}