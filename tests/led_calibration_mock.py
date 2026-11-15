"""Loopback-only browser fixtures. No hardware, ADB, plugin proxy or config writes."""
import json
import re
import subprocess
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlsplit

PAGE = Path(__file__).resolve().parents[1] / "web/led-calibration.html"
CASES = {
    "mini": [("logo", 6)],
    "midi": [("ring", 24), ("logo", 6)],
    "pickle": [("ring", 16), ("logo", 6)],
    "mango": [("ring", 4), ("logo", 3)],
    "orion": [("ring", 36)],
    "max": [("ring", 24)],
    "future": [("ring", 12), ("logo", 2)],
    "station2": [("ring", 89)],
    "none": [],
}
state = {"case": "midi", "gains": {"gain_r": 1, "gain_g": 1, "gain_b": 1},
         "presets": {}, "expires": 0, "last": None}

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass
    def send(self, value, status=200):
        body = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def do_GET(self):
        url = urlsplit(self.path)
        if url.path == "/calibration":
            case = parse_qs(url.query).get("fixture", ["midi"])[0]
            if case not in CASES:
                return self.send({}, 404)
            state.update(case=case, expires=0, last=None)
            body = PAGE.read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(body)
            return
        if url.path == "/api/led-panel/status":
            reason = "Native framebuffer ownership is not qualified" if state["case"] == "station2" else None
            zones = [{"id": name, "count": count, "supported": not reason} for name, count in CASES[state["case"]]]
            active = time.monotonic() < state["expires"]
            return self.send({"zones": zones, "unavailable_reason": reason,
                "preview": {"active": active, "allowed": active, "suppressed": "", "last_error": ""}})
        if url.path == "/api/led-panel/settings":
            return self.send(state["gains"])
        if url.path == "/api/led-panel/presets":
            return self.send(state["presets"])
        if url.path == "/fixture-state":
            return self.send(state)
        if url.path == "/favicon.ico":
            return self.send({})
        self.send({}, 404)
    def body(self):
        return json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))))
    def do_POST(self):
        if self.path == "/__shutdown":
            self.send({})
            threading.Thread(target=self.server.shutdown, daemon=True).start()
            return
        if self.headers.get("X-Luna-Local") != "1":
            return self.send({}, 403)
        body = self.body()
        if self.path != "/api/led-panel/preview":
            return self.send({}, 404)
        count = dict(CASES[state["case"]]).get(body.get("zone"))
        if count != len(body.get("colors", [])) or state["case"] == "station2":
            return self.send({}, 400)
        state["last"] = body
        state["expires"] = time.monotonic() + 4
        self.send({"id": "fixture-lease", "ttl_ms": 4000, "sent": body["colors"]})
    def do_DELETE(self):
        self.body()
        if self.headers.get("X-Luna-Local") != "1":
            return self.send({}, 403)
        state["expires"] = 0
        self.send({"stopped": True})
    def do_PUT(self):
        body = self.body()
        if self.headers.get("X-Luna-Local") != "1":
            return self.send({}, 403)
        if self.path == "/api/led-panel/settings":
            state["gains"] = body
        elif self.path == "/api/led-panel/presets":
            state["presets"] = body
        else:
            return self.send({}, 404)
        self.send({"saved": True, "restart_required": False})

if __name__ == "__main__":
    script = re.search(r"<script>(.*?)</script>", PAGE.read_text(), re.S).group(1)
    subprocess.run(["node", "--check"], input=script, text=True, check=True)
    ThreadingHTTPServer(("127.0.0.1", 18806), Handler).serve_forever()
