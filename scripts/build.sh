#!/bin/bash
set -e

PROGRAMS_DIR="programs"
KERNEL_ELF="target/x86_64-unknown-none/debug/vitamin_os26"
ISO_DIR="target/os.iso.dir"
ISO="target/os.iso"
IMG="target/os.img"

echo "================================="
echo "Building VitaminOS26..."
echo "================================="

###############################################################################
# 1. Check toolchain
###############################################################################
if ! command -v cargo &> /dev/null; then
    echo "Error: Cargo not found."
    exit 1
fi

TOOLCHAIN="nightly-2022-11-01"

if ! rustup toolchain list | grep -q "$TOOLCHAIN"; then
    echo "Installing $TOOLCHAIN toolchain..."
    rustup toolchain install "$TOOLCHAIN"
fi

if ! rustup component list --toolchain "$TOOLCHAIN" | grep -q "rust-src.*installed"; then
    echo "Installing rust-src for $TOOLCHAIN..."
    rustup component add rust-src --toolchain "$TOOLCHAIN"
fi

if ! command -v nasm &> /dev/null; then
    echo "Error: nasm not found. Install it with: apt install nasm"
    exit 1
fi

if ! command -v grub-mkrescue &> /dev/null; then
    echo "Error: grub-mkrescue not found. Install grub-common and xorriso."
    exit 1
fi

###############################################################################
# 2. Check for GCC (C program compilation)
###############################################################################
CC="gcc"
if ! command -v "$CC" &> /dev/null; then
    echo "Warning: gcc not found. C programs will not be compiled."
    CC=""
fi

###############################################################################
# 3. Compile programs (C and Rust) to static ELF executables
#
# Ядро грузит программы как статические ELF64 (ET_EXEC) с диска: сегменты
# PT_LOAD маппятся по p_vaddr, вход — e_entry. База линковки 0x400000
# (начало user-области). Символы срезаются (-s): содержимое /bin хранится
# в слепке VFS на диске (лимит 128 КБ).
###############################################################################
PROGRAM_LIST="target/programs.list"
: > "$PROGRAM_LIST"

if [ -n "$CC" ]; then
    echo ""
    echo "Compiling C programs..."

    LDSCRIPT="/tmp/vitamin_ldscript_$$"
    cat > "$LDSCRIPT" << 'ENDLD'
    PHDRS
    {
        all PT_LOAD FLAGS(7);
    }
    SECTIONS
    {
        . = 0x400000;
        .text : { *(.text._start) *(.text) *(.rodata) *(.data) } :all
        .bss : { *(.bss) *(COMMON) } :all
        /DISCARD/ : { *(.eh_frame) *(.comment) *(.note.*) }
    }
ENDLD
fi

for prog_dir in "$PROGRAMS_DIR"/*/; do
    prog_name=$(basename "$prog_dir")
    c_src="${prog_dir}main.c"
    rs_src="${prog_dir}main.rs"
    elf_file="${prog_dir}${prog_name}.elf"

    if [ ! -f "$c_src" ] && [ ! -f "$rs_src" ]; then
        echo ""
        echo "  Skipping $prog_name (no main.c / main.rs)"
        continue
    fi

    if [ -f "$c_src" ]; then
        echo ""
        echo "  Compiling (C): $c_src"
        obj_file="${prog_dir}${prog_name}.o"

        $CC -m64 -nostdlib -ffreestanding -fno-stack-protector -fno-pic -mno-red-zone \
            -c "$c_src" -o "$obj_file" 2>&1

        ld -nostdlib -s -T "$LDSCRIPT" -e _start \
            -o "$elf_file" "$obj_file" 2>&1

        rm -f "$obj_file"
    else
        echo ""
        echo "  Compiling (Rust): $rs_src"
        # static + --no-pie: на выходе ET_EXEC (загрузчик ядра не понимает
        # ET_DYN/релокации). -s срезает символы.
        RUSTFLAGS="-C relocation-model=static -C link-arg=--no-pie -C link-arg=-s -C link-arg=-T$(pwd)/${prog_dir}linker.ld" \
            cargo build -Z build-std=core,compiler_builtins \
            --target x86_64-unknown-none \
            --manifest-path "${prog_dir}Cargo.toml" \
            --release 2>&1

        built="${prog_dir}target/x86_64-unknown-none/release/${prog_name}"
        cp "$built" "$elf_file"
    fi

    size=$(stat -c%s "$elf_file")
    echo "    -> ${elf_file} (${size} bytes)"

    echo -e "${prog_name}\t$(pwd)/${elf_file}" >> "$PROGRAM_LIST"

    # Устаревшие flat-артефакты больше не нужны.
    rm -f "${prog_dir}${prog_name}.bin"
done

if [ -n "$CC" ]; then
    rm -f "$LDSCRIPT"
fi

###############################################################################
# 5. Build kernel (Rust + NASM)
###############################################################################
echo ""
echo "Running: cargo build"
cargo build

echo ""
echo "Kernel ELF: $KERNEL_ELF"

###############################################################################
# 6. Build GRUB ISO
###############################################################################
echo ""
echo "Building GRUB ISO..."

rm -rf "$ISO_DIR"
mkdir -p "$ISO_DIR/boot/grub"

cp "$KERNEL_ELF" "$ISO_DIR/boot/vitamin_os26.elf"
cp grub.cfg "$ISO_DIR/boot/grub/grub.cfg"

grub-mkrescue -o "$ISO" "$ISO_DIR" 2>&1
rm -rf "$ISO_DIR"

###############################################################################
# 6.5. Disk image: VITAFS, пересобирается на каждой сборке (пересев /bin).
# Пользовательские данные на os.img при пересборке теряются - это dev-flow.
###############################################################################
echo ""
echo "Creating disk image: $IMG (VITAFS 8M, seeded with /bin programs)..."
python3 tools/mkfs.py "$IMG" "$PROGRAM_LIST"

echo ""
echo "================================="
echo "Build SUCCESS!"
echo "ISO: $ISO"
echo "Disk: $IMG"
echo "Run: qemu-system-x86_64 -cdrom $ISO -drive file=$IMG,format=raw,if=ide"
echo "================================="
