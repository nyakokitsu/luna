#!/usr/bin/env python3
"""Exercise both embedded remote installers without a writable system root."""
import os
import pathlib
import re
import signal
import subprocess
import tempfile
import time

ROOT = pathlib.Path(__file__).resolve().parent.parent


def main():
    shell = (ROOT / "install.sh").read_text()
    remote_sh = re.findall(r"cat >\"\$REMOTE\" <<'REMOTE'\n(.*?)\nREMOTE", shell, re.S)[-1]
    powershell = (ROOT / "install.ps1").read_text()
    remote_ps = re.findall(r"\$remoteText = @'\n(.*?)\n'@", powershell, re.S)[-1]
    for label, script in [("sh", remote_sh), ("ps1", remote_ps)]:
        with tempfile.TemporaryDirectory(prefix="luna-ro-install-") as tmp:
            root = pathlib.Path(tmp)
            (root / "bin").mkdir()
            # This binary stands in for Luna; the test must never reach mount.
            binary = root / "bin/luna.new"
            binary.write_text('#!/bin/sh\nprintf "%s\\n" "$@" >"$LUNA_ROOT/args"\nexec sleep 10\n')
            binary.chmod(0o755)
            mount = root / "bin/mount"
            mount.write_text('#!/bin/sh\ntouch "$LUNA_ROOT/mount-called"\nexit 1\n')
            mount.chmod(0o755)
            env = dict(os.environ, LUNA_ROOT=tmp, LUNA_BOOT_HOOK="0", LUNA_LISTEN="127.0.0.1:18123")
            env["PATH"] = str(root / "bin") + ":" + env["PATH"]
            subprocess.run(["sh", "-s"], input=script, text=True, env=env, check=True, timeout=5)
            try:
                assert not (root / "mount-called").exists()
                assert (root / "bin/luna").exists()
                assert (root / "data").is_dir()
                args = (root / "args").read_text().splitlines()
                assert args == ["--listen", "127.0.0.1:18123", "--data-dir", str(root / "data")], args
                print(label, "read-only installation and custom listen address: ok")
            finally:
                pid = int((root / "luna.pid").read_text())
                try:
                    os.kill(pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass


if __name__ == "__main__":
    main()
