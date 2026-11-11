#!/usr/bin/env python3
"""极简控制客户端：往 agentc 的控制 socket 发一行 JSON，打印回包。"""
import json
import socket
import sys

SOCK = sys.argv[1] if len(sys.argv) > 1 and sys.argv[1].startswith("/") else "/home/calvin/projects/agentc/agentc.sock"
args = [a for a in sys.argv[1:] if not a.startswith("/")]


def send(payload, sock=SOCK, timeout=30):
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    s.settimeout(timeout)
    s.connect(sock)
    s.sendall((json.dumps(payload) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        chunk = s.recv(1 << 20)
        if not chunk:
            break
        buf += chunk
    s.close()
    return json.loads(buf.decode())


if __name__ == "__main__":
    cmd = json.loads(" ".join(args))
    print(json.dumps(send(cmd), indent=2))
