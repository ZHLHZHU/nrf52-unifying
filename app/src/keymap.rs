//! ASCII -> USB HID keyboard scancode mapping, ported from the rust-unifying
//! CLI. Returns `(scancode, needs_shift)`.

pub fn char_to_hid(c: u8) -> Option<(u8, bool)> {
    let r = match c {
        b'a'..=b'z' => (0x04 + (c - b'a'), false),
        b'A'..=b'Z' => (0x04 + (c - b'A'), true),
        b'1'..=b'9' => (0x1E + (c - b'1'), false),
        b'0' => (0x27, false),
        b'\n' | b'\r' => (0x28, false),
        0x1b => (0x29, false), // Escape
        0x08 => (0x2A, false), // Backspace
        b'\t' => (0x2B, false),
        b' ' => (0x2C, false),
        b'-' => (0x2D, false),
        b'=' => (0x2E, false),
        b'[' => (0x2F, false),
        b']' => (0x30, false),
        b'\\' => (0x31, false),
        b';' => (0x33, false),
        b'\'' => (0x34, false),
        b'`' => (0x35, false),
        b',' => (0x36, false),
        b'.' => (0x37, false),
        b'/' => (0x38, false),
        b'!' => (0x1E, true),
        b'@' => (0x1F, true),
        b'#' => (0x20, true),
        b'$' => (0x21, true),
        b'%' => (0x22, true),
        b'^' => (0x23, true),
        b'&' => (0x24, true),
        b'*' => (0x25, true),
        b'(' => (0x26, true),
        b')' => (0x27, true),
        b'_' => (0x2D, true),
        b'+' => (0x2E, true),
        b'{' => (0x2F, true),
        b'}' => (0x30, true),
        b'|' => (0x31, true),
        b':' => (0x33, true),
        b'"' => (0x34, true),
        b'~' => (0x35, true),
        b'<' => (0x36, true),
        b'>' => (0x37, true),
        b'?' => (0x38, true),
        _ => return None,
    };
    Some(r)
}
