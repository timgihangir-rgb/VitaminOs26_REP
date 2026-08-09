#!/bin/bash
set -e

if ! command -v grub-mkrescue &> /dev/null; then
    echo "Error: grub-mkrescue not found."
    echo "Install: sudo apt install grub-common xorriso"
    exit 1
fi

ISO_DIR="target/os.iso.dir"
ISO="target/os.iso"
KERNEL_ELF="target/x86_64-unknown-none/debug/vitamin_os26"

if [ ! -f "$KERNEL_ELF" ]; then
    echo "Kernel not built. Run ./scripts/build.sh first."
    exit 1
fi

rm -rf "$ISO_DIR"
mkdir -p "$ISO_DIR/boot/grub"

cp "$KERNEL_ELF" "$ISO_DIR/boot/vitamin_os26.elf"
cp grub.cfg "$ISO_DIR/boot/grub/grub.cfg"

echo "Creating ISO: $ISO"
grub-mkrescue -o "$ISO" "$ISO_DIR" 2>&1
rm -rf "$ISO_DIR"
echo "Done: $ISO"
