use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let boot_obj = out_dir.join("boot.o");

    let status = Command::new("nasm")
        .args(["-f", "elf64", "src/boot.asm", "-o"])
        .arg(&boot_obj)
        .status()
        .expect("nasm not found; install nasm (apt install nasm)");
    assert!(status.success(), "nasm failed");

    let archive = out_dir.join("libboot.a");
    let status = Command::new("ar")
        .args(["crs", &archive.to_string_lossy()])
        .arg(&boot_obj)
        .status()
        .expect("ar not found");
    assert!(status.success(), "ar failed");

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=boot");
    println!("cargo:rerun-if-changed=src/boot.asm");
    println!("cargo:rerun-if-changed=linker.ld");
}
