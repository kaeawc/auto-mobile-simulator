# Test servers on the host's LAN address: TCP echo, UDP echo, WebSocket echo, HTTP bytes.
import base64, hashlib, http.server, socket, socketserver, struct, sys, threading
HOST = sys.argv[1]
class TCPEcho(socketserver.BaseRequestHandler):
    def handle(self):
        while True:
            d = self.request.recv(65536)
            if not d: return
            self.request.sendall(d)
class UDPEcho(socketserver.BaseRequestHandler):
    def handle(self):
        data, sock = self.request
        sock.sendto(data, self.client_address)
class WSEcho(socketserver.BaseRequestHandler):
    def handle(self):
        s = self.request; head = b""
        while b"\r\n\r\n" not in head:
            c = s.recv(4096)
            if not c: return
            head += c
        key = [l.split(b":",1)[1].strip() for l in head.split(b"\r\n") if l.lower().startswith(b"sec-websocket-key")][0]
        acc = base64.b64encode(hashlib.sha1(key + b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11").digest())
        s.sendall(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: " + acc + b"\r\n\r\n")
        def rx(n):
            b = b""
            while len(b) < n:
                c = s.recv(n - len(b))
                if not c: raise EOFError
                b += c
            return b
        try:
            while True:
                b0, b1 = rx(2)
                op, n = b0 & 15, b1 & 127
                if n == 126: n = struct.unpack(">H", rx(2))[0]
                elif n == 127: n = struct.unpack(">Q", rx(8))[0]
                mask = rx(4) if b1 & 128 else b"\0\0\0\0"
                p = bytes(x ^ mask[i % 4] for i, x in enumerate(rx(n)))
                if op == 8: return
                if op == 9: op = 10
                hdr = bytes([0x80 | op]) + (bytes([n]) if n < 126 else (b"\x7e" + struct.pack(">H", n) if n < 65536 else b"\x7f" + struct.pack(">Q", n)))
                s.sendall(hdr + p)
        except (EOFError, ConnectionError):
            return
class HTTP(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        n = int(self.path.rsplit("/", 1)[-1]) if self.path.startswith("/bytes/") else 5
        self.send_response(200); self.send_header("Content-Length", str(n)); self.end_headers()
        chunk = b"x" * 65536
        while n > 0:
            k = min(n, len(chunk)); self.wfile.write(chunk[:k]); n -= k
    def log_message(self, *a): pass
class T(socketserver.ThreadingMixIn, socketserver.TCPServer): allow_reuse_address = True; daemon_threads = True
class U(socketserver.ThreadingMixIn, socketserver.UDPServer): allow_reuse_address = True; daemon_threads = True
class H(socketserver.ThreadingMixIn, http.server.HTTPServer): allow_reuse_address = True; daemon_threads = True
servers = [T((HOST, 18083), TCPEcho), U((HOST, 18084), UDPEcho), T((HOST, 18082), WSEcho), H((HOST, 18081), HTTP)]
for srv in servers: threading.Thread(target=srv.serve_forever, daemon=True).start()
print("ready", flush=True)
threading.Event().wait()
