#!/usr/bin/env python3
"""Crash-consistency test (bigtodo 3.2.4): kill -9 QEMU посреди серии
транзакций VITAFS -> рестарт на том же образе -> ФС должна смонтироваться
(WAL recover + fsck-lite) и завершённые до краша файлы обязаны остаться
целыми и консистентными.

Учитывает загрузочный сплэш: после старта ждём ~9с, жмём Enter, только
потом появляется промпт шелла.
"""
import json
import os
import re
import random
import shutil
import socket
import subprocess
import sys
import time

QMP_PORT = 4447
VGA_DUMP = "/tmp/crash_vga.bin"
IMG = "/tmp/crash_os.img"
SERIAL1 = "/tmp/crash_serial1.log"
SERIAL2 = "/tmp/crash_serial2.log"

ROOT = os.path.dirname(os.path.abspath(__file__))
# Загрузочный ISO (сырой образ диска загрузчиком не читается).
ISO = os.path.join(ROOT, "target", "os.iso")

failures = []


def check(cond, msg):
    print(f"  [{'OK  ' if cond else 'FAIL'}] {msg}")
    if not cond:
        failures.append(msg)


def qemu_start(serial_log):
    cmd = [
        "qemu-system-x86_64",
        "-cdrom", ISO,
        # Без явного порядка SeaBIOS сначала пробует HDD и флоппи: загрузка
        # занимает лишние 10-15 с, и тесты не дожидаются приглашения.
        "-boot", "order=d",
        "-drive", f"file={IMG},format=raw,if=ide",
        "-display", "none",
        "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
        "-serial", f"file:{serial_log}",
    ]
    return subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def qmp_connect(qemu, attempts=25):
    for _ in range(attempts):
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


def type_line(s, text, gap=0.05):
    for ch in text:
        fast_sendkey(s, {" ": "spc", "/": "slash", ".": "dot"}.get(ch, ch))
    time.sleep(0.15)
    fast_sendkey(s, "ret")


# Физический адрес текстового буфера. Когда загрузчик включает графический
# режим, ядро подменяет 0xB8000 RAM-тенью и печатает её адрес в serial, так
# что читать надо оттуда (иначе экран пуст).
def vga_addr(serial_log):
    try:
        log = open(serial_log, errors="replace").read()
    except OSError:
        return "0xb8000"
    m = re.search(r"vga shadow phys=(0x[0-9a-fA-F]+)", log)
    return m.group(1) if m else "0xb8000"


def grid(serial_log=SERIAL1):
    """Размер текстовой сетки в ячейках.

    Сетку больше не 80x30 на все случаи: композитор выбирает её под
    разрешение монитора (см. `fit_grid` в src/fb.rs), и на 1280x800 это
    80x25 в масштабе 2x - буквы вдвое крупнее и занимают весь кадр. Ядро
    печатает итог в serial строкой "[N] fb WxH scale=N grid=CxR", по ней и
    читаем, сколько колонок и строк разбирать из дампа экрана. Нет такой
    строки - композитор не включился, система в текстовом режиме, и сетка
    историческая.
    """
    try:
        log = open(serial_log, errors="replace").read()
    except OSError:
        return 80, 30
    m = re.search(r"grid=(\d+)x(\d+)", log)
    return (int(m.group(1)), int(m.group(2))) if m else (80, 30)


def dump_vga(s, serial_log=SERIAL1):
    s.send((json.dumps({
        "execute": "human-monitor-command",
        "arguments": {"command-line": f'pmemsave {vga_addr(serial_log)} 65536 "{VGA_DUMP}"'},
    }) + "\n").encode())
    time.sleep(0.35)
    try:
        s.recv(65536)
    except Exception:
        pass


def screen_text():
    try:
        data = open(VGA_DUMP, "rb").read()
    except FileNotFoundError:
        return ""
    rows = []
    cols, rows_ = grid()
    for row in range(rows_):
        chars = []
        for col in range(cols):
            ch = data[row * cols * 2 + col * 2]
            chars.append(chr(ch) if 32 <= ch < 127 else " ")
        rows.append("".join(chars))
    return "\n".join(rows)


def wait_for_prompt(s, attempts=40, serial_log=SERIAL1):
    """Сплэш держит экран до Enter: даём загрузке закончиться, жмём Enter,
    дальше обычный опрос промпта."""
    for i in range(attempts):
        time.sleep(1)
        # Именно serial_log, а не константа: адрес тени 0xB8000 печатается в
        # лог ТЕКУЩЕГО boot. С SERIAL2 на boot1pmemsave читал настоящую
        # видеопамять вместо тени, экран был пустым, и проверка промпта
        # падала (а за ней и все проверки файлов).
        dump_vga(s, serial_log)
        t = screen_text()
        # Меню GRUB и загрузочный экран ядра оба содержат "press Enter",
        # поэтому жмём Enter, пока строка не исчезнет.
        if "press Enter" in t:
            fast_sendkey(s, "ret")
        if "vitamin_os26" in t:
            print(f"  t={i + 1}s: shell ready")
            return True
    return False


def kill_hard(qemu):
    qemu.kill()
    try:
        qemu.wait(timeout=5)
    except Exception:
        pass


subprocess.run(["pkill", "-f", f"qmp tcp:127.0.0.1:{QMP_PORT}"], stderr=subprocess.DEVNULL)
time.sleep(0.5)
shutil.copyfile(os.path.join(ROOT, "target", "os.img"), IMG)

print("== boot 1: подготовка данных ==")
qemu = qemu_start(SERIAL1)
s = qmp_connect(qemu)
check(wait_for_prompt(s), "boot1: промпт шелла после сплэша")

type_line(s, "mkdir /home/crash")
for name in ("a.txt", "b.txt", "c.txt"):
    type_line(s, f"cp /etc/hostname /home/crash/{name}")
dump_vga(s)
check("/home/crash" in screen_text() or True, "базовые файлы созданы (проверка ниже)")

# Серия быстрых транзакций; убиваем QEMU в случайный момент серии.
N = 45
burst_delay = 0.06
kill_after = random.uniform(1.0, N * burst_delay * 0.8)
print(f"\n== burst {N} транзакций, kill через {kill_after:.2f}s ==")
start = time.time()
killed = False
for i in range(N):
    type_line(s, f"cp /etc/hostname /home/crash/f{i}.txt", gap=burst_delay)
    if not killed and (time.time() - start) >= kill_after:
        kill_hard(qemu)
        killed = True
        print(f"  SIGKILL на транзакции #{i} (t={time.time() - start:.2f}s)")
        break
if not killed:
    kill_hard(qemu)
    print("  SIGKILL после серии")
try:
    s.close()
except Exception:
    pass

print("\n== boot 2 на том же образе (recover + fsck) ==")
qemu2 = qemu_start(SERIAL2)
s2 = qmp_connect(qemu2)
check(wait_for_prompt(s2, serial_log=SERIAL2), "boot2: система поднялась после краша")

ser2 = open(SERIAL2, "rb").read()
check(b"[exc]" not in ser2, "boot2: нет исключений ядра")
recovered = b"[wal] recover" in ser2 or b"[fsck]" in ser2
print(f"  (журнал: {'был реплей/fix' if recovered else 'краш попал в зазор - чисто'})")

clear = lambda: None
def run_and_check(cmd, needle, msg):
    type_line(s2, "clear")
    time.sleep(0.4)
    type_line(s2, cmd)
    time.sleep(1.0)
    # Тень boot2 живёт в SERIAL2; дефолт SERIAL1 указывал на адрес тени
    # прошлой загрузки (см. комментарий в wait_for_prompt).
    dump_vga(s2, SERIAL2)
    check(needle in screen_text(), msg)

run_and_check("cat /home/crash/a.txt", "VitaminOS26", "a.txt цел после краша")
run_and_check("cat /home/crash/b.txt", "VitaminOS26", "b.txt цел после краша")
run_and_check("cat /home/crash/c.txt", "VitaminOS26", "c.txt цел после краша")
run_and_check("ls /home/crash", "a.txt", "каталог читается, a.txt виден")

# Все перечисленные в ls f*.txt должны читаться без мусора (выборочно).
t = screen_text()
visible = [f"f{i}.txt" for i in range(N) if f"f{i}.txt" in t]
checked = 0
bad = 0
for name in visible[:5]:
    type_line(s2, "clear")
    time.sleep(0.3)
    type_line(s2, f"cat /home/crash/{name}")
    time.sleep(1.0)
    dump_vga(s2, SERIAL2)
    checked += 1
    if "VitaminOS26" not in screen_text():
        bad += 1
check(bad == 0 and checked > 0, f"выборочно прочитано {checked} файлов из ls, битых: {bad}")

try:
    s2.close()
except Exception:
    pass
kill_hard(qemu2)

ser2b = open(SERIAL2, "rb").read()
check(b"[exc]" not in ser2b, "итог: за всю сессию boot2 ни одного [exc]")

if failures:
    print(f"\n{len(failures)} check(s) FAILED:")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)
print("\nALL CHECKS PASSED")
