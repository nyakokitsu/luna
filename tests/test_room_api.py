#!/usr/bin/env python3
"""Host-only CLI/HTTP/runtime-token fixtures; never connect to a speaker."""
import concurrent.futures
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request


def main():
    binary = str(Path(sys.argv[1]).resolve())
    with tempfile.TemporaryDirectory(prefix="yam-v2-room-") as temporary:
        root = Path(temporary)
        data = root / "runtime"

        def cli(*args, ok=True):
            result = subprocess.run([binary, "--data-dir", str(data), "room", *args], capture_output=True, text=True, timeout=10)
            assert (result.returncode == 0) == ok, result.stderr
            return result.stdout.strip()

        assert cli() == ""
        assert not data.exists(), "read-only CLI created daemon state"
        assert cli("  Кухня  ") == "Кухня"
        assert list(data.iterdir()) == [data / "room.toml"], "CLI initialized runtime/hardware"
        cli("bad\nroom", ok=False)
        cli("Кухня", "--clear", ok=False)
        assert cli() == "Кухня"
        data.joinpath("settings.toml").write_text('''allow_plugin_binaries = true
[led]
profile = "custom"
count = 1
sysfs_root = "/nonexistent-yam-room-fixture-leds"
[web]
ui = true
api = true
[updates]
enabled = false
interval_minutes = 360
''')
        # Known identity comes only from this fixture, not detection on the host.
        identity = {"model": "room-fixture", "architecture": "fixture", "features": [], "source": "fixture"}
        data.joinpath("device.toml").write_text('model = "room-fixture"\narchitecture = "fixture"\nfeatures = []\nsource = "fixture"\n')
        hardware_bytes = data.joinpath("device.toml").read_bytes()
        plugins = data / "plugins"
        plugin = plugins / "room-probe"
        plugin.mkdir(parents=True)
        plugin.joinpath("plugin.toml").write_text('''id = "room-probe"
name = "Host-only room token fixture"
version = "0.0.0"
entry = "run.sh"
permissions = ["exec"]
[launch]
kind = "binary"
interpreter = "sh"
path = "run.sh"
''')
        plugin.joinpath("run.sh").write_text('exec python3 probe.py\n')
        plugin.joinpath("probe.py").write_text('''import json,os,urllib.request
url=os.environ["YAM_API_URL"]+"/device"
request=urllib.request.Request(url,headers={"X-YAM-Token":os.environ["YAM_API_TOKEN"]})
with urllib.request.urlopen(request,timeout=3) as response:
 print("ROOM_TOKEN_PROBE="+json.dumps(json.load(response),ensure_ascii=False),flush=True)
''')
        plugins.joinpath("enabled.json").write_text('["room-probe"]')
        plugins.joinpath("services.json").write_text('[]')
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        url = f"http://127.0.0.1:{port}"
        proc = None
        log = root / "daemon.log"

        def request(path, payload=None, method=None, status=200, headers=None):
            method = method or ("PUT" if path == "/api/room" and payload is not None else None)
            req = urllib.request.Request(url + path, data=None if payload is None else json.dumps(payload).encode(), method=method,
                                         headers={"Content-Type": "application/json", **(headers or {})})
            try:
                with urllib.request.urlopen(req, timeout=5) as response:
                    code, raw = response.status, response.read()
            except urllib.error.HTTPError as error:
                code, raw = error.code, error.read()
            assert code == status, (path, code, raw)
            return json.loads(raw) if raw and raw[:1] in (b"{", b"[") else raw

        def start():
            with log.open("ab") as output:
                child = subprocess.Popen([binary, "--data-dir", str(data), "--listen", f"127.0.0.1:{port}"], stdout=output, stderr=output)
            for _ in range(100):
                if child.poll() is not None:
                    raise AssertionError(log.read_text())
                try:
                    request("/api/room")
                    return child
                except (OSError, urllib.error.URLError):
                    time.sleep(.05)
            child.terminate()
            child.wait(timeout=5)
            raise AssertionError("runtime did not start")

        try:
            proc = start()
            initial = request("/api/device")
            assert initial == {**identity, "room": "Кухня"}, initial
            assert request("/api/room") == {"room": "Кухня"}
            assert b'id="room"' in request("/")
            for value, normalized in [("  Детская  ", "Детская"), ("я" * 64, "я" * 64), (None, None), ("   ", None)]:
                assert request("/api/room", {"room": value}) == {"room": normalized}
                assert request("/api/device") == {**identity, "room": normalized}
            request("/api/room", {"room": "Гостиная"})
            for bad in [{"room": "x\ny"}, {"room": "я" * 65}, {"room": "Kitchen", "model": "spoof"}, {"room": 1}]:
                request("/api/room", bad, status=422 if isinstance(bad.get("room"), int) or "model" in bad else 400)
                assert request("/api/room")["room"] == "Гостиная"
            request("/api/runtime/room-probe/device", status=400)
            request("/api/runtime/room-probe/device", status=400, headers={"X-YAM-Token": "invalid"})
            request("/api/plugins/room-probe/invoke", {}, status=202)
            for _ in range(100):
                output = request("/api/plugins/room-probe/logs").decode()
                if "ROOM_TOKEN_PROBE=" in output:
                    snapshot = json.loads(output.split("ROOM_TOKEN_PROBE=", 1)[1].splitlines()[0])
                    assert snapshot == {**identity, "room": "Гостиная"}, snapshot
                    break
                time.sleep(.05)
            else:
                raise AssertionError(output)
            # CLI changes are visible immediately without daemon restart.
            cli("Кабинет")
            assert request("/api/device")["room"] == "Кабинет"
            with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
                list(pool.map(lambda i: request("/api/room", {"room": f"Комната {i}"}), range(24)))
            assert request("/api/room")["room"].startswith("Комната ")
            assert not list(data.glob(".room-*.tmp"))
            assert data.joinpath("device.toml").read_bytes() == hardware_bytes
            request("/api/room", {"room": "Спальня"})
            proc.terminate()
            proc.wait(timeout=5)
            proc = start()
            assert request("/api/device") == {**identity, "room": "Спальня"}
            assert cli("--clear") == ""
            assert request("/api/device")["room"] is None
            data.joinpath("room.toml").write_text("broken [")
            request("/api/room", status=400)
            request("/api/device", status=400)
            proc.terminate()
            proc.wait(timeout=5)
            result = subprocess.run([binary, "--data-dir", str(data), "--listen", f"127.0.0.1:{port}"], capture_output=True, timeout=10)
            assert result.returncode != 0 and b"invalid room.toml" in result.stderr
            cli(ok=False)
            assert data.joinpath("device.toml").read_bytes() == hardware_bytes
        finally:
            if proc is not None and proc.poll() is None:
                proc.terminate()
                proc.wait(timeout=5)
    print("V2_RUNTIME_ROOM_API_CLI_TOKEN_PERSISTENCE_PASS")


if __name__ == "__main__":
    main()
