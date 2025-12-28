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
            VariableAttributes,
            VariableVendor,
        },
        system::{
            with_stdin,
            with_stdout,
        },
    },
};

#[derive(Copy, Clone, Debug)]
enum ChatParticipant {
    Local,
    Remote,
}

/// Prints a raw message using the UEFI simple text output protocol.
///
/// The message will be terminated with a newline, if it doesn't include a
/// newline already.
fn println_raw(msg: &str) -> fmt::Result {
    with_stdout(|stdout| {
        // TODO this is wrong
        if msg.ends_with(['\r', '\n']) {
            stdout.write_str(msg)
        } else if msg.ends_with('\r') || msg.ends_with('\n') {
            stdout.write_str(&msg[0..msg.len() - 1])?;

            stdout.write_str("\r\n")
        } else {
            stdout.write_str(msg)?;

            stdout.write_str("\r\n")
        }
    })
}

/// Prints a chat message using the UEFI simple text output protocol.
fn println_chat_msg(participant: ChatParticipant, msg: &str) {
    let participant = format!("{:?}", participant).to_uppercase();
    let msg = format!("[{participant:>6}]: {msg}");
    println_raw(&msg).unwrap();
}

/// Module for interaction with the UEFI console, which will act as
/// [`ChatParticipant::Local`].
///
/// The console is what is handled by UEFI stdout and stdin service, which is
/// backed by the simple text input/output protocols.
mod console {
    use {
        alloc::string::String,
        anyhow::anyhow,
        log::{
            debug,
            warn,
        },
        uefi::{
            ResultExt,
            Status,
            boot,
            proto::console::{
                serial::Serial,
                text::Key,
            },
            system::with_stdin,
        },
    };

    /// Tries to read a message from the console, if there is any input.
    pub fn try_read() -> anyhow::Result<Option<String>> {
        let event =
            with_stdin(|input| input.wait_for_key_event()).ok_or(anyhow!("missing event"))?;
        if !boot::check_event(event)? {
            return Ok(None);
        }

        let mut msg = String::new();

        // Read keystrokes until all are consumed and collect them all in `msg`.
        loop {
            let res = with_stdin(|input| input.read_key())?;
            let key = match res {
                Some(key) => key,
                None => {
                    // EFI_NOT_READY: no more data available
                    break;
                }
            };

            match key {
                Key::Printable(c) => msg.push(char::from(c)),
                Key::Special(c) => {
                    log::warn!("Ignoring special key: {c:?}");
                }
            }
        }

        Ok(Some(msg))
    }

    /// Blocking call that reads a message from the consol that returns after
    /// the first newline (pressed enter key) was found.
    ///
    /// The returned string does contain the newline.
    pub fn read_line() -> anyhow::Result<String> {
        let mut line = String::new();

        let event =
            with_stdin(|input| input.wait_for_key_event()).ok_or(anyhow!("missing event"))?;
        let wait_events = &mut [event];
        loop {
            // Wait for next keystroke.
            boot::wait_for_event(wait_events)?;
            let key = with_stdin(|input| input.read_key())?
                .ok_or(anyhow!("missing input when an event was signaled"))?;
            match key {
                Key::Printable(c) => {
                    let c = char::from(c);
                    line.push(c);
                    // enter
                    if char::from(c) == '\r' {
                        // properly terminate the newline
                        line.push('\n');
                        break;
                    }
                }
                Key::Special(c) => {
                    warn!("received special key code that will be ignored: {c:?}");
                }
            }
        }

        Ok(line)
    }
}

/// Module for interaction with the serial device, which will act as
/// [`ChatParticipant::Remote`].
mod serial {
    use {
        crate::chat::ChatParticipant,
        alloc::string::String,
        anyhow::Context,
        core::fmt::Write,
        log::{
            debug,
            info,
        },
        uefi::{
            Handle,
            Status,
            boot::{
                self,
                OpenProtocolAttributes,
                OpenProtocolParams,
            },
            proto::{
                console::serial::Serial,
                device_path::{
                    DevicePath,
                    text::{
                        AllowShortcuts,
                        DisplayOnly,
                    },
                },
            },
        },
    };
    use crate::chat::console;

    /// Prompts the user with the available handles and a request to select one of
    /// those.
    ///
    /// Returns the selected handle.
    pub fn select_handle(handles: &[Handle]) -> anyhow::Result<(usize /* index */, Handle)> {
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
            // Automatically select the first handle.
            Ok((0, handles[0]))
        } else {
            info!("Please select a serial handle (0..{}):", handles.len() - 1);

            let selection = console::read_line()?;
            // remove newline
            let selection = selection.trim();
            let selection =
                usize::from_str_radix(selection, 10).context("parsing selection as number")?;

            Ok((selection, handles[selection]))
        }
    }

    /// Sets up the input handling and the serial device.
    ///
    /// - set timeout to a minimal value to enable non-blocking reads
    /// - disconnect any users, such as UEFIs console service (via the simple
    ///   text input protocol)
    pub fn setup(serial_handle: Handle, serial: &mut Serial) -> anyhow::Result<()> {
        info!("Setting up serial device:");

        // Disconnect UEFIs console service (simple text input protocol) from the
        // serial device. Otherwise, we can't have distinct reads from the serial
        // device and the simple text input protocol (which still supports USB
        // keyboard strokes).
        boot::disconnect_controller(serial_handle, None /* disconnect all drivers */, None)?;

        // Prepare serial mode.
        {
            let mode = {
                let mut mode = *serial.io_mode();
                // At least in OVMF, setting this to 0, will cause an override
                // with the default. Therefore, we put a minimum value here for
                // low latency.
                mode.timeout_us = 1 /* us*/;
                mode
            };
            serial.set_attributes(&mode)?;
            info!("  io_mode: {:#?}", serial.io_mode());
        }

        Ok(())
    }

    /// Writes a message to the serial device.
    pub fn write(serial: &mut Serial, msg: &str) -> anyhow::Result<()> {
        serial.write_str(msg).map_err(|e| e.into())
    }

    /// Tries to read a message from the serial device, if it received any from
    /// the remote so far.
    pub fn try_read(serial: &mut Serial) -> anyhow::Result<Option<String>> {
        let read = serial.read_to_end()?;
        if read.is_empty() {
            Ok(None)
        } else {
            let msg = String::from_utf8(read).context("reading text from serial")?;
            Ok(Some(msg))
        }
    }
}

/// Starts a chat with the serial device.
///
/// This machine is `[LOCAL]` and the data received via serial is `[REMOTE]`.
pub fn start_chat(handles: &[Handle]) -> anyhow::Result<()> {
    if handles.is_empty() {
        return Err(anyhow!("No Serial handle available!"));
    }

    let (serial_handle_i, serial_handle) = serial::select_handle(handles)?;
    info!("Chosen handle {serial_handle_i}");

    let mut serial_proto = unsafe {
        boot::open_protocol::<Serial>(
            OpenProtocolParams {
                handle: serial_handle,
                agent: boot::image_handle(),
                controller: None,
            },
            OpenProtocolAttributes::GetProtocol,
        )?
    };

    serial::setup(serial_proto.open_params().handle, &mut serial_proto)?;
    info!("Successfully set up input and serial device!");
    info!("  LOCAL : USB keyboard input");
    info!("  Remote: Serial input");

    /*println_raw("Entering chat!")?;
    serial_write(&mut serial_proto, "Entering chat!")?;
    println_raw("To exit, one side must send \"EXIT\"")?;
    serial_write(&mut serial_proto, "To exit, one side must send \"EXIT\"")?;*/

    // We read from the remote (the serial device) and the user all the time.

    let mut current_local_line = String::new();
    let mut current_remote_line = String::new();

    loop {
        let maybe_msg = serial::try_read(&mut serial_proto)?;
        if let Some(msg) = maybe_msg {
            with_stdout(|output| output.write_str(&msg))?;
            current_remote_line.push_str(&msg);
        }
        let maybe_msg = console::try_read()?;
        if let Some(msg) = maybe_msg {
            with_stdout(|output| output.write_str(&msg))?;
            current_local_line.push_str(&msg);
        }

        // Check for EXIT message
        if [&current_local_line, &current_remote_line].iter().any(|msg| msg.contains("EXIT")) {
            break;
        }



        //stall(Duration::from_millis(200));
        //println_chat_msg(ChatParticipant::Local, &current_local_line);
        //println_chat_msg(ChatParticipant::Remote, &current_remote_line);
    }

    Ok(())
}
