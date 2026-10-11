"""Minimal RFC6455 text-frame client - just enough to drive video-sync-play.

Stdlib only, so the tests need nothing installed either.
"""
import socket, base64, os, json, struct, threading, time

class WS:
    def __init__(self, host='127.0.0.1', port=8787, path='/ws'):
        self.sock = socket.create_connection((host, port))
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall((
            f"GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\n"
            "Upgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        ).encode())
        buf = b""
        while b"\r\n\r\n" not in buf:
            buf += self.sock.recv(4096)
        assert b"101" in buf.split(b"\r\n")[0], buf[:120]
        self.rest = buf.split(b"\r\n\r\n", 1)[1]
        self.inbox = []
        self.alive = True
        threading.Thread(target=self._reader, daemon=True).start()

    def _recv(self, n):
        while len(self.rest) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise ConnectionError
            self.rest += chunk
        out, self.rest = self.rest[:n], self.rest[n:]
        return out

    def _reader(self):
        try:
            while True:
                b1, b2 = self._recv(2)
                op, ln = b1 & 0x0F, b2 & 0x7F
                if ln == 126:   ln = struct.unpack('!H', self._recv(2))[0]
                elif ln == 127: ln = struct.unpack('!Q', self._recv(8))[0]
                data = self._recv(ln)
                if op == 8: break
                if op == 1: self.inbox.append(json.loads(data))
        except Exception:
            pass
        self.alive = False

    def send(self, obj):
        data = json.dumps(obj).encode()
        mask = os.urandom(4)
        head = bytes([0x81])
        n = len(data)
        if n < 126:    head += bytes([0x80 | n])
        elif n < 65536: head += bytes([0x80 | 126]) + struct.pack('!H', n)
        else:           head += bytes([0x80 | 127]) + struct.pack('!Q', n)
        self.sock.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(data)))

    def close(self):
        # shutdown() forces the FIN out even though the reader thread still
        # holds a reference to the socket; close() alone would not.
        try: self.sock.shutdown(socket.SHUT_RDWR)
        except Exception: pass
        try: self.sock.close()
        except Exception: pass

    def ping(self):
        self.send({"t": "ping", "c": 0})

    def last_state(self):
        for m in reversed(self.inbox):
            if m.get('t') == 'state': return m
        return None

    def wait_state(self, pred, timeout=3.0):
        end = time.time() + timeout
        while time.time() < end:
            s = self.last_state()
            if s and pred(s): return s
            time.sleep(0.05)
        return self.last_state()

def summary(s):
    if not s: return 'NO STATE'
    peers = ','.join(f"{p['name']}[{'P' if p['paused'] else '-'}{'S' if p['stalled'] else '-'}{'Q' if p['silent'] else '-'}]"
                     for p in sorted(s['peers'], key=lambda p: p['name']))
    return f"playing={str(s['playing']):5} pos={s['position']:6.2f} peers={peers}"
