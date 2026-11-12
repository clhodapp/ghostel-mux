"""Protocol tests for the ghostel-mux session process.

Run with GHOSTEL_MUX set to the binary.  Each test gets a separate
XDG_RUNTIME_DIR and starts sessions with the fork launcher.
"""

import json
import os
import socket
import subprocess
import tempfile
import time
import unittest

BINARY = os.environ["GHOSTEL_MUX"]


class Ctl:
    """A connection to a session's ctl socket."""

    def __init__(self, path):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.settimeout(5)
        self.buf = b""

    def send(self, **request):
        self.sock.sendall(json.dumps(request).encode() + b"\n")

    def _fill(self):
        chunk = self.sock.recv(65536)
        if not chunk:
            raise EOFError("ctl closed")
        self.buf += chunk

    def event(self):
        while b"\n" not in self.buf:
            self._fill()
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def raw(self, n):
        while len(self.buf) < n:
            self._fill()
        data, self.buf = self.buf[:n], self.buf[n:]
        return data

    def closed(self):
        try:
            return self.sock.recv(1) == b""
        except TimeoutError:
            return False

    def close(self):
        self.sock.close()


class SessionTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.env = dict(os.environ, XDG_RUNTIME_DIR=self.tmp.name, GHOSTEL_MUX_LAUNCHER="fork")

    def tearDown(self):
        for info in self.list():
            subprocess.run([BINARY, "kill", info["id"]], env=self.env, capture_output=True, check=False)
        self.tmp.cleanup()

    def mux(self, *args):
        out = subprocess.run([BINARY, *args], env=self.env, capture_output=True, text=True, check=True)
        return out.stdout

    def list(self):
        return json.loads(self.mux("list", "--json"))

    def dir(self, sid):
        return os.path.join(self.tmp.name, "ghostel-mux", sid)

    def start_shell(self):
        sid = self.mux("start").strip()
        ctl = Ctl(os.path.join(self.dir(sid), "ctl"))
        ctl.send(op="spawn", client="first", argv=["/bin/sh"],
                 env=["PATH=" + os.environ["PATH"], "TERM=xterm-256color", "PS1=$ "],
                 cwd=self.tmp.name, rows=24, cols=80, scrollback=1 << 20)
        event = ctl.event()
        self.assertEqual(event["type"], "attached", event)
        return sid, ctl, self.data(sid)

    def data(self, sid):
        data = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        data.connect(os.path.join(self.dir(sid), "data"))
        data.settimeout(5)
        return data

    def read_until(self, data, needle):
        seen = b""
        deadline = time.time() + 5
        while needle not in seen and time.time() < deadline:
            chunk = data.recv(65536)
            if not chunk:
                break
            seen += chunk
        self.assertIn(needle, seen)
        return seen

    def test_spawn_and_round_trip(self):
        sid, _ctl, data = self.start_shell()
        data.sendall(b"echo hello-$((40+2))\n")
        self.read_until(data, b"hello-42")
        info = self.list()[0]
        self.assertEqual(info["id"], sid)
        self.assertEqual(info["owner"]["client"], "first")
        self.assertTrue(info["tty"].startswith("/dev/pts/"))

    def test_detach_and_reattach_with_snapshot(self):
        sid, ctl, data = self.start_shell()
        # The arithmetic keeps the marker out of the echoed command line, so
        # seeing it means the printf before it has run.
        data.sendall(b"printf '\\033]2;my-title\\007'; echo marker-$((0+1))\n")
        self.read_until(data, b"marker-1")
        ctl.close()
        data.close()
        # The session keeps running detached and still sees output.
        deadline = time.time() + 5
        while self.list()[0]["owner"] is not None and time.time() < deadline:
            time.sleep(0.05)
        info = self.list()[0]
        self.assertIsNone(info["owner"])
        self.assertEqual(info["last_owner"], "first")
        self.assertEqual(info["title"], "my-title")

        ctl2 = Ctl(os.path.join(self.dir(sid), "ctl"))
        ctl2.send(op="attach", client="second")
        header = ctl2.event()
        self.assertEqual(header["type"], "snapshot")
        snap = ctl2.raw(header["bytes"])
        self.assertTrue(snap.startswith(b"GHOSTSNP"))
        attached = ctl2.event()
        self.assertEqual(attached["type"], "attached")
        data2 = self.data(sid)
        data2.sendall(b"echo marker-2\n")
        self.read_until(data2, b"marker-2")

    def test_busy_and_steal(self):
        sid, ctl, _data = self.start_shell()
        other = Ctl(os.path.join(self.dir(sid), "ctl"))
        other.send(op="attach", client="other")
        busy = other.event()
        self.assertEqual(busy["type"], "busy")
        self.assertEqual(busy["owner"]["client"], "first")

        other.send(op="attach", client="other", steal=True)
        self.assertEqual(other.event()["type"], "snapshot")
        stolen = ctl.event()
        self.assertEqual(stolen, {"type": "stolen", "by": "other"})

    def test_snapshot_leaves_owner(self):
        sid, ctl, _data = self.start_shell()
        other = Ctl(os.path.join(self.dir(sid), "ctl"))
        other.send(op="snapshot")
        header = other.event()
        self.assertEqual(header["type"], "snapshot")
        self.assertTrue(other.raw(header["bytes"]).startswith(b"GHOSTSNP"))
        info = other.event()
        self.assertEqual(info["type"], "info")
        self.assertEqual(info["info"]["owner"]["client"], "first")
        self.assertEqual(self.list()[0]["owner"]["client"], "first")
        self.assertFalse(ctl.closed())

    def test_evict(self):
        sid, ctl, _data = self.start_shell()
        self.mux("evict", sid)
        self.assertEqual(ctl.event(), {"type": "evicted"})
        self.assertIsNone(self.list()[0]["owner"])

    def test_exit_reaches_owner_after_output(self):
        sid, ctl, data = self.start_shell()
        data.sendall(b"echo last-words; exit 3\n")
        self.read_until(data, b"last-words")
        self.assertEqual(ctl.event(), {"type": "exit", "status": 3})
        ctl.send(op="exit-ack")
        deadline = time.time() + 5
        while os.path.exists(self.dir(sid)) and time.time() < deadline:
            time.sleep(0.05)
        self.assertFalse(os.path.exists(self.dir(sid)))

    def test_exit_while_detached_lingers_until_adopted(self):
        sid, ctl, data = self.start_shell()
        ctl.close()
        data.close()
        time.sleep(0.2)
        subprocess.run([BINARY, "info", sid], env=self.env, check=True, capture_output=True)
        # Exit from inside: reattach briefly to type, then detach again.
        ctl2 = Ctl(os.path.join(self.dir(sid), "ctl"))
        ctl2.send(op="attach", client="typist")
        ctl2.raw(ctl2.event()["bytes"])
        ctl2.event()
        data2 = self.data(sid)
        ctl2.close()
        data2.close()
        time.sleep(0.2)
        os.kill(self.list()[0]["pid"], 9)
        deadline = time.time() + 5
        while self.list()[0]["exit_status"] is None and time.time() < deadline:
            time.sleep(0.05)
        self.assertEqual(self.list()[0]["exit_status"], 137)
        ctl3 = Ctl(os.path.join(self.dir(sid), "ctl"))
        ctl3.send(op="attach", client="adopter")
        ctl3.raw(ctl3.event()["bytes"])
        self.assertEqual(ctl3.event()["type"], "attached")
        self.data(sid)
        self.assertEqual(ctl3.event(), {"type": "exit", "status": 137})

    def test_kill_ends_session(self):
        sid, ctl, data = self.start_shell()
        ctl.close()
        data.close()
        self.mux("kill", sid)
        deadline = time.time() + 7
        while os.path.exists(self.dir(sid)) and time.time() < deadline:
            time.sleep(0.05)
        self.assertFalse(os.path.exists(self.dir(sid)))

    def test_kill_before_spawn_ends_session(self):
        sid = self.mux("start").strip()
        self.mux("kill", sid)
        deadline = time.time() + 3
        while os.path.exists(self.dir(sid)) and time.time() < deadline:
            time.sleep(0.05)
        self.assertFalse(os.path.exists(self.dir(sid)))

    def test_data_socket_rejects_other_processes(self):
        sid, ctl, data = self.start_shell()
        ctl.close()
        data.close()
        ctl2 = Ctl(os.path.join(self.dir(sid), "ctl"))
        ctl2.send(op="attach", client="second")
        ctl2.raw(ctl2.event()["bytes"])
        ctl2.event()
        script = ("import socket,sys; s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]);"
                  "s.settimeout(2); print(repr(s.recv(10)))")
        out = subprocess.run(["python3", "-c", script, os.path.join(self.dir(sid), "data")],
                             capture_output=True, text=True, check=False)
        self.assertIn("b''", out.stdout)


if __name__ == "__main__":
    unittest.main()
