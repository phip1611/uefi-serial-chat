//! Implementation of the UEFI serial protocol for the device at `0x3f8`
//! (com1 port).

#![allow(static_mut_refs)]

use anyhow::Context;
use uart_16550_port::{BaudRate, DataBits, SerialConfig, StopBits, Uart16550Port};
use {
    alloc::{
        boxed::Box,
        vec::Vec,
    },
    core::slice,
    log::debug,
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
            Parity,
            SerialIoMode,
            SerialIoProtocol,
            StopBits as UefiStopBits,
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
    device: Uart16550Port,
}

impl CustomSerialIoProtocol {
    /// Updates the [`IoMode`] in the protocol and also in hardware.
    fn update_io_mode(&mut self, new_io_mode: IoMode) {
        self.mode = new_io_mode;
        let ptr = &raw const self.mode;
        // SAFETY: We take care of that `self.mode` will not be invalidated.
        self.inner.mode = ptr;

        // Update the hardware.
        let config = SerialConfig {
            baud_rate: BaudRate::try_from_value(u32::try_from(new_io_mode.baud_rate).unwrap()).unwrap(),
            data_bits: DataBits::try_from_value(u32::try_from(new_io_mode.data_bits).unwrap()).unwrap(),
            stop_bits: match new_io_mode.stop_bits {
                UefiStopBits::ONE => StopBits::One,
                UefiStopBits::DEFAULT => StopBits::One,
                UefiStopBits::ONE_FIVE => StopBits::One,
                UefiStopBits::TWO => StopBits::Two,
                val=> panic!("invalid value: {val:?}")
            },
        };
        unsafe {
            self.device.init(&config);
        }
    }
}

// SAFETY: We only initialize it once in a single-threaded context.
unsafe impl Send for CustomSerialIoProtocol {}
// SAFETY: We only initialize it once in a single-threaded context.
unsafe impl Sync for CustomSerialIoProtocol {}

unsafe extern "efiapi" fn serial_protocol_reset(this: *mut SerialIoProtocol) -> Status {
    debug!("hello");
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };
    Status::UNSUPPORTED
}

unsafe extern "efiapi" fn serial_protocol_set_attributes(
    this: *mut SerialIoProtocol,
    baud_rate: u64,
    receive_fifo_depth: u32,
    timeout: u32,
    parity: Parity,
    data_bits: u8,
    stop_bits: UefiStopBits,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };
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
    bits: ControlBits,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };
    Status::UNSUPPORTED
}

unsafe extern "efiapi" fn serial_protocol_get_control_bits(
    this: *const SerialIoProtocol,
    bits: *mut ControlBits,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_ref().unwrap() };
    Status::UNSUPPORTED
}

unsafe extern "efiapi" fn serial_protocol_write(
    this: *mut SerialIoProtocol,
    len: *mut usize,
    data: *const u8,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };

    let msg = unsafe { slice::from_raw_parts(data, *len) };

    this.device.write_bytes_saturating(msg);

    Status::SUCCESS
}

unsafe extern "efiapi" fn serial_protocol_read(
    this: *mut SerialIoProtocol,
    len: *mut usize,
    data: *mut u8,
) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe { this.cast::<CustomSerialIoProtocol>().as_mut().unwrap() };

    let slice = unsafe { slice::from_raw_parts_mut(data, *len) };
    let n = this.device.read_bytes(slice);

    unsafe {
        *len = n;
    }

    Status::SUCCESS
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

    let mut device = unsafe { Uart16550Port::new(0x3f8) };
    unsafe {
        device.test_loopback().context("performing serial loopback test")?;
    };

    let protocol_interface = CustomSerialIoProtocol {
        inner: SerialIoProtocol {
            revision: 1,
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
            timeout: 1, /* µs */
            baud_rate: 115200,
            receive_fifo_depth: 16,
            data_bits: 8,
            parity: Parity::NONE,
            stop_bits: UefiStopBits::ONE,
        },
        device_path,
        // SAFETY: We know the device is there.
        device
    };

    let _ = unsafe { INTERFACE.call_once(|| protocol_interface) };
    // Ensure the pointer of the inner protocol is updated to the initial value.
    // Also synchronize everything to the hardware port.
    unsafe {
        let prot = INTERFACE.get_mut().unwrap();
        prot.update_io_mode(prot.mode);
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
