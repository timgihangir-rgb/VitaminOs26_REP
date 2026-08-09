#!/usr/bin/env python3
"""Test cp, mv, find shell commands by driving QEMU via QMP and reading the VGA screen.

Note: the keyboard driver has no shift support, so the test avoids typing '_' and '>'.
File contents are taken from /etc/hostname ("VitaminOS26").
"""
import subprocess
import time
import os
import socket
import json
import sys

QMP_PORT = 4445
VGA_DUMP = "/tmp/cmd_vga.bin"

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
    """Send a key without the 0.3s settle delay of qmp_send."""
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
    """Clear the screen, then run a command and dump it."""
    type_line(s, "clear")
    type_line(s, command)


iso = os.path.join(os.path.dirname(os.path.abspath(__file__)), "target", "os.iso")
img = "/tmp/cmd_os.img"
qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-drive", f"file={img},format=raw,if=ide",
    "-display", "none",
    "-no-reboot",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", "file:/tmp/cmd_serial.log",
]
for p in ("/tmp/cmd_serial.log", VGA_DUMP):
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

    print("\n== cat /etc/hostname (baseline) ==")
    clear_and_run(s, "cat /etc/hostname")
    dump_vga(s)
    check("VitaminOS26" in screen_text(), "cat shows /etc/hostname content")

    print("\n== echo hello world (prints, no redirect) ==")
    clear_and_run(s, "echo hello world")
    dump_vga(s)
    check("hello world" in screen_text(), "echo output visible")

    print("\n== cp /etc/hostname /home/h.txt ; cat ==")
    type_line(s, "cp /etc/hostname /home/h.txt")
    clear_and_run(s, "cat /home/h.txt")
    dump_vga(s)
    check("VitaminOS26" in screen_text(), "copied file has same content")

    print("\n== mkdir /tmp/d ; cp file INTO dir ==")
    type_line(s, "mkdir /tmp/d")
    type_line(s, "cp /etc/hostname /tmp/d")
    clear_and_run(s, "cat /tmp/d/hostname")
    dump_vga(s)
    check("VitaminOS26" in screen_text(), "cp into directory worked")

    print("\n== cp /tmp/d /home/dcopy (recursive dir copy) ==")
    type_line(s, "cp /tmp/d /home/dcopy")
    clear_and_run(s, "cat /home/dcopy/hostname")
    dump_vga(s)
    check("VitaminOS26" in screen_text(), "recursive directory copy worked")

    print("\n== find hostname (matches /etc, /tmp/d, /home/dcopy) ==")
    clear_and_run(s, "find hostname")
    dump_vga(s)
    t = screen_text()
    check("/etc/hostname" in t, "find: /etc/hostname")
    check("/tmp/d/hostname" in t, "find: /tmp/d/hostname")
    check("/home/dcopy/hostname" in t, "find: /home/dcopy/hostname")

    print("\n== find h.txt ==")
    clear_and_run(s, "find h.txt")
    dump_vga(s)
    check("/home/h.txt" in screen_text(), "find locates h.txt")

    print("\n== mv /home/h.txt /etc/h.txt ; find h.txt ==")
    type_line(s, "mv /home/h.txt /etc/h.txt")
    clear_and_run(s, "find h.txt")
    dump_vga(s)
    t = screen_text()
    check("/etc/h.txt" in t, "mv: file now at /etc/h.txt")
    check("/home/h.txt" not in t, "mv: old path gone from find output")

    print("\n== mv /tmp/d /etc/d ; find d ==")
    type_line(s, "mv /tmp/d /etc/d")
    clear_and_run(s, "find d")
    dump_vga(s)
    t = screen_text()
    check("/etc/d" in t, "mv: dir now at /etc/d")
    check("/tmp/d" not in t, "mv: old dir path gone")

    print("\n== mv /etc/d/hostname /etc/renamed.txt ; cat ==")
    type_line(s, "mv /etc/d/hostname /etc/renamed.txt")
    clear_and_run(s, "cat /etc/renamed.txt")
    dump_vga(s)
    check("VitaminOS26" in screen_text(), "mv rename preserved content")

    print("\n== find nonexistent ==")
    clear_and_run(s, "find nonexistent")
    dump_vga(s)
    check("find: no such file: nonexistent" in screen_text(), "find reports missing file")

    print("\n== cp nonexistent /tmp/x.txt ==")
    clear_and_run(s, "cp nonexistent /tmp/x.txt")
    dump_vga(s)
    check("cp: cannot copy" in screen_text(), "cp reports error for missing source")

    print("\n== mv a a (no-op) ==")
    type_line(s, "mv /etc/renamed.txt /etc/renamed.txt")
    clear_and_run(s, "cat /etc/renamed.txt")
    dump_vga(s)
    check("VitaminOS26" in screen_text(), "mv same path is harmless")

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
