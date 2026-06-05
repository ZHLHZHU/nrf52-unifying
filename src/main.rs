#![no_std]
#![no_main]

use nrf52840_hal as hal;

use hal::clocks::Clocks;
use hal::usbd::{UsbPeripheral, Usbd};
use usb_device::class_prelude::UsbBusAllocator;
use usb_device::device::{StringDescriptors, UsbDeviceBuilder, UsbVidPid};
use usb_device::UsbError;
use usbd_serial::{SerialPort, USB_CLASS_CDC};

#[cortex_m_rt::entry]
fn main() -> ! {
    let p = hal::pac::Peripherals::take().unwrap();
    let clocks = Clocks::new(p.CLOCK).enable_ext_hfosc();

    let usb_bus = UsbBusAllocator::new(Usbd::new(UsbPeripheral::new(p.USBD, &clocks)));
    let mut serial = SerialPort::new(&usb_bus);
    let mut usb_dev = UsbDeviceBuilder::new(&usb_bus, UsbVidPid(0x1209, 0x0001))
        .strings(&[StringDescriptors::default()
            .manufacturer("nrf-demo")
            .product("nRF52840 CDC")
            .serial_number("0001")])
        .unwrap()
        .device_class(USB_CLASS_CDC)
        .max_packet_size_0(64)
        .unwrap()
        .build();

    let mut greeted = false;
    let hello = b"Hello, World! from nRF52840 USB CDC\r\n";

    loop {
        if !usb_dev.poll(&mut [&mut serial]) {
            continue;
        }

        if !greeted {
            match serial.write(hello) {
                Ok(count) if count == hello.len() => greeted = true,
                Ok(_) | Err(UsbError::WouldBlock) => {}
                Err(_) => {}
            }
        }

        let mut buf = [0u8; 64];
        match serial.read(&mut buf) {
            Ok(count) if count > 0 => {
                let mut written = 0;
                while written < count {
                    match serial.write(&buf[written..count]) {
                        Ok(len) if len > 0 => written += len,
                        Ok(_) | Err(UsbError::WouldBlock) => {}
                        Err(_) => break,
                    }
                }
            }
            Ok(_) | Err(UsbError::WouldBlock) => {}
            Err(_) => {}
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        cortex_m::asm::bkpt();
    }
}
