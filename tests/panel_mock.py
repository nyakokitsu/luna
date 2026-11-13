"""Loopback-only UI fixture. Never connects to a speaker or executes commands."""
import json
import re
import subprocess
from pathlib import Path
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PAGE = Path(__file__).resolve().parents[1] / "web/calibration.html"
state = {"settings": {"voice": True, "native_volume": True, "leds": True, "spectrum": True}, "native": {"spectrum_gain_r":1,"spectrum_gain_g":1,"spectrum_gain_b":1}, "presets": {}, "active": False}

class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass
    def send(self, value, code=200):
        data = json.dumps(value).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path in ("/", "/calibration"):
            data = PAGE.read_bytes()
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        elif self.path == "/api/device": self.send({"model": "yandexplum", "room": "TEST FIXTURE (not a speaker)"})
        elif self.path.endswith("/settings"): self.send(state["native"] if self.path.startswith('/api/led-panel/') else state["settings"])
        elif self.path.endswith("/presets"): self.send(state["presets"])
        elif self.path.endswith("/status"): self.send({"led_count":3,"preview":{"active":state["active"],"allowed":state["active"],"last_error":"","suppressed":""}})
        else: self.send({},404)

    def do_PUT(self):
        data = json.loads(self.rfile.read(int(self.headers.get("Content-Length",0))))
        if self.path.endswith("/settings"): state["native" if self.path.startswith('/api/led-panel/') else "settings"]=data
        elif self.path.endswith("/presets"): state["presets"]=data
        else: return self.send({},404)
        self.send({"saved":True,"restart_required":not self.path.startswith('/api/led-panel/')})

    def do_POST(self):
        data=json.loads(self.rfile.read(int(self.headers.get("Content-Length",0))))
        state["active"]=True
        self.send({"id":"fixture-lease","sent":data["colors"],"ttl_ms":4000})

    def do_DELETE(self):
        self.rfile.read(int(self.headers.get("Content-Length",0)))
        state["active"]=False
        self.send({"stopped":True})

if __name__ == "__main__":
    script=re.search(r"<script>(.*?)</script>",PAGE.read_text(),re.S).group(1)
    syntax=subprocess.run(["node","--check"],input=script,text=True)
    if syntax.returncode: raise SystemExit(syntax.returncode)
    ThreadingHTTPServer(("127.0.0.1",18797),Handler).serve_forever()
