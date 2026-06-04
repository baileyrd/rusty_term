"""Smoke test for rusty_term's config file support (Windows ConPTY path).

1. Spawns rusty_term.exe with --config pointing at a config whose `shell`
   key launches cmd.exe with a marker banner -- the marker proves the
   configured shell (not %COMSPEC%) was spawned through the whole bridge.
2. Spawns it with a config full of bad keys/values -- the terminal must
   still start (warnings only), proving config errors are never fatal.
"""

import os
import sys
import tempfile
import threading
import time

from winpty import PtyProcess

BIN = r"C:\dev\rusty_term\target\debug\rusty_term.exe"
CONFIG = r"C:\dev\rusty_term\extra\smoke_config.toml"


class Output:
    def __init__(self, proc):
        self.proc = proc
        self.buf = []
        self.lock = threading.Lock()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                data = self.proc.read(4096)
            except Exception:
                return
            if not data:
                return
            with self.lock:
                self.buf.append(data)

    def text(self):
        with self.lock:
            return "".join(self.buf)

    def wait_for(self, needle, timeout, start=0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            t = self.text()
            if needle in t[start:]:
                return t
            time.sleep(0.1)
        return None


def end(proc):
    try:
        proc.write("exit\r\n")
        deadline = time.time() + 8
        while proc.isalive() and time.time() < deadline:
            time.sleep(0.2)
        if proc.isalive():
            proc.terminate(force=True)
            return False
    except Exception:
        pass
    return True


def main():
    failures = []

    # --- 1. Configured shell is spawned ---
    proc = PtyProcess.spawn(f"{BIN} --config {CONFIG}", dimensions=(30, 100))
    out = Output(proc)
    if out.wait_for("CONFIG-SHELL-OK", 8):
        print("PASS  config shell: configured `shell` key spawned (marker seen)")
    else:
        failures.append("config-shell")
        print("FAIL  config shell: marker not seen; got %r" % out.text()[-300:])
    if not end(proc):
        failures.append("config-shell-exit")
        print("FAIL  config shell: did not exit cleanly")

    # --- 2. Broken config is non-fatal ---
    bad = os.path.join(tempfile.gettempdir(), "rusty_bad_config.toml")
    with open(bad, "w") as f:
        f.write('shell = 42\nnonsense\n[colors]\nforeground = "#zz"\nbogus = 1\n')
    proc = PtyProcess.spawn(f"{BIN} --config {bad}", dimensions=(30, 100))
    out = Output(proc)
    if out.wait_for(">", 8):
        print("PASS  bad config: terminal still starts (warnings only)")
    else:
        failures.append("bad-config")
        print("FAIL  bad config: no prompt; got %r" % out.text()[-300:])
    if not end(proc):
        failures.append("bad-config-exit")
        print("FAIL  bad config: did not exit cleanly")

    print()
    if failures:
        print("RESULT: FAIL (%s)" % ", ".join(failures))
        return 1
    print("RESULT: ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
