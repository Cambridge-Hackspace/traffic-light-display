#!/usr/bin/env python3
import argparse
import socket
import time
import sys

try:
    from PIL import Image, ImageDraw, ImageFont
except ImportError:
    print("Error: Pillow library is required. Run 'pip install pillow'")
    sys.exit(1)

# Standard DDP header for a 300-byte payload (v1, push, default type, source 1)
DDP_HEADER = bytearray([0x41, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01, 0x2C])
DISPLAY_WIDTH = 30
DISPLAY_HEIGHT = 10

def main():
    parser = argparse.ArgumentParser(description="Scroll text as a marquee over DDP.")
    parser.add_argument("target", help="Target IP:PORT (e.g. 192.168.1.129:4048)")
    parser.add_argument("text", help="Text to scroll across the screen")
    parser.add_argument("--speed", type=float, default=0.05, help="Delay between frames in seconds (default: 0.05)")
    args = parser.parse_args()

    # Parse target IP and port
    try:
        ip, port_str = args.target.split(":")
        port = int(port_str)
    except ValueError:
        print("Error: Target must be in the exact format IP:PORT (e.g. 192.168.1.129:4048)")
        sys.exit(1)

    # Load default bitmap font
    font = ImageFont.load_default()
    
    # Calculate the text bounding box to determine the required image width
    dummy_img = Image.new('L', (1, 1))
    draw = ImageDraw.Draw(dummy_img)
    bbox = draw.textbbox((0, 0), args.text, font=font)
    text_width = bbox[2] - bbox[0]
    text_height = bbox[3] - bbox[1]

    # Create an image just wide enough to hold the text, strictly 10 pixels high
    text_img = Image.new('L', (text_width, DISPLAY_HEIGHT), color=0)
    text_draw = ImageDraw.Draw(text_img)
    
    # Center the text vertically to ensure it fits perfectly within the 10px height
    y_offset = (DISPLAY_HEIGHT - text_height) // 2 - bbox[1]
    text_draw.text((0, y_offset), args.text, fill=255, font=font)

    print(f"> rendering '{args.text}' (calculated width: {text_width}px)")
    
    # Setup network socket
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    print(f"> streaming marquee to {ip}:{port} via DDP...")
    print(f"> press CTRL+C to stop")

    try:
        # Start the text completely off-screen to the right
        x_pos = DISPLAY_WIDTH 
        
        while True:
            # Create a blank 30x10 frame representing the physical matrix
            frame = Image.new('L', (DISPLAY_WIDTH, DISPLAY_HEIGHT), color=0)
            
            # Paste the text image onto the frame at the shifting X coordinate
            frame.paste(text_img, (x_pos, 0))
            
            # Convert grayscale image data to a flat bytearray and send
            payload = bytearray(frame.getdata())
            packet = DDP_HEADER + payload
            sock.sendto(packet, (ip, port))
            
            # Advance the scroll by 1 pixel to the left
            x_pos -= 1
            
            # Reset if the text has completely scrolled off the left edge
            if x_pos < -text_width:
                x_pos = DISPLAY_WIDTH
                
            time.sleep(args.speed)

    except KeyboardInterrupt:
        print("\n> goodbye!")

if __name__ == "__main__":
    main()
