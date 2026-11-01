"""Stand-in for a session: greet, echo one input chunk, then close."""
import os
import socket
import sys

path = sys.argv[1]
if os.path.exists(path):
    os.unlink(path)
srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
srv.bind(path)
srv.listen(1)
print("ready", flush=True)
conn, _ = srv.accept()
conn.sendall(b"hello from socket\r\n")
data = conn.recv(4096)
conn.sendall(b"got:" + data + b"\r\n")
conn.recv(4096)  # wait for "bye"
conn.close()
srv.close()
