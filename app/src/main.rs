#![no_std]
#![no_main]

mod esb_radio;
mod keymap;
mod storage;
mod unifying_hal;

use core::{cell::RefCell, str};

use cortex_m::peripheral::SCB;
use embassy_boot::State as BootState;
use embassy_boot_nrf::{BlockingFirmwareUpdater, FirmwareUpdaterConfig};
use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{select, Either};
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::usb::vbus_detect::{HardwareVbusDetect, VbusDetect};
use embassy_nrf::usb::Driver;
use embassy_nrf::{bind_interrupts, pac, peripherals, usb};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::{Duration, Timer};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
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
use storage::{Profile, SharedFlash};
use unifying_hal::{EmbassyClock, SwAesCtr};

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

const ACTIVE_IMAGE_LIMIT: usize = 480 * 1024;
const READY_BANNER: &[u8] =
    concat!("nrf-unifying ready build=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_BOOT: &[u8] = concat!("STATE BOOT BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_SWAP: &[u8] = concat!("STATE SWAP BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_REVERT: &[u8] = concat!("STATE REVERT BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_DFU: &[u8] = concat!("STATE DFU BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();
const INFO_ERR: &[u8] = concat!("STATE ERR BUILD=", env!("BUILD_TIME"), "\r\n").as_bytes();

/// Concrete `UnifyingDevice` for this board.
type Device = UnifyingDevice<EsbRadio, EmbassyClock, SwAesCtr>;

/// Persistent Unifying connection state held across CDC commands.
struct UnifyingState {
    device: Device,
    paired: bool,
    connected: bool,
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
        }
    }

    /// Restore a persisted pairing into the in-RAM device state.
    fn apply_profile(&mut self, p: &Profile) {
        self.device.address = p.address;
        self.device.aes_key = p.aes_key;
        self.device.aes_counter = p.aes_counter;
        self.device.channel = p.channel;
        self.paired = true;
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

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());

    pac::CLOCK.tasks_hfclkstart().write_value(1);
    while pac::CLOCK.events_hfclkstarted().read() != 1 {}

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

    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );
    let mut class = CdcAcmClass::new(&mut builder, &mut cdc_state, 64);
    let mut usb = builder.build();

    let updater_config = FirmwareUpdaterConfig::from_linkerfile_blocking(&flash, &flash);
    let mut aligned = [0u8; 4];
    let mut updater = BlockingFirmwareUpdater::new(updater_config, &mut aligned);

    if matches!(updater.get_state(), Ok(BootState::Swap | BootState::Revert)) {
        let _ = updater.mark_booted();
    }

    let mut unifying = UnifyingState::new();
    if let Some(profile) = storage::load(&flash) {
        unifying.apply_profile(&profile);
    }

    let usb_fut = usb.run();
    let serial_fut = async {
        let mut packet = [0u8; 64];
        let mut line = [0u8; 256];
        let mut staging = [0u8; 68];

        loop {
            class.wait_connection().await;
            let _ = write_reply(&mut class, READY_BANNER).await;

            let mut line_len = 0usize;
            let mut upload_active = false;
            let mut upload_size = 0usize;
            let mut upload_received = 0usize;
            let mut image_ready = false;
            let mut staging_len = 0usize;

            loop {
                let n = if unifying.connected && !upload_active {
                    // Race the next host command against a keep-alive tick.
                    // Unifying keeps the link alive with periodic keep-alive
                    // packets (like a real keyboard); without them the receiver
                    // drops channel sync within a couple of seconds and the next
                    // keystroke fails. tick() only actually transmits when the
                    // keep-alive interval has elapsed, so an 8ms poll mirrors the
                    // real keyboard cadence cheaply (most ticks are no-ops).
                    match select(
                        class.read_packet(&mut packet),
                        Timer::after(Duration::from_millis(8)),
                    )
                    .await
                    {
                        Either::First(Ok(n)) => n,
                        Either::First(Err(_)) => break,
                        Either::Second(_) => {
                            let _ = unifying.device.tick();
                            continue;
                        }
                    }
                } else {
                    match class.read_packet(&mut packet).await {
                        Ok(n) => n,
                        Err(_) => break,
                    }
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
                            upload_size = 0;
                            upload_received = 0;
                            image_ready = false;
                            staging_len = 0;
                            let _ = write_reply(&mut class, b"ERR WRITE\r\n").await;
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
                                upload_size = 0;
                                upload_received = 0;
                                image_ready = false;
                                staging_len = 0;
                                let _ = write_reply(&mut class, b"ERR WRITE\r\n").await;
                                continue;
                            }
                            upload_received += staging_len;
                            staging_len = 0;
                        }

                        upload_active = false;
                        image_ready = true;
                        let _ = write_reply(&mut class, b"OK\r\n").await;
                    }

                    continue;
                }

                for &b in &packet[..n] {
                    if b == b'\n' {
                        let consumed_line_len = line_len;
                        line_len = 0;

                        // OTA commands first (kept byte-for-byte compatible).
                        match parse_ota_command(&line[..consumed_line_len]) {
                            Some(OtaCommand::Ping) => {
                                let _ = write_reply(&mut class, b"PONG\r\n").await;
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
                                let _ = write_reply(&mut class, resp).await;
                                continue;
                            }
                            Some(OtaCommand::Write(size)) => {
                                if size == 0 || size > ACTIVE_IMAGE_LIMIT {
                                    let _ = write_reply(&mut class, b"ERR SIZE\r\n").await;
                                } else {
                                    upload_active = true;
                                    upload_size = size;
                                    upload_received = 0;
                                    staging_len = 0;
                                    image_ready = false;
                                    let _ = write_reply(&mut class, b"READY\r\n").await;
                                }
                                continue;
                            }
                            Some(OtaCommand::Boot) => {
                                if !image_ready {
                                    let _ = write_reply(&mut class, b"ERR NOIMAGE\r\n").await;
                                } else if updater.mark_updated().is_err() {
                                    let _ = write_reply(&mut class, b"ERR MARK\r\n").await;
                                } else {
                                    let _ = write_reply(&mut class, b"REBOOT\r\n").await;
                                    Timer::after(Duration::from_millis(100)).await;
                                    SCB::sys_reset();
                                }
                                continue;
                            }
                            Some(OtaCommand::Abort) => {
                                upload_active = false;
                                image_ready = false;
                                staging_len = 0;
                                let _ = write_reply(&mut class, b"ABORTED\r\n").await;
                                continue;
                            }
                            Some(OtaCommand::Reboot) => {
                                let _ = write_reply(&mut class, b"REBOOT\r\n").await;
                                Timer::after(Duration::from_millis(100)).await;
                                SCB::sys_reset();
                            }
                            None => {}
                        }

                        // Otherwise, handle Unifying commands.
                        handle_unifying_command(
                            &mut class,
                            &mut unifying,
                            &flash,
                            &line[..consumed_line_len],
                        )
                        .await;
                    } else if b != b'\r' && line_len < line.len() {
                        line[line_len] = b;
                        line_len += 1;
                    }
                }
            }
        }
    };

    join(usb_fut, serial_fut).await;
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
async fn handle_unifying_command<'d, V: VbusDetect + 'd>(
    class: &mut CdcAcmClass<'d, Driver<'d, V>>,
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
            let _ = write_reply(class, b"ERR RADIO\r\n").await;
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
            let _ = write_reply(class, &out[..n]).await;
        }
        let _ = write_reply(class, b"RADIOTEST DONE\r\n").await;
        return;
    }

    if line == b"UPAIR" {
        let params = PairingParams {
            id: (embassy_time::Instant::now().as_ticks() as u8) | 0x01,
            product_id: 0x1025,
            device_type: 0x0147,
            crypto: embassy_time::Instant::now().as_ticks() as u32,
            serial: 0xA580_94B6,
            capabilities: 0x1E40,
            name: b"RustKbd",
        };

        if state.device.configure_radio().is_err() {
            let _ = write_reply(class, b"ERR RADIO\r\n").await;
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
            // Persist the new pairing (address, AES key, channel, counter) so
            // it survives resets and OTA updates.
            storage::save(flash, &state.current_profile());
            let mut out = [0u8; 64];
            let n = format_paired(&mut out, &state.device.address, state.device.channel);
            let _ = write_reply(class, &out[..n]).await;
        } else {
            let _ = write_reply(class, b"ERR PAIR\r\n").await;
        }
        return;
    }

    if line == b"UCONNECT" {
        if !state.paired {
            let _ = write_reply(class, b"ERR NOTPAIRED\r\n").await;
            return;
        }
        if state.device.configure_radio().is_err() {
            let _ = write_reply(class, b"ERR RADIO\r\n").await;
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
            let mut out = [0u8; 32];
            let n = format_connected(&mut out, state.device.channel);
            let _ = write_reply(class, &out[..n]).await;
        } else {
            let _ = write_reply(class, b"ERR CONNECT\r\n").await;
        }
        return;
    }

    if line == b"UKEEPALIVE" {
        if !state.connected {
            let _ = write_reply(class, b"ERR NOTCONN\r\n").await;
            return;
        }
        let _ = state.device.tick();
        let _ = write_reply(class, b"TICK\r\n").await;
        return;
    }

    if line == b"USTATUS" {
        let mut out = [0u8; 96];
        let n = format_status(&mut out, state);
        let _ = write_reply(class, &out[..n]).await;
        return;
    }

    if line == b"VER" {
        let _ = write_reply(
            class,
            concat!("VER unifying/1 build=", env!("BUILD_TIME"), "\r\n").as_bytes(),
        )
        .await;
        return;
    }

    if line == b"UDELETE" {
        storage::clear(flash);
        state.paired = false;
        state.connected = false;
        let _ = write_reply(class, b"DELETED\r\n").await;
        return;
    }

    // UKEY <mod-hex> [keycode-hex ...up to 6]
    // Sends one raw HID keyboard report (press) followed by an all-zero report
    // (release). Lets the host send function keys, Ctrl/Alt combos, etc.
    // Example: UKEY 05 4C  -> Ctrl(01)+Alt(04) + Delete(0x4C)  => Ctrl+Alt+Del
    if let Some(rest) = line.strip_prefix(b"UKEY ") {
        if !state.connected {
            let _ = write_reply(class, b"ERR NOTCONN\r\n").await;
            return;
        }
        let mut tokens = rest.split(|&b| b == b' ').filter(|t| !t.is_empty());
        let modifier = match tokens.next().and_then(parse_u8_hex) {
            Some(m) => m,
            None => {
                let _ = write_reply(class, b"ERR ARG\r\n").await;
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
            let _ = write_reply(class, b"ERR ARG\r\n").await;
            return;
        }
        for _ in 0..5 {
            let _ = state.device.tick();
        }
        let ok = send_key_report(&mut state.device, &keys, modifier);
        if state.paired {
            storage::save(flash, &state.current_profile());
        }
        let _ = write_reply(class, if ok { b"OK\r\n" } else { b"ERR SEND\r\n" }).await;
        return;
    }

    if let Some(text) = line.strip_prefix(b"UTYPE ") {
        if !state.connected {
            let _ = write_reply(class, b"ERR NOTCONN\r\n").await;
            return;
        }
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
            // Yield so the USB task can service the host between characters.
            Timer::after(Duration::from_millis(1)).await;
        }
        let mut out = [0u8; 48];
        let n = format_typed(&mut out, ok_count, total);
        let _ = write_reply(class, &out[..n]).await;
        // The AES counter advanced with each keystroke. Persist it so a reset
        // resumes from a fresh value the receiver hasn't seen (replay defense).
        if state.paired {
            storage::save(flash, &state.current_profile());
        }
        return;
    }

    let _ = write_reply(
        class,
        b"UCMDS: VER UPAIR UCONNECT UTYPE <text> UKEY <mod> [keys] UKEEPALIVE USTATUS UDELETE\r\n",
    )
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
    release_keys(device);
    pressed
}

/// Send one raw HID keyboard report (press) then an all-zero report (release).
/// `keys` holds up to 6 USB HID usage IDs; `modifier` is the HID modifier
/// bitmask (bit0=LCtrl, bit1=LShift, bit2=LAlt, bit3=LGui, ...). Returns true
/// if the press was acknowledged by the receiver.
///
/// The link is kept fresh by the background keep-alive woven into the serial
/// loop, so a single send attempt is normally enough.
fn send_key_report(device: &mut Device, keys: &[u8; KEYS_LEN], modifier: u8) -> bool {
    let pressed = press_key(device, keys, modifier);
    release_keys(device);
    pressed
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
    for &b in b"STATUS PAIRED=" {        out[pos] = b;
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

async fn write_reply<'d, V: VbusDetect + 'd>(
    class: &mut CdcAcmClass<'d, Driver<'d, V>>,
    data: &[u8],
) -> Result<(), Disconnected> {
    let mut offset = 0usize;
    while offset < data.len() {
        let end = (offset + 64).min(data.len());
        class.write_packet(&data[offset..end]).await?;
        offset = end;
    }
    Ok(())
}
