"""Local TLS acceptance proxy: truncate one file body, transparently tunnel WSS."""
import select
import socket
import socketserver
import ssl
import threading


def head(stream):
    data = bytearray()
    while not data.endswith(b"\r\n\r\n"):
        byte = stream.recv(1)
        if not byte:
            raise ConnectionError("Peer closed during headers")
        data.extend(byte)
        if len(data) > 65536:
            raise ValueError("Oversized headers")
    return bytes(data)


def fields(header):
    return {line.split(b":", 1)[0].lower(): line.split(b":", 1)[1].strip()
            for line in header.split(b"\r\n")[1:] if b":" in line}


def copy_bytes(source, destination, count):
    while count:
        data = source.recv(min(count, 65536))
        if not data:
            raise ConnectionError("Peer closed during body")
        destination.sendall(data)
        count -= len(data)


class FaultProxy:
    def __init__(self, backend_port, certificate, key, ca):
        self.lock = threading.Lock()
        self.cut = False
        self.cut_path = None
        self.upload_cut_path = None
        self.credential_cut_path = None
        self.requests = {}
        frontend = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        frontend.load_cert_chain(str(certificate), str(key))
        frontend.set_alpn_protocols(["http/1.1"])
        backend = ssl.create_default_context(cafile=str(ca))
        backend.set_alpn_protocols(["http/1.1"])
        owner = self

        class Server(socketserver.ThreadingTCPServer):
            allow_reuse_address = True
            daemon_threads = True
            request_queue_size = 32

            def get_request(self):
                raw, address = super().get_request()
                raw.settimeout(10)
                try:
                    return frontend.wrap_socket(raw, server_side=True), address
                except (ssl.SSLError, OSError):
                    raw.close()
                    raise

        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                connection = None
                try:
                    request = head(self.request)
                    first = request.split(b"\r\n", 1)[0]
                    path = first.split(b" ")[1].decode("ascii")
                    request_fields = fields(request)
                    websocket = request_fields.get(b"upgrade", b"").lower() == b"websocket"
                    raw = socket.create_connection(("127.0.0.1", backend_port), timeout=10)
                    connection = backend.wrap_socket(raw, server_hostname="localhost")
                    if not websocket:
                        lines = request.split(b"\r\n")
                        request = b"\r\n".join([lines[0], *[line for line in lines[1:] if line and not line.lower().startswith(b"connection:")], b"Connection: close", b"", b""])
                    connection.sendall(request)
                    copy_bytes(self.request, connection, int(request_fields.get(b"content-length", b"0")))
                    response = head(connection)
                    response_fields = fields(response)
                    self.request.sendall(response)
                    if websocket and response.split(b" ", 2)[1] == b"101":
                        while True:
                            ready = [stream for stream in (self.request, connection) if stream.pending()]
                            if not ready:
                                ready, _, _ = select.select([self.request, connection], [], [], 1)
                            for stream in ready:
                                data = stream.recv(65536)
                                if not data:
                                    return
                                (connection if stream is self.request else self.request).sendall(data)
                    else:
                        length = int(response_fields.get(b"content-length", b"0"))
                        truncate = False
                        if path.startswith("/v1/runner/transfers/input-") and "/chunks?" in path and response.split(b" ", 2)[1] == b"200":
                            with owner.lock:
                                owner.requests[path] = owner.requests.get(path, 0) + 1
                                if not owner.cut:
                                    owner.cut = True
                                    owner.cut_path = path
                                    truncate = True
                        if path.startswith("/v1/runner/artifacts/artifact-") and "/chunks?" in path and response.split(b" ", 2)[1] == b"200":
                            with owner.lock:
                                owner.requests[path] = owner.requests.get(path, 0) + 1
                                if owner.upload_cut_path is None:
                                    owner.upload_cut_path = path
                                    truncate = True
                        if path.startswith("/v1/runner/credential-grants/") and path.endswith("/refresh") and response.split(b" ", 2)[1] == b"200":
                            with owner.lock:
                                owner.requests[path] = owner.requests.get(path, 0) + 1
                                if owner.credential_cut_path is None:
                                    owner.credential_cut_path = path
                                    truncate = True
                        if truncate:
                            copy_bytes(connection, self.request, max(1, length // 2))
                            self.request.shutdown(socket.SHUT_RDWR)
                            return
                        copy_bytes(connection, self.request, length)
                except (OSError, ConnectionError, ValueError):
                    return
                finally:
                    if connection:
                        connection.close()

        self.server = Server(("127.0.0.1", 0), Handler)
        self.url = f"https://localhost:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)
