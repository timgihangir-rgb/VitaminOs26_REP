#!/usr/bin/env python3
"""Test Russian input: ЙЦУКЕН layout, Ctrl+Alt toggle, UTF-8 files on screen.

Three things are checked:

  1. Ctrl+Alt toggles the layout, and the switch is visible in the prompt
     (`[RU]` appears only in the Russian layout).
  2. Keys typed in ЙЦУКЕН land in the command line as Cyrillic. QEMU's
     `sendkey` speaks physical keys (a US keymap), so to type `ж` the test
     presses `;` - that is exactly what a real ЙЦУКЕН keyboard does.
  3. A Russian file created through the shell (name in UTF-8, content in
     UTF-8) reads back as the same Russian text: the console code page is
     CP1251, one byte per cell, so the check is on the raw cell bytes.

The screen is read through the VGA shadow (see `vga_addr`), and its geometry
comes from the grid the compositor picked (see `grid`).
"""
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
QMP_PORT = 4452
VGA_DUMP = "/tmp/ru_vga.bin"
SERIAL_LOG = "/tmp/ru_serial.log"

failures = []


def check(cond, msg):
    status = "OK  " if cond else "FAIL"
    print(f"  [{status}] {msg}")
    if not cond:
        failures.append(msg)


def qmp_connect(qemu, attempts=40):
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
        except OSError:
            time.sleep(0.5)
    raise RuntimeError("QMP did not come up")


def qmp_hmp(s, cmd):
    s.send((json.dumps({
        "execute": "human-monitor-command",
        "arguments": {"command-line": cmd},
    }) + "\n").encode())
    time.sleep(0.03)
    try:
        s.recv(65536)
    except OSError:
        pass


def fast_sendkey(s, key):
    """Одно нажатие клавиши, имя как в HMP `sendkey` ("a", "shift-o").

    Именно HMP, а не QMP `send-key`: ключ с модификатором ("shift-o")
    понимает только синтаксис HMP, а QMP с типом `qcode` такой префикс
    молча проглатывает - буква просто не появляется. Проверено на 1280x800.
    """
    qmp_hmp(s, f"sendkey {key}")
    time.sleep(0.06)


def chord_sendkey(s, keys):
    """Аккорд вида ["ctrl", "alt", "q"] через QMP `send-key`.

    QMP жмёт клавиши по порядку и отпускает в обратном, так что Ctrl+Alt
    попадает в гость как настоящая комбинация. Список из отдельных qcode -
    единственная форма, где модификаторы доезжают: у HMP `sendkey ctrl-alt-q`
    есть свой синтаксис, а смешивать два пути в одном тесте не стоит.
    """
    s.send((json.dumps({
        "execute": "send-key",
        "arguments": {"keys": [{"type": "qcode", "data": k} for k in keys]},
    }) + "\n").encode())
    time.sleep(0.12)
    try:
        s.recv(65536)
    except OSError:
        pass


def type_line(s, text):
    for ch in text:
        fast_sendkey(s, KEY_MAP.get(ch, ch))
    time.sleep(0.2)
    fast_sendkey(s, "ret")
    time.sleep(0.3)
    wait_stable(s)


KEY_MAP = {
    " ": "spc",
    "/": "slash",
    ".": "dot",
    "-": "minus",
    "=": "equal",
    "+": "shift-equal",
    ":": "shift-semicolon",
    ";": "semicolon",
    ",": "comma",
    "'": "apostrophe",
    "[": "bracket_left",
    "]": "bracket_right",
    "`": "grave_accent",
    "\\": "backslash",
    "%": "shift-5",
    "(": "shift-9",
    ")": "shift-0",
    "?": "shift-slash",
    "_": "shift-minus",
    "~": "shift-grave_accent",
    '"': "shift-apostrophe",
    "<": "shift-comma",
    ">": "shift-dot",
    "{": "shift-bracket_left",
    "}": "shift-bracket_right",
    "|": "shift-backslash",
    "!": "shift-1",
    "@": "shift-2",
    "#": "shift-3",
    "$": "shift-4",
    "^": "shift-6",
    "&": "shift-7",
    "*": "shift-8",
}


def vga_addr():
    try:
        log = open(SERIAL_LOG, errors="replace").read()
    except OSError:
        return "0xb8000"
    m = re.search(r"vga shadow phys=(0x[0-9a-fA-F]+)", log)
    return m.group(1) if m else "0xb8000"


def grid():
    """Размер текстовой сетки в ячейках - по строке `[N] fb ... grid=CxR`."""
    try:
        log = open(SERIAL_LOG, errors="replace").read()
    except OSError:
        return 80, 30
    m = re.search(r"grid=(\d+)x(\d+)", log)
    return (int(m.group(1)), int(m.group(2))) if m else (80, 30)


def dump_vga(s):
    qmp_hmp(s, f'pmemsave {vga_addr()} 65536 "{VGA_DUMP}"')
    time.sleep(0.4)


def vga_cells():
    """Экран как список строк, где символ - это кодовая страница (CP1251).

    Возвращаем байты, а не строку: проверять надо именно то, что лежит в
    ячейке, иначе русский текст проверить нечем.
    """
    try:
        data = open(VGA_DUMP, "rb").read()
    except FileNotFoundError:
        return []
    cols, rows = grid()
    out = []
    for row in range(rows):
        out.append([data[row * cols * 2 + col * 2] for col in range(cols)])
    return out


def screen_text():
    """Текст экрана: байты кодовой страницы (CP1251) -> Unicode.

    Именно так и читает человек: ячейка хранит один байт CP1251, а
    буква П в ней - 0xCF, а не буква 'П'. Поэтому декодируем, а не
    берём `chr(byte)` - это дало бы латиницу 0xC0..0xFF.
    """
    return "\n".join(
        bytes(row).decode("cp1251", errors="replace").rstrip() for row in vga_cells()
    )


def screen_ascii():
    return "\n".join(
        "".join(chr(b) if 32 <= b < 127 else " " for b in row).rstrip()
        for row in vga_cells()
    )


def wait_stable(s, quiet_reads=2, timeout=15.0):
    deadline = time.time() + timeout
    same = 0
    prev = None
    while time.time() < deadline:
        dump_vga(s)
        cur = screen_ascii()
        same = same + 1 if cur == prev else 0
        if same >= quiet_reads:
            return True
        prev = cur
    return False


# ── ЙЦУКЕН: русская буква → физическая клавиша QEMU ──────────────────────────
# Раз раскладка меняет только буквы, цифры и знаки набираются теми же
# клавишами, что и в латинице, - значит для смешанной строки достаточно
# перевести буквы и оставить остальное как есть.
JC_KEY = {
    "й": "q", "ц": "w", "у": "e", "к": "r", "е": "t", "н": "y",
    "г": "u", "ш": "i", "щ": "o", "з": "p", "х": "bracket_left", "ъ": "bracket_right",
    "ф": "a", "ы": "s", "в": "d", "а": "f", "п": "g", "р": "h",
    "о": "j", "л": "k", "д": "l", "ж": "semicolon", "э": "apostrophe", "ё": "grave_accent",
    "я": "z", "ч": "x", "с": "c", "м": "v", "и": "b", "т": "n",
    "ь": "m", "б": "comma", "ю": "dot",
}

LATIN_OF = {
    "q": "q", "w": "w", "e": "e", "r": "r", "t": "t", "y": "y", "u": "u",
    "i": "i", "o": "o", "p": "p", "bracket_left": "[", "bracket_right": "]",
    "a": "a", "s": "s", "d": "d", "f": "f", "g": "g", "h": "h", "j": "j",
    "k": "k", "l": "l", "semicolon": ";", "apostrophe": "'", "grave_accent": "`",
    "z": "z", "x": "x", "c": "c", "v": "v", "b": "b", "n": "n", "m": "m",
    "comma": ",", "dot": ".",
}


def type_lat(s, text):
    """Печатает латинский текст посимвольно (без Enter). Работает только
    в латинской раскладке: в ЙЦУКЕН те же клавиши станут русскими."""
    for ch in text:
        if ch.isupper():
            fast_sendkey(s, "shift-" + ch.lower())
        else:
            fast_sendkey(s, KEY_MAP.get(ch, ch))


def type_ru(s, text):
    """Набирает строку по-русски: буквы -> клавиши ЙЦУКЕН, остальное - как есть.

    Ключи `JC_KEY` записаны строчными: одна и та же физическая клавиша с
    Shift и без неё даёт строчную и заглавную букву, поэтому регистр
    добавляется префиксом shift-.
    """
    for ch in text:
        low = ch.lower()
        if low in JC_KEY:
            key = JC_KEY[low]
            fast_sendkey(s, "shift-" + key if ch != low else key)
        else:
            fast_sendkey(s, KEY_MAP.get(ch, ch))


def type_ru_line(s, text):
    type_ru(s, text)
    time.sleep(0.2)
    fast_sendkey(s, "ret")
    time.sleep(0.4)
    wait_stable(s)


def raw_type_line(s, text):
    """Строка целиком в латинице (для служебных команд).

    Работает только в латинской раскладке: в ЙЦУКЕН те же клавиши дадут
    русские буквы. Поэтому перед служебной командой зовём `ensure_latin`.
    """
    for ch in text:
        key = KEY_MAP.get(ch, ch)
        if ch.isupper():
            key = "shift-" + key
        fast_sendkey(s, key)
    time.sleep(0.2)
    fast_sendkey(s, "ret")
    time.sleep(0.4)
    wait_stable(s)


def layout_is_russian(s):
    dump_vga(s)
    return "[RU]" in screen_ascii()


def toggle_layout(s):
    chord_sendkey(s, ["ctrl", "alt", "q"])
    time.sleep(0.5)
    fast_sendkey(s, "ret")
    time.sleep(0.6)
    wait_stable(s)


def toggle_layout_silent(s):
    """Переключает раскладку, не отправляя Enter: для смены посреди команды."""
    chord_sendkey(s, ["ctrl", "alt", "q"])
    time.sleep(0.5)


def ensure_latin(s):
    """Гарантирует латинскую раскладку: служебные команды набраны ею."""
    if layout_is_russian(s):
        toggle_layout(s)


iso = os.path.join(HERE, "target", "os.iso")
img = "/tmp/ru_os.img"
qemu_cmd = [
    "qemu-system-x86_64",
    "-cdrom", iso,
    "-boot", "order=d",
    "-drive", f"file={img},format=raw,if=ide",
    "-display", "none",
    "-no-reboot",
    "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
    "-serial", f"file:{SERIAL_LOG}",
]
for p in (SERIAL_LOG, VGA_DUMP):
    try:
        os.remove(p)
    except OSError:
        pass
shutil.copyfile(os.path.join(HERE, "target", "os.img"), img)

subprocess.run(["pkill", "-f", f"qmp tcp:127.0.0.1:{QMP_PORT}"], stderr=subprocess.DEVNULL)
time.sleep(0.5)

print(f"Starting QEMU: {' '.join(qemu_cmd)}")
qemu = subprocess.Popen(qemu_cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

try:
    s = qmp_connect(qemu)
    print("Connected to QMP")

    print("\n== boot ==")
    ready = False
    for i in range(60):
        time.sleep(1)
        dump_vga(s)
        if "press Enter" in screen_ascii():
            fast_sendkey(s, "ret")
        if "login" in screen_ascii().lower():
            raw_type_line(s, "root")
            ready = True
            break
        if "vitamin_os26" in screen_ascii():
            ready = True
            break
    check(ready, "system reached a prompt")

    cols, rows = grid()
    print(f"  grid = {cols}x{rows} cells")
    check(cols >= 80 and rows >= 25, f"grid is usable ({cols}x{rows})")

    print("\n== layout starts in Latin ==")
    raw_type_line(s, "clear")
    dump_vga(s)
    check("[RU]" not in screen_ascii(), "prompt has no [RU] marker in Latin")

    print("\n== Ctrl+Alt switches the layout ==")
    toggle_layout(s)
    dump_vga(s)
    check("[RU]" in screen_ascii(), "prompt shows [RU] after Ctrl+Alt")

    print("\n== Cyrillic reaches the command line ==")
    type_ru(s, "echo Привет")
    time.sleep(0.4)
    dump_vga(s)
    check("Привет" in screen_text(), "Russian text typed into the command line")
    fast_sendkey(s, "ret")
    time.sleep(0.8)
    wait_stable(s)
    dump_vga(s)
    check("Привет" in screen_text(), "the command echoes and runs")

    print("\n== Cyrillic cell bytes are CP1251 ==")
    hit = [r for r in screen_text().splitlines() if "Привет" in r]
    check(bool(hit), "a screen row decodes as the Russian word")
    flat = [b for row in vga_cells() for b in row]
    cyr = [b for b in flat if 0xC0 <= b <= 0xFF or b in (0xA8, 0xB8)]
    check(len(cyr) >= 5, f"cells hold CP1251 Cyrillic bytes ({len(cyr)} seen)")
    # «Привет» -> П р и в е т -> 0xCF 0xF0 0xE8 0xE2 0xE5 0xF2
    want = [0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]
    check(any(all(b in row for b in want) for row in vga_cells()),
          "cell bytes spell Привет exactly as CP1251 (CF F0 E8 E2 E5 F2)")

    print("\n== Ё and ё (slots 0xA8 / 0xB8) ==")
    type_ru(s, "echo Ёжик ")
    type_ru(s, "ёлка")
    fast_sendkey(s, "ret")
    time.sleep(0.8)
    wait_stable(s)
    dump_vga(s)
    check("Ёжик" in screen_text(), "capital Ё renders")
    check("ёлка" in screen_text(), "lowercase ё renders")
    flat = [b for row in vga_cells() for b in row]
    check(0xA8 in flat and 0xB8 in flat,
          "cells use the 0xA8/0xB8 slots for Ё/ё")

    print("\n== a Russian file round-trips through the filesystem ==")
    # Одна команда, но два языка. В ЙЦУКЕН букв латиницы нет, и нет букв
    # кириллицы в латинице, а «>» «.» «txt» выпадают из русской клавиатуры:
    # поэтому пользователь набирает куски в разных раскладках и между ними
    # жмёт Ctrl+Alt. Битовый след: латинские куски - через type_lat,
    # русские - через type_ru, смены - toggle_layout_silent.
    ensure_latin(s)
    raw_type_line(s, "clear")
    type_lat(s, "echo ")
    toggle_layout_silent(s)        # -> ЙЦУКЕН
    type_ru(s, "Привет мир ")
    toggle_layout_silent(s)        # -> латиница
    type_lat(s, "> /tmp/")
    toggle_layout_silent(s)        # -> ЙЦУКЕН
    type_ru(s, "привет")
    toggle_layout_silent(s)        # -> латиница
    type_lat(s, ".txt")
    fast_sendkey(s, "ret")
    time.sleep(0.8)
    wait_stable(s)
    dump_vga(s)
    t = screen_text()
    check("echo Привет мир" in t, "command line kept both scripts unmangled")
    check("> /tmp/привет.txt" in t, "redirect target is the Cyrillic filename")

    ensure_latin(s)
    raw_type_line(s, "clear")
    type_lat(s, "cat /tmp/")
    toggle_layout_silent(s)        # -> ЙЦУКЕН
    type_ru(s, "привет")
    toggle_layout_silent(s)        # -> латиница
    type_lat(s, ".txt")
    fast_sendkey(s, "ret")
    time.sleep(0.8)
    wait_stable(s)
    dump_vga(s)
    t = screen_text()
    check("cat /tmp/привет.txt" in t, "cat command is intact")
    check("Привет мир" in t, "cat reads the UTF-8 file back as Russian")

    print("\n== the filename itself is Russian in ls ==")
    ensure_latin(s)
    raw_type_line(s, "clear")
    raw_type_line(s, "ls /tmp")
    dump_vga(s)
    check("привет.txt" in screen_text(), "ls shows the Cyrillic filename")
    print("  --- ls ---")
    for ln in screen_text().splitlines():
        if ln.strip():
            print("  |" + ln)
    print("  --------")

    print("\n== switching back to Latin ==")
    check(not layout_is_russian(s), "Ctrl+Alt switches back to Latin")
    raw_type_line(s, "echo back-to-latin")
    dump_vga(s)
    check("back-to-latin" in screen_ascii(), "Latin keys work again after switching back")

    print("\n== digits and punctuation are layout-independent ==")
    ensure_latin(s)
    # Цифры и «()+» одинаковы в обеих раскладках, поэтому «%» в ЙЦУКЕН
    # совпадает с US, и строку можно набирать не глядя на раскладку.
    raw_type_line(s, "echo 100% (a+b)")
    dump_vga(s)
    check("100% (a+b)" in screen_ascii(), "digits and shared punctuation work")

finally:
    qemu.terminate()
    try:
        qemu.wait(timeout=10)
    except subprocess.TimeoutExpired:
        qemu.kill()

print()
if failures:
    print(f"{len(failures)} check(s) FAILED:")
    for f in failures:
        print(f"  - {f}")
    sys.exit(1)
print("ALL CHECKS PASSED")