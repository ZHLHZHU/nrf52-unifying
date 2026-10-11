//! Standalone diagnostic framing, independent of radio/CDC command routing.
//! Host verification: rustc --edition=2021 --test app/src/uart_smoke.rs -o /tmp/nrf-uart-smoke-tests

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, Ordering};

const PEER_PREFIX: &[u8] = b"#RP2040 PING ";
const TX_PREFIX: &[u8] = b"#NRF52840 PING ";
const HEX: &[u8] = b"0123456789ABCDEF";
const LINE_LIMIT: usize = 32;

// Debugger ABI: exactly six contiguous 32-bit atomic words, offsets 0,4,8,12,16,20.
// Order: rxbytes, peerframes, badframes, uart_errors, lastseq, txframes.
// Counts saturate; lastseq is the last fully validated peer sequence.
#[no_mangle]
pub static NRF_UART_SMOKE_COUNTERS: [AtomicU32; 6] = [
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
    AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0),
];

fn add(index: usize, value: u32) {
    let _ = NRF_UART_SMOKE_COUNTERS[index].fetch_update(
        Ordering::Relaxed, Ordering::Relaxed, |old| Some(old.saturating_add(value)),
    );
}

pub fn received_bytes(count: usize) { add(0, count as u32); }
pub fn bad_frame() { add(2, 1); }
pub fn uart_error() { add(3, 1); }

pub fn received_peer(sequence: u32) {
    add(1, 1);
    NRF_UART_SMOKE_COUNTERS[4].store(sequence, Ordering::Relaxed);
}

// Counts successful completion of the driver's write, not electrical capture or peer receipt.
pub fn transmitted_frame() { add(5, 1); }

pub fn snapshot() -> [u32; 6] {
    core::array::from_fn(|index| NRF_UART_SMOKE_COUNTERS[index].load(Ordering::Relaxed))
}

pub fn encode_ping(sequence: u32) -> [u8; 24] {
    let mut out = [0; 24];
    out[..TX_PREFIX.len()].copy_from_slice(TX_PREFIX);
    for index in 0..8 {
        out[TX_PREFIX.len() + index] = HEX[((sequence >> (28 - index * 4)) & 15) as usize];
    }
    out[23] = b'\n';
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    Peer(u32),
    Bad,
}

pub struct Parser {
    line: [u8; LINE_LIMIT],
    len: usize,
    discard: bool,
}

impl Parser {
    pub const fn new() -> Self {
        Self { line: [0; LINE_LIMIT], len: 0, discard: false }
    }

    /// Hardware damage invalidates this partial line and every suffix through the next LF.
    /// Return true exactly once for the invalidated line, including an empty partial line.
    pub fn damaged(&mut self) -> bool {
        let newly_damaged = !self.discard;
        self.len = 0;
        self.discard = true;
        newly_damaged
    }

    pub fn push(&mut self, byte: u8) -> Option<Frame> {
        if byte == b'\n' {
            let frame = if self.discard { None } else {
                Some(match parse_line(&self.line[..self.len]) {
                    Some(sequence) => Frame::Peer(sequence),
                    None => Frame::Bad,
                })
            };
            self.len = 0;
            self.discard = false;
            return frame;
        }
        if self.discard { return None; }
        if self.len == LINE_LIMIT || !(byte == b'\r' || (0x20..=0x7e).contains(&byte)) {
            self.damaged();
            return Some(Frame::Bad);
        }
        self.line[self.len] = byte;
        self.len += 1;
        None
    }
}

fn parse_line(mut line: &[u8]) -> Option<u32> {
    if let Some(without_cr) = line.strip_suffix(b"\r") { line = without_cr; }
    let digits = line.strip_prefix(PEER_PREFIX)?;
    if digits.len() != 8 { return None; }
    let mut sequence = 0;
    for &byte in digits {
        let nibble = match byte {
            b'0'..=b'9' => byte - b'0',
            b'A'..=b'F' => byte - b'A' + 10,
            _ => return None,
        };
        sequence = (sequence << 4) | u32::from(nibble);
    }
    Some(sequence)
}

struct Output<'a> { bytes: &'a mut [u8], len: usize }

impl Write for Output<'_> {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let end = self.len + text.len();
        if end > self.bytes.len() { return Err(fmt::Error); }
        self.bytes[self.len..end].copy_from_slice(text.as_bytes());
        self.len = end;
        Ok(())
    }
}

pub fn format_response(values: [u32; 6], buffer: &mut [u8]) -> Result<usize, fmt::Error> {
    let mut out = Output { bytes: buffer, len: 0 };
    write!(out,
        "UARTTEST rxbytes={} peerframes={} badframes={} uart_errors={} lastseq={:08X} txframes={}\r\n",
        values[0], values[1], values[2], values[3], values[4], values[5],
    )?;
    Ok(out.len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(parser: &mut Parser, bytes: &[u8]) -> std::vec::Vec<Frame> {
        bytes.iter().filter_map(|&byte| parser.push(byte)).collect()
    }

    #[test]
    fn split_coalesced_lf_crlf_and_full_sequence_range() {
        let mut parser = Parser::new();
        assert!(feed(&mut parser, b"#RP2040 PIN").is_empty());
        assert_eq!(feed(&mut parser, b"G 00000001\n#RP2040 PING FFFFFFFF\r\n#RP2040 PING 00000000\n"),
            [Frame::Peer(1), Frame::Peer(u32::MAX), Frame::Peer(0)]);
    }

    #[test]
    fn invalid_prefix_hex_width_and_trailing_data_cannot_be_peer_frames() {
        for invalid in [
            b"#NRF52840 PING 00000001\n".as_slice(),
            b"#RP2040 PING 0000000a\n", b"#RP2040 PING 000000001\n",
            b"#RP2040 PING 0000001\n", b"#RP2040 PING 00000001 extra\n",
            b"#RP2040 PING 0000\r001\n", b"#RP2040 PING 00000001\r\r\n",
            b"\n",
        ] {
            let mut parser = Parser::new();
            assert_eq!(feed(&mut parser, invalid), [Frame::Bad]);
            assert_eq!(feed(&mut parser, b"#RP2040 PING 00000002\n"), [Frame::Peer(2)]);
        }
    }

    #[test]
    fn oversized_and_binary_lines_discard_valid_looking_suffixes() {
        let mut parser = Parser::new();
        assert_eq!(feed(&mut parser, &[b'X'; LINE_LIMIT + 1]), [Frame::Bad]);
        assert!(feed(&mut parser, b"#RP2040 PING 00000001\n").is_empty());
        assert_eq!(feed(&mut parser, b"\0#RP2040 PING 00000002\n"), [Frame::Bad]);
        assert_eq!(feed(&mut parser, b"#RP2040 PING 00000003\n"), [Frame::Peer(3)]);
    }

    #[test]
    fn hardware_damage_never_accepts_a_partial_or_its_suffix() {
        let mut parser = Parser::new();
        assert!(feed(&mut parser, b"#RP2040 PING 0000").is_empty());
        assert!(parser.damaged());
        assert!(!parser.damaged());
        assert!(feed(&mut parser, b"0001#RP2040 PING 00000002\n").is_empty());
        assert_eq!(feed(&mut parser, b"#RP2040 PING 00000003\n"), [Frame::Peer(3)]);
    }

    #[test]
    fn outgoing_ping_uses_exact_wire_contract() {
        assert_eq!(&encode_ping(1), b"#NRF52840 PING 00000001\n");
        assert_eq!(&encode_ping(u32::MAX), b"#NRF52840 PING FFFFFFFF\n");
    }
}
