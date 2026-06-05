#![no_std]
#![no_main]

use core::cell::RefCell;

use cortex_m_rt::{entry, exception};
use embassy_boot_nrf::{BootLoader, BootLoaderConfig};
use embassy_nrf::nvmc::Nvmc;
use embassy_sync::blocking_mutex::Mutex;

#[entry]
fn main() -> ! {
    let p = embassy_nrf::init(Default::default());

    let flash = Nvmc::new(p.NVMC);
    let flash = Mutex::new(RefCell::new(flash));

    let config = BootLoaderConfig::from_linkerfile_blocking(&flash, &flash, &flash);
    let active_offset = config.active.offset();
    let bootloader: BootLoader = BootLoader::prepare(config);

    unsafe { bootloader.load(active_offset) }
}

#[no_mangle]
#[cfg_attr(target_os = "none", link_section = ".HardFault.user")]
unsafe extern "C" fn HardFault() {
    cortex_m::peripheral::SCB::sys_reset();
}

#[exception]
unsafe fn DefaultHandler(_: i16) -> ! {
    cortex_m::peripheral::SCB::sys_reset();
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    cortex_m::asm::udf();
}
