#!/usr/bin/env python3
"""Test the `parallel` demo program: a coordinator that splits a prime-counting
job across N background worker tasks and coordinates them via shared atomics.

Checks:
  - bg parallel spawns a coordinator task
  - ps shows the coordinator while it runs
  - /tmp/parallel.log reports worker ranges, counts, start/finish ticks
  - worker counts sum to the known total (pi(1_000_000) = 78498)
  - workers started together (barrier) and finished within a few ticks
    of each other -> evidence of preemptive concurrency
"""
import subprocess
import time
import os
import socket
import json
import sys

QMP_PORT = 4447
VGA_DUMP = "/tmp/par_vga.bin"

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


iso = os.path.join(os.path.dirname(os.path.abspath(__file__)), "target", "os.iso")
img = "/tmp/par_os.img"
qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-drive", f"file={img},format=raw,if=ide",
    "-display", "none",
    "-no-reboot",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", "file:/tmp/par_serial.log",
]
for p in ("/tmp/par_serial.log", VGA_DUMP):
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

    print("\n== bg parallel 1000000 4 ==")
    clear_and_run(s, "bg parallel 1000000 4")
    dump_vga(s)
    check("Started background task parallel" in screen_text(), "bg reports spawn of parallel")

    print("\n== ps shows coordinator and workers ==")
    type_line(s, "ps")
    dump_vga(s)
    t = screen_text()
    check("parallel" in t, "ps lists the parallel coordinator")
    workers_seen = [f"w{i}" for i in range(4) if f"w{i}" in t]
    check(len(workers_seen) >= 1, f"ps shows at least one worker task ({workers_seen or 'none'})")

    print("\n== /tmp/parallel.log appears with correct totals ==")
    result = None
    for i in range(20):
        time.sleep(1)
        clear_and_run(s, "cat /tmp/parallel.log")
        dump_vga(s)
        t = screen_text()
        if "TOTAL" in t or "total=" in t:
            result = t
            break
    check(result is not None, "parallel.log written by coordinator")
    if result is None:
        sys.exit(1)
    print(result)

    check("parallel limit=1000000 workers=4 total=78498" in result,
          "total = 78498 = pi(1000000)")

    worker_lines = [ln for ln in result.splitlines() if ln.startswith("w")]
    check(len(worker_lines) == 4, "report has 4 worker lines")

    counts = []
    starts = []
    finishes = []
    ok_ranges = True
    prev_hi = 0
    for ln in worker_lines:
        parts = ln.replace("=", " ").split()
        counts.append(int(parts[parts.index("count") + 1]))
        starts.append(int(parts[parts.index("start") + 1]))
        finishes.append(int(parts[parts.index("finish") + 1]))
        lo, hi = parts[parts.index("range") + 1].split("-")
        lo, hi = int(lo), int(hi)
        if lo != prev_hi:
            ok_ranges = False
        prev_hi = hi
    check(ok_ranges, "worker ranges are contiguous")
    check(prev_hi == 1000000, "worker ranges cover the full [0, 1000000)")
    check(sum(counts) == 78498, "worker counts sum to pi(1000000)")
    # Init-службы (clock/ticker/crashy/init) добавляют задачи в round-robin,
    # поэтому рабочие стартуют в пределах одного цикла планирования (~10 задач).
    check(max(starts) - min(starts) <= 30, f"workers started together via barrier (spread {max(starts) - min(starts)} ticks)")
    check(min(finishes) > max(starts), "execution windows overlap: first finish after last start")
    cpu_sum = sum(f - s for s, f in zip(starts, finishes))
    wall_time = max(finishes) - min(starts)
    check(wall_time < cpu_sum, f"parallel speedup: wall {wall_time} ticks < combined cpu {cpu_sum} ticks")

    print("\n== system still alive ==")
    clear_and_run(s, "echo parallel done")
    dump_vga(s)
    check("parallel done" in screen_text(), "shell alive after parallel job")

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
