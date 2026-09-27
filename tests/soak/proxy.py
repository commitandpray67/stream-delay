#!/usr/bin/env python3
"""A TCP proxy for the soak test that can take the destination down.

    proxy.py LISTEN_PORT TARGET_PORT DOWN_FLAG

Forwards 127.0.0.1:LISTEN_PORT to 127.0.0.1:TARGET_PORT. While the file
DOWN_FLAG exists, the destination is down: every connection is reset on the
relay's side (closed, cleanly, on the sink's, so its decoder sees the stream
end rather than an error), and new ones are reset as soon as they come.
"""

import os
import socket
import struct
import sys
import threading
import time

listen_port, target_port, flag = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
pairs = []
lock = threading.Lock()


def close(sock, reset):
    """Closes `sock`, waking any thread reading it; with `reset`, with a TCP reset."""
    try:
        sock.shutdown(socket.SHUT_RD)
    except OSError:
        pass
    if reset:
        try:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
        except OSError:
            pass
    try:
        sock.close()
    except OSError:
        pass


def pump(src, dst):
    try:
        while True:
            data = src.recv(65536)
            if not data:
                break
            dst.sendall(data)
    except OSError:
        pass


def serve(client, upstream):
    t = threading.Thread(target=pump, args=(upstream, client), daemon=True)
    t.start()
    pump(client, upstream)
    t.join(timeout=1)
    with lock:
        if (client, upstream) in pairs:
            pairs.remove((client, upstream))
    close(client, reset=True)
    close(upstream, reset=False)


def watch():
    while True:
        if os.path.exists(flag):
            with lock:
                down = list(pairs)
                pairs.clear()
            for client, upstream in down:
                close(client, reset=True)
                close(upstream, reset=False)
        time.sleep(0.2)


threading.Thread(target=watch, daemon=True).start()
server = socket.create_server(("127.0.0.1", listen_port))
while True:
    client, _ = server.accept()
    if os.path.exists(flag):
        close(client, reset=True)
        continue
    try:
        upstream = socket.create_connection(("127.0.0.1", target_port), timeout=5)
        upstream.settimeout(None)
    except OSError:
        # The sink is between connections: the relay tries again.
        close(client, reset=True)
        continue
    with lock:
        pairs.append((client, upstream))
    threading.Thread(target=serve, args=(client, upstream), daemon=True).start()
