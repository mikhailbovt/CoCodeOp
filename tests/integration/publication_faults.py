"""Drop the checkout host's HTTP acknowledgement after PostgreSQL commits it."""

import contextlib
import http.client
import http.server
import json
import socket
import threading


class _PublicationProxy(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self._forward()

    def do_POST(self):
        self._forward()

    def _forward(self):
        length = int(self.headers.get("Content-Length", 0))
        assert 0 <= length <= 192 * 1024
        data = self.rfile.read(length) if length else None
        if self.server.mode == "run_mcp_unavailable" and self.path == "/run/mcp":
            self.server.dropped.set()
            self.send_response(503)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        if (self.server.mode == "release" and self.path.endswith("/coordinator/release-receipt")
                and self.server.armed.is_set() and data
                and json.loads(data)["outcome"]["kind"] == "accepted"):
            self._drop()
            return
        forwarded = ("Authorization", "Content-Type", "Accept", "MCP-Protocol-Version")
        headers = {name: self.headers[name] for name in forwarded if name in self.headers}
        connection = http.client.HTTPConnection(self.server.upstream, timeout=15)
        try:
            connection.request(self.command, self.path, body=data, headers=headers)
            response = connection.getresponse()
            status, content_type, body = response.status, response.getheader("Content-Type"), response.read()
        finally:
            connection.close()
        if self.server.observe:
            self.server.observe(self.path, data, status, body)
        if (self.server.mode == "publication" and status == 200 and self.path.endswith("/coordinator/publication-receipt")
                and self.server.armed.is_set() and data
                and json.loads(data)["outcome"]["status"] == "applied"):
            self._drop()
            return
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        if content_type:
            self.send_header("Content-Type", content_type)
        self.end_headers()
        with contextlib.suppress(ConnectionError):
            self.wfile.write(body)

    def _drop(self):
        self.server.dropped.set()
        self.close_connection = True
        with contextlib.suppress(OSError):
            self.connection.shutdown(socket.SHUT_RDWR)

    def log_message(self, _format, *_arguments):
        return


@contextlib.contextmanager
def publication_proxy(upstream, observe=None, mode="publication", tls=None):
    proxy = http.server.ThreadingHTTPServer(("127.0.0.1", 0), _PublicationProxy)
    if tls is not None:
        proxy.socket = tls.wrap_socket(proxy.socket, server_side=True)
    proxy.upstream = upstream
    proxy.observe = observe
    proxy.mode = mode
    proxy.endpoint = f"127.0.0.1:{proxy.server_port}"
    proxy.armed = threading.Event()
    proxy.dropped = threading.Event()
    thread = threading.Thread(target=proxy.serve_forever, daemon=True)
    thread.start()
    try:
        yield proxy
    finally:
        proxy.shutdown()
        thread.join(5)
        proxy.server_close()
