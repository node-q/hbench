"""A stand-in for the market API's `whoami` in the end-to-end test: it answers with the bearer token's claims
(not verified: it only shows which token hbench sent). Usage: market_api.py <port>"""

import base64
import json
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer


class H(BaseHTTPRequestHandler):
    def do_GET(self):
        auth = self.headers.get("Authorization", "")
        if self.path != "/v1/whoami" or not auth.startswith("Bearer "):
            self.send_response(401)
            self.end_headers()
            return
        payload = auth[7:].split(".")[1]
        claims = json.loads(base64.urlsafe_b64decode(payload + "=" * (-len(payload) % 4)))
        body = json.dumps({"person": {"email": claims.get("email"), "name": claims.get("name")}, "azp": claims.get("azp"), "iss": claims.get("iss")}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
