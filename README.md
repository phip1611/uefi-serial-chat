# uefi-serial-chat

Simple example to demonstrate that you can write a simple chat application in
UEFI using the serial device (COM1 port).


# Prerequisites (on non-Nix system)


## Building

- OVMF in `/usr/share/ovmf/OVMF.fd` or provide the `OVMF` env var
- rustup


## Running

- qemu-system-x86_64

# Steps to Run

- Only on NixOS: `$ nix develop .` to open the Nix shell
- `$ cargo run --release`
