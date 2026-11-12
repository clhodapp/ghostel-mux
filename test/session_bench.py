"""Throughput of a session without Emacs: PTY output through the session's
terminal and out of its data socket, read as fast as Python can.

Run with GHOSTEL_MUX set to the binary.
"""

import os
import socket
import subprocess
import tempfile
import time

from session_test import Ctl

BINARY = os.environ["GHOSTEL_MUX"]
LINES = 4_000_000

with tempfile.TemporaryDirectory() as tmp:
    env = dict(os.environ, XDG_RUNTIME_DIR=tmp, GHOSTEL_MUX_LAUNCHER="fork")
    for _ in range(3):
        sid = subprocess.run([BINARY, "start"], env=env, capture_output=True, text=True, check=True).stdout.strip()
        base = os.path.join(tmp, "ghostel-mux", sid)
        ctl = Ctl(os.path.join(base, "ctl"))
        ctl.send(op="spawn", client="bench", argv=["/bin/sh", "-c", f"seq 1 {LINES}"],
                 env=["PATH=" + os.environ["PATH"]], cwd=tmp, rows=50, cols=200,
                 scrollback=5 << 20)
        assert ctl.event()["type"] == "attached"
        data = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        start = time.time()
        data.connect(os.path.join(base, "data"))
        total = 0
        tail = b""
        last = f"\r\n{LINES}\r\n".encode()
        while last not in tail:
            chunk = data.recv(1 << 20)
            if not chunk:
                break
            total += len(chunk)
            tail = (tail + chunk)[-64:]
        elapsed = time.time() - start
        print(f"session  {total / 1e6:.1f} MB in {elapsed:.3f} s  {total / 1e6 / elapsed:.0f} MB/s")
        ctl.event()
        ctl.send(op="exit-ack")
