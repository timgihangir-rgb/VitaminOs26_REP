#!/usr/bin/env python3
"""tools/qemu-run.py - запуск гостя и снимок экрана.

Нужен для проверки загрузчика: QEMU не умеет отдавать текстовый режим
иначе, чем через дамп видеопамяти, а SeaBIOS сам по себе ничего в serial
не пишет.

  tools/qemu-run.py pc -drive file=target/boot-test.img,format=raw,if=ide -boot c

Печатает содержимое текстового режима 80x25 и, если ядро туда пишет,
serial. Останавливает гостя сам, оставляя образ на диске как был.
"""
import argparse
import json
import os
import socket
import subprocess
import sys
import time

VGA_BASE = 0xB8000
COLS, ROWS = 80, 25
CELL = 2
SCREEN_BYTES = COLS * ROWS * CELL


def qmp_session(port):
    s = None
    for _ in range(80):
        try:
            s = socket.create_connection(("127.0.0.1", port), timeout=5)
            s.settimeout(10)
            s.recv(65536)
            s.send((json.dumps({"execute": "qmp_capabilities"}) + "\n").encode())
            s.recv(65536)
            return s
        except OSError:
            if s:
                s.close()
            time.sleep(0.25)
    raise RuntimeError(f"QMP не поднялся на порту {port}")


_cmd_id = [0]


def hmp(s, cmd):
    """Человекочитаемая команда монитора. Возвращает ответ QEMU.

    Ответ ловим по идентификатору запроса: QMP шлёт события (RESET,
    STOP, SHUTDOWN) в тот же сокет в случайном порядке, и ждать просто
    подстроку 'return' или 'error' нельзя - событие может прийти раньше.
    """
    _cmd_id[0] += 1
    cid = _cmd_id[0]
    s.send((json.dumps({"execute": "human-monitor-command",
                        "arguments": {"command-line": cmd},
                        "id": cid}) + "\n").encode())
    deadline = time.time() + 5
    buf = ""
    while time.time() < deadline:
        try:
            chunk = s.recv(65536).decode(errors="replace")
        except OSError:
            break
        buf += chunk
        for line in buf.splitlines():
            line = line.strip()
            if not line:
                continue
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue          # строка обрезалась, доберём остальное
            if msg.get("id") == cid:
                return line
    return buf


def read_screen(s, path):
    reply = hmp(s, f'pmemsave 0x{VGA_BASE:x} {SCREEN_BYTES} "{path}"')
    if '"error"' in reply:
        raise RuntimeError(f"pmemsave вернул ошибку: {reply[:200]}")
    # pmemsave пишет файл асинхронно: сразу после команды он может быть ещё
    # пустым или недописанным, и экран получится обрезанным - а это выглядит
    # как «гость ничего не напечатал». Ждём ровно полный размер.
    for _ in range(50):
        if os.path.exists(path) and os.path.getsize(path) >= SCREEN_BYTES:
            break
        time.sleep(0.2)
    data = open(path, "rb").read()[:SCREEN_BYTES].ljust(SCREEN_BYTES, b"\0")
    rows = []
    for r in range(ROWS):
        row = "".join(chr(data[r * COLS * CELL + c * CELL])
                      if 32 <= data[r * COLS * CELL + c * CELL] < 127 else " "
                      for c in range(COLS)).rstrip()
        rows.append(row)
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("machine", help="pc, q35, MicroVM и т. п.")
    ap.add_argument("--memory", default="256M")
    ap.add_argument("--wait", type=int, default=20)
    ap.add_argument("--qmp-port", type=int, default=4740)
    ap.add_argument("--vga", default="/tmp/opencode/vga.bin")
    ap.add_argument("--serial", default="/tmp/opencode/serial.log")
    ap.add_argument("--boot", help="после старта: нажать клавиши, напр. a")
    # Остаток - аргументы QEMU. parse_known_args, а не REMAINDER: тот
    # захватывает всё после первого позиционного, вместе с нашими флагами.
    args, qemu_args = ap.parse_known_args()

    for p in (args.vga, args.serial):
        try:
            os.remove(p)
        except OSError:
            pass

    port = args.qmp_port
    # Порт мог остаться занят прошлым прогоном - QMP без этого не поднимется.
    # pkill -x не годится: имя процесса длиннее 15 символов, -x его не найдёт,
    # а -f убьёт и сам вызывающий shell, если строка попала в его argv.
    subprocess.run(["pkill", "-9", "-f", "qemu-syste[m]-x86_64"],
                   stderr=subprocess.DEVNULL)
    time.sleep(0.4)

    cmd = ["qemu-system-x86_64", "-m", args.memory, "-machine", args.machine,
           "-display", "none", "-qmp", f"tcp:127.0.0.1:{port},server,nowait",
           "-serial", f"file:{args.serial}"] + qemu_args
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)

    try:
        s = qmp_session(port)
        if args.boot:
            time.sleep(3)
            for ch in args.boot:
                # shift-dot: QMP не печатает верхний регистр сам
                hmp(s, "sendkey shift-dot" if ch == ">" else f"sendkey {ch}")
                time.sleep(0.3)
        time.sleep(args.wait)
        print("=== ЭКРАН ===")
        rows = read_screen(s, args.vga)
        for row in rows:
            if row:
                print(row)
        if not any(rows):
            print("(пусто)")
        print("=== SERIAL ===")
        try:
            text = open(args.serial, encoding="utf-8", errors="replace").read()
            print(text[:4000] if text else "(пусто)")
        except OSError:
            print("(нет файла)")
    finally:
        proc.kill()
        out = proc.communicate()[0].decode(errors="replace").strip()
        if out:
            print("=== stderr QEMU ===")
            print(out)


if __name__ == "__main__":
    sys.exit(main())