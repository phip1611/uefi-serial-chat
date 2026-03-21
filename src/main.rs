#![no_std]
#![cfg_attr(not(test), no_main)]

mod chat;
mod serial_impl;

extern crate alloc;

#[cfg(test)]
extern crate std;

use uart_16550::{Config, Uart16550Tty};
use {
    crate::chat::start_chat,
    alloc::{
        vec,
        vec::Vec,
    },
    anyhow::Context,
    core::{
        fmt::Write,
        hint,
        panic::PanicInfo,
        time::Duration,
    },
    log::{
        error,
        info,
    },
    uefi::{
        Handle,
        ResultExt,
        Status,
        boot::{
            self,
            OpenProtocolAttributes,
            OpenProtocolParams,
        },
        helpers,
        proto::console::serial::Serial,
        runtime::{
            self,
            ResetType,
        },
        system::{
            self,
            with_stdout,
        },
    },
};

#[cfg_attr(not(test), panic_handler)]
fn handle_panic(info: &PanicInfo) -> ! {
    error!("PANIC: {}", info);
    loop {
        hint::spin_loop()
    }
}

fn find_serial_handles() -> anyhow::Result<Vec<Handle>> {
    let mut handles = {
        let result = boot::find_handles::<Serial>();
        let status = result.status();
        match (result, status) {
            (Ok(handles), _) => handles,
            (Err(_), Status::NOT_FOUND) => vec![],
            (err @ Err(_), _) => return err.context("finding serial device handles"),
        }
    };

    // Retain only those who do allow to open the protocol on. UEFI is sometimes
    // weird!
    handles.retain(|handle| unsafe {
        boot::open_protocol::<Serial>(
            OpenProtocolParams {
                handle: *handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )
        .is_ok()
    });

    Ok(handles)
}

fn inner_main() -> anyhow::Result<()> {
    helpers::init()?;
    // We always install our own SERIAL_IO protocol implementation:
    // - some UEFI on real hardware has no SERIAL_IO protocol implementation,
    //   even tho there is a UART 16550 and activated COM port
    // - this helps to develop it easily when running it in a VM
    let _ = serial_impl::install()?;

    let serial_handles = find_serial_handles()?;
    // Disconnect any serial handle from the console device:
    //
    // - UEFI console won't read its input from that device
    // - UEFI console won't write to the screen AND the serial device
    // - We have exclusive device control, which we need to install our own
    //   protocol implementation
    for handle in &serial_handles {
        boot::disconnect_controller(*handle, None, None)?;
    }

    start_chat(&serial_handles)?;

    Ok(())
}

#[cfg_attr(not(test), uefi::entry)]
fn main() -> Status {
    if let Err(e) = inner_main() {
        error!("\n{e}\n{e:#}\n{e:#?}");
    }

    let seconds = 20;
    error!("Reached end of main() function. Shutting down in {seconds}s");
    boot::stall(Duration::from_secs(seconds));
    runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None);
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_hello() {
        std::println!("Hello, world!");
    }
}
