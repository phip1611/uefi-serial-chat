use {
    alloc::{
        collections::VecDeque,
        format,
        string::{
            String,
            ToString,
        },
        vec::Vec,
    },
    anyhow::anyhow,
    core::{
        fmt::Write,
        time::Duration,
    },
    log::info,
    uefi::{
        Handle,
        boot::{
            self,
            OpenProtocolAttributes,
            OpenProtocolParams,
            stall,
        },
        proto::console::serial::Serial,
        system::with_stdout,
    },
};

#[derive(Copy, Clone, Debug)]
enum ChatParticipant {
    Local,
    Remote,
}

/// Prints a chat message using the UEFI simple text output protocol.
///
/// It assumes that each messages is a single line.
fn format_chat_message(participant: ChatParticipant, msg: &str) -> String {
    let participant = format!("{:?}", participant).to_uppercase();
    format!("[{participant:>6}]: {msg}")
}

/// Module for interaction with the UEFI console, which will act as
/// [`ChatParticipant::Local`].
///
/// The console is what is handled by UEFI stdout and stdin service, which is
/// backed by the simple text input/output protocols.
mod console {
    use {
        alloc::{
            string::String,
            vec,
            vec::Vec,
        },
        anyhow::anyhow,
        core::fmt::Write,
        log::{
            info,
            warn,
        },
        uefi::{
            boot,
            proto::console::text::Key,
            system::{
                with_stdin,
                with_stdout,
            },
        },
    };

    /// Tries to read a message from the console, if there is any input.
    ///
    /// The data that we read from the underlying source will be provided as
    /// [`Char16`]. We skip some unused special key codes and only return
    /// printable characters as well as typical ASCII control characters, such
    /// as backspace. Newlines will be represented as `\r\n`.
    ///
    /// [`Char16`]: uefi::Char16
    pub fn try_read() -> anyhow::Result<Vec<char>> {
        let event =
            with_stdin(|input| input.wait_for_key_event()).ok_or(anyhow!("missing event"))?;
        if !boot::check_event(event)? {
            return Ok(vec![]);
        }

        let mut data = Vec::new();

        // Read all available keystrokes and collect them in the vec.
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
                Key::Printable(c) => data.push(char::from(c)),
                Key::Special(c) => {
                    warn!("Ignoring special key: {c:?}");
                }
            }
        }

        Ok(data)
    }

    /// Prompts the user for its input.
    ///
    /// As the user types, their typed characters will be shown to the screen.
    pub fn prompt_input(prompt_msg: &str) -> anyhow::Result<String> {
        if !prompt_msg.is_empty() {
            with_stdout(|stdout| stdout.write_str(prompt_msg))?;
        }

        let event =
            with_stdin(|input| input.wait_for_key_event()).ok_or(anyhow!("missing event"))?;
        let wait_events = &mut [event];
        let mut input = String::new();
        loop {
            // Wait for next keystroke.
            boot::wait_for_event(wait_events)?;
            let key = with_stdin(|input| input.read_key())?
                .ok_or(anyhow!("missing input when an event was signaled"))?;

            match key {
                Key::Printable(c) => {
                    let c = char::from(c);
                    match c {
                        '\u{8}' /* backspace */ => {
                            if input.len() > 1 {
                                input.remove(input.len() - 1);
                                // UEFI console handles a backspace properly on
                                // screen already by default.
                                with_stdout(|stdout| stdout.write_char(c))?;
                            }
                        }
                        '\r' /* enter */ => {
                            info!("end of input line: {input}");
                            input.push_str("\r\n");
                            with_stdout(|stdout| stdout.write_str("\r\n"))?;
                            break;
                        }
                        // 0-9, A-Z, a-z
                        c if c.is_ascii_alphanumeric() => {
                            input.push(c);
                            // type what the user just printed
                            with_stdout(|stdout| stdout.write_char(c))?;
                        }
                        c if c.is_ascii_punctuation() => {
                            input.push(c);
                            // type what the user just printed
                            with_stdout(|stdout| stdout.write_char(c))?;
                        }
                        c => {
                            return Err(anyhow!("Unsupported character: {c:?}"));
                        }
                    }
                }
                Key::Special(c) => {
                    warn!("received special key code that will be ignored: {c:?}");
                }
            }
        }
        Ok(input)
    }

    /// Extracts all complete UTF-8–encoded lines from the buffer and returns
    /// them as a single `String`.
    ///
    /// The returned string does **not** include the final newline character.
    /// All bytes corresponding to the returned lines are removed from `data`.
    /// All `\r\n` sequences will be replaced by `\n`.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(String))` if at least one complete line was extracted.
    /// - `Ok(None)` if no complete line is present in the buffer.
    /// - `Err(_)` if UTF-8 validation or conversion fails.
    pub fn remove_lines_to_string(data: &mut Vec<char>) -> anyhow::Result<Option<String>> {
        // We search for the last newline.
        let pos = data.iter().rposition(|char| *char == '\r');
        if pos.is_none() {
            return Ok(None);
        }
        let pos = pos.unwrap();
        let lines = &data[0..pos];
        let lines = lines.iter().collect::<String>();
        let lines = lines.replace("\r\n", "\n");

        // remove everything including the newline
        for _ in 0..=pos {
            data.remove(0);
        }

        Ok(Some(lines))
    }
}

/// Module for interaction with the serial device, which will act as
/// [`ChatParticipant::Remote`].
mod serial {
    use {
        crate::chat::console,
        alloc::{
            format,
            string::String,
            vec::Vec,
        },
        anyhow::Context,
        core::fmt::Write,
        log::info,
        uefi::{
            Handle,
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
            let msg = format!("Please select a serial handle (0..{}): ", handles.len() - 1);
            let selection = console::prompt_input(&msg)?;
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

    /// Tries to read the latest raw data from the serial device, if it received
    /// any from the remote so far.
    ///
    /// The data is raw and unprocessed, but likely to be valid UTF-8 with
    /// some control characters.
    pub fn try_read(serial: &mut Serial) -> anyhow::Result<Vec<u8>> {
        serial.read_to_end().map_err(|e| e.into())
    }

    /// Extracts all complete UTF-8–encoded lines from the buffer and returns them
    /// as a single `String`.
    ///
    /// The returned string does **not** include the final newline character.
    /// All bytes corresponding to the returned lines are removed from `data`.
    ///
    /// # Returns
    ///
    /// - `Ok(Some(String))` if at least one complete line was extracted.
    /// - `Ok(None)` if no complete line is present in the buffer.
    /// - `Err(_)` if UTF-8 validation or conversion fails.
    pub fn remove_lines_to_string(data: &mut Vec<u8>) -> anyhow::Result<Option<String>> {
        // We search for the last newline.
        // In a serial terminal by convention, the terminal will send a `\r` for
        // a newline.
        let pos = data.iter().rposition(|char| *char == b'\r');
        if pos.is_none() {
            return Ok(None);
        }
        let pos = pos.unwrap();
        let lines = &data[0..pos];
        let lines = String::from_utf8(lines.to_vec())?;

        // remove everything including the newline
        for _ in 0..=pos {
            data.remove(0);
        }

        Ok(Some(lines))
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
    info!("  REMOTE: Serial input");

    // TODO enable again console::prompt_input("Ready to enter chat? Press ENTER.")?;

    /*println_raw("Entering chat!")?;
    serial_write(&mut serial_proto, "Entering chat!")?;
    println_raw("To exit, one side must send \"EXIT\"")?;
    serial_write(&mut serial_proto, "To exit, one side must send \"EXIT\"")?;*/

    // We read from the remote (the serial device) and the user all the time.

    // Current raw processed input including control characters.
    let mut current_local_raw_input = Vec::new();
    let mut current_remote_raw_input = Vec::new();

    // all parsed messages from old to new
    let mut messages = VecDeque::<(ChatParticipant, String)>::new();

    // Refetch the latest data and redraw the game board + prompts all the time.
    loop {
        // query latest data from data sources
        current_local_raw_input.extend(console::try_read()?);
        current_remote_raw_input.extend(serial::try_read(&mut serial_proto)?);

        // Process raw input, extract lines, and put that into `messages`
        {
            let lines = console::remove_lines_to_string(&mut current_local_raw_input)?;
            if let Some(lines) = lines {
                for line in lines.lines() {
                    messages.push_back((ChatParticipant::Local, line.to_string()));
                }
            }
            let lines = serial::remove_lines_to_string(&mut current_remote_raw_input)?;
            if let Some(lines) = lines {
                for line in lines.lines() {
                    messages.push_back((ChatParticipant::Remote, line.to_string()));
                }
            }
        }

        // remove messages in case we have too many on the screen
        while messages.len() > 20 {
            messages.pop_front();
        }

        // Clear screens
        {
            // local
            {
                with_stdout(|output| output.clear())?;
            }
            // for remote
            {
                // Assuming the workload uses a VT100-compatible terminal emulator.
                // clear screen, clear line, cursor to pos 1:1
                serial::write(&mut serial_proto, &"\x1B[2J\x1B[H")?;
            }
        }

        // Print all messages as single lines.
        for (participant, message) in &messages {
            let message = format_chat_message(*participant, message);
            with_stdout(|stdout| stdout.write_str(&message))?;
            with_stdout(|stdout| stdout.write_str("\r\n"))?;
            serial::write(&mut serial_proto, &message)?;
            serial::write(&mut serial_proto, "\n")?;
        }

        // print each user what they are currently typing
        {
            let input = current_local_raw_input.iter().collect::<String>();
            with_stdout(|stdout| stdout.write_str(&input))?;

            let input = String::from_utf8_lossy(&current_remote_raw_input);
            serial::write(&mut serial_proto, &input)?;
        }

        /*// Check for EXIT message
        if [&current_local_line, &current_remote_line]
            .iter()
            .any(|msg| msg.contains("EXIT"))
        {
            break;
        }*/

        stall(Duration::from_millis(100));
    }

    Ok(())
}
