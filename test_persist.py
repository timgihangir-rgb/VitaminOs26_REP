#!/usr/bin/env python3
"""Test that VFS data persists across a reboot via the .img disk.

Keyboard has no shift support, so '_' and '>' are avoided. File content is
taken from /etc/hostname ("VitaminOS26") via cp.
"""
import subprocess
import time
import os
import socket
import json
import sys

QMP_PORT = 4446
VGA_DUMP = "/tmp/persist_vga.bin"
IMG = "/tmp/persist_os.img"

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
    dump_vga(s)


def wait_for_prompt(s, attempts=20):
    for i in range(attempts):
        time.sleep(1)
        dump_vga(s)
        if "vitamin_os26" in screen_text():
            print(f"  t={i + 1}s: shell ready")
            return True
    return False


iso = os.path.join(os.path.dirname(os.path.abspath(__file__)), "target", "os.iso")
qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-drive", f"file={IMG},format=raw,if=ide",
    "-display", "none",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", "file:/tmp/persist_serial.log",
]
for p in ("/tmp/persist_serial.log", VGA_DUMP):
    try:
        os.remove(p)
    except OSError:
        pass
open(IMG, "wb").truncate(8 * 1024 * 1024)

subprocess.run(["pkill", "-f", f"qmp tcp:127.0.0.1:{QMP_PORT}"], stderr=subprocess.DEVNULL)
time.sleep(0.5)

print(f"Starting QEMU: {' '.join(qemu_cmd)}")
qemu = subprocess.Popen(qemu_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

try:
    s = qmp_connect(qemu)
    print("Connected to QMP")

    print("Waiting for shell prompt (boot 1)...")
    check(wait_for_prompt(s), "first boot shell prompt")

    print("\n== create files ==")
    type_line(s, "mkdir /home/notes")
    type_line(s, "cp /etc/hostname /home/notes/h.txt")
    type_line(s, "rm /etc/hostname")

    print("\n== verify before reboot ==")
    clear_and_run(s, "ls /home/notes")
    check("h.txt" in screen_text(), "before reboot: /home/notes has h.txt")
    clear_and_run(s, "cat /home/notes/h.txt")
    check("VitaminOS26" in screen_text(), "before reboot: h.txt content")
    clear_and_run(s, "cat /etc/hostname")
    check("File not found: /etc/hostname" in screen_text(), "before reboot: /etc/hostname deleted")

    print("\n== system_reset ==")
    qmp_send(s, {"execute": "system_reset"})
    print("Reset sent, waiting for shell prompt (boot 2)...")
    try:
        s.close()
    except Exception:
        pass
    time.sleep(1.0)
    s = qmp_connect(qemu)
    print("Reconnected to QMP after reset")
    check(wait_for_prompt(s, attempts=25), "second boot shell prompt")

    print("\n== verify persistence after reboot ==")
    clear_and_run(s, "ls /home/notes")
    check("h.txt" in screen_text(), "after reboot: /home/notes still has h.txt")
    clear_and_run(s, "cat /home/notes/h.txt")
    check("VitaminOS26" in screen_text(), "after reboot: h.txt content persisted")
    clear_and_run(s, "cat /etc/hostname")
    check("File not found: /etc/hostname" in screen_text(), "after reboot: deletion persisted")
    clear_and_run(s, "find h.txt")
    check("/home/notes/h.txt" in screen_text(), "after reboot: find locates h.txt")
    clear_and_run(s, "ls /bin")
    t = screen_text()
    check("help" in t and "snake" in t and "vita" in t, "after reboot: builtin programs restored in /bin")

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
