#!/usr/bin/env python3
"""Проверка выбора станции установщиками по luna-manifest.json."""
import json
import pathlib
import re
import subprocess
import sys
import tempfile

ROOT = pathlib.Path(__file__).resolve().parent.parent

CASES = [
    ("YandexStationMini_F6D0", False, "yandexmini"),
    ("yandexstationmini_ab12", False, "yandexmini"),
    ("yandex-mini2-16AK", False, "yandexmini_2"),
    ("Yandex-Station-Midi", False, "yandexmidi"),
    ("yandex-station-lite-2", False, "yandexplum"),
    ("yandexmini", False, "yandexmini"),
    ("yandexmini_2", False, "yandexmini_2"),
    ("yandexmini3", False, "yandexmini3"),
    ("bergamot", False, "yandexmini3"),
    ("yandexmini", True, "yandexmini"),
    ("cucumber", True, "yandexmidi"),
    ("plum", True, "yandexplum"),
    ("yandexstation_2", True, "yandexstation_2"),
]


def matcher() -> str:
    text = (ROOT / "install.sh").read_text(encoding="utf-8")
    found = re.search(r"<<'PY'\n(.*?)\nPY\n", text, re.S)
    if not found:
        raise SystemExit("matcher not found in install.sh")
    return found.group(1)


def main() -> int:
    manifest = json.loads((ROOT / "luna-manifest.json").read_text(encoding="utf-8"))
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        script = pathlib.Path(tmp, "match.py")
        script.write_text(matcher(), encoding="utf-8")
        data = pathlib.Path(tmp, "manifest.json")
        data.write_text(json.dumps(manifest), encoding="utf-8")
        for target, explicit, want in CASES:
            run = subprocess.run(
                [sys.executable, "-I", str(script), str(data), target, "1" if explicit else "0"],
                capture_output=True,
                text=True,
                check=False,
            )
            got = run.stdout.split("\t")[0] if run.returncode == 0 else "(none)"
            ok = got == want
            failures += not ok
            print(f"{'ok  ' if ok else 'FAIL'} {target!r:28} explicit={explicit!s:5} -> {got}")
    print("all passed" if not failures else f"{failures} failed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
