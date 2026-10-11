#![no_std]
#![no_main]


// SWD snapshots only: no diagnostic text is injected into the protocol.
#[repr(C)]
pub struct UartDebug {
    rx_bytes: u32, rx_chunks: u32, rx_frames: u32, rx_errors: u32,
    tx_bytes: u32, tx_frames: u32, tx_errors: u32,
    rx_len: u32, tx_len: u32, rx: [u8; 256], tx: [u8; 256],
}
#[no_mangle]
pub static mut nrf_uart_debug: UartDebug = UartDebug {
    rx_bytes: 0, rx_chunks: 0, rx_frames: 0, rx_errors: 0,
    tx_bytes: 0, tx_frames: 0, tx_errors: 0,
    rx_len: 0, tx_len: 0, rx: [0; 256], tx: [0; 256],
};
fn debug_add(p: *mut u32, n: u32) {
    unsafe { p.write_volatile(p.read_volatile().wrapping_add(n)); }
}
fn debug_packet(tx: bool, bytes: &[u8]) {
    unsafe {
        let d = core::ptr::addr_of_mut!(nrf_uart_debug);
        let (buf, len) = if tx {
            (core::ptr::addr_of_mut!((*d).tx).cast::<u8>(), core::ptr::addr_of_mut!((*d).tx_len))
        } else {
            (core::ptr::addr_of_mut!((*d).rx).cast::<u8>(), core::ptr::addr_of_mut!((*d).rx_len))
        };
        len.write_volatile(0);
        for (i, &b) in bytes.iter().take(256).enumerate() { buf.add(i).write_volatile(b); }
        len.write_volatile(bytes.len().min(256) as u32);
    }
}

mod esb_radio;
mod keymap;
mod storage;
mod unifying_hal;
mod uart_link;
mod uart_ota;
#[cfg(feature = "uart-smoke")]
mod uart_smoke;

use core::{cell::RefCell, str, sync::atomic::{AtomicBool, AtomicU32, Ordering}};

use cortex_m::peripheral::SCB;
use embassy_boot::State as BootState;
use embassy_boot_nrf::{BlockingFirmwareUpdater, FirmwareUpdaterConfig};
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{select, Either};
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::usb::vbus_detect::{HardwareVbusDetect, VbusDetect};
use embassy_nrf::usb::Driver;
use embassy_nrf::{bind_interrupts, buffered_uarte, pac, peripherals, usb, uarte};
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, NoopRawMutex};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::{channel::Channel, signal::Signal};
use embassy_time::{Duration, Instant, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State, Sender};
use embassy_usb::driver::EndpointError;
use embassy_usb::{Builder, Config};
use panic_reset as _;

use rust_unifying::constants::{
    ADDRESS_LEN, AES_BLOCK_LEN, CHANNELS, KEYS_LEN, PAIRING_ADDRESS, PAIRING_CHANNELS,
};
use rust_unifying::radio::UnifyingRadio;
use rust_unifying::{PairingParams, UnifyingDevice};

use esb_radio::EsbRadio;
use keymap::char_to_hid;
use storage::{Profile, SharedFlash, MAX_PROFILES};
use unifying_hal::{EmbassyClock, SwAesCtr};

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
    UARTE0 => buffered_uarte::InterruptHandler<peripherals::UARTE0>;
});

const ACTIVE_IMAGE_LIMIT: usize = 480 * 1024;
#[cfg(not(feature = "uart-smoke"))]
const VERSION_REPLY: &[u8] =
    concat!("VER unifying/1 build=", env!("BUILD_TIME"), "\r\n").as_bytes();
#[cfg(feature = "uart-smoke")]
const VERSION_REPLY: &[u8] =
    concat!("VER unifying/1 build=", env!("BUILD_TIME"), " uart-smoke\r\n").as_bytes();

const READY_BANNER: &[u8] =
    concat!("nrf-unifying ready build=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_BOOT: &[u8] = concat!("STATE BOOT BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_SWAP: &[u8] = concat!("STATE SWAP BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_REVERT: &[u8] = concat!("STATE REVERT BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_DFU: &[u8] = concat!("STATE DFU BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_ERR: &[u8] = concat!("STATE ERR BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();

/// Concrete `UnifyingDevice` for this board.
type Device = UnifyingDevice<EsbRadio, EmbassyClock, SwAesCtr>;

/// Persistent connection state owned exclusively by the radio worker.
struct UnifyingState {
    device: Device,
    paired: bool,
    connected: bool,
    /// Currently active profile slot (0..MAX_PROFILES).
    active_slot: u8,
    /// Last keystroke activity timestamp — used for idle timeout backoff.
    last_activity: Instant,
    /// Whether the receiver of the active slot answered our last transmission. While `false`
    /// (searching) the background keep-alive hops channels; key reports only probe once so every
    /// UART command finishes in bounded time instead of blocking ~1.4s on an unreachable target.
    link_up: bool,
}

impl UnifyingState {
    fn new() -> Self {
        let radio = EsbRadio::new();
        let clock = EmbassyClock::new();
        let device = UnifyingDevice::new(
            radio,
            clock,
            SwAesCtr,
            [0u8; ADDRESS_LEN],
            [0u8; AES_BLOCK_LEN],
            0,
            CHANNELS[0],
        );
        Self {
            device,
            paired: false,
            connected: false,
            active_slot: 0,
            last_activity: Instant::now(),
            link_up: false,
        }
    }

    /// Restore a persisted pairing into the in-RAM device state.
    fn apply_profile(&mut self, p: &Profile) {
        self.device.address = p.address;
        self.device.aes_key = p.aes_key;
        self.device.aes_counter = p.aes_counter;
        self.device.channel = p.channel;
        self.paired = true;
        self.connected = false;
        self.link_up = false;
    }

    /// Snapshot the current pairing for persistence.
    fn current_profile(&self) -> Profile {
        Profile {
            address: self.device.address,
            aes_key: self.device.aes_key,
            aes_counter: self.device.aes_counter,
            channel: self.device.channel,
        }
    }

    /// Switch to a different profile slot and load it from flash.
    /// Returns true if the slot has a stored pairing.
    fn switch_to(&mut self, flash: &SharedFlash<'_>, slot: u8) -> bool {
        if slot == self.active_slot {
            // Never overwrite the live counter with an older flash snapshot.
            return self.paired;
        }
        if self.paired {
            storage::save(flash, self.active_slot, &self.current_profile());
        }
        self.connected = false;
        self.link_up = false;
        self.active_slot = slot;
        storage::save_active_slot(flash, slot);
        if let Some(profile) = storage::load(flash, slot) {
            self.apply_profile(&profile);
            true
        } else {
            self.paired = false;
            false
        }
    }
}

struct Disconnected;

impl From<EndpointError> for Disconnected {
    fn from(err: EndpointError) -> Self {
        match err {
            EndpointError::BufferOverflow => panic!("usb buffer overflow"),
            EndpointError::Disabled => Disconnected,
        }
    }
}

#[derive(Clone, Copy)]
enum Source {
    Cdc(u32),
    Uart { number: u32, session: u32, fault_generation: u32 },
}

struct Request {
    source: Source,
    line: [u8; 256],
    len: usize,
    error: Option<uart_link::CommandError>,
}

struct Reply {
    source: Source,
    bytes: [u8; 512],
    len: usize,
}

// Radio ownership never awaits either transport's output endpoint.
static REQUESTS: Channel<CriticalSectionRawMutex, Request, 8> = Channel::new();
static CDC_REPLIES: Channel<CriticalSectionRawMutex, Reply, 1> = Channel::new();
static UART_REPLIES: Channel<CriticalSectionRawMutex, Reply, 1> = Channel::new();
static MAINTENANCE_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
static MAINTENANCE_READY: Signal<CriticalSectionRawMutex, u32> = Signal::new();
#[cfg(not(feature = "uart-smoke"))]
static UART_SESSION_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
#[cfg(not(feature = "uart-smoke"))]
static UART_SYNC_REQUIRED: AtomicBool = AtomicBool::new(true);
static CDC_ACTIVE: AtomicBool = AtomicBool::new(false);
static CDC_SESSION: AtomicU32 = AtomicU32::new(0);
static UART_OTA_ACTIVE: AtomicBool = AtomicBool::new(false);
static UPLOAD_ACTIVE: AtomicBool = AtomicBool::new(false);
static UART_DAMAGED: AtomicBool = AtomicBool::new(false);
static UART_GENERATION: AtomicU32 = AtomicU32::new(0);
static USB_AVAILABLE: AtomicBool = AtomicBool::new(false);
static USB_DTR_VALID: AtomicBool = AtomicBool::new(false);
static USB_LIFECYCLE_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();
// Once accepted, BOOT/REBOOT must survive cancellation of the CDC session.
static REBOOT_REQUESTED: Signal<CriticalSectionRawMutex, ()> = Signal::new();


struct UartDfuStore<'a, 'b, D: embedded_storage::nor_flash::NorFlash, S: embedded_storage::nor_flash::NorFlash> {
    updater: &'a mut BlockingFirmwareUpdater<'b, D, S>,
}
impl<D: embedded_storage::nor_flash::NorFlash, S: embedded_storage::nor_flash::NorFlash> uart_ota::Storage for UartDfuStore<'_, '_, D, S> {
    fn write(&mut self, offset: usize, bytes: &[u8]) -> bool { self.updater.write_firmware(offset, bytes).is_ok() }
    fn read(&mut self, offset: usize, bytes: &mut [u8]) -> bool {
        if offset.checked_add(bytes.len()).is_none_or(|end| end > uart_ota::LIMIT) { return false; }
        self.updater.read_dfu(offset as u32, bytes).is_ok()
    }
    fn commit(&mut self) -> bool { self.updater.mark_updated().is_ok() }
}

struct UsbLifecycle;

impl UsbLifecycle {
    fn unavailable(&mut self) {
        USB_AVAILABLE.store(false, Ordering::Relaxed);
        USB_DTR_VALID.store(false, Ordering::Relaxed);
        // CDC Control does not clear DTR on PowerRemoved in embassy-usb 0.6.
        // Cancel this generation synchronously even when the nRF stays powered.
        CDC_ACTIVE.store(false, Ordering::Relaxed);
        CDC_SESSION.fetch_add(1, Ordering::Relaxed);
        UPLOAD_ACTIVE.store(false, Ordering::Relaxed);
        while CDC_REPLIES.try_receive().is_ok() {}
        MAINTENANCE_CHANGED.signal(());
        #[cfg(not(feature = "uart-smoke"))]
        UART_SESSION_CHANGED.signal(());
        USB_LIFECYCLE_CHANGED.signal(());
    }
}

impl embassy_usb::Handler for UsbLifecycle {
    fn enabled(&mut self, enabled: bool) {
        if !enabled { self.unavailable(); }
    }

    fn reset(&mut self) {
        self.unavailable();
    }

    fn configured(&mut self, configured: bool) {
        if configured {
            USB_AVAILABLE.store(true, Ordering::Relaxed);
            USB_LIFECYCLE_CHANGED.signal(());
        } else {
            self.unavailable();
        }
    }

    fn control_out(
        &mut self, req: embassy_usb::control::Request, _data: &[u8],
    ) -> Option<embassy_usb::control::OutResponse> {
        use embassy_usb::control::{Recipient, RequestType};
        // This application has one CDC function, with communication interface 0.
        // Observe fresh SET_CONTROL_LINE_STATE before CDC handles it; never
        // reactivate maintenance from the stale DTR retained after unplugging.
        if req.request_type == RequestType::Class
            && req.recipient == Recipient::Interface && req.index == 0
            && req.request == 0x22
        {
            USB_DTR_VALID.store(req.value & 1 != 0, Ordering::Relaxed);
            USB_LIFECYCLE_CHANGED.signal(());
        }
        None // CDC remains the sole owner of the actual control response.
    }
}

struct ReplySink {
    reply: Reply,
}

impl ReplySink {
    fn new(source: Source) -> Self {
        Self { reply: Reply { source, bytes: [0; 512], len: 0 } }
    }

    async fn write(&mut self, data: &[u8]) -> Result<(), ()> {
        let end = self.reply.len + data.len();
        if end > self.reply.bytes.len() { return Err(()); }
        self.reply.bytes[self.reply.len..end].copy_from_slice(data);
        self.reply.len = end;
        Ok(())
    }
}

async fn session_ended(session: u32) {
    while CDC_ACTIVE.load(Ordering::Relaxed)
        && CDC_SESSION.load(Ordering::Relaxed) == session
    {
        Timer::after(Duration::from_millis(1)).await;
    }
}

#[cfg(not(feature = "uart-smoke"))]
fn poll_radio(state: &mut UnifyingState, flash: &SharedFlash<'_>) {
    if state.connected && !UPLOAD_ACTIVE.load(Ordering::Relaxed) && !UART_OTA_ACTIVE.load(Ordering::Relaxed) {
        let idle_ms = Instant::now().duration_since(state.last_activity).as_millis();
        state.device.timeout = if idle_ms > 500 { 110 } else { 20 };
        // tick() is Ok both when nothing was due and when a frame was ACKed; only a new transmit
        // timestamp proves the receiver heard us.
        let before = state.device.previous_transmit;
        match state.device.tick() {
            Ok(()) if state.device.previous_transmit != before => state.link_up = true,
            Err(_) => state.link_up = false,
            Ok(()) => {}
        }
    }
    // A polled warning, not a measured power-loss guarantee.
    if pac::POWER.events_pofwarn().read() != 0 {
        pac::POWER.events_pofwarn().write_value(0);
        if state.paired {
            storage::save(flash, state.active_slot, &state.current_profile());
        }
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

    // Preserve the existing polled 2.7V power warning and persistence behavior.
    // Available hold-up time is unmeasured; this is not a power-loss guarantee.
    pac::POWER.pofcon().write(|w| {
        w.set_pof(true);
        w.set_threshold(pac::power::vals::Threshold::V27);
    });
    pac::POWER.events_pofwarn().write_value(0);

    let flash = Mutex::<NoopRawMutex, _>::new(RefCell::new(Nvmc::new(p.NVMC)));

    let driver = Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));

    let mut config = Config::new(0x1209, 0x0001);
    config.manufacturer = Some("nrf-demo");
    config.product = Some("nRF52840 Unifying CDC");
    config.serial_number = Some("0001");
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    let mut config_descriptor = [0; 256];
    let mut bos_descriptor = [0; 256];
    let mut msos_descriptor = [0; 256];
    let mut control_buf = [0; 64];
    let mut cdc_state = State::new();
    let mut usb_lifecycle = UsbLifecycle;

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );
    // Register first so lifecycle observes CDC control requests without consuming them.
    builder.handler(&mut usb_lifecycle);
    let class = CdcAcmClass::new(&mut builder, &mut cdc_state, 64);
    let mut usb = builder.build();

    let updater_config = FirmwareUpdaterConfig::from_linkerfile_blocking(&flash, &flash);
    let mut aligned = [0u8; 4];
    let mut updater = BlockingFirmwareUpdater::new(updater_config, &mut aligned);

    if matches!(updater.get_state(), Ok(BootState::Swap | BootState::Revert)) {
        let _ = updater.mark_booted();
    }

    let mut unifying = UnifyingState::new();
    #[cfg(not(feature = "uart-smoke"))]
    {
    // Restore persisted active slot and its profile from flash.
    if let Some(slot) = storage::load_active_slot(&flash) {
        unifying.active_slot = slot;
        if let Some(mut profile) = storage::load(&flash, slot) {
            // Safety margin: jump counter forward to cover any un-persisted
            // keystrokes from before a crash/power-loss.
            profile.aes_counter = profile.aes_counter.wrapping_add(512);
            unifying.apply_profile(&profile);
            storage::save(&flash, slot, &profile);
        }
    } else if let Some(mut profile) = storage::load(&flash, 0) {
        // Backward compat: no active-slot stored, try slot 0.
        profile.aes_counter = profile.aes_counter.wrapping_add(512);
        unifying.apply_profile(&profile);
        storage::save(&flash, 0, &profile);
    }
    }

    // Board wiring (docs/BOARD-KVM.md): P0.06 RX ← RP GP4, P0.08 TX → RP GP5.
    let mut uart_config = uarte::Config::default();
    uart_config.baudrate = uarte::Baudrate::BAUD1M;
    uart_config.parity = uarte::Parity::EXCLUDED;
    // Continuous ring-buffered RX. The plain Uarte only receives while a read future is polled, so
    // lines arriving while the worker blocks on the radio (or across a select) were silently lost:
    // the RP then timed out with no reply. 1 KiB holds ~10ms at 1 Mbaud; TX is one reply at a time.
    static mut UART_RX_BUF: [u8; 1024] = [0; 1024];
    static mut UART_TX_BUF: [u8; 600] = [0; 600];
    let uart = buffered_uarte::BufferedUarte::new(
        p.UARTE0, p.TIMER0, p.PPI_CH0, p.PPI_CH1, p.PPI_GROUP0, p.P0_06, p.P0_08, Irqs, uart_config,
        unsafe { &mut *core::ptr::addr_of_mut!(UART_RX_BUF) },
        unsafe { &mut *core::ptr::addr_of_mut!(UART_TX_BUF) },
    );
    // Keep RX idle high while the Pico resets or its TX is disconnected.
    pac::P0.pin_cnf(6).modify(|w| w.set_pull(pac::gpio::vals::Pull::PULLUP));
    let (mut uart_rx, mut uart_tx) = uart.split();
    let (mut cdc_tx, mut cdc_rx, cdc_control) = class.split_with_control();

    let control_fut = async {
        // USB configuration plus a fresh DTR assertion are both required.
        loop {
            let next = USB_AVAILABLE.load(Ordering::Relaxed)
                && USB_DTR_VALID.load(Ordering::Relaxed) && cdc_control.dtr();
            if next != CDC_ACTIVE.load(Ordering::Relaxed) {
                CDC_SESSION.fetch_add(1, Ordering::Relaxed);
                CDC_ACTIVE.store(next, Ordering::Relaxed);
                #[cfg(not(feature = "uart-smoke"))]
                if next {
                    // Even a whole CDC window between UART requests must be noticed.
                    UART_SYNC_REQUIRED.store(true, Ordering::Relaxed);
                }
                if !next {
                    UPLOAD_ACTIVE.store(false, Ordering::Relaxed);
                    while CDC_REPLIES.try_receive().is_ok() {}
                }
                MAINTENANCE_CHANGED.signal(());
                #[cfg(not(feature = "uart-smoke"))]
                UART_SESSION_CHANGED.signal(());
            }
            let _ = select(
                cdc_control.control_changed(), USB_LIFECYCLE_CHANGED.wait(),
            ).await;
        }
    };
    #[cfg(not(feature = "uart-smoke"))]
    let worker_fut = async {
        let mut ota = uart_ota::Ota::default();
        let config = FirmwareUpdaterConfig::from_linkerfile_blocking(&flash, &flash);
        let mut ota_aligned = [0u8; 4];
        let mut ota_updater = BlockingFirmwareUpdater::new(config, &mut ota_aligned);
        let mut ota_deadline = Instant::now();
        let interval = Duration::from_millis(8);
        let mut deadline = Instant::now() + interval;
        let mut session_seen = 0;
        loop {
            if ota.active && Instant::now() >= ota_deadline {
                ota.abort(); UART_OTA_ACTIVE.store(false, Ordering::Relaxed);
                UART_SYNC_REQUIRED.store(true, Ordering::Relaxed);
            }
            let session = CDC_SESSION.load(Ordering::Relaxed);
            if session != session_seen {
                session_seen = session;
                if CDC_ACTIVE.load(Ordering::Relaxed) {
                    // Complete the current command, then all-up before CDC owns the radio.
                    if unifying.connected {
                        let _ = press_key(&mut unifying.device, &[0; KEYS_LEN], 0);
                    }
                    MAINTENANCE_READY.signal(session);
                }
            }
            if UART_DAMAGED.swap(false, Ordering::Relaxed) && unifying.connected {
                let _ = press_key(&mut unifying.device, &[0; KEYS_LEN], 0);
            }
            if Instant::now() >= deadline {
                poll_radio(&mut unifying, &flash);
                while deadline <= Instant::now() { deadline += interval; }
            }
            let request = match select(
                REQUESTS.receive(),
                select(Timer::at(deadline), MAINTENANCE_CHANGED.wait()),
            ).await {
                Either::First(request) => request,
                Either::Second(_) => continue,
            };
            // Absolute deadline also checked before every command under sustained load.
            if Instant::now() >= deadline {
                poll_radio(&mut unifying, &flash);
                while deadline <= Instant::now() { deadline += interval; }
            }
            let mut sink = ReplySink::new(request.source);
            match request.source {
                Source::Cdc(epoch) => {
                    if !CDC_ACTIVE.load(Ordering::Relaxed)
                        || CDC_SESSION.load(Ordering::Relaxed) != epoch
                        || session_seen != epoch
                    { continue; }
                }
                Source::Uart { session, fault_generation, .. } => {
                    if fault_generation != UART_GENERATION.load(Ordering::Relaxed) {
                        continue; // Never execute an input queued before UART damage.
                    }
                    if CDC_ACTIVE.load(Ordering::Relaxed)
                        || session != CDC_SESSION.load(Ordering::Relaxed)
                    {
                        let _ = sink.write(b"ERR BUSY\r\n").await;
                    }
                    else if UART_SYNC_REQUIRED.load(Ordering::Relaxed) {
                        if request.error.is_none() && &request.line[..request.len] == b"VER" {
                            UART_SYNC_REQUIRED.store(false, Ordering::Relaxed);
                        } else {
                            let _ = sink.write(b"ERR BUSY\r\n").await;
                        }
                    }
                }
            }
            if sink.reply.len == 0 && matches!(request.source, Source::Uart { .. }) && request.line[..request.len].starts_with(b"OTA ") {
                if UPLOAD_ACTIVE.load(Ordering::Relaxed) {
                    let _ = sink.write(b"ERR BUSY\r\n").await;
                } else {
                    if request.line[..request.len].starts_with(b"OTA BEGIN ") && unifying.connected {
                        let _ = press_key(&mut unifying.device, &[0; KEYS_LEN], 0);
                    }
                    if request.line[..request.len].starts_with(b"OTA BEGIN ") {
                        drop(ota_updater);
                        let config = FirmwareUpdaterConfig::from_linkerfile_blocking(&flash, &flash);
                        ota_updater = BlockingFirmwareUpdater::new(config, &mut ota_aligned);
                    }
                    let mut store = UartDfuStore { updater: &mut ota_updater };
                    let (reply, reboot) = if &request.line[..request.len] == b"OTA INFO" {
                        (if matches!(store.updater.get_state(), Ok(BootState::Boot)) { b"OTA BOOT\r\n" as &[u8] } else { b"OTA PENDING\r\n" }, false)
                    } else { ota.handle(&request.line[..request.len], &mut store) };
                    UART_OTA_ACTIVE.store(ota.active, Ordering::Relaxed);
                    ota_deadline = Instant::now() + Duration::from_secs(60);
                    let _ = sink.write(reply).await;
                    if reboot { REBOOT_REQUESTED.signal(()); }
                }
            }
            if sink.reply.len == 0 && ota.active {
                let _ = sink.write(b"ERR BUSY\r\n").await;
            }
            if sink.reply.len == 0 {
                match request.error {
                    Some(uart_link::CommandError::Command) => {
                        let _ = sink.write(b"ERR CMD\r\n").await;
                    }
                    Some(uart_link::CommandError::Argument) => {
                        let _ = sink.write(b"ERR ARG\r\n").await;
                    }
                    None => handle_unifying_command(
                        &mut sink, &mut unifying, &flash, &request.line[..request.len],
                    ).await,
                }
            }
            match request.source {
                Source::Cdc(epoch) if CDC_ACTIVE.load(Ordering::Relaxed)
                    && CDC_SESSION.load(Ordering::Relaxed) == epoch => {
                    // CDC submits only one request at a time; stale replies never block.
                    let _ = CDC_REPLIES.try_send(sink.reply);
                }
                Source::Uart { .. } => {
                    // The TX task drains one reply while the next is queued; back-to-back requests
                    // (the RP abandons and re-sends) used to overflow this capacity-1 channel and
                    // silently discard every queued request. The RP keeps at most a few lines in
                    // flight, so waiting for TX here is bounded by a single ~0.5ms frame.
                    UART_REPLIES.send(sink.reply).await;
                }
                _ => {}
            }
        }
    };
    #[cfg(feature = "uart-smoke")]
    let worker_fut = async {
        let mut session_seen = 0;
        loop {
            let session = CDC_SESSION.load(Ordering::Relaxed);
            if session != session_seen {
                session_seen = session;
                if CDC_ACTIVE.load(Ordering::Relaxed) {
                    // No radio ownership or all-up is required in diagnostic mode.
                    MAINTENANCE_READY.signal(session);
                }
            }
            let request = match select(REQUESTS.receive(), MAINTENANCE_CHANGED.wait()).await {
                Either::First(request) => request,
                Either::Second(_) => continue,
            };
            let Source::Cdc(epoch) = request.source else { continue; };
            if !CDC_ACTIVE.load(Ordering::Relaxed)
                || CDC_SESSION.load(Ordering::Relaxed) != epoch || session_seen != epoch
            { continue; }
            let mut sink = ReplySink::new(request.source);
            handle_unifying_command(
                &mut sink, &mut unifying, &flash, &request.line[..request.len],
            ).await;
            if CDC_ACTIVE.load(Ordering::Relaxed)
                && CDC_SESSION.load(Ordering::Relaxed) == epoch
            {
                let _ = CDC_REPLIES.try_send(sink.reply);
            }
        }
    };
    #[cfg(feature = "uart-smoke")]
    let uart_rx_fut = async {
        let mut parser = uart_smoke::Parser::new();
        let mut packet = [0u8; 256];
        loop {
            match uart_rx.read(&mut packet).await {
                Ok(n) => {
                    uart_smoke::received_bytes(n);
                    for &byte in &packet[..n] {
                        match parser.push(byte) {
                            Some(uart_smoke::Frame::Peer(sequence)) =>
                                uart_smoke::received_peer(sequence),
                            Some(uart_smoke::Frame::Bad) => uart_smoke::bad_frame(),
                            None => {}
                        }
                    }
                }
                Err(_) => {
                    uart_smoke::uart_error();
                    if parser.damaged() { uart_smoke::bad_frame(); }
                }
            }
        }
    };
    #[cfg(feature = "uart-smoke")]
    let uart_tx_fut = async {
        let interval = Duration::from_secs(1);
        let mut deadline = Instant::now() + interval;
        let mut sequence = 1u32;
        loop {
            Timer::at(deadline).await;
            let wire = uart_smoke::encode_ping(sequence);
            let mut sent = 0;
            let mut ok = true;
            while sent < wire.len() {
                match uart_tx.write(&wire[sent..]).await { Ok(n) => sent += n, Err(_) => { ok = false; break; } }
            }
            if ok { uart_smoke::transmitted_frame(); } else { uart_smoke::uart_error(); }
            sequence = sequence.wrapping_add(1);
            // No CDC DTR, maintenance or upload state gates either diagnostic UART task.
            while deadline <= Instant::now() { deadline += interval; }
        }
    };
    #[cfg(not(feature = "uart-smoke"))]
    let uart_rx_fut = async {
        let mut parser = uart_link::Parser::new();
        let mut packet = [0u8; 256];
        let mut parser_session = CDC_SESSION.load(Ordering::Relaxed);
        loop {
            // Both session publishers run on this executor. Consume any old wake first.
            UART_SESSION_CHANGED.reset();
            let read_session = CDC_SESSION.load(Ordering::Relaxed);
            if read_session != parser_session {
                parser.session_changed(&[]);
                parser_session = read_session;
            }
            // The ring keeps receiving in hardware; a session change only resets the parser.
            let result = select(UART_SESSION_CHANGED.wait(), uart_rx.read(&mut packet)).await;
            let n = match result {
                Either::First(_) => continue,
                Either::Second(Err(_)) => {
                    unsafe { debug_add(core::ptr::addr_of_mut!(nrf_uart_debug.rx_errors), 1); }
                    parser.damaged();
                    UART_GENERATION.fetch_add(1, Ordering::Relaxed);
                    UART_DAMAGED.store(true, Ordering::Relaxed);
                    MAINTENANCE_CHANGED.signal(());
                    continue;
                }
                Either::Second(Ok(n)) => n,
            };
            unsafe { debug_add(core::ptr::addr_of_mut!(nrf_uart_debug.rx_bytes), n as u32); }
            unsafe { debug_add(core::ptr::addr_of_mut!(nrf_uart_debug.rx_chunks), 1); }
            debug_packet(false, &packet[..n]);
            let current_session = CDC_SESSION.load(Ordering::Relaxed);
            if current_session != read_session {
                parser.session_changed(&packet[..n]);
                parser_session = current_session;
                continue;
            }
            for &byte in &packet[..n] {
                if let Some(frame) = parser.push(byte) {
                    unsafe { debug_add(core::ptr::addr_of_mut!(nrf_uart_debug.rx_frames), 1); }
                    let request = Request {
                        source: Source::Uart {
                            number: frame.number,
                            session: read_session,
                            fault_generation: UART_GENERATION.load(Ordering::Relaxed),
                        }, line: frame.command,
                        len: frame.len, error: frame.error,
                    };
                    if REQUESTS.try_send(request).is_err() {
                        parser.damaged();
                        UART_GENERATION.fetch_add(1, Ordering::Relaxed);
                        UART_DAMAGED.store(true, Ordering::Relaxed);
                        MAINTENANCE_CHANGED.signal(());
                    }
                }
            }
        }
    };
    #[cfg(not(feature = "uart-smoke"))]
    let uart_tx_fut = async {
        let mut wire = [0u8; 522];
        loop {
            let reply = UART_REPLIES.receive().await;
            if let Source::Uart { number, .. } = reply.source {
                if let Some(len) =
                    uart_link::encode_reply(number, &reply.bytes[..reply.len], &mut wire)
                {
                    debug_packet(true, &wire[..len]);
                    // BufferedUarteTx::write may accept a prefix; push the whole frame.
                    let mut sent = 0;
                    let mut failed = false;
                    while sent < len {
                        match uart_tx.write(&wire[sent..len]).await {
                            Ok(n) => sent += n,
                            Err(_) => { failed = true; break; }
                        }
                    }
                    if failed {
                        unsafe { debug_add(core::ptr::addr_of_mut!(nrf_uart_debug.tx_errors), 1); }
                        UART_GENERATION.fetch_add(1, Ordering::Relaxed);
                        UART_DAMAGED.store(true, Ordering::Relaxed);
                        MAINTENANCE_CHANGED.signal(());
                    } else {
                        unsafe { debug_add(core::ptr::addr_of_mut!(nrf_uart_debug.tx_bytes), len as u32); }
                        unsafe { debug_add(core::ptr::addr_of_mut!(nrf_uart_debug.tx_frames), 1); }
                    }
                }
            }
        }
    };

    let usb_fut = usb.run();
    let serial_fut = async {
        let mut packet = [0u8; 64];
        let mut line = [0u8; 256];
        let mut staging = [0u8; 68];

        loop {
            cdc_rx.wait_connection().await;
            while !CDC_ACTIVE.load(Ordering::Relaxed) {
                Timer::after(Duration::from_millis(1)).await;
            }
            let session = CDC_SESSION.load(Ordering::Relaxed);
            let session_fut = async {
            loop {
                if MAINTENANCE_READY.wait().await == session { break; }
            }
            let _ = write_reply(&mut cdc_tx, READY_BANNER).await;

            let mut line_len = 0usize;
            let mut discard_line = false;
            let mut upload_active = false;
            let mut upload_size = 0usize;
            let mut upload_received = 0usize;
            let mut image_ready = false;
            let mut staging_len = 0usize;

            loop {
                let n = match cdc_rx.read_packet(&mut packet).await {
                    Ok(n) => n,
                    Err(_) => break,
                };

                if upload_active {
                    staging[staging_len..staging_len + n].copy_from_slice(&packet[..n]);
                    let total = staging_len + n;
                    let aligned_len = total & !0x03;

                    if aligned_len > 0 {
                        if updater
                            .write_firmware(upload_received, &staging[..aligned_len])
                            .is_err()
                        {
                            upload_active = false;
                            UPLOAD_ACTIVE.store(false, Ordering::Relaxed);
                            upload_size = 0;
                            upload_received = 0;
                            image_ready = false;
                            staging_len = 0;
                            let _ = write_reply(&mut cdc_tx, b"ERR WRITE\r\n").await;
                            continue;
                        }
                        upload_received += aligned_len;
                    }

                    let remaining = total - aligned_len;
                    if remaining > 0 {
                        staging.copy_within(aligned_len..total, 0);
                    }
                    staging_len = remaining;

                    if upload_received + staging_len >= upload_size {
                        if staging_len > 0 {
                            let mut padded = [0xFFu8; 4];
                            padded[..staging_len].copy_from_slice(&staging[..staging_len]);
                            if updater.write_firmware(upload_received, &padded).is_err() {
                                upload_active = false;
                                UPLOAD_ACTIVE.store(false, Ordering::Relaxed);
                                upload_size = 0;
                                upload_received = 0;
                                image_ready = false;
                                staging_len = 0;
                                let _ = write_reply(&mut cdc_tx, b"ERR WRITE\r\n").await;
                                continue;
                            }
                            upload_received += staging_len;
                            staging_len = 0;
                        }

                        upload_active = false;
                        UPLOAD_ACTIVE.store(false, Ordering::Relaxed);
                        image_ready = true;
                        let _ = write_reply(&mut cdc_tx, b"OK\r\n").await;
                    }

                    continue;
                }

                for &b in &packet[..n] {
                    if b == b'\n' {
                        let consumed_line_len = line_len;
                        line_len = 0;
                        if discard_line {
                            discard_line = false;
                            let _ = write_reply(&mut cdc_tx, b"ERR ARG\r\n").await;
                            continue;
                        }

                        // OTA commands first (kept byte-for-byte compatible).
                        match parse_ota_command(&line[..consumed_line_len]) {
                            Some(OtaCommand::Ping) => {
                                let _ = write_reply(&mut cdc_tx, b"PONG\r\n").await;
                                continue;
                            }
                            Some(OtaCommand::Info) => {
                                let resp = match updater.get_state() {
                                    Ok(BootState::Boot) => INFO_BOOT,
                                    Ok(BootState::Swap) => INFO_SWAP,
                                    Ok(BootState::Revert) => INFO_REVERT,
                                    Ok(BootState::DfuDetach) => INFO_DFU,
                                    Err(_) => INFO_ERR,
                                };
                                let _ = write_reply(&mut cdc_tx, resp).await;
                                continue;
                            }
                            Some(OtaCommand::Write(size)) => {
                                if UART_OTA_ACTIVE.load(Ordering::Relaxed) {
                                    let _ = write_reply(&mut cdc_tx, b"ERR BUSY\r\n").await;
                                    continue;
                                }
                                if size == 0 || size > ACTIVE_IMAGE_LIMIT {
                                    let _ = write_reply(&mut cdc_tx, b"ERR SIZE\r\n").await;
                                } else {
                                    upload_active = true;
                                    UPLOAD_ACTIVE.store(true, Ordering::Relaxed);
                                    upload_size = size;
                                    upload_received = 0;
                                    staging_len = 0;
                                    image_ready = false;
                                    let _ = write_reply(&mut cdc_tx, b"READY\r\n").await;
                                }
                                continue;
                            }
                            Some(OtaCommand::Boot) => {
                                if UART_OTA_ACTIVE.load(Ordering::Relaxed) {
                                    let _ = write_reply(&mut cdc_tx, b"ERR BUSY\r\n").await;
                                    continue;
                                }
                                if !image_ready {
                                    let _ = write_reply(&mut cdc_tx, b"ERR NOIMAGE\r\n").await;
                                } else if updater.mark_updated().is_err() {
                                    let _ = write_reply(&mut cdc_tx, b"ERR MARK\r\n").await;
                                } else {
                                    REBOOT_REQUESTED.signal(());
                                    let _ = write_reply(&mut cdc_tx, b"REBOOT\r\n").await;
                                }
                                continue;
                            }
                            Some(OtaCommand::Abort) => {
                                upload_active = false;
                                UPLOAD_ACTIVE.store(false, Ordering::Relaxed);
                                image_ready = false;
                                staging_len = 0;
                                let _ = write_reply(&mut cdc_tx, b"ABORTED\r\n").await;
                                continue;
                            }
                            Some(OtaCommand::Reboot) => {
                                REBOOT_REQUESTED.signal(());
                                let _ = write_reply(&mut cdc_tx, b"REBOOT\r\n").await;
                            }
                            None => {}
                        }

                        if consumed_line_len == 0 { continue; }
                        let mut request = Request {
                            source: Source::Cdc(session), line: [0; 256],
                            len: consumed_line_len, error: None,
                        };
                        request.line[..consumed_line_len]
                            .copy_from_slice(&line[..consumed_line_len]);
                        REQUESTS.send(request).await;
                        loop {
                            let reply = CDC_REPLIES.receive().await;
                            if matches!(reply.source, Source::Cdc(epoch) if epoch == session) {
                                let _ = write_reply(&mut cdc_tx, &reply.bytes[..reply.len]).await;
                                break;
                            }
                        }
                    } else if b != b'\r' && !discard_line {
                        if line_len == line.len() {
                            discard_line = true;
                        } else {
                            line[line_len] = b;
                            line_len += 1;
                        }
                    }
                }
            }
            };
            // DTR loss cancels endpoint writes and waits even if the host never reads.
            let _ = select(session_ended(session), session_fut).await;
            UPLOAD_ACTIVE.store(false, Ordering::Relaxed);
            while CDC_REPLIES.try_receive().is_ok() {}
        }
    };

    let reboot_fut = async {
        REBOOT_REQUESTED.wait().await;
        // Allow the reply to reach the host; DTR loss cannot cancel this timer.
        Timer::after(Duration::from_millis(100)).await;
        SCB::sys_reset();
    };

    join(reboot_fut, join(usb_fut, join(control_fut,
        join(worker_fut, join(serial_fut, join(uart_rx_fut, uart_tx_fut))),
    ))).await;
}

enum OtaCommand {
    Ping,
    Info,
    Write(usize),
    Boot,
    Abort,
    Reboot,
}

fn parse_ota_command(line: &[u8]) -> Option<OtaCommand> {
    match line {
        b"PING" => Some(OtaCommand::Ping),
        b"INFO" => Some(OtaCommand::Info),
        b"BOOT" => Some(OtaCommand::Boot),
        b"ABORT" => Some(OtaCommand::Abort),
        b"REBOOT" => Some(OtaCommand::Reboot),
        _ => {
            if let Some(rest) = line.strip_prefix(b"WRITE ") {
                if let Ok(text) = str::from_utf8(rest) {
                    if let Ok(size) = text.parse::<usize>() {
                        return Some(OtaCommand::Write(size));
                    }
                }
            }
            None
        }
    }
}

#[cfg(feature = "uart-smoke")]
async fn handle_unifying_command(
    sink: &mut ReplySink,
    _state: &mut UnifyingState,
    _flash: &SharedFlash<'_>,
    line: &[u8],
) {
    if line.is_empty() { return; }
    if line == b"VER" {
        let _ = sink.write(VERSION_REPLY).await;
    } else if line == b"UARTTEST" && matches!(sink.reply.source, Source::Cdc(_)) {
        let mut out = [0u8; 192];
        if let Ok(n) = uart_smoke::format_response(uart_smoke::snapshot(), &mut out) {
            let _ = sink.write(&out[..n]).await;
        }
    } else {
        // Never invoke any RF operation, including radio self-tests, in this build.
        let _ = sink.write(b"ERR DIAGNOSTIC\r\n").await;
    }
}

/// Handle one Unifying CDC command line.
///
/// Protocol (ASCII, `\n`-terminated):
/// - `VER`                report protocol version and build; `VER unifying/1 build=..`
/// - `UPAIR`              pair with a receiver in pairing mode; replies
///                        `PAIRED <addr-hex> CH=<n>` or `ERR PAIR`
/// - `UCONNECT`           wake/connect to the paired receiver; `CONNECTED CH=<n>`
/// - `UTYPE <text>`       type the text as keystrokes; `TYPED <ok>/<total>`
/// - `UKEY <mod> [keys]`  send one raw HID report (modifier + up to 6 hex
///                        keycodes), then release; `OK` or `ERR SEND`
/// - `UKEEPALIVE`         send one keep-alive tick; `TICK`
/// - `USTATUS`            report state; `STATUS PAIRED=.. CONN=.. CH=.. CNT=..`
/// - `UDELETE`            erase the stored pairing; `DELETED`
/// - unknown              prints the command list
#[cfg(not(feature = "uart-smoke"))]
async fn handle_unifying_command(
    sink: &mut ReplySink,
    state: &mut UnifyingState,
    flash: &SharedFlash<'_>,
    line: &[u8],
) {
    if line.is_empty() {
        return;
    }

    if line == b"URADIOTEST" {
        // Radio-layer self-test, independent of the protocol stack. For each
        // candidate address encoding, transmit a short wake-up-style packet on
        // every pairing channel and count how many CRC-valid ACKs come back.
        // The receiver must be in pairing mode (e.g. `sudo ltunify pair 60`).
        // The winning encoding is whichever scores ACKs.
        if state.device.configure_radio().is_err() {
            let _ = sink.write(b"ERR RADIO\r\n").await;
            return;
        }

        // A minimal 5-byte keep-alive request: any well-formed ESB frame to the
        // pairing address will be ACKed by a receiver in pairing mode.
        let probe: [u8; 5] = [0x00, 0x4F, 0x00, 0x00, 0xB1];

        for enc in 0..EsbRadio::ADDR_ENCODINGS {
            let radio = state.device.radio_mut();
            radio.set_addr_encoding(enc);
            let _ = radio.set_address(&PAIRING_ADDRESS);

            let mut acks: u32 = 0;
            for _ in 0..3 {
                for &ch in PAIRING_CHANNELS.iter() {
                    let radio = state.device.radio_mut();
                    let _ = radio.set_channel(ch);
                    if radio.probe_once(&probe) {
                        acks += 1;
                    }
                }
                // Yield to keep USB serviced between sweeps.
                Timer::after(Duration::from_millis(2)).await;
            }

            let mut out = [0u8; 48];
            let n = format_radiotest(&mut out, enc, acks);
            let _ = sink.write(&out[..n]).await;
        }
        let _ = sink.write(b"RADIOTEST DONE\r\n").await;
        return;
    }

    if line == b"UPAIR" || line.starts_with(b"UPAIR ") {
        // UPAIR [slot] — pair into specified slot (default: current active_slot).
        let slot = if line.len() > 6 {
            match parse_u8_hex(&line[6..]) {
                Some(s) if s < MAX_PROFILES => s,
                _ => {
                    let _ = sink.write(b"ERR SLOT\r\n").await;
                    return;
                }
            }
        } else {
            state.active_slot
        };

        let params = PairingParams {
            id: (embassy_time::Instant::now().as_ticks() as u8) | 0x01,
            product_id: 0x1025,
            device_type: 0x0147,
            crypto: embassy_time::Instant::now().as_ticks() as u32,
            serial: 0xA580_94B6,
            capabilities: 0x1E40,
            name: b"RustKbd",
        };

        // pair() rewrites address/key/channel as it goes; keep the old link to restore on failure.
        let previous = state.paired.then(|| (state.current_profile(), state.connected));
        if state.device.configure_radio().is_err() {
            let _ = sink.write(b"ERR RADIO\r\n").await;
            return;
        }

        // Pairing is probabilistic; the receiver hops channels. Retry a few
        // times before giving up so a single UPAIR has a fair chance. Keep the
        // count modest: each pair() attempt already scans all channels and
        // blocks the executor, so too many retries would starve USB.
        let mut paired = false;
        for _ in 0..8 {
            if state.device.pair(&params).is_ok() {
                paired = true;
                break;
            }
            Timer::after(Duration::from_millis(20)).await;
        }

        if paired {
            state.paired = true;
            state.connected = true;
            state.link_up = true;
            state.active_slot = slot;
            // Persist the new pairing into the specified slot.
            storage::save(flash, slot, &state.current_profile());
            storage::save_active_slot(flash, slot);
            let mut out = [0u8; 64];
            let n = format_paired(&mut out, &state.device.address, state.device.channel);
            let _ = sink.write(&out[..n]).await;
        } else {
            // A failed re-pair must leave the existing target usable, not half-overwritten.
            if let Some((profile, connected)) = previous {
                state.apply_profile(&profile);
                state.connected = connected && state.device.configure_radio().is_ok();
            }
            let _ = sink.write(b"ERR PAIR\r\n").await;
        }
        return;
    }

    if line == b"UCONNECT" {
        if !state.paired {
            let _ = sink.write(b"ERR NOTPAIRED\r\n").await;
            return;
        }
        if state.device.configure_radio().is_err() {
            let _ = sink.write(b"ERR RADIO\r\n").await;
            return;
        }
        let mut ok = false;
        for _ in 0..60 {
            if state.device.connect().is_ok() {
                ok = true;
                break;
            }
            Timer::after(Duration::from_millis(20)).await;
        }
        if ok {
            state.connected = true;
            state.link_up = true;
            let mut out = [0u8; 32];
            let n = format_connected(&mut out, state.device.channel);
            let _ = sink.write(&out[..n]).await;
        } else {
            let _ = sink.write(b"ERR CONNECT\r\n").await;
        }
        return;
    }

    if line == b"UKEEPALIVE" {
        if !state.connected {
            let _ = sink.write(b"ERR NOTCONN\r\n").await;
            return;
        }
        let _ = state.device.tick();
        let _ = sink.write(b"TICK\r\n").await;
        return;
    }

    // USLEEP — stop keep-alive and disable the radio. The link to the receiver
    // will drop (receiver sees device go offline). Use when the KVM switches to
    // local mode and there's no need to maintain a remote connection. USWITCH
    // or UCONNECT will wake everything back up.
    if line == b"USLEEP" {
        if state.connected {
            state.connected = false;
            // Disable the RADIO peripheral to save power.
            pac::RADIO.tasks_disable().write_value(1);
            // Persist counter before sleeping (in case we don't wake for a while).
            if state.paired {
                storage::save(flash, state.active_slot, &state.current_profile());
            }
        }
        let _ = sink.write(b"SLEEP\r\n").await;
        return;
    }

    if line == b"USTATUS" {
        let mut out = [0u8; 96];
        let n = format_status(&mut out, state);
        let _ = sink.write(&out[..n]).await;
        return;
    }

    if line == b"VER" {
        let _ = sink.write(VERSION_REPLY).await;
        return;
    }

    if line == b"UDELETE" || line.starts_with(b"UDELETE ") {
        // UDELETE [slot] — erase a specific slot, or all if no arg.
        if line.len() > 8 {
            match parse_u8_hex(&line[8..]) {
                Some(s) if s < MAX_PROFILES => {
                    storage::clear_slot(flash, s);
                    if state.active_slot == s {
                        state.paired = false;
                        state.connected = false;
                    }
                    let _ = sink.write(b"DELETED\r\n").await;
                }
                _ => {
                    let _ = sink.write(b"ERR SLOT\r\n").await;
                }
            }
        } else {
            storage::clear_all(flash);
            state.paired = false;
            state.connected = false;
            let _ = sink.write(b"DELETED ALL\r\n").await;
        }
        return;
    }

    // USWITCH <id> — switch active profile slot (0..3), load + reconnect.
    if let Some(rest) = line.strip_prefix(b"USWITCH ") {
        match parse_u8_hex(rest) {
            Some(slot) if slot < MAX_PROFILES => {
                let has_profile = state.switch_to(flash, slot);
                if has_profile {
                    // Don't block on connect(): just configure the radio and
                    // set connected=true. The background keep-alive tick (every
                    // 8ms) will find the right channel within ~200ms via the
                    // protocol's built-in channel hopping on transmit failure.
                    // This keeps USWITCH instant for the KVM event loop.
                    if state.device.configure_radio().is_ok() {
                        state.connected = true;
                    } else {
                        let _ = sink.write(b"ERR RADIO\r\n").await;
                        return;
                    }
                    let mut out = [0u8; 48];
                    let n = format_switched(&mut out, slot, true);
                    let _ = sink.write(&out[..n]).await;
                } else {
                    let mut out = [0u8; 48];
                    let n = format_switched_empty(&mut out, slot);
                    let _ = sink.write(&out[..n]).await;
                }
            }
            _ => {
                let _ = sink.write(b"ERR SLOT\r\n").await;
            }
        }
        return;
    }

    // UKEY <mod-hex> [keycode-hex ...up to 6]
    // Sends one raw HID keyboard report (press) followed by an all-zero report
    // (release). Lets the host send function keys, Ctrl/Alt combos, etc.
    // Example: UKEY 05 4C  -> Ctrl(01)+Alt(04) + Delete(0x4C)  => Ctrl+Alt+Del
    if let Some(rest) = line.strip_prefix(b"UKEY ") {
        if !state.connected {
            let _ = sink.write(b"ERR NOTCONN\r\n").await;
            return;
        }
        state.last_activity = Instant::now();
        state.device.timeout = 20;
        let mut tokens = rest.split(|&b| b == b' ').filter(|t| !t.is_empty());
        let modifier = match tokens.next().and_then(parse_u8_hex) {
            Some(m) => m,
            None => {
                let _ = sink.write(b"ERR ARG\r\n").await;
                return;
            }
        };
        let mut keys = [0u8; KEYS_LEN];
        let mut count = 0usize;
        let mut bad = false;
        for tok in tokens {
            if count >= KEYS_LEN {
                bad = true;
                break;
            }
            match parse_u8_hex(tok) {
                Some(k) => {
                    keys[count] = k;
                    count += 1;
                }
                None => {
                    bad = true;
                    break;
                }
            }
        }
        if bad {
            let _ = sink.write(b"ERR ARG\r\n").await;
            return;
        }
        for _ in 0..5 {
            let _ = state.device.tick();
        }
        let ok = send_key_report(&mut state.device, &keys, modifier);
        if state.paired {
            storage::save(flash, state.active_slot, &state.current_profile());
        }
        let _ = sink.write(if ok { b"OK\r\n" } else { b"ERR SEND\r\n" }).await;
        return;
    }

    // UKEYDOWN <mod-hex> [keycode-hex ...up to 6]
    // Like UKEY but sends ONLY the press report (no auto-release). Used by the
    // KVM forwarder which sends state-based reports: each UKEYDOWN is the full
    // set of currently-held keys, and a final `UKEYDOWN 00` acts as all-up.
    if let Some(rest) = line.strip_prefix(b"UKEYDOWN ") {
        if !state.connected {
            let _ = sink.write(b"ERR NOTCONN\r\n").await;
            return;
        }
        state.last_activity = Instant::now();
        state.device.timeout = 20;
        let mut tokens = rest.split(|&b| b == b' ').filter(|t| !t.is_empty());
        let modifier = match tokens.next().and_then(parse_u8_hex) {
            Some(m) => m,
            None => {
                let _ = sink.write(b"ERR ARG\r\n").await;
                return;
            }
        };
        let mut keys = [0u8; KEYS_LEN];
        let mut count = 0usize;
        let mut bad = false;
        for tok in tokens {
            if count >= KEYS_LEN {
                bad = true;
                break;
            }
            match parse_u8_hex(tok) {
                Some(k) => {
                    keys[count] = k;
                    count += 1;
                }
                None => {
                    bad = true;
                    break;
                }
            }
        }
        if bad {
            let _ = sink.write(b"ERR ARG\r\n").await;
            return;
        }
        // Send only the press — no release. The KVM sends the next state
        // (or UKEYDOWN 00) when keys change.
        let ok = send_report(state, &keys, modifier);
        // Periodic save: every 128 keystrokes persist the counter to limit
        // the gap on unexpected power loss (startup +512 covers the rest).
        if state.paired && state.device.aes_counter & 0x7F == 0 {
            storage::save(flash, state.active_slot, &state.current_profile());
        }
        let _ = sink.write(if ok { b"OK\r\n" } else { b"ERR SEND\r\n" }).await;
        return;
    }

    if let Some(text) = line.strip_prefix(b"UTYPE ") {
        if !state.connected {
            let _ = sink.write(b"ERR NOTCONN\r\n").await;
            return;
        }
        state.last_activity = Instant::now();
        state.device.timeout = 20;
        // Stabilize the link with a few keep-alives first.
        for _ in 0..5 {
            let _ = state.device.tick();
        }
        let mut ok_count: u32 = 0;
        let mut total: u32 = 0;
        for &c in text {
            if char_to_hid(c).is_some() {
                total += 1;
                if type_char(&mut state.device, c) {
                    ok_count += 1;
                }
            }
            // Inter-key delay: receiver/OS HID stack needs time to process
            // each press+release pair. 15ms matches the proven Pi CLI timing.
            Timer::after(Duration::from_millis(15)).await;
        }
        let mut out = [0u8; 48];
        let n = format_typed(&mut out, ok_count, total);
        let _ = sink.write(&out[..n]).await;
        // The AES counter advanced with each keystroke. Persist it so a reset
        // resumes from a fresh value the receiver hasn't seen (replay defense).
        if state.paired {
            storage::save(flash, state.active_slot, &state.current_profile());
        }
        return;
    }

    let _ = sink.write(b"UCMDS: VER UPAIR [slot] UCONNECT UTYPE <text> UKEY <mod> [keys] UKEYDOWN <mod> [keys] USWITCH <slot> USLEEP UKEEPALIVE USTATUS UDELETE [slot]\r\n",)
    .await;
}

/// Send a single character as a key press followed by a release.
/// Returns true if the press keystroke was acknowledged by the receiver.
fn type_char(device: &mut Device, c: u8) -> bool {
    let (scancode, shift) = match char_to_hid(c) {
        Some(v) => v,
        None => return false,
    };
    let modifiers = if shift { 0x02 } else { 0x00 };
    let mut keys = [0u8; KEYS_LEN];
    keys[0] = scancode;

    let pressed = press_key(device, &keys, modifiers);
    // Hold the key for ~10ms before releasing. The receiver/OS needs time to
    // register the press before seeing the release. 64MHz * 10ms = 640k cycles.
    cortex_m::asm::delay(640_000);
    release_keys(device);
    pressed
}

/// Send one raw HID keyboard report (press) then an all-zero report (release).
/// `keys` holds up to 6 USB HID usage IDs; `modifier` is the HID modifier
/// bitmask (bit0=LCtrl, bit1=LShift, bit2=LAlt, bit3=LGui, ...). Returns true
/// if the press was acknowledged by the receiver.
///
/// The radio worker maintains the link independently of either transport.
fn send_key_report(device: &mut Device, keys: &[u8; KEYS_LEN], modifier: u8) -> bool {
    let pressed = press_key(device, keys, modifier);
    cortex_m::asm::delay(640_000); // ~10ms hold before release
    release_keys(device);
    pressed
}

/// Bounded KVM key-report send driven by the link state. Up: up to three channel attempts
/// (each 16 tries with ≤1.5ms gaps, ~35ms; ~110ms worst case). Searching: one probe (~35ms);
/// the 8ms keep-alive keeps hunting between UART commands.
/// Either way the UART reply never waits on a long channel scan, so the RP can abandon a request
/// without a hidden backlog building up behind it.
fn send_report(state: &mut UnifyingState, keys: &[u8; KEYS_LEN], modifier: u8) -> bool {
    let attempts = if state.link_up { 3 } else { 1 };
    for _ in 0..attempts {
        // Each failure already advances the channel inside the protocol layer.
        if state.device.send_encrypted_keystroke(keys, modifier).is_ok() {
            state.link_up = true;
            return true;
        }
    }
    state.link_up = false;
    false
}

/// Transmit a key-press report, retrying across channels. Returns true on ACK.
fn press_key(device: &mut Device, keys: &[u8; KEYS_LEN], modifier: u8) -> bool {
    for _ in 0..20 {
        if device.send_encrypted_keystroke(keys, modifier).is_ok() {
            return true;
        }
        let _ = device.tick();
    }
    false
}

/// Transmit the all-keys-up release report (best effort).
fn release_keys(device: &mut Device) {
    let release = [0u8; KEYS_LEN];
    for _ in 0..20 {
        if device.send_encrypted_keystroke(&release, 0x00).is_ok() {
            break;
        }
        let _ = device.tick();
    }
}

/// Parse an ASCII hex byte token ("0F", "4c", "5") into a u8.
fn parse_u8_hex(tok: &[u8]) -> Option<u8> {
    if tok.is_empty() || tok.len() > 2 {
        return None;
    }
    let mut value = 0u8;
    for &c in tok {
        let nibble = match c {
            b'0'..=b'9' => c - b'0',
            b'a'..=b'f' => c - b'a' + 10,
            b'A'..=b'F' => c - b'A' + 10,
            _ => return None,
        };
        value = (value << 4) | nibble;
    }
    Some(value)
}

fn format_typed(out: &mut [u8], ok: u32, total: u32) -> usize {
    let mut pos = 0;
    for &b in b"TYPED " {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, ok);
    out[pos] = b'/';
    pos += 1;
    pos = write_u32(out, pos, total);
    out[pos] = b'\r';
    out[pos + 1] = b'\n';
    pos + 2
}

fn hex_byte(out: &mut [u8], idx: usize, b: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out[idx] = HEX[(b >> 4) as usize];
    out[idx + 1] = HEX[(b & 0x0F) as usize];
}

fn write_u32(out: &mut [u8], mut pos: usize, mut value: u32) -> usize {
    if value == 0 {
        out[pos] = b'0';
        return pos + 1;
    }
    let mut digits = [0u8; 10];
    let mut count = 0;
    while value > 0 {
        digits[count] = b'0' + (value % 10) as u8;
        value /= 10;
        count += 1;
    }
    while count > 0 {
        count -= 1;
        out[pos] = digits[count];
        pos += 1;
    }
    pos
}

fn format_paired(out: &mut [u8], address: &[u8; ADDRESS_LEN], channel: u8) -> usize {
    let mut pos = 0;
    for &b in b"PAIRED " {
        out[pos] = b;
        pos += 1;
    }
    for &byte in address.iter() {
        hex_byte(out, pos, byte);
        pos += 2;
    }
    for &b in b" CH=" {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, channel as u32);
    out[pos] = b'\r';
    out[pos + 1] = b'\n';
    pos + 2
}

fn format_connected(out: &mut [u8], channel: u8) -> usize {
    let mut pos = 0;
    for &b in b"CONNECTED CH=" {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, channel as u32);
    out[pos] = b'\r';
    out[pos + 1] = b'\n';
    pos + 2
}

fn format_radiotest(out: &mut [u8], encoding: u8, acks: u32) -> usize {
    let mut pos = 0;
    for &b in b"RADIOTEST ENC=" {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, encoding as u32);
    for &b in b" ACKS=" {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, acks);
    out[pos] = b'\r';
    out[pos + 1] = b'\n';
    pos + 2
}

fn format_status(out: &mut [u8], state: &UnifyingState) -> usize {
    let mut pos = 0;
    for &b in b"STATUS SLOT=" {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, state.active_slot as u32);
    for &b in b" PAIRED=" {
        out[pos] = b;
        pos += 1;
    }
    out[pos] = if state.paired { b'1' } else { b'0' };
    pos += 1;
    for &b in b" CONN=" {
        out[pos] = b;
        pos += 1;
    }
    out[pos] = if state.connected { b'1' } else { b'0' };
    pos += 1;
    for &b in b" CH=" {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, state.device.channel as u32);
    for &b in b" CNT=" {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, state.device.aes_counter);
    out[pos] = b'\r';
    out[pos + 1] = b'\n';
    pos + 2
}

fn format_switched(out: &mut [u8], slot: u8, connected: bool) -> usize {
    let mut pos = 0;
    for &b in b"SWITCHED " {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, slot as u32);
    if connected {
        for &b in b" CONNECTED" {
            out[pos] = b;
            pos += 1;
        }
    } else {
        for &b in b" NOCONN" {
            out[pos] = b;
            pos += 1;
        }
    }
    out[pos] = b'\r';
    out[pos + 1] = b'\n';
    pos + 2
}

fn format_switched_empty(out: &mut [u8], slot: u8) -> usize {
    let mut pos = 0;
    for &b in b"SWITCHED " {
        out[pos] = b;
        pos += 1;
    }
    pos = write_u32(out, pos, slot as u32);
    for &b in b" EMPTY" {
        out[pos] = b;
        pos += 1;
    }
    out[pos] = b'\r';
    out[pos + 1] = b'\n';
    pos + 2
}

async fn write_reply<'d, V: VbusDetect + 'd>(
    class: &mut Sender<'d, Driver<'d, V>>,
    data: &[u8],
) -> Result<(), Disconnected> {
    let mut offset = 0usize;
    while offset < data.len() {
        let end = (offset + 64).min(data.len());
        class.write_packet(&data[offset..end]).await?;
        offset = end;
    }
    if !data.is_empty() && data.len() % 64 == 0 {
        class.write_packet(&[]).await?;
    }
    Ok(())
}
