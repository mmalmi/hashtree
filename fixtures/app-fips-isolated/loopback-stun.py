"""Local RFC 5389 binding responses for the pinned native ICE agent in a no-egress test."""
import ipaddress
import selectors
import socket
import struct

COOKIE = 0x2112A442
selector = selectors.DefaultSelector()
for port in [3478, 19302]:
    service = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    service.bind(('127.0.0.1', port))
    selector.register(service, selectors.EVENT_READ)
print('ready', flush=True)
while True:
    for ready, _ in selector.select():
        sock = ready.fileobj
        packet, (host, port) = sock.recvfrom(2048)
        if len(packet) < 20: continue
        kind, size, cookie, transaction = struct.unpack('!HHI12s', packet[:20])
        if kind != 1 or cookie != COOKIE or 20 + size != len(packet): continue
        value = struct.pack('!BBHI', 0, 1, port ^ (COOKIE >> 16), int(ipaddress.IPv4Address(host)) ^ COOKIE)
        attribute = struct.pack('!HH', 0x20, len(value)) + value
        sock.sendto(struct.pack('!HHI12s', 0x0101, len(attribute), COOKIE, transaction) + attribute, (host, port))
