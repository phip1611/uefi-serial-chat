//! Implementation for the UEFI serial protocol.

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Write;
use core::pin::Pin;
use core::ptr::null;
use core::slice;
use log::debug;
use spin::Spin;
use uart_16550::{SerialPort, WouldBlockError};
use uefi::{boot, Handle, Identify};
use uefi::proto::console::serial::{IoMode, Serial};
use uefi::proto::device_path::{build, messaging, DevicePath};
use uefi::proto::device_path::build::DevicePathBuilder;
use uefi_raw::protocol::console::serial::{ControlBits, Parity, SerialIoMode, SerialIoProtocol, StopBits};
use uefi_raw::Status;

static INTERFACE: spin::Once<CustomSerialIoProtocol> = spin::Once::new();

#[derive(Debug)]
#[repr(C)]
struct CustomSerialIoProtocol {
    inner: SerialIoProtocol,
    /* End ABI-compatible portion: now extra stuff */
    mode: SerialIoMode,
    initialized: bool,
    device_path: Box<DevicePath>,
    device: uart_16550::SerialPort,
}

impl CustomSerialIoProtocol {
    fn init(&mut self) {
        if self.initialized {
            panic!("already initialized");
        }

        self.device.init();

        self.initialized = true;
    }

    fn update_io_mode(&mut self, new_io_mode: IoMode) {
        self.mode = new_io_mode;
        let ptr = &raw const self.mode;
        // SAFETY: We take care of that `self.mode` will not be invalidated.
        self.inner.mode = ptr;
    }
}

// SAFETY: We only initialize it once in a single-threaded context.
unsafe impl Send for CustomSerialIoProtocol {}
// SAFETY: We only initialize it once in a single-threaded context.
unsafe impl Sync for CustomSerialIoProtocol {}

unsafe extern "efiapi" fn serial_protocol_reset(this: *mut SerialIoProtocol) -> Status {
    debug!("hello");
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe {
        this.cast::<CustomSerialIoProtocol>().as_mut().unwrap()
    };
    Status::UNSUPPORTED

}

unsafe extern "efiapi" fn serial_protocol_set_attributes(this: *mut SerialIoProtocol, baud_rate: u64,
                                                         receive_fifo_depth: u32,
                                                         timeout: u32,
                                                         parity: Parity,
                                                         data_bits: u8,
                                                         stop_bits_type: StopBits,) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe {
        this.cast::<CustomSerialIoProtocol>().as_mut().unwrap()
    };
    if !this.initialized {
        this.init();
    }
    let attributes = IoMode {
        baud_rate,
        parity,
        receive_fifo_depth,
        timeout,
        control_mask: this.mode.control_mask,
        data_bits: this.mode.data_bits,
        stop_bits: this.mode.stop_bits,
    };
    this.update_io_mode(attributes);
    Status::SUCCESS

}

unsafe extern "efiapi" fn serial_protocol_set_control_bits(this: *mut SerialIoProtocol, bits: ControlBits) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe {
        this.cast::<CustomSerialIoProtocol>().as_mut().unwrap()
    };
    if !this.initialized {
        this.init();
    }
    Status::UNSUPPORTED
}

unsafe extern "efiapi" fn serial_protocol_get_control_bits(this: *const SerialIoProtocol, bits: *mut ControlBits) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe {
        this.cast::<CustomSerialIoProtocol>().as_ref().unwrap()
    };
    Status::UNSUPPORTED

}

unsafe extern "efiapi" fn serial_protocol_write(this: *mut SerialIoProtocol, len: *mut usize, data: *const u8) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe {
        this.cast::<CustomSerialIoProtocol>().as_mut().unwrap()
    };
    if !this.initialized {
        this.init();
    }

    let msg = unsafe { slice::from_raw_parts(data, *len) };
    let msg = str::from_utf8(msg).unwrap();

    //if !msg.is_empty() {
        this.device.write_str(msg).unwrap();
    //}

    Status::SUCCESS
}

unsafe extern "efiapi" fn serial_protocol_read(this: *mut SerialIoProtocol, len: *mut usize, data: *mut u8) -> Status {
    // SAFETY: We installed the protocol interface before and know the ABI.
    let this = unsafe {
        this.cast::<CustomSerialIoProtocol>().as_mut().unwrap()
    };
    if !this.initialized {
        this.init();
    }

    let n = unsafe { *len };
    for i in 0..n {
        let byte = this.device.try_receive();

        match byte {
            Ok(byte) => {
                unsafe {
                    data.add(i).write(byte);
                }
            }
            Err(_) => {
                unsafe {
                    *len = i;
                }
                break;
            }
        }
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
            timeout: 1,
            baud_rate: 115200,
            receive_fifo_depth: 16,
            data_bits: 8,
            parity: Parity::DEFAULT,
            stop_bits: StopBits::DEFAULT,
        },
        initialized: false,
        device_path,
        // SAFETY: We know the device is there.
        device: unsafe { SerialPort::new(0x3f8) },
    };

    let prot = INTERFACE.call_once(|| {
        protocol_interface
    });
    let serial_interface_ptr = &raw const *prot;
    let dvp_interface_ptr = prot.device_path.as_ffi_ptr();

    let handle = unsafe { boot::install_protocol_interface(None, &Serial::GUID, serial_interface_ptr.cast()) }?;
    let _ = unsafe { boot::install_protocol_interface(Some(handle), &DevicePath::GUID, dvp_interface_ptr.cast()) }?;

    Ok(handle)
}
