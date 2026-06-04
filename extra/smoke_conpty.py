"""Interactive smoke test for rusty_term's Windows ConPTY + tokio bridge.

Spawns rusty_term.exe inside a pseudoconsole (pywinpty), types a command,
verifies the round-trip (stdin -> bridge -> ConPTY -> cmd.exe -> output pipe
-> parser -> renderer), tests resize, then exits the shell and verifies
rusty_term itself terminates (the child-exit watcher).
"""

import sys
import threading
import time

from winpty import PtyProcess

BIN = r"C:\dev\rusty_term\target\debug\rusty_term.exe"


class Output:
    """Continuously drains the pty on a daemon thread (reads block in pywinpty)."""

    def __init__(self, proc):
        self.proc = proc
        self.buf = []
        self.lock = threading.Lock()
        t = threading.Thread(target=self._pump, daemon=True)
        t.start()

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


def main():
    failures = []
    proc = PtyProcess.spawn(BIN, dimensions=(30, 100))
    out = Output(proc)

    # 1. Startup: cmd.exe banner/prompt should render through the bridge.
    if out.wait_for(">", 8):
        print("PASS  startup: shell banner/prompt rendered")
    else:
        failures.append("startup")
        print("FAIL  startup: no prompt seen; got %r" % out.text()[-200:])

    # 2. Input path: type a command, expect its output echoed back.
    mark = len(out.text())
    proc.write("echo bridge&echo -roundtrip-ok\r\n")
    if out.wait_for("-roundtrip-ok", 8, start=mark):
        print("PASS  input: keystrokes forwarded, output parsed + rendered")
    else:
        failures.append("input")
        print("FAIL  input: marker not echoed; tail %r" % out.text()[-200:])

    # 3. Resize: shrink the host console; the 150ms poll should propagate it
    #    to ConPTY so `mode con` reports the new width.
    proc.setwinsize(24, 60)
    time.sleep(1.0)  # > resize poll interval
    mark = len(out.text())
    proc.write("mode con\r\n")
    got = out.wait_for("Columns:", 8, start=mark)
    if got and "60" in got[mark:].split("Columns:", 1)[-1][:20]:
        print("PASS  resize: poll propagated 100->60 cols to ConPTY")
    else:
        tail = (got or out.text())[-300:]
        failures.append("resize")
        print("FAIL  resize: new width not reflected; tail %r" % tail)

    # 4. Child exit: `exit` must terminate rusty_term itself (watcher thread).
    proc.write("exit\r\n")
    deadline = time.time() + 10
    while proc.isalive() and time.time() < deadline:
        time.sleep(0.2)
    if not proc.isalive():
        print("PASS  exit: child exit terminated rusty_term (watcher works)")
    else:
        failures.append("exit-watcher")
        print("FAIL  exit: rusty_term still alive 10s after shell exit")
        proc.terminate(force=True)

    print()
    if failures:
        print("RESULT: FAIL (%s)" % ", ".join(failures))
        return 1
    print("RESULT: ALL PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
