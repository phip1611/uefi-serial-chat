//! Implementation of the UEFI serial protocol for the device at `0x3f8`
//! (com1 port).

#![allow(static_mut_refs)]

use {
    alloc::{
        boxed::Box,
        vec::Vec,
    },
    core::slice,
    uart_16550::{
        BaudRate,
        Config as Uart16550Config,
        Uart16550,
        backend::PioBackend,
        spec::{
            CLK_FREQUENCY_HZ,
            registers::{
                FifoTriggerLevel,
                IER,
                Parity,
                WordLength,
            },
        },
    },
    uefi::{
        Handle,
        Identify,
        boot,
        proto::{
            console::serial::{
                IoMode,
                Serial,
            },
            device_path::{
                DevicePath,
                build::{
                    self,
                    DevicePathBuilder,
                },
                messaging,
            },
        },
    },
    uefi_raw::{
        Status,
        protocol::console::serial::{
            ControlBits,
            Parity as EfiParity,
            SerialIoMode,
            SerialIoProtocol,
            StopBits as EfiStopBits,
        },
    },
};

static mut INTERFACE: spin::Once<CustomSerialIoProtocol> = spin::Once::new();

#[derive(Debug)]
#[repr(C)]
struct CustomSerialIoProtocol {
    inner: SerialIoProtocol,
    /* End ABI-compatible portion: now extra stuff */
    mode: SerialIoMode,
    device_path: Box<DevicePath>,
    device: Uart16550<PioBackend>,
    is_initialized: bool,
}

impl CustomSerialIoProtocol {
    fn init_if_necessary(&mut self) {
        if !self.is_initialized {
            self.is_initialized = true;
            self.init_device(Default::default());
        }
    }

    fn init_device(&mut self, config: Uart16550Config) {
        self.device
            .init(config)
            .expect("should initialize serial port");
        self.device
            .test_loopback()
            .expect("should perform serial loopback test");
        // Fails as DSR is not set when booted on real hardware. I think this
        // used to work?! TODO Investigate
        self.device
            .check_connected()
            .expect("should check remote ready to receive")
    }

    /// Updates the [`IoMode`] in the protocol and also in hardware.
    fn update_io_mode(&mut self, new_io_mode: IoMode) {
        self.mode = new_io_mode;
        let ptr = &raw const self.mode;
        // SAFETY: We take care of that `self.mode` will not be invalidated.
        self.inner.mode = ptr;

        // Update the hardware.
        let config = Uart16550Config {
            interrupts: IER::empty(),
            frequency: CLK_FREQUENCY_HZ,
            prescaler_division_factor: None,
            fifo_trigger_level: Some(FifoTriggerLevel::Fourteen),
            baud_rate: BaudRate::Baud115200,
            data_bits: WordLength::from_integer(u8::try_from(new_io_mode.data_bits).unwrap()),
            extra_stop_bits: match new_io_mode.stop_bits {
                EfiStopBits::DEFAULT => false,
                EfiStopBits::ONE => false,
                EfiStopBits::TWO => true,
                EfiStopBits::ONE_FIVE => true,
                _ => false,
            },
            parity: match new_io_mode.parity {
                EfiParity::DEFAULT => Parity::Disabled,
                EfiParity::NONE => Parity::Disabled,
                EfiParity::ODD => Parity::Odd,
                EfiParity::EVEN => Parity::Even,
                EfiParity::MARK => Parity::Disabled,
                EfiParity::SPACE => Parity::Disabled,
                _ => Parity::Disabled,
            },
        };

        log::debug!("Updating serial config: {config:#?}");

        self.init_device(config);
    }
}

// SAFETY: We only initialize it once in a single-threaded context.
unsafe impl Send for CustomSerialIoProtocol {}
// SAFETY: We only initialize it once in a single-threaded context.
unsafe impl Sync for CustomSerialIoProtocol {}

unsafe extern "efiapi" fn serial_protocol_reset(this: *mut SerialIoProtocol) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let _this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };
    Status::UNSUPPORTED
}

unsafe extern "efiapi" fn serial_protocol_set_attributes(
    this: *mut SerialIoProtocol,
    baud_rate: u64,
    receive_fifo_depth: u32,
    timeout: u32,
    parity: EfiParity,
    data_bits: u8,
    stop_bits: EfiStopBits,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };
    this.init_if_necessary();
    let attributes = IoMode {
        baud_rate,
        parity,
        receive_fifo_depth,
        timeout,
        stop_bits,
        data_bits: data_bits as u32,
        control_mask: this.mode.control_mask,
    };
    // nothing is actually synced to the device at this point
    this.update_io_mode(attributes);
    Status::SUCCESS
}

unsafe extern "efiapi" fn serial_protocol_set_control_bits(
    this: *mut SerialIoProtocol,
    _bits: ControlBits,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };
    this.init_if_necessary();
    Status::UNSUPPORTED
}

unsafe extern "efiapi" fn serial_protocol_get_control_bits(
    this: *const SerialIoProtocol,
    _bits: *mut ControlBits,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let _this = unsafe { this.cast::<CustomSerialIoProtocol>().as_ref().unwrap() };
    Status::UNSUPPORTED
}

unsafe extern "efiapi" fn serial_protocol_write(
    this: *mut SerialIoProtocol,
    len: *mut usize,
    data: *const u8,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };
    this.init_if_necessary();

    let msg = unsafe { slice::from_raw_parts(data, *len) };

    this.device.send_bytes_exact(msg);

    // TODO: We currently totally ignore the timeout semantics of this protocol.
    Status::SUCCESS
}

unsafe extern "efiapi" fn serial_protocol_read(
    this: *mut SerialIoProtocol,
    len: *mut usize,
    data: *mut u8,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };
    this.init_if_necessary();

    // SAFETY: Layout is valid.
    let len = unsafe { len.as_mut().expect("should be not null") };

    // SAFETY: We know the layout and trust the caller.
    let slice = unsafe { slice::from_raw_parts_mut(data, *len) };

    // TODO: We currently ignore proper timeout handling.
    // It should nevertheless be correct to just return early with
    // Status::TIMEOUT, as the caller then has to fetch more frequently.
    let n = this.device.try_receive_bytes(slice);

    if n == *len {
        *len = n;
        Status::SUCCESS
    } else {
        *len = n;
        Status::TIMEOUT
    }
}

/// Allocates a new handle for the port I/O mapped serial port (COM1 port) and
/// installs the Serial IO protocol on it.
pub fn install() -> anyhow::Result<Handle> {
    let device_path = {
        let mut dvp_vec = Vec::new();
        let dvp = DevicePathBuilder::with_vec(&mut dvp_vec)
            .push(&build::messaging::Uart {
                baud_rate: 115200,
                data_bits: 8,
                parity: messaging::Parity::NO,
                stop_bits: messaging::StopBits::ONE,
            })?
            .finalize()?;
        dvp.to_boxed()
    };

    // SAFETY: This is okay as free the memory in a matching uninstall call.
    // Further, it is okay if this lives for the whole time of the application.
    //let device_path = Box::leak(device_path);

    let device = unsafe { Uart16550::new_port(0x3f8) }?;

    let protocol_interface = CustomSerialIoProtocol {
        inner: SerialIoProtocol {
            revision: SerialIoProtocol::REVISION,
            reset: serial_protocol_reset,
            set_attributes: serial_protocol_set_attributes,
            set_control_bits: serial_protocol_set_control_bits,
            get_control_bits: serial_protocol_get_control_bits,
            write: serial_protocol_write,
            read: serial_protocol_read,
            mode: core::ptr::null_mut(),
        },
        mode: SerialIoMode {
            control_mask: Default::default(),
            timeout: 1000, /* µs */
            baud_rate: 115200,
            receive_fifo_depth: 16,
            data_bits: 8,
            parity: EfiParity::NONE,
            stop_bits: EfiStopBits::ONE,
        },
        device_path,
        // SAFETY: We know the device is there.
        device,
        is_initialized: false,
    };

    let _ = unsafe { INTERFACE.call_once(|| protocol_interface) };
    // Ensure the pointer of the inner protocol is updated to the initial value.
    // Also synchronize everything to the hardware port.
    unsafe {
        let prot = INTERFACE.get_mut().unwrap();
        prot.update_io_mode(prot.mode);
        let dump = prot.device.config_register_dump();
        log::info!("{dump:#?}");
    }
    let prot = unsafe { INTERFACE.get().unwrap() };
    let serial_interface_ptr = &raw const *prot;
    let dvp_interface_ptr = prot.device_path.as_ffi_ptr();

    let handle = unsafe {
        boot::install_protocol_interface(None, &Serial::GUID, serial_interface_ptr.cast())
    }?;
    let _ = unsafe {
        boot::install_protocol_interface(Some(handle), &DevicePath::GUID, dvp_interface_ptr.cast())
    }?;

    Ok(handle)
}
