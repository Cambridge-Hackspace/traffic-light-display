import socket
import time
import math

UDP_IP = "192.168.1.129"
UDP_PORT = 4048

sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

# byte 0: flags             = 0x41 (v1, push)
# byte 1: sequence          = 0x00 (ignore)
# byte 2: data type         = 0x00 (default)
# byte 3: source identifier = 0x01
# bytes 4-7: data offset    = 0x0x00000000
# bytes 8-9: data length    = 0x00C0 (192)
header = bytearray([0x41, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0])
print(f"> sending DDP stream to {UDP_IP}:{UDP_PORT}...")
print(f"> press CTRL+C to stop")

try:
    frame = 0
    while True:
        payload = bytearray(192)

        # 24 columns x 8 rows
        for y in range(8):
            for x in range(24):
                # sweeping sine wave animation
                sine_val = math.sin((x - frame) * 0.4)
                brightness = int((sine_val + 1.0) * 50)
                payload[y * 24 + x] = brightness

        packet = header + payload
        sock.sendto(packet, (UDP_IP, UDP_PORT))

        frame += 1
        time.sleep(0.05)

except KeyboardInterrupt:
    print(f"> goodbye!")
