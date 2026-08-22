#!/usr/bin/env python3
"""Test shell redirection (>, >>, <) and command history (history.log).

Drives QEMU via QMP and reads the VGA screen:
  - `echo text >> file` appends (does not clobber existing content),
  - `echo text > file` overwrites,
  - `cat < file` reads input from a file,
  - `cat < src > dst` copies content through redirections,
  - `cat file >> out` appends file content,
  - `history` prints the numbered /var/log/history.log.
"""
import subprocess
import time
import os
import socket
import json
import sys
import shutil

QMP_PORT = 4447
VGA_DUMP = "/tmp/redir_vga.bin"

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


# '>' is shift-dot, '<' is shift-comma (no literal qcodes exist for them).
KEY_MAP = {
    " ": "spc",
    "/": "slash",
    ">": "shift-dot",
    "<": "shift-comma",
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
img = "/tmp/redir_os.img"
qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-drive", f"file={img},format=raw,if=ide",
    "-display", "none",
    "-no-reboot",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", "file:/tmp/redir_serial.log",
]
for p in ("/tmp/redir_serial.log", VGA_DUMP):
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

    print("\n== echo hello >> /tmp/log (append, file created) ==")
    clear_and_run(s, "echo hello >> /tmp/log")
    clear_and_run(s, "cat /tmp/log")
    dump_vga(s)
    t = screen_text()
    check("hello" in t, "append created file with content")

    print("\n== echo world >> /tmp/log (second append keeps first) ==")
    clear_and_run(s, "echo world >> /tmp/log")
    clear_and_run(s, "cat /tmp/log")
    dump_vga(s)
    t = screen_text()
    check("hello" in t and "world" in t, "both lines present after appends")
    check("helloworld" not in t, "lines separated by newline")

    print("\n== echo first > /tmp/log (overwrite) ==")
    clear_and_run(s, "echo first > /tmp/log")
    clear_and_run(s, "cat /tmp/log")
    dump_vga(s)
    t = screen_text()
    check("first" in t and "hello" not in t and "world" not in t, "overwrite replaced content")

    print("\n== cat < /etc/hostname (input redirection) ==")
    clear_and_run(s, "cat < /etc/hostname")
    dump_vga(s)
    t = screen_text()
    check("VitaminOS26" in t, "cat < file prints its content")

    print("\n== cat < /etc/hostname > /tmp/copy (both redirects) ==")
    type_line(s, "cat < /etc/hostname > /tmp/copy")
    clear_and_run(s, "cat /tmp/copy")
    dump_vga(s)
    t = screen_text()
    check("VitaminOS26" in t, "copy written via < and >")

    print("\n== cat /tmp/copy >> /tmp/log (append file content) ==")
    type_line(s, "cat /tmp/copy >> /tmp/log")
    clear_and_run(s, "cat /tmp/log")
    dump_vga(s)
    t = screen_text()
    check("first" in t and "VitaminOS26" in t, "file content appended to log")

    print("\n== history (1.4: commands logged to /var/log/history.log) ==")
    clear_and_run(s, "history")
    dump_vga(s)
    t = screen_text()
    check("echo hello >> /tmp/log" in t, "history contains 'echo hello >> /tmp/log'")
    check("cat < /etc/hostname" in t, "history contains 'cat < /etc/hostname'")
    check("history" in t, "history lists itself")
    check("   1  " in t or "  1  " in t, "history output is numbered")

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