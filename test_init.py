#!/usr/bin/env python3
"""Test the init system: boot services, supervisor respawn, restart limit, control commands."""
import subprocess
import time
import os
import socket
import json
import re
import sys
import shutil

QMP_PORT = 4451
VGA_DUMP = "/tmp/init_vga.bin"

failures = []


def check(cond, msg):
    status = "OK  " if cond else "FAIL"
    print(f"  [{status}] {msg}")
    if not cond:
        failures.append(msg)


def qmp_connect(qemu, attempts=20):
    for i in range(attempts):
        if qemu.poll() is not None:
            raise RuntimeError("QEMU exited before QMP was up")
        try:
            s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            s.settimeout(10)
            s.connect(("127.0.0.1", QMP_PORT))
            s.recv(4096)
            s.send((json.dumps({"execute": "qmp_capabilities"}) + "\n").encode())
            s.recv(4096)
            return s
        except (ConnectionRefusedError, OSError):
            s.close()
            time.sleep(0.5)
    raise RuntimeError("QMP socket never became available")


def qmp_send(s, cmd):
    s.send((json.dumps(cmd) + "\n").encode())
    time.sleep(0.3)
    try:
        return json.loads(s.recv(4096))
    except Exception:
        return None


def qmp_hmp(s, cmd):
    return qmp_send(s, {"execute": "human-monitor-command", "arguments": {"command-line": cmd}})


def fast_sendkey(s, key_qcode):
    s.send((json.dumps({
        "execute": "human-monitor-command",
        "arguments": {"command-line": f"sendkey {key_qcode}"},
    }) + "\n").encode())
    time.sleep(0.03)
    try:
        s.recv(65536)
    except Exception:
        pass


KEY_MAP = {
    " ": "spc",
    "/": "slash",
    ".": "dot",
    "+": "shift-equal",
    ">": "shift-dot",
    ":": "shift-semicolon",
}


def type_line(s, text):
    for ch in text:
        fast_sendkey(s, KEY_MAP.get(ch, ch))
    time.sleep(0.2)
    fast_sendkey(s, "ret")
    time.sleep(0.9)


def dump_vga(s):
    qmp_hmp(s, f'pmemsave 0xb8000 9600 "{VGA_DUMP}"')
    time.sleep(0.4)


def vga_text():
    try:
        data = open(VGA_DUMP, "rb").read()
    except FileNotFoundError:
        return []
    lines = []
    for row in range(30):
        chars = []
        for col in range(80):
            ch = data[row * 160 + col * 2]
            chars.append(chr(ch) if 32 <= ch < 127 else " ")
        lines.append("".join(chars))
    return lines


def screen_text():
    return "\n".join(vga_text())


ROW_RE = re.compile(r"^\s+(\S+)\s+(\S+)\s+(-|\d+)\s+(\S+)\s+(\d+)\s+(\S+)\s*$")


def parse_status(s):
    """Issues `clear` + `init status` and returns a dict {name: row}."""
    type_line(s, "clear")
    type_line(s, "init status")
    dump_vga(s)
    rows = {}
    for line in vga_text():
        m = ROW_RE.match(line)
        if m:
            rows[m.group(1)] = {
                "kind": m.group(2),
                "pid": m.group(3),
                "state": m.group(4),
                "restarts": int(m.group(5)),
                "resp": m.group(6),
            }
    return rows


def cat(s, path):
    type_line(s, f"cat {path}")
    dump_vga(s)
    return screen_text()


iso = os.path.join(os.path.dirname(os.path.abspath(__file__)), "target", "os.iso")
img = "/tmp/init_os.img"
qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-drive", f"file={img},format=raw,if=ide",
    "-display", "none",
    "-no-reboot",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", "file:/tmp/init_serial.log",
]
for p in ("/tmp/init_serial.log", VGA_DUMP):
    try:
        os.remove(p)
    except OSError:
        pass
shutil.copyfile(os.path.join(os.path.dirname(os.path.abspath(__file__)), "target", "os.img"), img)

subprocess.run(["pkill", "-f", f"qmp tcp:127.0.0.1:{QMP_PORT}"], stderr=subprocess.DEVNULL)
time.sleep(0.5)

print(f"Starting QEMU: {' '.join(qemu_cmd)}")
qemu = subprocess.Popen(qemu_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

try:
    s = qmp_connect(qemu)
    print("Connected to QMP")

    print("Waiting for shell prompt...")
    for i in range(12):
        time.sleep(1)
        dump_vga(s)
        if "vitamin_os26" in screen_text():
            print(f"  t={i+1}s: shell ready")
            break
    else:
        check(False, "shell prompt appeared")
        sys.exit(1)
    check(True, "shell prompt appeared")

    # Let the crashy respawn loop hit the restart limit before asserting.
    time.sleep(3)

    print("\n== services started at boot ==")
    rows = parse_status(s)
    check("clock" in rows, "clock service present")
    check("ticker" in rows, "ticker service present")
    check("crashy" in rows, "crashy service present")
    check(rows.get("clock", {}).get("state") == "running", "clock is running")
    check(rows.get("ticker", {}).get("state") == "running", "ticker is running")
    check(rows.get("crashy", {}).get("state") == "failed", "crashy hit restart limit and failed")

    print("\n== ticker heartbeat ==")
    t1 = cat(s, "/tmp/ticker.log")
    check("ticker alive" in t1, "ticker writes heartbeat")
    m1 = re.search(r"ticks=(\d+)", t1)
    check(m1 is not None and int(m1.group(1)) > 0, "heartbeat has a positive tick count")

    print("\n== supervisor respawns a killed service ==")
    tpid = rows.get("ticker", {}).get("pid")
    check(tpid != "-" and tpid is not None, f"ticker has a live pid ({tpid})")
    type_line(s, f"kill {tpid}")
    dump_vga(s)
    time.sleep(4)
    rows = parse_status(s)
    trow = rows.get("ticker", {})
    check(trow.get("state") == "running", "ticker restarted and is running again")
    check(trow.get("restarts", 0) >= 1, "ticker restart counter incremented")
    check(trow.get("pid") != "-", "ticker got a fresh pid")

    print("\n== init stop / start ==")
    type_line(s, "init stop ticker")
    dump_vga(s)
    time.sleep(1)
    rows = parse_status(s)
    check(rows.get("ticker", {}).get("state") == "stopped", "init stop ticker -> stopped")
    type_line(s, "init start ticker")
    dump_vga(s)
    time.sleep(1)
    rows = parse_status(s)
    check(rows.get("ticker", {}).get("state") == "running", "init start ticker -> running")
    check(rows.get("ticker", {}).get("restarts", 0) == 0, "manual start resets restart counter")

    print("\n== init restart ==")
    type_line(s, "init restart ticker")
    dump_vga(s)
    time.sleep(1)
    rows = parse_status(s)
    check(rows.get("ticker", {}).get("state") == "running", "init restart ticker keeps it running")

    print("\n== init list + log ==")
    type_line(s, "init list")
    dump_vga(s)
    t = screen_text()
    check("clock" in t and "ticker" in t and "crashy" in t, "init list shows configured services")
    check("respawn" in t, "init list shows respawn flag")

    t = cat(s, "/var/log/init.log")
    check("[init] boot:" in t and "supervisor started" in t, "boot events logged")
    check("[init] respawn" in t, "respawn events logged")

    print("\n== clock command prints current time ==")
    type_line(s, "clock")
    dump_vga(s)
    t = screen_text()
    m = re.search(r"\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}", t)
    check(m is not None, "clock prints current time (YYYY-MM-DD HH:MM:SS)")

    print("\n== /etc/timezone default config ==")
    t = cat(s, "/etc/timezone")
    check(re.search(r"\d{2}:\d{2}:\d{2}", t) is not None,
          "/etc/timezone defaults to the current clock time")
    check("#" in t, "/etc/timezone has a header comment")

    print("\n== setting the clock via /etc/timezone ==")
    type_line(s, "echo 20:00:00 > /etc/timezone")
    type_line(s, "clear")
    type_line(s, "clock")
    dump_vga(s)
    t = screen_text()
    m0 = re.search(r"(\d{2}):\d{2}:\d{2}", t)
    check(m0 is not None, "clock prints a time after the set")
    check(m0.group(1) == "20", f"clock shows the set hour (got {m0.group(1)})")
    type_line(s, "echo 21:30:00 > /etc/timezone")
    type_line(s, "clear")
    type_line(s, "clock")
    dump_vga(s)
    t = screen_text()
    m1 = re.search(r"(\d{2}):\d{2}:\d{2}", t)
    check(m1 is not None, "clock prints a time after the second set")
    check(m1.group(1) == "21", f"second set applies (got {m1.group(1)})")

    print("\n-- shell still responsive --")
    type_line(s, "echo ok")
    dump_vga(s)
    check("ok" in screen_text(), "shell works after init commands")

    s.close()
except Exception as e:
    print(f"Error: {e}")
    import traceback
    traceback.print_exc()
    failures.append(f"test harness error: {e}")
finally:
    qemu.terminate()
    time.sleep(1)
    if qemu.poll() is None:
        qemu.kill()
    qemu.wait()

print(f"\nQEMU exit code: {qemu.returncode}")
if failures:
    print(f"\n{len(failures)} check(s) FAILED:")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)
print("\nALL CHECKS PASSED")
