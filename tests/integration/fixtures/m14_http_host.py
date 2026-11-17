"""Run the worker's application through HTTP for the live acceptance fixture."""

import json
import runpy
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path


root = Path(__file__).resolve().parents[2]
handle_request = runpy.run_path(str(root / "src/backend/server.py"))["handle"]


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        self.respond(None)

    def do_POST(self):
        try:
            body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        except (ValueError, UnicodeError):
            body = None
        self.respond(body)

    def respond(self, body):
        status, result = handle_request(self.command, self.path, body)
        payload = json.dumps(result, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


with HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler) as server:
    print(server.server_port, flush=True)
    server.serve_forever()
