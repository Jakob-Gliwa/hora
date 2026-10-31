#!/usr/bin/env python3
"""Offline HTTP, DNS, MQTT publisher and push producers for the benchmark."""
import base64
import http.server
import json
from pathlib import Path
import socket
import struct
import threading
import time
import urllib.request

FIXTURES = json.loads(Path('/data/fixtures.json').read_text())


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'
    def do_GET(self):
        row = FIXTURES['http'].get(self.path, {'status': 200, 'body': ''})
        payload = base64.b64decode(row['body'])
        self.send_response(row['status'])
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)
    do_POST = do_GET
    def do_HEAD(self):
        self.send_response(200)
        self.send_header('Content-Length', '0')
        self.end_headers()
    def log_message(self, *_args): pass


def dns():
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(('127.0.0.1', 15353))
    while True:
        data, addr = sock.recvfrom(4096)
        # Only echo the question, not EDNS OPT records in the additional section.
        cursor = 12
        while cursor < len(data) and data[cursor]:
            cursor += 1 + data[cursor]
        cursor += 1
        if cursor + 4 > len(data): continue
        qtype = struct.unpack('!H', data[cursor:cursor + 2])[0]
        value = socket.inet_pton(socket.AF_INET6, '::1') if qtype == 28 else b'\x7f\0\0\x01'
        answer = b'\xc0\x0c' + struct.pack('!HHIH', qtype, 1, 30, len(value)) + value
        sock.sendto(data[:2] + b'\x81\x80\0\x01\0\x01\0\0\0\0' + data[12:cursor + 4] + answer, addr)


def mqtt_packet(header, body):
    length = len(body)
    size = bytearray()
    while True:
        byte = length % 128
        length //= 128
        size.append(byte | (128 if length else 0))
        if not length: break
    return bytes([header]) + size + body


def mqtt():
    while True:
        try:
            with socket.create_connection(('127.0.0.1', 1883), timeout=5) as sock:
                sock.sendall(mqtt_packet(0x10, b'\0\x04MQTT\x04\x02\0\x1e\0\x07fixture'))
                if sock.recv(4) != b'\x20\x02\0\0': raise OSError('broker refused')
                published = {}
                last_ping = time.monotonic()
                while True:
                    now = time.monotonic()
                    for topic, row in FIXTURES['mqtt'].items():
                        if topic not in published or (row['period'] and now - published[topic] >= row['period']):
                            encoded_topic = topic.encode()
                            sock.sendall(mqtt_packet(0x31, struct.pack('!H', len(encoded_topic)) + encoded_topic + base64.b64decode(row['body'])))
                            published[topic] = now
                    if now - last_ping >= 10:
                        sock.sendall(mqtt_packet(0xc0, b''))
                        last_ping = now
                    time.sleep(1)
        except OSError:
            time.sleep(1)


def heartbeats():
    published = {}
    while True:
        now = time.monotonic()
        for row in FIXTURES['push']:
            token = row['token']
            if now - published.get(token, -1000000) < row['period']: continue
            try:
                with urllib.request.urlopen(f'http://127.0.0.1:8787/api/push/{token}?status=up&ping=1', timeout=2) as reply: reply.read()
                published[token] = now
            except OSError: pass
        try:
            request = urllib.request.Request('http://127.0.0.1:8787/api/status', headers={'Authorization': 'Bearer benchmark-only-viewer-token'})
            with urllib.request.urlopen(request, timeout=2) as reply: reply.read()
        except OSError: pass
        time.sleep(5)


for target in (dns, mqtt, heartbeats):
    threading.Thread(target=target, daemon=True).start()
http.server.ThreadingHTTPServer(('127.0.0.1', 18080), Handler).serve_forever()
