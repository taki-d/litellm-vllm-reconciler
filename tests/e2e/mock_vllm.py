"""GPU-free discovery server; /test/models changes its served model list."""

import json
import os
from http.server import BaseHTTPRequestHandler, HTTPServer


class Handler(BaseHTTPRequestHandler):
    models = []

    def reply(self, status, payload):
        body = json.dumps(payload).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def authenticated(self):
        if self.headers.get("Authorization") == f"Bearer {os.environ['VLLM_API_KEY']}":
            return True
        self.reply(401, {"error": "unauthorized"})
        return False

    def do_GET(self):
        if not self.authenticated():
            return
        if self.path == "/v1/models":
            self.reply(200, {"object": "list", "data": [
                {"id": model, "object": "model", "created": 0, "owned_by": "vllm"}
                for model in self.models
            ]})
        else:
            self.reply(404, {"error": "not found"})

    def do_POST(self):
        if not self.authenticated():
            return
        if self.path != "/test/models":
            self.reply(404, {"error": "not found"})
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 65536:
                raise ValueError("invalid length")
            models = json.loads(self.rfile.read(length))["models"]
            if not isinstance(models, list) or any(
                not isinstance(model, str) or not model.strip() for model in models
            ):
                raise ValueError("invalid models")
        except (ValueError, KeyError, TypeError):
            self.reply(400, {"error": "expected models: list of nonempty strings"})
            return
        Handler.models = list(dict.fromkeys(models))
        self.reply(200, {"models": Handler.models})


if __name__ == "__main__":
    HTTPServer(("0.0.0.0", 8000), Handler).serve_forever()
