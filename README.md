# uefi-serial-chat

Code here is a bit… adventurous. Definitely more “YOLO” than my usual style.
Please bear with me! 😉

## TL;DR

UEFI chat application written in Rust where the machine booting the software is
the host (`LOCAL`) and a remote connected via a serial interface (UART 16550) is
the chat partner (`REMOTE`).

This was a fun journey with the main motivation of utilizing the COM1 pin header
on my computers mainboard for something fun.


## Background, Insights, Challenges

The idea in my head was to create an interactive chat application with two chat
partners: `LOCAL` and `REMOTE`. Both chat partners see all previously written
messages in correct order and what the currently typed without submitting
- nothing to crazy, just a basic chat.



The `LOCAL` machine is hosting the chat, which is written as EFI application in
Rust. On the `LOCAL` side, I'm using the UEFI console, which is usable via the
`SIMPLE_TEXT_INPUT_PROTOCOL` and `SIMPLE_TEXT_OUTPUT_PROTOCOL` to get input from
the USB keyboard as well as writing text to the screen. For the `REMOTE` side,
I wanted to use the `EFI_SERIAL_IO_PROTOCOL`. I also wanted to use  everything without interrupts but just using polling.

While working on this project, my three biggest surprises where:

- the serial device was feeding the UEFI console .... which of course
  - I had to use `DisconnectController`


## Building & Running

### Prerequisites

#### Nix Shell

- `$ nix develop .`


#### Non Nix

- OVMF in `/usr/share/ovmf/OVMF.fd` or provide the `OVMF` env var
- rustup

- qemu-system-x86_64

### Running Unit Tests

- `$ cargo test`

### Running in a VM

`$ cargo run --release --target x86_64-unknown-uefi`


### Running on Real Hardware

```bash
cargo build --release --target x86_64-unknown-uefi
USB_STICK="/run/media/$USER/<name-of-stick>"
mkdir -p "$USB_STICK/EFI/BOOT"
cp target/x86_64-unknown-uefi/release/uefi-serial-chat.efi "$USB_STICK/EFI/BOOT/BOOTX64.EFI"
sync
```

and then unplug the device, boot from the stick, and let the fun begin!
