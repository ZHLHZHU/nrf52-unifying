//! Native nRF52840 RADIO driver implementing `UnifyingRadio`.
//!
//! The Logitech Unifying protocol rides on Nordic Enhanced ShockBurst (ESB),
//! the same air protocol used by the nRF24L01+. The nRF52840 has an on-chip
//! 2.4 GHz RADIO that can be configured to be byte-for-byte compatible with an
//! nRF24L01+ transmitter, so we can drop the external SPI radio entirely and
//! talk to the Logitech receiver directly.
//!
//! This is a blocking Primary-Transmitter (PTX) implementation:
//!   1. transmit a packet,
//!   2. hardware-shortcut turnaround into RX to catch the auto-ACK,
//!   3. if no ACK arrives in time, retransmit (software auto-retransmit).
//!
//! The ACK frame may carry an ACK payload (the receiver's response), which we
//! surface through `payload_available` / `receive_payload`.
//!
//! Register configuration mirrors the proven `esb` crate
//! (https://github.com/thalesfragoso/esb) which is known to interoperate with
//! real nRF24L01+ peripherals.

use cortex_m::peripheral::DWT;
use embassy_nrf::pac;
use embassy_nrf::pac::radio::vals;
use rust_unifying::constants::{ADDRESS_LEN, MAX_PAYLOAD_LEN};
use rust_unifying::radio::UnifyingRadio;

/// nRF24L01+ compatible CRC-16 configuration.
const CRC_INIT: u32 = 0x0000_FFFF;
const CRC_POLY: u32 = 0x0001_1021;

/// DMA scratch buffers. Layout expected by the RADIO with LFLEN=6 / S1LEN=3:
/// `[LENGTH, S1(pid_no_ack), payload...]`. The RADIO DMA reads/writes starting
/// at the LENGTH byte.
const DMA_HEADER: usize = 2;
const DMA_BUF_LEN: usize = DMA_HEADER + MAX_PAYLOAD_LEN;

/// How long (microseconds) to wait for an ACK frame after a transmission
/// before declaring the attempt failed. The receiver turns the packet around
/// in roughly 130 us; a 600 us window comfortably covers turnaround plus a
/// full 32-byte ACK payload at 2 Mbit/s.
const ACK_WAIT_US: u32 = 600;

/// The native RADIO never fails at the register level, so we expose a
/// zero-sized error type to satisfy the `UnifyingRadio` trait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EsbError;

pub struct EsbRadio {
    tx_buf: [u8; DMA_BUF_LEN],
    rx_buf: [u8; DMA_BUF_LEN],
    /// 2-bit packet id, incremented for every *new* (non-retransmitted) packet.
    pid: u8,
    /// Auto-retransmit count (number of *re*transmissions after the first try).
    retransmit_count: u8,
    /// Auto-retransmit delay in units of 250 us (matching the nRF24L01+ ARD).
    retransmit_delay: u8,
    /// Length of the last received ACK payload (0 = none pending).
    rx_len: u8,
    /// CPU cycles per microsecond, derived from the core clock (64 MHz).
    cycles_per_us: u32,
    /// Address-encoding variant. Selects how a 5-byte Unifying address maps to
    /// the nRF52 BASE0/PREFIX0 registers. Defaults to the best-known mapping;
    /// the radio self-test (`URADIOTEST`) sweeps the alternatives.
    addr_encoding: u8,
}

impl EsbRadio {
    /// Create the driver and enable the DWT cycle counter used for
    /// microsecond-accurate ACK timing. The DWT/DCB units are not used by
    /// embassy, so stealing them here is safe.
    pub fn new() -> Self {
        let mut core = unsafe { cortex_m::Peripherals::steal() };
        core.DCB.enable_trace();
        core.DWT.enable_cycle_counter();
        Self {
            tx_buf: [0u8; DMA_BUF_LEN],
            rx_buf: [0u8; DMA_BUF_LEN],
            pid: 0,
            retransmit_count: 15,
            retransmit_delay: 15,
            rx_len: 0,
            cycles_per_us: 64, // 64 MHz HFCLK
            // Encoding 1 is the variant empirically confirmed (via URADIOTEST)
            // to elicit ACKs from a real Logitech Unifying receiver.
            addr_encoding: 1,
        }
    }

    /// Select the address-encoding variant used by `set_address`.
    pub fn set_addr_encoding(&mut self, encoding: u8) {
        self.addr_encoding = encoding;
    }

    /// Number of address-encoding variants the self-test should sweep.
    pub const ADDR_ENCODINGS: u8 = 4;

    /// One blocking transmit attempt of `payload` on the current channel/address.
    /// Returns true if a CRC-valid ACK frame was received. Used by the radio
    /// self-test to probe encodings without the full protocol stack.
    pub fn probe_once(&mut self, payload: &[u8]) -> bool {
        let len = payload.len().min(MAX_PAYLOAD_LEN);
        self.pid = (self.pid + 1) & 0x03;
        self.tx_buf[0] = len as u8;
        self.tx_buf[1] = (self.pid << 1) | 0x01;
        self.tx_buf[DMA_HEADER..DMA_HEADER + len].copy_from_slice(&payload[..len]);
        self.rx_len = 0;
        self.tx_once()
    }

    #[inline]
    fn radio() -> pac::radio::Radio {
        pac::RADIO
    }

    fn delay_us(&self, us: u32) {
        let start = DWT::cycle_count();
        let target = us.wrapping_mul(self.cycles_per_us);
        while DWT::cycle_count().wrapping_sub(start) < target {}
    }

    /// Fully disable the radio and wait for it to settle in DISABLED state.
    fn disable(&self) {
        let r = Self::radio();
        r.shorts().write(|w| w.0 = 0);
        r.events_disabled().write_value(0);
        r.tasks_disable().write_value(1);
        while r.events_disabled().read() == 0 {}
        r.events_disabled().write_value(0);
    }

    /// Transmit the current `tx_buf` once and listen for the ACK.
    /// Returns true if a CRC-valid ACK frame was received.
    ///
    /// The sequence is fully sequential (no hardware turnaround shortcut) so
    /// there is no race re-pointing the DMA buffer:
    ///   1. TX: txen -> READY -(short)-> START -> [send] -> END -(short)-> DISABLE
    ///   2. wait for DISABLED
    ///   3. RX: rxen -> READY -(short)-> START -> [recv ACK] -> END -(short)-> DISABLE
    ///   4. wait for END (ACK) or time out
    ///
    /// The receiver (PRX) turns the packet around and sends its ACK roughly
    /// 130 us after our packet ends; fast ramp-up (~40 us) leaves us listening
    /// well before then.
    fn tx_once(&mut self) -> bool {
        let r = Self::radio();

        // PTX always uses logical pipe/address 0.
        r.txaddress().write(|w| w.set_txaddress(0));
        r.rxaddresses().write(|w| w.0 = 1 << 0);

        // --- Transmit ---
        r.events_ready().write_value(0);
        r.events_end().write_value(0);
        r.events_disabled().write_value(0);
        r.packetptr().write_value(self.tx_buf.as_ptr() as u32);
        r.shorts().write(|w| {
            w.set_ready_start(true);
            w.set_end_disable(true);
        });
        cortex_m::asm::dsb();
        r.tasks_txen().write_value(1);

        // Wait for TX to finish and the radio to disable.
        while r.events_disabled().read() == 0 {}
        r.events_disabled().write_value(0);
        r.events_end().write_value(0);
        cortex_m::asm::dsb();

        // --- Receive the ACK ---
        r.packetptr().write_value(self.rx_buf.as_ptr() as u32);
        r.shorts().write(|w| {
            w.set_ready_start(true);
            w.set_end_disable(true);
        });
        cortex_m::asm::dsb();
        r.tasks_rxen().write_value(1);

        // Wait for the ACK (END) or time out.
        let start = DWT::cycle_count();
        let budget = ACK_WAIT_US.wrapping_mul(self.cycles_per_us);
        let mut got_end = false;
        while DWT::cycle_count().wrapping_sub(start) < budget {
            if r.events_end().read() != 0 {
                got_end = true;
                break;
            }
        }

        if !got_end {
            // No ACK in time. Force the radio back to a known DISABLED state.
            self.disable();
            return false;
        }
        // END fired; the end_disable short returns us to DISABLED.
        while r.events_disabled().read() == 0 {}
        r.events_end().write_value(0);
        r.events_disabled().write_value(0);
        cortex_m::asm::dsb();

        // Validate CRC.
        let crc_ok = r.crcstatus().read().crcstatus() == vals::Crcstatus::CRCOK;
        if !crc_ok {
            self.rx_len = 0;
            return false;
        }

        // Capture any ACK payload. rx_buf[0] holds the on-air LENGTH byte.
        let len = self.rx_buf[0] as usize;
        if len > 0 && len <= MAX_PAYLOAD_LEN {
            self.rx_len = len as u8;
        } else {
            self.rx_len = 0;
        }
        true
    }
}

impl UnifyingRadio for EsbRadio {
    type Error = EsbError;

    fn configure_unifying(&mut self) -> Result<(), Self::Error> {
        let r = Self::radio();

        // Power-cycle the peripheral to a clean state.
        r.power().write(|w| w.set_power(false));
        r.power().write(|w| w.set_power(true));

        r.intenclr().write(|w| w.0 = 0xFFFF_FFFF);

        // 2 Mbit/s Nordic proprietary mode (Unifying runs at 2 Mbps).
        r.mode().write(|w| w.set_mode(vals::Mode::NRF_2MBIT));

        // Fast ramp-up keeps the TX->RX turnaround inside the ACK window.
        r.modecnf0().write(|w| {
            w.set_ru(vals::Ru::FAST);
            w.set_dtx(vals::Dtx::CENTER);
        });

        r.txpower().write(|w| w.set_txpower(vals::Txpower::POS8_DBM));

        // Packet layout: 6-bit length field + 3-bit S1 (PID + NO_ACK), no S0.
        // Dynamic payload length, big-endian, no whitening: nRF24L01+ ESB.
        r.pcnf0().write(|w| {
            w.set_lflen(6);
            w.set_s0len(false);
            w.set_s1len(3);
        });
        r.pcnf1().write(|w| {
            w.set_maxlen(MAX_PAYLOAD_LEN as u8);
            w.set_statlen(0);
            w.set_balen(4); // 4-byte base + 1-byte prefix = 5-byte address
            w.set_endian(vals::Endian::BIG);
            w.set_whiteen(false);
        });

        // CRC-16, computed over the address as well (skipaddr = INCLUDE).
        r.crccnf().write(|w| {
            w.set_len(vals::Len::TWO);
            w.set_skipaddr(vals::Skipaddr::INCLUDE);
        });
        r.crcinit().write(|w| w.0 = CRC_INIT & 0x00FF_FFFF);
        r.crcpoly().write(|w| w.0 = CRC_POLY & 0x00FF_FFFF);

        self.rx_len = 0;
        Ok(())
    }

    fn transmit_payload(&mut self, payload: &[u8]) -> Result<bool, Self::Error> {
        let len = payload.len().min(MAX_PAYLOAD_LEN);

        // Advance PID for this new packet (2-bit wrap).
        self.pid = (self.pid + 1) & 0x03;

        // Build the DMA buffer: [LENGTH, S1, payload...].
        // S1 = (pid << 1) | ack_bit. We always request an ACK, matching the
        // `esb` crate's encoding (no_ack=false sets bit 0).
        self.tx_buf[0] = len as u8;
        self.tx_buf[1] = (self.pid << 1) | 0x01;
        self.tx_buf[DMA_HEADER..DMA_HEADER + len].copy_from_slice(&payload[..len]);

        self.rx_len = 0;

        // First attempt plus `retransmit_count` retries, same PID throughout.
        let attempts = self.retransmit_count as u32 + 1;
        for attempt in 0..attempts {
            if self.tx_once() {
                return Ok(true);
            }
            if attempt + 1 < attempts {
                // ARD is in 250 us units on the nRF24L01+. Cap the gap so a
                // failing transmit (receiver out of range) can't starve the
                // shared USB task for too long; the protocol layer hops
                // channels and retries on top of this anyway.
                let gap = (self.retransmit_delay as u32 * 250).min(1500);
                self.delay_us(gap);
            }
        }
        Ok(false)
    }

    fn receive_payload(&mut self, payload: &mut [u8]) -> Result<u8, Self::Error> {
        let len = (self.rx_len as usize).min(payload.len());
        payload[..len].copy_from_slice(&self.rx_buf[DMA_HEADER..DMA_HEADER + len]);
        self.rx_len = 0;
        Ok(len as u8)
    }

    fn payload_available(&mut self) -> Result<bool, Self::Error> {
        Ok(self.rx_len != 0)
    }

    fn payload_size(&mut self) -> Result<u8, Self::Error> {
        Ok(self.rx_len)
    }

    fn set_address(&mut self, address: &[u8; ADDRESS_LEN]) -> Result<(), Self::Error> {
        let r = Self::radio();
        let (base, prefix) = hw_address(address, self.addr_encoding);
        r.base0().write_value(base);
        r.prefix0().write(|w| w.set_ap0(prefix));
        Ok(())
    }

    fn set_channel(&mut self, channel: u8) -> Result<(), Self::Error> {
        // Unifying "channel" is the RF frequency offset above 2400 MHz, which
        // is exactly the nRF52 FREQUENCY register value (0..100).
        Self::radio()
            .frequency()
            .write(|w| w.set_frequency(channel.min(100)));
        Ok(())
    }

    fn set_retries(&mut self, delay: u8, count: u8) -> Result<(), Self::Error> {
        self.retransmit_delay = delay;
        self.retransmit_count = count;
        Ok(())
    }
}

/// Map a 5-byte nRF24L01+ on-air address to the nRF52 (BASE0, PREFIX0.AP0) pair.
///
/// The nRF24L01+ handles address bit-ordering in hardware; to put the same bits
/// on air from the nRF52 we always bit-reverse each byte. What is *not* obvious
/// from the datasheets is the byte ordering between the nRF24 `TX_ADDR` register
/// layout and the nRF52 BASE/PREFIX split, so `encoding` selects among the
/// plausible permutations. The radio self-test sweeps these against a receiver
/// in pairing mode and the correct one is whichever gets ACKs.
///
/// `address` is the Unifying address as used by the protocol layer, e.g. the
/// pairing address `[0xBB, 0x0A, 0xDC, 0xA5, 0x75]`.
fn hw_address(address: &[u8; ADDRESS_LEN], encoding: u8) -> (u32, u8) {
    // rf24.rs reverses the 5 bytes before handing them to the nRF24 TX_ADDR.
    let rev = [
        address[4], address[3], address[2], address[1], address[0],
    ];

    // Helper: BASE0 is a 32-bit register; the nRF52 transmits its most
    // significant byte first. We pack 4 bytes big-endian (so b0 goes on air
    // first) and bit-reverse each byte for nRF24 compatibility.
    let pack_base = |b: [u8; 4]| -> u32 {
        u32::from_be_bytes([
            b[0].reverse_bits(),
            b[1].reverse_bits(),
            b[2].reverse_bits(),
            b[3].reverse_bits(),
        ])
    };

    match encoding {
        // 0: reversed bytes; base = rev[0..4], prefix = rev[4] (= a0).
        0 => (
            pack_base([rev[0], rev[1], rev[2], rev[3]]),
            rev[4].reverse_bits(),
        ),
        // 1: reversed bytes; prefix = rev[0] (= a4), base = rev[1..5].
        1 => (
            pack_base([rev[1], rev[2], rev[3], rev[4]]),
            rev[0].reverse_bits(),
        ),
        // 2: original order; base = address[0..4], prefix = address[4].
        2 => (
            pack_base([address[0], address[1], address[2], address[3]]),
            address[4].reverse_bits(),
        ),
        // 3: original order; prefix = address[0], base = address[1..5].
        _ => (
            pack_base([address[1], address[2], address[3], address[4]]),
            address[0].reverse_bits(),
        ),
    }
}
