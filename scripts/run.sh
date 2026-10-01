#!/bin/bash
set -e

echo "================================="
echo "Running VitaminOS26 in QEMU..."
echo "================================="

ISO="target/os.iso"
IMG="target/os.img"

if [ ! -f "$ISO" ]; then
    echo "ISO not found. Building..."
    ./scripts/build.sh
fi

if [ ! -f "$IMG" ]; then
    echo "Creating disk image: $IMG (8M)..."
    truncate -s 8M "$IMG"
fi

if ! command -v qemu-system-x86_64 &> /dev/null; then
    echo "Error: QEMU not found."
    exit 1
fi

echo "Starting QEMU (net: user-mode, rtl8139, гость видит хост как 10.0.2.2)..."
# -netdev user — NAT-стек QEMU: 10.0.2.2 = хост, 10.0.2.15 = гость, DNS 10.0.2.3.
# rtl8139 — единственная карта, которую умеет polled-драйвер ядра (src/net.rs).
qemu-system-x86_64 -cdrom "$ISO" -drive file="$IMG",format=raw,if=ide \
    -netdev user,id=n0 -device rtl8139,netdev=n0 "$@"
