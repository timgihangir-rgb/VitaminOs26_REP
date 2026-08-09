#!/usr/bin/env python3
"""Test Vita launch from shell using QEMU's QMP to send keystrokes.

Verifies via the VGA framebuffer (pmemsave) that:
  1. the shell reaches its prompt,
  2. `vita <file>` renders the full editor UI (title bar + hotkey bar),
  3. typing inside Vita does not crash the system,
  4. Ctrl+Q -> n quits back to the shell.
"""
import subprocess
import time
import os
import socket
import json
import sys

QMP_PORT = 4444
VGA_DUMP1 = "/tmp/vga_dump.bin"
VGA_DUMP2 = "/tmp/vga_dump2.bin"

failures = []


def check(cond, msg):
    status = "OK  " if cond else "FAIL"
    print(f"  [{status}] {msg}")
    if not cond:
        failures.append(msg)


def qmp_connect(qemu, attempts=20):
    """Wait for QEMU's QMP socket and connect."""
    for i in range(attempts):
        if not qemu_alive(qemu):
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


def qmp_sendkey(s, key_qcode):
    return qmp_hmp(s, f"sendkey {key_qcode}")


def qemu_alive(qemu):
    return qemu.poll() is None


def dump_vga(s, path):
    """Dump physical VGA framebuffer to a file via HMP pmemsave."""
    # Quoted path: '/' is parsed as division by HMP's expression parser.
    r = qmp_hmp(s, f'pmemsave 0xb8000 9600 "{path}"')
    time.sleep(0.4)
    return r


def vga_text(path):
    """Return the VGA buffer as a 30-line text grid."""
    try:
        data = open(path, "rb").read()
    except FileNotFoundError:
        return []
    lines = []
    for row in range(30):
        chars = []
        for col in range(80):
            ch = data[row * 160 + col * 2]
            if 32 <= ch < 127:
                chars.append(chr(ch))
            else:
                chars.append(" ")
        lines.append("".join(chars))
    return lines


def print_screen(lines):
    for row, line in enumerate(lines):
        print(f"{row:2d}|{line}|")


iso = os.path.join(os.path.dirname(os.path.abspath(__file__)), "target", "os.iso")
img = "/tmp/vita_os.img"
qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-drive", f"file={img},format=raw,if=ide",
    "-display", "none",
    "-no-reboot",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", "file:/tmp/vita_serial.log",
]
for p in ("/tmp/vita_serial.log", VGA_DUMP1, VGA_DUMP2):
    try:
        os.remove(p)
    except OSError:
        pass
open(img, "wb").truncate(8 * 1024 * 1024)

print(f"Starting QEMU: {' '.join(qemu_cmd)}")
qemu = subprocess.Popen(qemu_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
rc = qemu.poll()
if rc is not None:
    print(f"ERROR: QEMU exited with code {rc}")
    sys.exit(1)

try:
    s = qmp_connect(qemu)
    print("Connected to QMP")

    # Wait for the shell prompt to appear on the VGA screen.
    print("Waiting for shell prompt...")
    shell_ready = False
    for i in range(10):
        time.sleep(1)
        if not qemu_alive(qemu):
            break
        dump_vga(s, VGA_DUMP1)
        lines = vga_text(VGA_DUMP1)
        if any("vitamin_os26" in ln for ln in lines):
            print(f"  t={i+1}s: shell prompt found")
            shell_ready = True
            break
        print(f"  t={i+1}s: still booting...")
    check(shell_ready, "shell prompt appeared within 10s")
    if not shell_ready:
        sys.exit(1)

    # Launch vita with a file.
    print("\nSending: vita /etc/hostname<enter>")
    for key in ["v", "i", "t", "a", "spc", "slash", "e", "t", "c", "slash", "h", "o", "s", "t", "n", "a", "m", "e"]:
        qmp_sendkey(s, key)
        time.sleep(0.1)
    time.sleep(0.5)
    qmp_sendkey(s, "ret")
    time.sleep(1)
    check(qemu_alive(qemu), "QEMU alive after launching vita")

    # Wait for the Vita title bar to render.
    vita_started = False
    for i in range(6):
        time.sleep(1)
        if not qemu_alive(qemu):
            break
        dump_vga(s, VGA_DUMP1)
        lines = vga_text(VGA_DUMP1)
        if any("Vita:" in ln for ln in lines):
            print(f"  t={i+1}s: Vita UI detected")
            vita_started = True
            break
        print(f"  t={i+1}s: waiting for Vita UI...")
    check(vita_started, "Vita full-screen UI rendered")

    lines = vga_text(VGA_DUMP1)
    print("\n=== Vita screen before typing ===")
    print_screen(lines)
    check(any("Vita: /etc/hostname" in ln for ln in lines), "title bar shows file name 'Vita: /etc/hostname'")
    check(any("Ctrl+Q: Quit" in ln for ln in lines), "hotkey bar shows 'Ctrl+Q: Quit'")
    check(any("Ctrl+S: Save" in ln for ln in lines), "hotkey bar shows 'Ctrl+S: Save'")

    # Type inside Vita; a crash would kill QEMU.
    print("\nTyping 'hello' inside Vita...")
    for key in ["h", "e", "l", "l", "o"]:
        qmp_sendkey(s, key)
        time.sleep(0.3)
        if not qemu_alive(qemu):
            print(f"  QEMU DIED after key '{key}'!")
            break
    else:
        print("  All keys accepted, QEMU alive")
    check(qemu_alive(qemu), "QEMU alive after typing 'hello'")

    dump_vga(s, VGA_DUMP2)
    lines = vga_text(VGA_DUMP2)
    print("\n=== Vita screen after typing ===")
    print_screen(lines)
    check(any("hello" in ln for ln in lines), "typed text 'hello' appears in the editor")

    # Quit without saving.
    print("\nSending: Ctrl+Q then n (don't save)")
    qmp_sendkey(s, "ctrl-q")
    time.sleep(0.5)
    qmp_sendkey(s, "n")
    time.sleep(1.5)
    check(qemu_alive(qemu), "QEMU alive after quitting vita")
    dump_vga(s, VGA_DUMP1)
    lines = vga_text(VGA_DUMP1)
    check(any("vitamin_os26" in ln for ln in lines), "shell prompt visible again after vita exited")

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
