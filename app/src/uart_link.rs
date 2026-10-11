//! Numbered UART transport only; radio commands remain in the application's handler.
//! Host verification: rustc --edition=2021 --test app/src/uart_link.rs -o /tmp/nrf-uart-tests

pub const LINE_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandError {
    Command,
    Argument,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub number: u32,
    pub command: [u8; LINE_LIMIT],
    pub len: usize,
    pub error: Option<CommandError>,
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

    /// After hardware damage no suffix of the damaged line may become a command.
    pub fn damaged(&mut self) {
        self.len = 0;
        self.discard = true;
    }

    /// Cancel old partial and DMA bytes, keeping a received LF as a safe boundary.
    pub fn session_changed(&mut self, pending: &[u8]) {
        if self.len != 0 || !pending.is_empty() {
            self.len = 0;
            self.discard = pending.last() != Some(&b'\n');
        }
    }

    pub fn push(&mut self, byte: u8) -> Option<Frame> {
        if byte == b'\n' {
            let frame = if self.discard { None } else { parse_line(&self.line[..self.len]) };
            self.len = 0;
            self.discard = false;
            return frame;
        }
        if self.discard { return None; }
        if self.len == LINE_LIMIT || !(byte == b'\r' || (0x20..=0x7e).contains(&byte)) {
            self.damaged();
        } else {
            self.line[self.len] = byte;
            self.len += 1;
        }
        None
    }
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn hex_byte(token: &[u8]) -> Option<u8> {
    if token.is_empty() || token.len() > 2 { return None; }
    let mut value = 0;
    for &byte in token { value = (value << 4) | hex(byte)?; }
    Some(value)
}

fn validate(command: &[u8]) -> Option<CommandError> {
    let mut tokens = command.split(|&b| b == b' ').filter(|s| !s.is_empty());
    match tokens.next() {
        Some(b"OTA") => None, // Full bounds and payload validation in uart_ota.
        Some(b"VER" | b"USTATUS") => {
            if tokens.next().is_some() { Some(CommandError::Argument) } else { None }
        }
        // Pairing over UART must name its slot so it never overwrites the active target implicitly.
        Some(b"USWITCH" | b"UPAIR") => {
            if tokens.next().and_then(hex_byte).is_some_and(|slot| slot < 4)
                && tokens.next().is_none() { None } else { Some(CommandError::Argument) }
        }
        Some(b"UKEYDOWN") => {
            if tokens.next().and_then(hex_byte).is_none() { return Some(CommandError::Argument); }
            let mut count = 0;
            for token in tokens {
                count += 1;
                if count > 6 || hex_byte(token).is_none() { return Some(CommandError::Argument); }
            }
            None
        }
        _ => Some(CommandError::Command),
    }
}

fn parse_line(mut line: &[u8]) -> Option<Frame> {
    if let Some(without_cr) = line.strip_suffix(b"\r") { line = without_cr; }
    if line.len() < 10 || line[0] != b'@' || line[9] != b' ' { return None; }
    let mut number = 0u32;
    for &byte in &line[1..9] {
        if !(byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte)) { return None; }
        number = (number << 4) | u32::from(hex(byte)?);
    }
    let command = &line[10..];
    // Canonicalize whitespace accepted by the original token parser. CR is only legal at EOL.
    let mut frame = Frame { number, command: [0; LINE_LIMIT], len: 0, error: None };
    let mut first = true;
    for token in command.split(|&b| b == b' ').filter(|s| !s.is_empty()) {
        if !first { frame.command[frame.len] = b' '; frame.len += 1; }
        first = false;
        frame.command[frame.len..frame.len + token.len()].copy_from_slice(token);
        frame.len += token.len();
    }
    frame.error = if command.contains(&b'\r') { Some(CommandError::Argument) }
        else { validate(&frame.command[..frame.len]) };
    Some(frame)
}

/// Prefix the unchanged handler response with its request number.
pub fn encode_reply(number: u32, body: &[u8], out: &mut [u8]) -> Option<usize> {
    if out.len() < body.len() + 10 { return None; }
    out[0] = b'@';
    for index in 0..8 {
        out[index + 1] = b"0123456789ABCDEF"[((number >> (28 - index * 4)) & 15) as usize];
    }
    out[9] = b' ';
    out[10..10 + body.len()].copy_from_slice(body);
    Some(body.len() + 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(parser: &mut Parser, bytes: &[u8]) -> std::vec::Vec<Frame> {
        bytes.iter().filter_map(|&b| parser.push(b)).collect()
    }

    #[test]
    fn split_and_coalesced_reports_preserve_press_release_and_numbers() {
        let mut parser = Parser::new();
        assert!(feed(&mut parser, b"@00000001 UKEY").is_empty());
        let frames = feed(&mut parser, b"DOWN 02 04\r\n@00000002 UKEYDOWN 00\n");
        assert_eq!((frames[0].number, frames[1].number), (1, 2));
        assert_eq!(&frames[0].command[..frames[0].len], b"UKEYDOWN 02 04");
        assert_eq!(&frames[1].command[..frames[1].len], b"UKEYDOWN 00");
        assert_eq!(frames[0].error, None);
        assert_eq!(frames[1].error, None);
    }

    #[test]
    fn invalid_number_is_discarded_not_reinterpreted() {
        let mut parser = Parser::new();
        let frames = feed(&mut parser, b"@0000000a VER\n@000000001 VER\n@FFFFFFFF VER\n");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].number, u32::MAX);
    }

    #[test]
    fn unsupported_and_invalid_arguments_have_numbered_errors() {
        let mut parser = Parser::new();
        let frames = feed(&mut parser, b"@00000003 UDELETE 00\n@00000004 USWITCH 04\n@00000005 VER extra\n@00000006 UKEYDOWN 00 04 05 06 07 08 09 0A\n@00000007 UKEYDOWN GG\n@00000008 UPAIR\n@00000009 UPAIR 04\n@0000000A UPAIR 01 02\n");
        assert_eq!(frames.len(), 8);
        assert_eq!(frames[0].error, Some(CommandError::Command));
        for frame in &frames[1..] { assert_eq!(frame.error, Some(CommandError::Argument)); }
        let mut out = [0; 32];
        let len = encode_reply(frames[1].number, b"ERR ARG\r\n", &mut out).unwrap();
        assert_eq!(&out[..len], b"@00000004 ERR ARG\r\n");
    }

    #[test]
    fn six_keys_and_normalized_whitespace_are_accepted() {
        let frames = feed(&mut Parser::new(), b"@00000009   UKEYDOWN  ff  04 05 06 07 08 09  \n@0000000A USWITCH 01\n@0000000B UPAIR  01\n");
        assert_eq!(&frames[0].command[..frames[0].len], b"UKEYDOWN ff 04 05 06 07 08 09");
        assert_eq!(&frames[2].command[..frames[2].len], b"UPAIR 01");
        for frame in &frames { assert_eq!(frame.error, None); }
    }

    #[test]
    fn overlong_and_hardware_damaged_lines_resynchronize_only_at_lf() {
        let mut parser = Parser::new();
        feed(&mut parser, b"@00000001 ");
        for _ in 0..LINE_LIMIT { parser.push(b' '); }
        assert!(feed(&mut parser, b"@00000002 VER\n").is_empty());
        assert_eq!(feed(&mut parser, b"@00000003 VER\n")[0].number, 3);
        feed(&mut parser, b"@00000004 UK");
        parser.damaged();
        assert!(feed(&mut parser, b"@00000005 VER\n").is_empty());
        assert_eq!(feed(&mut parser, b"@00000006 VER\n")[0].number, 6);
    }

    #[test]
    fn exact_limit_is_accepted_but_next_byte_discards_the_line() {
        let mut parser = Parser::new();
        feed(&mut parser, b"@00000001 VER");
        for _ in 13..LINE_LIMIT { parser.push(b' '); }
        assert_eq!(parser.push(b'\n').unwrap().error, None);
        feed(&mut parser, b"@00000002 VER");
        for _ in 13..=LINE_LIMIT { parser.push(b' '); }
        assert!(parser.push(b'\n').is_none());
    }

    #[test]
    fn maintenance_boundary_discards_partial_input_but_not_a_fresh_probe() {
        let mut parser = Parser::new();
        feed(&mut parser, b"@00000001 UKEYDOWN ");
        parser.session_changed(&[]);
        assert!(feed(&mut parser, b"00 04\n").is_empty());
        parser.session_changed(&[]);
        let frames = feed(&mut parser, b"@00000002 VER\n");
        assert_eq!(frames[0].number, 2);
        assert_eq!(frames[0].error, None);
    }

    #[test]
    fn embedded_cr_or_control_byte_cannot_hide_a_command() {
        let mut parser = Parser::new();
        let frame = feed(&mut parser, b"@00000001 VER\r UPAIR\n").remove(0);
        assert_eq!(frame.error, Some(CommandError::Argument));
        assert!(feed(&mut parser, b"@00000002 VE\0R\n").is_empty());
        assert_eq!(feed(&mut parser, b"@00000003 USTATUS\r\n")[0].error, None);
    }

    #[test]
    fn cancelled_dma_ending_at_lf_keeps_the_next_session_probe() {
        let mut parser = Parser::new();
        feed(&mut parser, b"@00000001 UKEY");
        parser.session_changed(b"DOWN 00 04\n@00000002 USWITCH 01\r\n");
        let frames = feed(&mut parser, b"@00000003 VER\n");
        assert_eq!(frames.iter().map(|frame| frame.number).collect::<std::vec::Vec<_>>(), [3]);
        assert_eq!(&frames[0].command[..frames[0].len], b"VER");
    }

    #[test]
    fn cancelled_dma_tail_cannot_be_completed_in_the_new_session() {
        let mut parser = Parser::new();
        parser.session_changed(b"@00000001 VER\n@00000002 UKEYDOWN");
        let frames = feed(&mut parser, b" 00 04\n@00000003 USTATUS\n");
        assert_eq!(frames.iter().map(|frame| frame.number).collect::<std::vec::Vec<_>>(), [3]);
        assert_eq!(&frames[0].command[..frames[0].len], b"USTATUS");
    }
}
