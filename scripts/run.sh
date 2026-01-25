#!/usr/bin/env bash

set -euo pipefail

BOOT_VOL=".boot-volume"
EFI_FILE=target/x86_64-unknown-none/uefi-serial-chat.efi
OVMF="${OVMF:-'/usr/share/ovmf/OVMF.fd'}"

rm -rf "$BOOT_VOL"
mkdir -p "$BOOT_VOL/EFI/BOOT"
mkdir -p "$BOOT_VOL/EFI/systemd"

if [ -f "$1" ]; then
    EFI_FILE=$1
fi

cp "$EFI_FILE" "$BOOT_VOL/EFI/BOOT/BOOTX64.EFI"
# We pretend we have systemd-boot here to test the chain loading
cp "$EFI_FILE" "$BOOT_VOL/EFI/systemd/systemd-bootx64.efi"

# Uart16550 connected to pseudo terminal => we can connect `minicom` to the VM
#  -device pci-serial,chardev=charserial0,addr=0x10 \
qemu-system-x86_64 \
    -bios $OVMF \
    -chardev pty,id=charserial0 \
    -cpu qemu64 \
    -debugcon file:debugcon.txt \
    -display gtk \
    -drive "format=raw,file=fat:rw:$BOOT_VOL" \
    -m 512M \
    -machine q35,accel=tcg \
    -monitor vc \
    -no-reboot \
    -nodefaults \
    -serial stdio \
    -smp 1 \
    -vga std
