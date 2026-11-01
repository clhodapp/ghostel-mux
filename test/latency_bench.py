"""Echo latency: one typed byte until the terminal echoes it back, through
a session and on a bare PTY.  Prints the median and 99th percentile.

Run with GHOSTEL_MUX set to the binary.
"""

import os
import pty
import socket
import statistics
import subprocess
import tempfile
import time

from session_test import Ctl

BINARY = os.environ["GHOSTEL_MUX"]
ROUNDS = 2000


def measure(send, recv):
    samples = []
    for _ in range(ROUNDS):
        start = time.perf_counter()
        send(b"x")
        got = b""
        while b"x" not in got:
            got += recv()
        samples.append((time.perf_counter() - start) * 1e6)
    samples.sort()
    return statistics.median(samples), samples[int(len(samples) * 0.99)]


# `cat` with the line discipline's echo on: every byte typed comes straight
# back from the kernel, so the measurement is the transport alone.
with tempfile.TemporaryDirectory() as tmp:
    env = dict(os.environ, XDG_RUNTIME_DIR=tmp, GHOSTEL_MUX_LAUNCHER="fork")
    sid = subprocess.run([BINARY, "start"], env=env, capture_output=True, text=True, check=True).stdout.strip()
    base = os.path.join(tmp, "ghostel-mux", sid)
    ctl = Ctl(os.path.join(base, "ctl"))
    ctl.send(op="spawn", client="bench", argv=["/bin/sh", "-c", "exec cat > /dev/null"],
             env=["PATH=" + os.environ["PATH"]], cwd=tmp, rows=24, cols=80, scrollback=1 << 20)
    assert ctl.event()["type"] == "attached"
    data = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    data.connect(os.path.join(base, "data"))
    time.sleep(0.2)
    median, p99 = measure(data.sendall, lambda: data.recv(4096))
    print(f"session   median {median:6.1f} us  p99 {p99:6.1f} us")
    subprocess.run([BINARY, "kill", sid], env=env, check=True)

pid, fd = pty.fork()
if pid == 0:
    os.execvp("sh", ["sh", "-c", "exec cat > /dev/null"])
time.sleep(0.2)
median, p99 = measure(lambda b: os.write(fd, b), lambda: os.read(fd, 4096))
print(f"bare pty  median {median:6.1f} us  p99 {p99:6.1f} us")
os.kill(pid, 9)
