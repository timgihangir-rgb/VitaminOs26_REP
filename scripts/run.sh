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

echo "Starting QEMU..."
qemu-system-x86_64 -cdrom "$ISO" -drive file="$IMG",format=raw,if=ide
