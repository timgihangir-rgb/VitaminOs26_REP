#!/usr/bin/env python3
"""Test ELF program loading from disk (execve-style).

Boot 1 (seeded disk): /bin programs load as ELF, argc/argv are passed,
foreground execution works, init starts the clock service from disk.
Boot 2 (empty disk): proves binaries are NOT embedded in the kernel -
/bin is empty and unknown commands fail.
"""
import subprocess
import time
import os
import socket
import json
import sys
import shutil

QMP_PORT = 4446

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


VGA_DUMP = "/tmp/exec_vga.bin"


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


def wait_for_prompt(s, tag):
    for i in range(10):
        time.sleep(1)
        dump_vga(s)
        if "vitamin_os26" in screen_text():
            print(f"  t={i+1}s: shell ready ({tag})")
            break
    else:
        check(False, f"shell prompt appeared ({tag})")
        return False
    check(True, f"shell prompt appeared ({tag})")
    return True


base = os.path.dirname(os.path.abspath(__file__))
iso = os.path.join(base, "target", "os.iso")
seeded_img = os.path.join(base, "target", "os.img")

subprocess.run(["pkill", "-f", f"qmp tcp:127.0.0.1:{QMP_PORT}"], stderr=subprocess.DEVNULL)
time.sleep(0.5)

qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-drive", "file={img},format=raw,if=ide",
    "-display", "none",
    "-no-reboot",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", "file:{serial}",
]

def start_qemu(img, serial):
    cmd = [c.format(img=img, serial=serial) for c in qemu_cmd]
    return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

# ---------- Boot 1: seeded disk ----------
img1 = "/tmp/exec_os_seeded.img"
serial1 = "/tmp/exec_serial1.log"
for p in (serial1, VGA_DUMP):
    try:
        os.remove(p)
    except OSError:
        pass
shutil.copyfile(seeded_img, img1)

print("Starting QEMU (boot 1, seeded disk)")
qemu = start_qemu(img1, serial1)

try:
    s = qmp_connect(qemu)
    print("Connected to QMP")

    if not wait_for_prompt(s, "boot 1"):
        sys.exit(1)

    print("\n== ls /bin (programs live on disk) ==")
    clear_and_run(s, "ls /bin")
    dump_vga(s)
    t = screen_text()
    for prog in ("clock", "echoargs", "example", "fetch", "help", "snake", "vita"):
        check(prog in t, f"/bin/{prog} listed")

    print("\n== echoargs foo bar (ELF, argc/argv) ==")
    clear_and_run(s, "echoargs foo bar")
    dump_vga(s)
    t = screen_text()
    check("argc=3" in t, "argc=3 (prog + 2 args)")
    check("arg: /bin/echoargs" in t, "argv[0]=/bin/echoargs")
    check("arg: foo" in t, "argv[1]=foo")
    check("arg: bar" in t, "argv[2]=bar")

    print("\n== run echoargs a b c ==")
    clear_and_run(s, "run echoargs a b c")
    dump_vga(s)
    check("argc=4" in screen_text(), "argc=4 via run")

    print("\n== fetch (Rust ELF from disk) ==")
    clear_and_run(s, "fetch")
    dump_vga(s)
    check("VitaminOS26" in screen_text(), "fetch output rendered")

    print("\n== example returns via ret -> exit stub ==")
    type_line(s, "example")
    time.sleep(1.5)
    clear_and_run(s, "ps")
    dump_vga(s)
    t = screen_text()
    check("PID" in t and "NAME" in t, "shell alive after program returned via ret")
    check("clock" in t, "clock service running (ELF loaded by init)")

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

# ---------- Boot 2: empty disk ----------
img2 = "/tmp/exec_os_empty.img"
serial2 = "/tmp/exec_serial2.log"
for p in (serial2, VGA_DUMP):
    try:
        os.remove(p)
    except OSError:
        pass
open(img2, "wb").truncate(8 * 1024 * 1024)

print("\nStarting QEMU (boot 2, empty disk)")
qemu = start_qemu(img2, serial2)

try:
    s = qmp_connect(qemu)
    print("Connected to QMP")

    if not wait_for_prompt(s, "boot 2"):
        sys.exit(1)

    print("\n== ls /bin must be empty (no embedded binaries) ==")
    clear_and_run(s, "ls /bin")
    dump_vga(s)
    t = screen_text()
    check("(empty)" in t, "/bin is empty on blank disk")
    # "vitamin_os26" в промпте содержит подстроку "vita" - вырезаем промпт.
    body = t.replace("vitamin_os26", "")
    for prog in ("clock", "echoargs", "example", "fetch", "help", "snake", "vita"):
        check(prog not in body, f"{prog} not auto-installed")

    print("\n== fetch must be an unknown command ==")
    clear_and_run(s, "fetch")
    dump_vga(s)
    t = screen_text()
    check("Unknown command: fetch" in t, "fetch not built into kernel")
    check("VitaminOS26" not in t, "no fetch output without binary")

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

if failures:
    print(f"\n{len(failures)} check(s) FAILED:")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)
print("\nALL CHECKS PASSED")
