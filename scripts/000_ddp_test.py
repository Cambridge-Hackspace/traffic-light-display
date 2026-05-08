#!/usr/bin/env python3
import argparse
import socket
import time
import math
import sys

# byte 0: flags             = 0x41 (v1, push)
# byte 1: sequence          = 0x00 (ignore)
# byte 2: data type         = 0x00 (default)
# byte 3: source identifier = 0x01
# bytes 4-7: data offset    = 0x0x00000000
# bytes 8-9: data length    = 0x012C (300)
DDP_HEADER = bytearray([0x41, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x2C])

def main():
    parser = argparse.ArgumentParser(description="Stream a sweeping sine wave animation via DDP.")
    parser.add_argument("target", help="Target IP:PORT (e.g. 192.168.1.129:4048)")
    args = parser.parse_args()

    # Parse target IP and port
    try:
        ip, port_str = args.target.split(":")
        port = int(port_str)
    except ValueError:
        print("Error: Target must be in the exact format IP:PORT (e.g. 192.168.1.129:4048)")
        sys.exit(1)

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    print(f"> sending DDP stream to {ip}:{port}...")
    print(f"> press CTRL+C to stop")

    try:
        frame = 0
        while True:
            payload = bytearray(300)

            # 30 columns x 10 rows
            for y in range(10):
                for x in range(30):
                    # sweeping sine wave animation
                    sine_val = math.sin((x - frame) * 0.4)
                    brightness = int((sine_val + 1.0) * 50)
                    payload[y * 30 + x] = brightness

            packet = DDP_HEADER + payload
            sock.sendto(packet, (ip, port))

            frame += 1
            time.sleep(0.05)

    except KeyboardInterrupt:
        print("\n> goodbye!")

if __name__ == "__main__":
    main()
