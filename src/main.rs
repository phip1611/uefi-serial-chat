#![no_std]
#![no_main]

mod chat;
mod serial_impl;

extern crate alloc;

use {
    crate::chat::start_chat,
    alloc::{
        vec,
        vec::Vec,
    },
    anyhow::{
        Context
    },
    core::{
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
        proto::{
            console::serial::Serial
        },
        runtime::{
            self,
            ResetType,
        },
        system,
    },
};

#[panic_handler]
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

    info!("Hello World from uefi-serial-chat");
    info!(
        "UEFI revision={}, vendor={}, version={}",
        system::uefi_revision(),
        system::firmware_vendor(),
        system::firmware_revision()
    );

    serial_impl::install()?;
    let handles = find_serial_handles()?;
    start_chat(&handles)?;

    Ok(())
}

#[uefi::entry]
fn main() -> Status {
    if let Err(e) = inner_main() {
        error!("\n{e:?}");
    }

    let seconds = 20;
    error!("Reached end of main() function. Shutting down in {seconds}s");
    boot::stall(Duration::from_secs(seconds));
    runtime::reset(ResetType::SHUTDOWN, Status::SUCCESS, None);
}
