#!/usr/bin/env python3
"""最小 WebSocket 客户端，用来验证 agentc 的实时通道（出帧 + 输入）。"""
import base64
import json
import os
import socket
import struct
import sys
import time

HOST, PORT = "127.0.0.1", 7788


def connect():
    s = socket.create_connection((HOST, PORT))
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        "GET /ws HTTP/1.1\r\n"
        f"Host: {HOST}:{PORT}\r\n"
        "Upgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    s.sendall(req.encode())
    buf = b""
    while b"\r\n\r\n" not in buf:
        buf += s.recv(1)
    assert b"101" in buf.split(b"\r\n")[0], buf[:120]
    return s


def send_text(s, obj):
    data = json.dumps(obj).encode()
    mask = os.urandom(4)
    head = bytearray([0x81])
    n = len(data)
    if n < 126:
        head.append(0x80 | n)
    elif n <= 0xFFFF:
        head.append(0x80 | 126)
        head += struct.pack(">H", n)
    else:
        head.append(0x80 | 127)
        head += struct.pack(">Q", n)
    head += mask
    s.sendall(bytes(head) + bytes(b ^ mask[i & 3] for i, b in enumerate(data)))


def recv(s, timeout=6.0):
    s.settimeout(timeout)
    h = s.recv(2)
    if len(h) < 2:
        return None
    op = h[0] & 0x0F
    n = h[1] & 0x7F
    if n == 126:
        n = struct.unpack(">H", s.recv(2))[0]
    elif n == 127:
        n = struct.unpack(">Q", s.recv(8))[0]
    buf = b""
    while len(buf) < n:
        buf += s.recv(n - len(buf))
    if op == 8:
        return None
    return buf.decode(errors="replace")


if __name__ == "__main__":
    s = connect()
    print("connected")
    send_text(s, {"t": "select", "window": 0})

    frames = 0
    t0 = time.time()
    while time.time() - t0 < 1.5:
        m = recv(s, 1.0)
        if not m:
            break
        j = json.loads(m)
        if j.get("changed"):
            frames += 1
    print(f"1.5s 内收到 {frames} 帧")

    print("发送文本（走剪贴板）…")
    send_text(s, {"t": "text", "text": "hello over websocket 中文"})

    # 收集后续帧
    got = 0
    t0 = time.time()
    while time.time() - t0 < 2.0:
        m = recv(s, 0.5)
        if not m:
            continue
        if json.loads(m).get("changed"):
            got += 1
    print(f"输入后收到 {got} 帧")
    s.close()
