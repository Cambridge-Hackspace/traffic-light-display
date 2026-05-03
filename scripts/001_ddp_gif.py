#!/usr/bin/env python3
import argparse
import socket
import time
import sys

try:
    from PIL import Image, ImageSequence, ImageOps
except ImportError:
    print(f"Error: Pillow library is required.")
    sys.exit(1)

DDP_HEADER = bytearray([0x41, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x2C])
DISPLAY_WIDTH = 30
DISPLAY_HEIGHT = 10

def process_gif(gif_path):
    try:
        img = Image.open(gif_path)
    except Exception as e:
        print(f"Failed to open GIF: {e}")
        sys.exit(1)

    frames = []

    # iterate through all frames of the GIF
    for frame in ImageSequence.Iterator(img):
        # convert to RGBA
        rgba_frame = frame.convert("RGBA")

        # create a solid black background to blend any transparent pixels
        bg = Image.new("RGBA", rgba_frame.size, (0, 0, 0, 255))
        bg.paste(rgba_frame, mask=rgba_frame)

        # convert to RGB, then use ImageOps.pad to fit the frame into 30/10 w/o
        # stretching; any leftover space is filled with black pixels
        rgb_frame = bg.convert("RGB")
        resized = ImageOps.pad(rgb_frame, (DISPLAY_WIDTH, DISPLAY_HEIGHT), color=(0, 0, 0))

        # convert to grayscale ('L' mode)
        gray = resized.convert("L")

        # extract frame duration in milliseconda (default to 100ms if missing)
        # some GIFs use 0 to indicate "as fast as possible" so we cap it to a sensible default
        duration = frame.info.get('duration', 100)
        if duration == 0:
            duration = 100

        # convert grayscale image data to a flat bytearray (300 bytes)
        payload = bytearray(gray.getdata())

        # store the payload alongside its intended udration (in seconds for time.sleep)
        frames.append((payload, duration / 1000.0))

    return frames

def main():
    parser = argparse.ArgumentParser(description="Stream a GIF to a DDP display in grayscale.")
    parser.add_argument("target", help="Target IP:PORT (e.g. 192.168.1.129:4048)")
    parser.add_argument("gif_path", help="Path to the GIF file")
    args = parser.parse_args()

    # parse target IP and port
    try:
        ip, port_str = args.target.split(":")
        port = int(port_str)
    except ValueError:
        print("Error: Target must be in the exact format IP:PORT (e.g. 192.168.1.129:4048)")
        sys.exit(1)

    print(f"> loading and processing '{args.gif_path}'...")
    frames = process_gif(args.gif_path)
    print(f"> extracted {len(frames)} frames.")

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    print(f"> streaming GIF to {ip}:{port} via DDP...")
    print(f"> press CTRL+C to stop")

    try:
        # loop the animation forever
        while True:
            for payload, duration in frames:
                packet = DDP_HEADER + payload
                sock.sendto(packet, (ip, port))
                time.sleep(duration)

    except KeyboardInterrupt:
        print("\n> goodbye!")

if __name__ == "__main__":
    main()
