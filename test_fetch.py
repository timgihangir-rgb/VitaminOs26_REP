#!/usr/bin/env python3
"""Test the `fetch` standalone Rust program (V logo inside a circle + system info)."""
import subprocess
import time
import os
import socket
import json
import sys

QMP_PORT = 4449
VGA_DUMP = "/tmp/fetch_vga.bin"

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


KEY_MAP = {" ": "spc"}


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


iso = os.path.join(os.path.dirname(os.path.abspath(__file__)), "target", "os.iso")
img = "/tmp/fetch_os.img"
qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-drive", f"file={img},format=raw,if=ide",
    "-display", "none",
    "-no-reboot",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", "file:/tmp/fetch_serial.log",
]
for p in ("/tmp/fetch_serial.log", VGA_DUMP):
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

    print("\n== run fetch ==")
    type_line(s, "clear")
    type_line(s, "run fetch")
    dump_vga(s)
    t = screen_text()

    # The logo: a V inside a circle.
    print("\n-- logo --")
    check("---------." in t and "---------'" in t, "circle border drawn around the V")
    check("| V       V V |" in t, "V letter drawn inside the circle")
    check("|      v      |" in t, "V tip present")

    print("\n-- values --")
    check("VitaminOS26 0.1.0" in t, "OS name + version")
    check("Kernel:" in t and "0.1.0" in t, "kernel version")
    check("Host:" in t, "host line")
    check("CPU:" in t and "Family" in t, "cpu line with family")
    check("Cores:" in t and "Threads:" in t, "cores + threads")
    check("Arch: x86_64" in t, "architecture")
    check("Memory:" in t and "MiB" in t, "memory total")
    check("CPU Flags:" in t, "cpu flags line")
    check("Bootloader: 0.9.23" in t, "bootloader version")
    check("Shell:" in t and "VitaminShell" in t, "shell name")
    check("Terminal: VGA text mode" in t, "terminal type")
    check("Resolution: 80x30 VGA" in t, "resolution")

    print("\n-- shell still responsive after fetch --")
    type_line(s, "echo ok")
    dump_vga(s)
    check("ok" in screen_text(), "shell continues working after fetch exits")

    print("\n== fetch scrolls the tty when its output does not fit ==")
    # Fill the screen so the prompt lands near the bottom row, then run fetch.
    for _ in range(14):
        type_line(s, "echo scroll")
    dump_vga(s)
    type_line(s, "run fetch")
    dump_vga(s)
    t = screen_text()
    check("Resolution: 80x30 VGA" in t, "bottom info row visible after scroll (output not cut off)")
    check("---------." in t and "| V       V V |" in t, "full logo visible after scroll")

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
