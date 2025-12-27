use uefi::runtime::VariableAttributes;
use {
    alloc::{
        format,
        string::{
            String,
            ToString,
        },
    },
    anyhow::{
        Context,
        anyhow,
    },
    core::{
        fmt::{
            self,
            Write,
        },
        time::Duration,
    },
    log::{
        debug,
        info,
        warn,
    },
    uefi::{
        CStr16,
        CString16,
        Handle,
        ResultExt,
        Status,
        boot::{
            self,
            OpenProtocolAttributes,
            OpenProtocolParams,
            stall,
        },
        cstr16,
        proto::{
            console::{
                serial::{
                    ControlBits,
                    Serial,
                },
                text::{
                    Input,
                    Key,
                    OutputMode,
                },
            },
            device_path::{
                DevicePath,
                text::{
                    AllowShortcuts,
                    DisplayOnly,
                },
            },
        },
        runtime::{
            self,
            VariableVendor,
        },
        system::{
            with_stdin,
            with_stdout,
        },
    },
};

/// Prints a raw message using the UEFI simple text output protocol.
fn println_raw(msg: &str) -> fmt::Result {
    with_stdout(|stdout| {
        if msg.ends_with(['\r', '\n']) {
            stdout.write_str(msg)
        } else if msg.ends_with('\r') || msg.ends_with('\n') {
            stdout.write_str(&msg[0..msg.len() - 2])?;

            stdout.write_str("\r\n")
        } else {
            stdout.write_str(msg)?;

            stdout.write_str("\r\n")
        }
    })
}

#[derive(Copy, Clone, Debug)]
enum Participant {
    Local,
    Remote,
}

/// Prints a chat message using the UEFI simple text output protocol.
fn println_chat_msg(participant: Participant, msg: &str) {
    let participant = format!("{:?}", participant).to_uppercase();
    let msg = format!("[{participant:>6}]: {msg}");
    println_raw(&msg).unwrap();
}

/// Reads the next line via the UEFI simple text input protocol and return
/// as soon as a whole line was entered.
fn read_line() -> anyhow::Result<String> {
    let mut line = String::new();

    let event = with_stdin(|input| input.wait_for_key_event()).ok_or(anyhow!("missing event"))?;
    let wait_events = &mut [event];
    loop {
        // Wait for next keystroke.
        //debug!("Waiting for event..");
        boot::wait_for_event(wait_events)?;
        //debug!("Got event!");
        let key = with_stdin(|input| input.read_key())?
            .ok_or(anyhow!("missing input when an event was signaled"))?;
        //debug!("Got key: {:?}", key);
        match key {
            Key::Printable(c) => {
                let c = char::from(c);
                if char::from(c) == '\r'
                /* enter */
                {
                    break;
                } else {
                    line.push(c);
                }
            }
            Key::Special(c) => {
                warn!("received special key code that will be ignored: {c:?}");
            }
        }
    }

    Ok(line)
}

/// Prompts the user with the available handles and a request to select one of
/// those.
///
/// Returns the selected handle.
fn select_serial_handle(handles: &[Handle]) -> anyhow::Result<(usize /* index */, Handle)> {
    assert!(!handles.is_empty());

    info!("Found the following handles supporting the Serial protocol:");
    for (i, handle) in handles.iter().enumerate() {
        let dvp = unsafe {
            boot::open_protocol::<DevicePath>(
                OpenProtocolParams {
                    handle: *handle,
                    agent: boot::image_handle(),
                    controller: None,
                },
                OpenProtocolAttributes::GetProtocol,
            )
        }?;
        let dvp_string = dvp.to_string(DisplayOnly(true), AllowShortcuts(true))?;
        info!("  {i}:  {dvp_string}");
    }

    if handles.len() == 1 {
        Ok((0, handles[0]))
    } else {
        info!("Please select a serial handle (0..{}):", handles.len());

        let selection = read_line()?;
        let selection =
            usize::from_str_radix(&selection, 10).context("parsing selection as number")?;

        Ok((selection, handles[selection]))
    }
}

// Sets up the simple text input and output protocols.
fn setup_input() -> anyhow::Result<()> {
    with_stdin(|_input| {});
    //with_stdout(|output| output.clear())?;

    Ok(())
}

/// Sets up the serial device.
///
/// - `set timeout to 0ms` to enable unblocking reads
fn serial_setup(serial: &mut Serial) -> anyhow::Result<()> {
    info!("Setting up serial device:");
    info!("  io_mode: {:#?}", serial.io_mode());

    let mode = {
        let mut mode = *serial.io_mode();
        // set non-blocking
        mode.timeout = 5000 /* ms*/;
        mode
    };

    serial.set_attributes(&mode)?;

    Ok(())
}

fn serial_write(serial: &mut Serial, msg: &str) -> anyhow::Result<()> {
    Ok(())
}

/// Tries to read a message from the remote, if any is available.
fn try_serial_read(serial: &mut Serial) -> anyhow::Result<Option<String>> {
    let mut buffer = [0; 8192];

    debug!("wah");
    let res = serial.read(&mut buffer);
    debug!("wah");
    let msg = match res {
        Ok(n) => &buffer[..n],
        Err(e) => {
            let status = e.status();
            let n = *e.data();
            debug!("wah: {status}, n={n}");
            return if status == Status::TIMEOUT {
                Ok(None)
            } else {
                Err(e.into())
            };
        }
    };
    let msg = String::from_utf8(msg.to_vec())?;
    Ok(Some(msg))
}

/// Starts a chat with the serial device.
///
/// This machine is `[LOCAL]` and the data received via serial is `[REMOTE]`.
pub fn start_chat(handles: &[Handle]) -> anyhow::Result<()> {
    if handles.is_empty() {
        return Err(anyhow!("No Serial handle available!"));
    }

    let (serial_handle_i, serial_handle) = select_serial_handle(handles)?;
    info!("Chosen handle {serial_handle_i}");

    unsafe {
        // disconnect the serial handle from the simple text iput protocol
        boot::disconnect_controller(serial_handle, None, None)?
    }

    let mut proto = unsafe {
        boot::open_protocol::<Serial>(
            OpenProtocolParams {
                handle: serial_handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )?
    };

    {
        let (var, vattr) = runtime::get_variable_boxed(cstr16!("ConIn"), &VariableVendor::GLOBAL_VARIABLE)?;
        runtime::set_variable(cstr16!("ConIn"), &VariableVendor::GLOBAL_VARIABLE, vattr, &[])?;
        let dvp = unsafe {
            DevicePath::from_ffi_ptr(var.as_ptr().cast())
        };
        let dvp_string = dvp.to_string(DisplayOnly(true), AllowShortcuts(true))?;
        info!("  {dvp_string}");
    }

    //setup_input()?;
    // todo what is the best value here?
    //serial_setup(&mut proto)?;



    println_raw("Starting chat!")?;
    serial_write(&mut proto, "Starting chat!")?;
    println_raw("To exit, one side must print \"EXIT\"")?;
    serial_write(&mut proto, "To exit, one side must print \"EXIT\"")?;

    loop {
        let remote_msg = try_serial_read(&mut proto);
        stall(Duration::from_millis(2000));
        // TODO ohh now the simple text input protocol also reads from the serial dev :D
        //let local_msg = read_line()?;
        //info!("local_msg: {local_msg}");
    }

    Ok(())
}
