use {
    alloc::{
        borrow::ToOwned,
        collections::VecDeque,
        fmt::format,
        format,
        string::{
            String,
            ToString,
        },
        vec::{
            self,
            Vec,
        },
    },
    anyhow::anyhow,
    core::{
        fmt::{
            self,
            Write,
        },
        iter,
        mem,
        time::Duration,
    },
    log::{
        error,
        info,
        warn,
    },
    uart_16550::spec::FIFO_SIZE,
    uefi::{
        Handle,
        boot::{
            self,
            OpenProtocolAttributes,
            OpenProtocolParams,
            ScopedProtocol,
            stall,
        },
        proto::console::{
            serial::Serial,
            text::Key,
        },
        system::{
            self,
            with_stdin,
            with_stdout,
        },
    },
    uefi_raw::Status,
};

const DELETE: char = '\x7f';
const BACKSPACE: char = '\x08';
const DELETE_STR: &str = "\x7f";
const BACKSPACE_STR: &str = "\x08";

const FPS_60: Duration = Duration::from_millis(1000 / 60);

mod helpers {
    use super::*;

    #[derive(Copy, Clone, Debug)]
    pub enum ChatParticipant {
        Local,
        Remote,
    }

    /// Prints a chat message using the UEFI simple text output protocol.
    ///
    /// It assumes that each messages is a single line.
    pub fn format_chat_message(participant: ChatParticipant, msg: &str) -> String {
        let participant = format!("{:?}", participant).to_uppercase();
        format!("[{participant:>6}]: {msg}")
    }

    /// Finds the first linebreak and return its position in the string.
    ///
    /// This works for Rust strings as well as the input from the UEFI console or
    /// a serial terminal.
    pub fn find_first_linebreak(string: &str) -> Option<usize> {
        ["\r\n", "\r", "\n"]
            .iter()
            .filter_map(|pattern| string.find(pattern))
            .min()
    }

    /// Limits the backspaces (ASCII BS (`0x09`) and DEL `(0x7f)`) in a string.
    ///
    /// Only retains as many backspaces as there are printable characters.
    /// This helps to prevent removing characters from the screen that weren't part
    /// of the user input.
    pub fn string_limit_backspaces(mut string: String, mut allowed_bs: usize) -> String {
        string.retain(|char| {
            let slice = [BACKSPACE, DELETE];
            if slice.contains(&char) {
                return if allowed_bs > 0 {
                    allowed_bs -= 1;
                    true
                } else {
                    false
                };
            } else {
                return true;
            }
        });
        string
    }

    /// Strips all backspaces from a string and returns the effective string.
    pub fn strip_backspaces(string: String) -> String {
        let bs_sequence = format!("{BACKSPACE} {BACKSPACE}");
        string
            .replace(&bs_sequence, "")
            .replace(BACKSPACE, "")
            .replace(DELETE, "")
    }
}

mod actions {
    use {
        super::*,
        core::str::FromStr,
        uefi::proto::device_path::{
            DevicePath,
            text::{
                AllowShortcuts,
                DisplayOnly,
            },
        },
    };

    /// Prompts the user for input.
    ///
    /// This method blocks until we received an answer.
    ///
    /// The returned string contains a whole line without the terminating
    /// newline character. Further, all backspace/delete characters will be
    /// stripped.
    pub fn prompt_user(backend: &mut impl ChatBackend, prompt: &str) -> anyhow::Result<String> {
        // Amount of visible chars. This helps to keep track how many DEL/BS
        // we can propagate to the backend to prevent deleting the prompt or
        // anything other than the user input.
        let mut visible_chars_n = 0;
        backend.write_str(prompt)?;
        // We continue until we've read a whole line.
        let pos = loop {
            // To owned: necessary to prevent borrowing issues.
            let mut latest_input = backend.poll()?.to_owned();
            // Remove superfluous backspaces, otherwise we will remove our
            // prompt from the screen.
            {
                visible_chars_n += latest_input.chars().filter(|c| !c.is_control()).count();
                latest_input = helpers::string_limit_backspaces(latest_input, visible_chars_n);
                let backspace_n = latest_input
                    .chars()
                    .filter(|c| [BACKSPACE, DELETE].contains(c))
                    .count();
                // Now substract how many chars we just erased from the screen.
                visible_chars_n = visible_chars_n.saturating_sub(backspace_n);
            }
            latest_input = backend.normalize_backspaces(latest_input);
            // Actually write the input back to the user. Our backspace handling
            // ensures that deleted characters are also erased from the screen.
            backend.write_str(&latest_input)?;

            // Did the user input a whole line?
            let Some(pos) = helpers::find_first_linebreak(backend.read_buffer_mut()) else {
                boot::stall(FPS_60);
                continue;
            };
            break pos;
        };

        // Print a newline after the user input
        backend.write_char('\n')?;

        // Remove anything after the newline.
        backend.read_buffer_mut().truncate(pos);
        let input = mem::replace(backend.read_buffer_mut(), String::new());
        // Remove any intermediate backspaces and only keep the effective
        // string.
        let input = helpers::strip_backspaces(input);

        Ok(input)
    }

    /// Prompts the user for input, wrapping [`prompt_user`].
    ///
    /// This method blocks until we received an answer.
    ///
    /// The returned string contains a whole line without the terminating
    /// newline character. Further, all backspace/delete characters will be
    /// stripped.
    fn _prompt_user_for_value<T>(
        backend: &mut impl ChatBackend,
        prompt: &str,
        parse_fn: impl Fn(&str) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        loop {
            let input = prompt_user(backend, prompt)?;
            match parse_fn(&input) {
                Ok(value) => return Ok(value),
                Err(_) => {
                    continue;
                }
            }
        }
    }

    /// Prompts the user until its input can be converted to the desired value.
    pub fn prompt_user_for_value<T: FromStr>(
        backend: &mut impl ChatBackend,
        prompt: &str,
    ) -> anyhow::Result<T>
    where
        <T as FromStr>::Err: core::error::Error + Send + Sync + 'static,
    {
        let t = _prompt_user_for_value(backend, prompt, |input| {
            T::from_str(input).map_err(|e| anyhow::Error::new(e))
        })?;
        Ok(t)
    }

    /// Broadcasts a message to all backends.
    pub fn broadcast(msg: &str, backends: &mut [&mut dyn ChatBackend]) -> anyhow::Result<()> {
        for backend in backends {
            backend.write_str(msg)?;
        }
        Ok(())
    }

    /// Prompts the user to select a handle.
    pub fn select_serial_handle(
        backend: &mut impl ChatBackend,
        handles: &[Handle],
    ) -> anyhow::Result<Handle> {
        backend.write_str("Available handles supporting the SERIAL_IO protocol:\n")?;

        if handles.is_empty() {
            anyhow::bail!("no handles available!")
        }

        for (i, handle) in handles.iter().enumerate() {
            let dvp = {
                let proto = unsafe {
                    boot::open_protocol::<DevicePath>(
                        OpenProtocolParams {
                            handle: *handle,
                            agent: boot::image_handle(),
                            controller: None,
                        },
                        OpenProtocolAttributes::GetProtocol,
                    )?
                };
                proto.to_string(DisplayOnly(true), AllowShortcuts(true))?
            };
            let msg = format!("[{i}]: {dvp}\n");
            backend.write_str(&msg)?;
        }

        let index =
            prompt_user_for_value::<usize>(backend, "Please select a handle (0, 1, 2, ...): ")?;

        Ok(handles[index].clone())
    }
}

/// Backend for a chat partner.
///
/// All backends are expected to support UTF-8 and take of the translation of
/// certain control characters and newlines between the output device and
/// normal Rust strings. All implementations of [`fmt::Write`] must take care of
/// proper newline handling on the remote.
trait ChatBackend: fmt::Write {
    /// Polls the underlying backend for newly available data and updates the
    /// internal queue of unprocessed raw input.
    ///
    /// Assumes that all incoming data is valid UTF-8.
    ///
    /// Returns a UTF-8–encoded `str` containing the newly polled data. If a
    /// UTF-8 code point is received only partially, it is buffered internally
    /// and emitted on a subsequent invocation once the complete sequence has
    /// been received.
    fn poll(&mut self) -> anyhow::Result<&'_ str>;

    /// Clears all internal buffers.
    fn clear_buffer(&mut self);

    /// Clears the screen.
    fn clear_screen(&mut self) -> anyhow::Result<()>;

    /// Returns a mutable reference underlying buffer of [`Self::poll`].
    fn read_buffer_mut(&mut self) -> &mut String;

    /// Normalizes backspaces for the given backend.
    ///
    /// This may replace BS with DEL or vice versa, or may replace a BS sequence
    /// with a `BS<space>BS` sequence.
    fn normalize_backspaces(&self, string: String) -> String {
        string
    }
}

/// Backend for the UEFI console which operates on the EFI_SIMPLE_TEXT_INPUT and
/// EFI_SIMPLE_TEXT_OUTPUT protocols.
struct ConsoleBackend {
    // UTF-8 input but ASCII control characters, such as delete and backspace.
    read_buffer: String,
}

impl ConsoleBackend {
    fn new() -> anyhow::Result<Self> {
        let this = Self {
            read_buffer: String::new(),
        };
        Ok(this)
    }
}

impl Write for ConsoleBackend {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let s = s.replace("\n", "\r\n");

        with_stdout(|stdout| write!(stdout, "{s}"))?;
        Ok(())
    }
}

impl ChatBackend for ConsoleBackend {
    fn poll(&mut self) -> anyhow::Result<&'_ str> {
        let event = with_stdin(|input| input.wait_for_key_event())?;
        if !boot::check_event(&event)? {
            return Ok("");
        }

        let old_len = self.read_buffer.len();
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
                Key::Printable(c) => self.read_buffer.push(char::from(c)),
                Key::Special(c) => {
                    warn!("Ignoring special key: {c:?}");
                }
            }
        }

        Ok(&self.read_buffer[old_len..])
    }

    fn clear_buffer(&mut self) {
        self.read_buffer.clear();
    }

    fn clear_screen(&mut self) -> anyhow::Result<()> {
        with_stdout(|stdout| stdout.clear()).map_err(|e| e.into())
    }

    fn read_buffer_mut(&mut self) -> &mut String {
        &mut self.read_buffer
    }
}

/// Backend for a serial device which is accessed via a SERIAL_IO_PROTOCOL
/// handle.
///
/// The remote is expected to be a VT100-like terminal.
struct SerialBackend {
    handle: Handle,
    protocol: ScopedProtocol<Serial>,
    // UTF-8 input but ASCII control characters, such as delete and backspace.
    read_buffer: String,
    // Raw byte input buffer. Data transitions from here to the read buffer for
    // every valid UTF-8 symbol.
    read_buffer_raw: Vec<u8>,
}

impl SerialBackend {
    fn new(handle: Handle) -> anyhow::Result<Self> {
        let protocol = unsafe {
            boot::open_protocol(
                OpenProtocolParams {
                    handle,
                    agent: boot::image_handle(),
                    controller: None,
                },
                OpenProtocolAttributes::GetProtocol,
            )
        }?;
        let this = Self {
            handle,
            protocol,
            read_buffer: String::new(),
            read_buffer_raw: Vec::with_capacity(4),
        };
        Ok(this)
    }

    /// Removes all complete UTF-8 symbols from the vector and pushes them to
    /// the string.
    fn drain_complete_utf8(buf: &mut Vec<u8>, out: &mut String) {
        // Fast path: everything is valid UTF-8
        if let Ok(s) = str::from_utf8(buf) {
            out.push_str(s);
            buf.clear();
            return;
        }

        // Otherwise, find the longest valid UTF-8 prefix
        let mut valid_up_to = 0;

        for i in 1..=buf.len() {
            if str::from_utf8(&buf[..i]).is_ok() {
                valid_up_to = i;
            }
        }

        if valid_up_to > 0 {
            // SAFETY: we just validated this prefix as UTF-8
            let s = unsafe { str::from_utf8_unchecked(&buf[..valid_up_to]) };
            out.push_str(s);
            buf.drain(..valid_up_to);
        }
    }
}

impl Write for SerialBackend {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let s = s.replace("\n", "\r\n");

        let mut remaining_bytes = s.as_bytes();
        // We loop until all byte were written.
        loop {
            match self.protocol.write(s.as_bytes()) {
                Ok(_) => break,
                Err(err) if err.status() == Status::TIMEOUT => {
                    let n = *err.data();
                    remaining_bytes = &remaining_bytes[n..];
                    continue;
                }
                Err(err) => {
                    error!("failed to write to serial: {err:#?}");
                    return Err(fmt::Error);
                }
            }
        }
        Ok(())
    }
}

impl ChatBackend for SerialBackend {
    fn poll(&mut self) -> anyhow::Result<&'_ str> {
        // first: read raw bytes
        {
            let mut buf = [0; FIFO_SIZE];

            let n = match self.protocol.read(&mut buf) {
                Ok(_) => buf.len(),
                Err(err) if err.status() == Status::TIMEOUT => *err.data(),
                Err(err) => return Err(err.into()),
            };
            let buf = &buf[..n];

            // At this point, we might have broken a UTF-8 symbol in between.
            // We therefore use the intermediate buffer.
            for &byte in buf.into_iter().take(n) {
                self.read_buffer_raw.push(byte)
            }
        }
        // second: move valid UTF-8 to string
        let old_len = self.read_buffer.len();
        Self::drain_complete_utf8(&mut self.read_buffer_raw, &mut self.read_buffer);

        Ok(&self.read_buffer[old_len..])
    }

    fn clear_buffer(&mut self) {
        self.read_buffer_raw.clear();
        self.read_buffer.clear();
    }

    fn clear_screen(&mut self) -> anyhow::Result<()> {
        // VT100 ANSI escape sequence to clear the screen.
        // This is the same as the `clear` command in a terminal.
        self.write_str("\x1b[H\x1b[2J\x1b[3J")?;
        Ok(())
    }

    fn read_buffer_mut(&mut self) -> &mut String {
        &mut self.read_buffer
    }

    fn normalize_backspaces(&self, string: String) -> String {
        let bs_sequence = format!("{BACKSPACE} {BACKSPACE}");
        string
            .replace(DELETE, BACKSPACE_STR)
            .replace(BACKSPACE_STR, &bs_sequence)
    }
}

/// Starts a chat with the serial device.
///
/// This expects that the UEFI console is already disconnected from any serial
/// handle.
///
/// This machine is `[LOCAL]` and the data received via serial is `[REMOTE]`.
pub fn start_chat(handles: &[Handle]) -> anyhow::Result<()> {
    if handles.is_empty() {
        return Err(anyhow!("No Serial handle available!"));
    }

    let mut console_backend = ConsoleBackend::new()?;
    let handle = actions::select_serial_handle(&mut console_backend, handles)?;

    let mut serial_backend = SerialBackend::new(handle)?;

    // Clear any remaining data.
    {
        console_backend.clear_screen()?;
        let _ = console_backend.poll()?;
        console_backend.clear_buffer();

        serial_backend.clear_screen()?;
        let _ = serial_backend.poll()?;
        serial_backend.clear_buffer();
    }

    // Welcome message
    {
        actions::broadcast(
            "Welcome to UEFI Serial Chat\n",
            &mut [&mut console_backend, &mut serial_backend],
        )?;
        actions::broadcast(
            &format!(
                "UEFI revision={}, vendor={}, version={}\n",
                system::uefi_revision(),
                system::firmware_vendor(),
                system::firmware_revision()
            ),
            &mut [&mut console_backend, &mut serial_backend],
        )?;
        actions::broadcast(
            "Chat will begin shortly ...\n",
            &mut [&mut console_backend, &mut serial_backend],
        )?;
    }

    // User name prompts
    {
        serial_backend.write_str("Other user is selecting their name, please wait ...\n")?;
        let local_username = actions::prompt_user(&mut console_backend, "Choose your username: ")?;
        actions::broadcast(
            &format!("Local machine chose username : '{local_username}'\n"),
            &mut [&mut console_backend, &mut serial_backend],
        )?;
        console_backend.write_str("Other user is selecting their name, please wait ...\n")?;
        let remote_username = actions::prompt_user(&mut serial_backend, "Choose your username: ")?;
        actions::broadcast(
            &format!("Remote machine chose username: '{remote_username}'\n"),
            &mut [&mut console_backend, &mut serial_backend],
        )?;
    }

    // Actual chat
    loop {
        let mut needs_refresh = true;
        if needs_refresh {
            console_backend.clear_screen()?;
            serial_backend.clear_screen()?;
        }
    }

    todo!();

    Ok(())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::chat::helpers::{
            string_limit_backspaces,
            strip_backspaces,
        },
    };

    #[test]
    fn test_string_limit_backspaces() {
        assert_eq!(
            string_limit_backspaces(String::from("Hello, world!"), 0),
            "Hello, world!"
        );
        assert_eq!(
            string_limit_backspaces(String::from("Hello, world!"), 5),
            "Hello, world!"
        );

        assert_eq!(
            string_limit_backspaces(String::from("Hello, \x7fw\x7fo\x7fr\x7fl\x7fd\x7f!"), 10),
            "Hello, \x7fw\x7fo\x7fr\x7fl\x7fd\x7f!"
        );
        assert_eq!(
            string_limit_backspaces(String::from("Hello, \x7fw\x7fo\x7fr\x7fl\x7fd\x7f!"), 1),
            "Hello, \x7fworld!"
        );
        assert_eq!(
            string_limit_backspaces(String::from("Hello, \x7fw\x7fo\x7fr\x7fl\x7fd\x7f!"), 0),
            "Hello, world!"
        );
        assert_eq!(
            string_limit_backspaces(String::from("\x7fH\x7f\x7f"), 0),
            "H"
        );
        assert_eq!(
            string_limit_backspaces(String::from("\x7fH\x7f\x7f"), 2),
            "\x7fH\x7f"
        );
        assert_eq!(
            string_limit_backspaces(String::from("\x7fH\x7f\x7f"), 3),
            "\x7fH\x7f\x7f"
        );
    }

    #[test]
    fn test_strip_backspaces() {
        assert_eq!(
            strip_backspaces(String::from("Hello, world!")),
            "Hello, world!"
        );
        assert_eq!(
            strip_backspaces(String::from("Hello, \x08 \x08world!")),
            "Hello, world!"
        );
        assert_eq!(
            strip_backspaces(String::from("Hello, \x08 \x08\x08world!")),
            "Hello, world!"
        );
        assert_eq!(
            strip_backspaces(String::from("Hello, \x08 \x08\x7fworld!")),
            "Hello, world!"
        );
        assert_eq!(
            strip_backspaces(String::from("Hello, \x7f\x08 \x08w\x7forld!")),
            "Hello, world!"
        );
    }
}
