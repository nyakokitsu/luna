#!/usr/bin/env python3
"""YAM postinstall and the boot hook it writes.

- An update keeps the listen address of an existing boot hook.
- Neither the postinstall nor the boot hook leaves / writable when it was
  read-only, and the boot hook remounts only to create a missing link.
"""
import os
import pathlib
import signal
import subprocess
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent
MANAGED = "#!/bin/sh\n# Managed by Luna YAM package\nset -eu\nlisten='{}'\n"
VENDOR_LIB = "/system/vendor/quasar/libs/libasound.so.2"
STUB = '#!/bin/sh\nprintf "%s\\n" "{} $*" >>"$LUNA_TEST/calls"\nexit 0\n'


def run(existing=None, env_listen=None, rootfs="ro", linked=False):
    script = (ROOT / "tools/luna-postinstall.sh").read_text()
    with tempfile.TemporaryDirectory(prefix="luna-postinstall-") as tmp:
        root = pathlib.Path(tmp)
        init_dir = root / "init.d"
        init_dir.mkdir()
        init = init_dir / "S99luna"
        if existing is not None:
            init.write_text(MANAGED.format(existing))
            init.chmod(0o755)
        mounts = root / "mounts"
        mounts.write_text("rootfs / rootfs rw 0 0\n/dev/root / ext4 {},relatime 0 0\n".format(rootfs))
        lib = root / "libasound.so.2"
        if linked:
            lib.symlink_to(VENDOR_LIB)
        stubs = root / "stubs"
        stubs.mkdir()
        # The hook and the generated init remount / and link libasound;
        # neither may touch the real system in a test.
        for name in ("mount", "ln"):
            stub = stubs / name
            stub.write_text(STUB.format(name))
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
        script = (
            script.replace("/etc/init.d", str(init_dir))
            .replace("/proc/mounts", str(mounts))
            .replace("/lib/libasound.so.2", str(lib))
        )
        try:
            subprocess.run(["sh", "-s"], input=script, text=True, env=env, check=True,
                           capture_output=True, timeout=10)
            lines = [l for l in init.read_text().splitlines() if l.startswith("listen=")]
            args = (root / "args").read_text().splitlines()
            calls_file = root / "calls"
            calls = calls_file.read_text().splitlines() if calls_file.exists() else []
            return lines, args, calls, (root / "pwned").exists()
        finally:
            pidfile = root / "var/luna/luna.pid"
            if pidfile.exists():
                try:
                    os.kill(int(pidfile.read_text()), signal.SIGTERM)
                except (ProcessLookupError, ValueError):
                    pass


def check_listen(label, expected, **kwargs):
    lines, args, _, pwned = run(**kwargs)
    assert lines == ["listen='{}'".format(expected)], (label, lines)
    assert args[:2] == ["--listen", expected], (label, args)
    assert not pwned, label
    print(label, "->", expected, ": ok")


RW = "mount -o remount,rw /"
RO = "mount -o remount,ro /"


def check_rootfs(label, rootfs, linked, expected):
    _, _, calls, _ = run(rootfs=rootfs, linked=linked)
    # ln's last argument is the temporary link path; keep only the command.
    calls = ["ln" if call.startswith("ln ") else call for call in calls]
    assert calls == expected, (label, calls)
    if rootfs == "ro":
        assert calls[-1] == RO, (label, calls)
    print(label, ": ok")


def main():
    check_listen("fresh install", "0.0.0.0:8099")
    check_listen("update keeps loopback", "127.0.0.1:8099", existing="127.0.0.1:8099")
    check_listen("update keeps IPv6 loopback", "[::1]:8099", existing="[::1]:8099")
    check_listen("LUNA_LISTEN wins", "0.0.0.0:9000", existing="127.0.0.1:8099",
                 env_listen="0.0.0.0:9000")
    check_listen("unsafe value ignored", "0.0.0.0:8099", existing="$(touch $LUNA_TEST/pwned)")

    # Postinstall remounts around writing the hook; the hook remounts only to link.
    check_rootfs("read-only /, link missing: link once, / read-only again", "ro", False,
                 [RW, RW, "ln", RO, RO])
    check_rootfs("read-only /, link present: hook does not remount", "ro", True, [RW, RO])
    check_rootfs("writable /, link missing: no remount", "rw", False, ["ln"])
    check_rootfs("writable /, link present: nothing to do", "rw", True, [])


if __name__ == "__main__":
    main()
