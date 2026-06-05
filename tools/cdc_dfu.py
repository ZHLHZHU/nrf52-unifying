#!/usr/bin/env python3
"""CDC OTA updater for the nRF52840 Unifying firmware (Linux).

Mirrors tools/cdc_dfu.ps1: INFO -> WRITE <size> -> READY -> raw image -> OK ->
BOOT -> REBOOT.

Usage:
    python3 tools/cdc_dfu.py --port /dev/ttyACM0 \
        --image target/thumbv7em-none-eabihf/release/nrf-demo-app.bin
"""

import argparse
import sys
import time

import serial


def read_line(ser, timeout=8.0):
    ser.timeout = timeout
    line = ser.readline().decode("ascii", errors="replace").strip()
    print(f"< {line}")
    return line


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", required=True)
    ap.add_argument("--image", required=True)
    args = ap.parse_args()

    with open(args.image, "rb") as f:
        payload = f.read()

    if len(payload) % 4 != 0:
        payload += b"\xff" * (4 - (len(payload) % 4))

    ser = serial.Serial(args.port, 115200, timeout=8, write_timeout=8)
    ser.dtr = True
    ser.rts = True
    time.sleep(0.5)
    ser.reset_input_buffer()
    ser.reset_output_buffer()

    try:
        print("> INFO")
        ser.write(b"INFO\n")
        read_line(ser)

        print(f"> WRITE {len(payload)}")
        ser.write(f"WRITE {len(payload)}\n".encode())
        ready = read_line(ser)
        if ready != "READY":
            print(f"device not ready: {ready}", file=sys.stderr)
            return 1

        print(f"> binary payload ({len(payload)} bytes)")
        # Write in chunks so we don't overrun the device's 64-byte CDC reads.
        chunk = 64
        for i in range(0, len(payload), chunk):
            ser.write(payload[i : i + chunk])
            ser.flush()
        ok = read_line(ser, timeout=15.0)
        if ok != "OK":
            print(f"image write failed: {ok}", file=sys.stderr)
            return 1

        print("> BOOT")
        ser.write(b"BOOT\n")
        read_line(ser)
        print("Update command sent. Device will reboot into the new firmware.")
    finally:
        ser.close()
    return 0


if __name__ == "__main__":
    sys.exit(main())
