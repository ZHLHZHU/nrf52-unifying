#!/usr/bin/env python3
"""unifying - host-side CLI for the nRF52840 Logitech Unifying transmitter.

Talks to the firmware over its USB-CDC serial line using the line protocol:
  VER / UPAIR / UCONNECT / UTYPE <text> / UKEY <mod> [keys] /
  UKEEPALIVE / USTATUS / UDELETE

Examples:
  unifying.py --port /dev/ttyACM0 ver
  unifying.py pair
  unifying.py connect
  unifying.py type "hello world"
  unifying.py key ctrl+alt+del
  unifying.py key f5
  unifying.py status
  unifying.py delete

Notes:
- `pair` requires the receiver to be in pairing mode first, e.g.
  `sudo ltunify pair 60` on the host the receiver is plugged into.
- Pairing is stored in the device's flash and survives reboots/OTA, so you
  normally only `pair` once, then `connect` + `type`/`key`.
"""

import argparse
import sys
import time

try:
    import serial
except ImportError:
    print("error: pyserial not installed (pip install pyserial)", file=sys.stderr)
    sys.exit(2)


# ---- HID modifier bitmask ----
MODIFIERS = {
    "ctrl": 0x01, "lctrl": 0x01, "rctrl": 0x10,
    "shift": 0x02, "lshift": 0x02, "rshift": 0x20,
    "alt": 0x04, "lalt": 0x04, "ralt": 0x40,
    "gui": 0x08, "win": 0x08, "meta": 0x08, "cmd": 0x08,
    "lgui": 0x08, "rgui": 0x80,
}

# ---- Named key -> USB HID usage id ----
KEY_NAMES = {
    "enter": 0x28, "return": 0x28, "esc": 0x29, "escape": 0x29,
    "backspace": 0x2A, "bksp": 0x2A, "tab": 0x2B, "space": 0x2C,
    "minus": 0x2D, "equal": 0x2E, "lbracket": 0x2F, "rbracket": 0x30,
    "backslash": 0x31, "semicolon": 0x33, "quote": 0x34, "grave": 0x35,
    "comma": 0x36, "period": 0x37, "slash": 0x38, "capslock": 0x39,
    "f1": 0x3A, "f2": 0x3B, "f3": 0x3C, "f4": 0x3D, "f5": 0x3E, "f6": 0x3F,
    "f7": 0x40, "f8": 0x41, "f9": 0x42, "f10": 0x43, "f11": 0x44, "f12": 0x45,
    "printscreen": 0x46, "scrolllock": 0x47, "pause": 0x48,
    "insert": 0x49, "home": 0x4A, "pageup": 0x4B, "delete": 0x4C, "del": 0x4C,
    "end": 0x4D, "pagedown": 0x4E,
    "right": 0x4F, "left": 0x50, "down": 0x51, "up": 0x52,
}
# letters/digits
for _i, _c in enumerate("abcdefghijklmnopqrstuvwxyz"):
    KEY_NAMES[_c] = 0x04 + _i
for _i, _c in enumerate("1234567890"):
    KEY_NAMES[_c] = 0x1E + _i


class Device:
    def __init__(self, port, debug=False):
        self.debug = debug
        self.ser = serial.Serial(port, 115200, timeout=3, write_timeout=5)
        self.ser.dtr = True
        self.ser.rts = True
        time.sleep(0.5)
        self.ser.reset_input_buffer()
        # Absorb the connect-handshake/banner with a throwaway newline.
        self.ser.write(b"\n")
        time.sleep(0.2)
        self.ser.reset_input_buffer()

    def cmd(self, line, timeout=5.0):
        self.ser.reset_input_buffer()
        if self.debug:
            print(f"> {line}", file=sys.stderr)
        self.ser.write(line.encode() + b"\n")
        self.ser.timeout = timeout
        resp = self.ser.readline().decode("ascii", errors="replace").strip()
        if self.debug:
            print(f"< {resp}", file=sys.stderr)
        return resp

    def close(self):
        self.ser.close()


def parse_key_combo(combo):
    """'ctrl+alt+del' -> (modifier_bitmask, [keycodes]). Raises ValueError."""
    modifier = 0
    keys = []
    for part in combo.lower().split("+"):
        part = part.strip()
        if not part:
            continue
        if part in MODIFIERS:
            modifier |= MODIFIERS[part]
        elif part in KEY_NAMES:
            keys.append(KEY_NAMES[part])
        elif part.startswith("0x"):
            keys.append(int(part, 16))
        else:
            raise ValueError(f"unknown key: {part!r}")
    if len(keys) > 6:
        raise ValueError("at most 6 simultaneous keys")
    return modifier, keys


def cmd_ver(dev, args):
    print(dev.cmd("VER"))


def cmd_pair(dev, args):
    print("Make sure the receiver is in pairing mode (e.g. sudo ltunify pair 60).")
    print(dev.cmd("UPAIR", timeout=30))


def cmd_connect(dev, args):
    print(dev.cmd("UCONNECT", timeout=20))


def cmd_type(dev, args):
    text = " ".join(args.text)
    # Send as hex so any byte is safe over the line protocol.
    # Firmware UTYPE currently takes literal text; keep literal but reject
    # newlines which would terminate the command.
    if "\n" in text or "\r" in text:
        print("error: use `key enter` for newlines", file=sys.stderr)
        return 1
    print(dev.cmd(f"UTYPE {text}", timeout=20))


def cmd_key(dev, args):
    try:
        modifier, keys = parse_key_combo(args.combo)
    except ValueError as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    tokens = " ".join(f"{k:02X}" for k in keys)
    line = f"UKEY {modifier:02X}" + (f" {tokens}" if tokens else "")
    print(dev.cmd(line, timeout=10))


def cmd_status(dev, args):
    print(dev.cmd("USTATUS"))


def cmd_keepalive(dev, args):
    print(dev.cmd("UKEEPALIVE"))


def cmd_delete(dev, args):
    print(dev.cmd("UDELETE"))


def cmd_raw(dev, args):
    print(dev.cmd(" ".join(args.line), timeout=30))


def build_parser():
    p = argparse.ArgumentParser(description="nRF52840 Unifying transmitter CLI")
    p.add_argument("--port", default="/dev/ttyACM0", help="serial port (default /dev/ttyACM0)")
    p.add_argument("--debug", action="store_true", help="print raw protocol I/O")
    sub = p.add_subparsers(dest="command", required=True)

    sub.add_parser("ver").set_defaults(func=cmd_ver)
    sub.add_parser("pair").set_defaults(func=cmd_pair)
    sub.add_parser("connect").set_defaults(func=cmd_connect)

    sp = sub.add_parser("type", help="type literal text")
    sp.add_argument("text", nargs="+")
    sp.set_defaults(func=cmd_type)

    sp = sub.add_parser("key", help="send a key/combo, e.g. ctrl+alt+del, f5, enter")
    sp.add_argument("combo")
    sp.set_defaults(func=cmd_key)

    sub.add_parser("status").set_defaults(func=cmd_status)
    sub.add_parser("keepalive").set_defaults(func=cmd_keepalive)
    sub.add_parser("delete").set_defaults(func=cmd_delete)

    sp = sub.add_parser("raw", help="send a raw protocol line")
    sp.add_argument("line", nargs="+")
    sp.set_defaults(func=cmd_raw)

    return p


def main():
    args = build_parser().parse_args()
    try:
        dev = Device(args.port, debug=args.debug)
    except serial.SerialException as e:
        print(f"error: cannot open {args.port}: {e}", file=sys.stderr)
        return 2
    try:
        return args.func(dev, args) or 0
    finally:
        dev.close()


if __name__ == "__main__":
    sys.exit(main())
