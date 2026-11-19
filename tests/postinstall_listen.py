#!/usr/bin/env python3
"""YAM postinstall keeps the listen address of an existing boot hook."""
import os
import pathlib
import signal
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
MANAGED = "#!/bin/sh\n# Managed by Luna YAM package\nset -eu\nlisten='{}'\n"


def run(existing=None, env_listen=None):
    script = (ROOT / "tools/luna-postinstall.sh").read_text()
    with tempfile.TemporaryDirectory(prefix="luna-postinstall-") as tmp:
        root = pathlib.Path(tmp)
        init_dir = root / "init.d"
        init_dir.mkdir()
        init = init_dir / "S99luna"
        if existing is not None:
            init.write_text(MANAGED.format(existing))
            init.chmod(0o755)
        stubs = root / "stubs"
        stubs.mkdir()
        # The hook and the generated init remount / and link libasound;
        # neither may touch the real system in a test.
        for name in ("mount", "ln"):
            stub = stubs / name
            stub.write_text('#!/bin/sh\ntouch "$LUNA_TEST/{}-called"\nexit 0\n'.format(name))
            stub.chmod(0o755)
        fake = '#!/bin/sh\nprintf "%s\\n" "$@" >"$LUNA_TEST/args"\nexec sleep 10\n'
        for bin_dir in (root / "pkg/bin", root / "yam/bin"):
            bin_dir.mkdir(parents=True)
            (bin_dir / "luna").write_text(fake)
            (bin_dir / "luna").chmod(0o755)
        env = dict(
            os.environ,
            LUNA_TEST=tmp,
            YAM_PKG=str(root / "pkg"),
            YAM_ROOT=str(root / "yam"),
            YAM_VAR=str(root / "var"),
            YAM_RUN=str(root / "run"),
        )
        env.pop("LUNA_LISTEN", None)
        if env_listen is not None:
            env["LUNA_LISTEN"] = env_listen
        env["PATH"] = str(stubs) + ":" + env["PATH"]
        script = script.replace("/etc/init.d", str(init_dir))
        try:
            subprocess.run(["sh", "-s"], input=script, text=True, env=env, check=True,
                           capture_output=True, timeout=10)
            lines = [l for l in init.read_text().splitlines() if l.startswith("listen=")]
            args = (root / "args").read_text().splitlines()
            return lines, args, (root / "pwned").exists()
        finally:
            pidfile = root / "var/luna/luna.pid"
            if pidfile.exists():
                try:
                    os.kill(int(pidfile.read_text()), signal.SIGTERM)
                except (ProcessLookupError, ValueError):
                    pass


def check(label, expected, **kwargs):
    lines, args, pwned = run(**kwargs)
    assert lines == ["listen='{}'".format(expected)], (label, lines)
    assert args[:2] == ["--listen", expected], (label, args)
    assert not pwned, label
    print(label, "->", expected, ": ok")


def main():
    check("fresh install", "0.0.0.0:8099")
    check("update keeps loopback", "127.0.0.1:8099", existing="127.0.0.1:8099")
    check("update keeps IPv6 loopback", "[::1]:8099", existing="[::1]:8099")
    check("LUNA_LISTEN wins", "0.0.0.0:9000", existing="127.0.0.1:8099", env_listen="0.0.0.0:9000")
    check("unsafe value ignored", "0.0.0.0:8099", existing="$(touch $LUNA_TEST/pwned)")


if __name__ == "__main__":
    main()
