#!/usr/bin/env python3
"""Test preemptive multitasking: bg / ps / kill, background task logs, shell responsiveness.

Drives QEMU via QMP, reads the VGA screen. Keyboard has no shift support, so only
lowercase letters and digits are typed.
"""
import subprocess
import time
import os
import socket
import json
import sys

QMP_PORT = 4446
VGA_DUMP = "/tmp/mt_vga.bin"

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


def clear_and_run(s, command):
    type_line(s, "clear")
    type_line(s, command)


def find_pid(screen, name):
    for line in screen.splitlines():
        parts = line.split()
        if len(parts) >= 4 and name in parts[1]:
            try:
                return int(parts[0])
            except ValueError:
                pass
    return None


iso = os.path.join(os.path.dirname(os.path.abspath(__file__)), "target", "os.iso")
img = "/tmp/mt_os.img"
qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-drive", f"file={img},format=raw,if=ide",
    "-display", "none",
    "-no-reboot",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", "file:/tmp/mt_serial.log",
]
for p in ("/tmp/mt_serial.log", VGA_DUMP):
    try:
        os.remove(p)
    except OSError:
        pass
open(img, "wb").truncate(8 * 1024 * 1024)

subprocess.run(["pkill", "-f", f"qmp tcp:127.0.0.1:{QMP_PORT}"], stderr=subprocess.DEVNULL)
time.sleep(0.5)

print(f"Starting QEMU: {' '.join(qemu_cmd)}")
qemu = subprocess.Popen(qemu_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

try:
    s = qmp_connect(qemu)
    print("Connected to QMP")

    print("Waiting for shell prompt...")
    for i in range(10):
        time.sleep(1)
        dump_vga(s)
        if "vitamin_os26" in screen_text():
            print(f"  t={i+1}s: shell ready")
            break
    else:
        check(False, "shell prompt appeared")
        sys.exit(1)
    check(True, "shell prompt appeared")

    print("\n== bg prime 200000 ; shell responsive while it runs ==")
    clear_and_run(s, "bg prime 200000")
    dump_vga(s)
    check("Started background task prime" in screen_text(), "bg reports spawn of prime")

    type_line(s, "echo still alive")
    dump_vga(s)
    check("still alive" in screen_text(), "shell responsive while prime runs")

    type_line(s, "ps")
    dump_vga(s)
    t = screen_text()
    check("prime" in t and "shell" in t, "ps lists shell and prime")

    print("\n== prime completes, writes /tmp/prime.log ==")
    for i in range(10):
        time.sleep(1)
        clear_and_run(s, "cat /tmp/prime.log")
        dump_vga(s)
        if "primes<200000" in screen_text():
            break
    t = screen_text()
    check("primes<200000 count=17984" in t, "prime result in /tmp/prime.log")
    check("count=17984" in t, "prime counted 17984 primes")

    print("\n== bg clock ; periodic log grows ==")
    clear_and_run(s, "bg clock")
    dump_vga(s)
    check("Started background task clock" in screen_text(), "bg reports spawn of clock")

    time.sleep(3)
    clear_and_run(s, "cat /tmp/clock.log")
    dump_vga(s)
    t = screen_text()
    n_lines = sum(1 for line in t.splitlines() if line.startswith("clock "))
    check(n_lines >= 2, f"clock.log grew ({n_lines} entries)")
    check("clock 1" in t, "clock.log has first entry")

    print("\n== ps shows states; kill bg clock ==")
    clear_and_run(s, "ps")
    dump_vga(s)
    t = screen_text()
    check("prime" in t, "ps still lists prime (finished)")
    pid = find_pid(t, "clock")
    check(pid is not None, "ps shows clock pid")
    check(find_pid(t, "prime") is not None, "ps shows prime pid")

    if pid is not None:
        # clock is also a permanent init service, so ps lists two "clock" tasks:
        # the service (lowest pid) and the bg task just started (highest pid).
        all_clock_pids = []
        for line in t.splitlines():
            parts = line.split()
            if len(parts) >= 4 and "clock" in parts[1]:
                try:
                    all_clock_pids.append(int(parts[0]))
                except ValueError:
                    pass
        bg_clock_pid = all_clock_pids[-1]
        type_line(s, f"kill {bg_clock_pid}")
        dump_vga(s)
        check(f"Killed task {bg_clock_pid}" in screen_text(), "kill acknowledges")
        clear_and_run(s, "ps")
        dump_vga(s)
        t = screen_text()
        check(str(bg_clock_pid) not in t.split(), "killed bg clock pid gone from ps (slot freed)")
        check("clock" in t, "init clock service still present")
        type_line(s, f"bg clock")
        dump_vga(s)
        check("Started background task clock" in screen_text(),
              "freed slot is reusable: bg clock works again")

    print("\n== bg fib 25 ; result in /tmp/fib.log ==")
    clear_and_run(s, "bg fib 25")
    dump_vga(s)
    check("Started background task fib" in screen_text(), "bg reports spawn of fib")
    for i in range(10):
        time.sleep(1)
        clear_and_run(s, "cat /tmp/fib.log")
        dump_vga(s)
        if "fib(25)=75025" in screen_text():
            break
    check("fib(25)=75025" in screen_text(), "fib(25)=75025 in /tmp/fib.log")

    print("\n== system still alive at the end ==")
    clear_and_run(s, "echo multitasking test done")
    dump_vga(s)
    check("multitasking test done" in screen_text(), "shell alive after all bg activity")

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
