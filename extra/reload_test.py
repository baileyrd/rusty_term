import time, threading, sys, os, tempfile
from winpty import PtyProcess

BIN = r"C:\dev\rusty_term\target\debug\rusty_term.exe"
cfg = os.path.join(tempfile.gettempdir(), "rusty_reload_test.toml")

with open(cfg, "w") as f:
    f.write('theme = "gruvbox-dark"\n')

proc = PtyProcess.spawn(f"{BIN} --config {cfg}", dimensions=(30, 100))
buf = []
def pump():
    while True:
        try:
            d = proc.read(4096)
        except Exception:
            return
        if not d:
            return
        buf.append(d)
threading.Thread(target=pump, daemon=True).start()

def wait_for(needle, timeout):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if needle in "".join(buf):
            return True
        time.sleep(0.2)
    return False

ok = True
# Phase 1: gruvbox at startup (fg ebdbb2 = 235;219;178).
if wait_for("38;2;235;219;178", 10):
    print("PASS  startup: gruvbox fg rendered")
else:
    ok = False
    print("FAIL  startup: gruvbox fg not seen")

# Phase 2: rewrite the config to dracula; the watcher should retheme live.
time.sleep(1)
mark = len("".join(buf))
with open(cfg, "w") as f:
    f.write('theme = "dracula"\n')
# dracula fg f8f8f2 = 248;248;242 on bg 282a36 = 40;42;54
deadline = time.time() + 10
seen = False
while time.time() < deadline:
    if "38;2;248;248;242" in "".join(buf)[mark:]:
        seen = True
        break
    time.sleep(0.2)
if seen:
    print("PASS  reload: dracula fg rendered after config save (no restart)")
else:
    ok = False
    print("FAIL  reload: dracula fg not seen after save")

try:
    proc.write("exit\r\n")
    time.sleep(2)
    if proc.isalive():
        proc.terminate(force=True)
except Exception:
    pass
os.remove(cfg)
print()
print("RESULT:", "ALL PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
