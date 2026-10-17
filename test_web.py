#!/usr/bin/env python3
"""E2E-тест мини-браузера VitaminOS (lynx-стиль): ссылки, навигация стрелками,
нумерованные ссылки, история, редиректы, chunked, goto.

Требует запущенный /tmp/opencode/web_test/srv.py на порту 8080.
"""
import json
import os
import shutil
import socket
import re
import subprocess
import sys
import time

QMP_PORT = 4451
VGA_DUMP = "/tmp/web_vga.bin"
HERE = os.path.dirname(os.path.abspath(__file__))

failures = []


def check(cond, msg):
    print(f"  [{'OK  ' if cond else 'FAIL'}] {msg}")
    if not cond:
        failures.append(msg)


def qmp_connect(qemu, attempts=30):
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


def qmp_hmp(s, cmd):
    s.send((json.dumps({"execute": "human-monitor-command",
                        "arguments": {"command-line": cmd}}) + "\n").encode())
    time.sleep(0.25)
    try:
        s.recv(65536)
    except Exception:
        pass


def sendkey(s, key, pause=0.06):
    qmp_hmp(s, f"sendkey {key}")
    time.sleep(pause)


KEY_MAP = {" ": "spc", "-": "minus", ".": "dot", "/": "slash",
           ":": "shift-semicolon", ">": "shift-dot", "_": "shift-minus",
           "=": "equal", "#": "shift-3"}


def type_line(s, text, settle=0.9):
    for ch in text:
        sendkey(s, KEY_MAP.get(ch, ch), 0.05)
    sendkey(s, "ret")
    time.sleep(settle)


SERIAL_LOG = "/tmp/web_serial.log"


# Физический адрес экранного буфера.
#
# Когда загрузчик включает графический режим, ядро подменяет 0xB8000
# RAM-тенью (paging::install_vga_shadow) и печатает её адрес в serial.
# pmemsave по 0xB8000 тогда читает видеопамять, а не текст, поэтому адрес
# берём из строки "[N] vga shadow phys=". Без неё остаётся 0xB8000 -
# верно для текстового режима, где тени нет.
def vga_addr():
    try:
        log = open(SERIAL_LOG, errors="replace").read()
    except OSError:
        return "0xb8000"
    m = re.search(r"vga shadow phys=(0x[0-9a-fA-F]+)", log)
    return m.group(1) if m else "0xb8000"


def grid():
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
        log = open(SERIAL_LOG, errors="replace").read()
    except OSError:
        return 80, 30
    m = re.search(r"grid=(\d+)x(\d+)", log)
    return (int(m.group(1)), int(m.group(2))) if m else (80, 30)


def dump_vga(s):
    qmp_hmp(s, f'pmemsave {vga_addr()} 65536 "{VGA_DUMP}"')
    time.sleep(0.4)


def vga_text():
    try:
        data = open(VGA_DUMP, "rb").read()
    except FileNotFoundError:
        return []
    out = []
    cols, rows_ = grid()
    for row in range(rows_):
        chars = []
        for col in range(cols):
            ch = data[row * cols * 2 + col * 2]
            chars.append(chr(ch) if 32 <= ch < 127 else " ")
        out.append("".join(chars))
    return out


def screen():
    return "\n".join(vga_text())


def status_line():
    """Строка состояния - последняя строка сетки (не 29-я: сетка по монитору)."""
    return vga_text()[-1]


def vga_attr(row, col):
    """Атрибут (цвет) клетки - для проверки color = off."""
    try:
        data = open(VGA_DUMP, "rb").read()
    except FileNotFoundError:
        return -1
    cols, _ = grid()
    return data[row * cols * 2 + col * 2 + 1]


def max_line_len(rows=None):
    """Самая длинная непустая строка из первых `rows` строк экрана.

    По умолчанию - весь документ, то есть все строки выше строки состояния."""
    if rows is None:
        rows = len(vga_text()) - 1
    lines = vga_text()[:rows]
    return max((len(l.rstrip()) for l in lines), default=0)


def doc_attrs(rows=None):
    """Множество атрибутов первых `rows` строк (документ)."""
    cols, all_rows = grid()
    if rows is None:
        rows = all_rows - 1
    return {vga_attr(r, c) for r in range(rows) for c in range(cols)}


def main():
    iso = os.path.join(HERE, "target", "os.iso")
    img = "/tmp/web_os.img"
    for p in (VGA_DUMP, "/tmp/web_serial.log"):
        try:
            os.remove(p)
        except OSError:
            pass
    shutil.copyfile(os.path.join(HERE, "target", "os.img"), img)

    cmd = [
        "qemu-system-x86_64",
        "-cdrom", iso,
        # Без явного порядка SeaBIOS сначала пробует HDD и флоппи: загрузка
        # занимает лишние 10-15 с, и тесты не дожидаются приглашения.
        "-boot", "order=d",
        "-drive", f"file={img},format=raw,if=ide",
        "-display", "none",
        "-no-reboot",
        "-netdev", "user,id=n0",
        "-device", "rtl8139,netdev=n0",
        "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
        "-serial", f"file:{SERIAL_LOG}",
    ]
    subprocess.run(["pkill", "-f", f"qmp tcp:127.0.0.1:{QMP_PORT}"],
                   stderr=subprocess.DEVNULL)
    time.sleep(0.5)
    print("Starting QEMU")
    qemu = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        s = qmp_connect(qemu)
        # Загрузочный экран держится до Enter: без нажатия приглашения не будет.
        booted = False
        for i in range(40):
            time.sleep(1)
            dump_vga(s)
            t = screen()
            if "press Enter" in t:
                sendkey(s, "ret")
                continue
            if "vitamin_os26" in t:
                booted = True
                print(f"  shell ready at t={i+1}s")
                break
        check(booted, "shell prompt appeared")
        if not booted:
            print(screen())
            return

        # ─── 1. Индексная страница ────────────────────────────────────────
        print("\n== index page ==")
        type_line(s, "run web 10.0.2.2 8080 /", settle=4.0)
        dump_vga(s)
        t = screen()
        check("VitaminOS test site" in t, "h1 rendered")
        check("This is the index page." in t, "paragraph text rendered")
        check("links" in t and "other pages" in t, "inline <i> text kept in paragraph")
        check("[1]" in t and "[2]" in t and "[3]" in t, "links numbered [1][2][3]")
        check("[4]" in t and "[5]" in t, "links numbered up to [5]")
        st = status_line()
        check("Ln 1/" in st, f"status shows line counter ({st.strip()!r})")
        check("Lnk" in st, "status shows link counter")
        check("Full test document" in t and "Relative link: page2" in t,
              "link labels rendered")

        # ─── 2. Переход по относительной ссылке ───────────────────────────
        print("\n== follow link 2 (relative) ==")
        sendkey(s, "2")
        time.sleep(0.3)
        sendkey(s, "ret")
        time.sleep(3.0)
        dump_vga(s)
        t = screen()
        check("Page 2" in t, "page2 loaded via relative href")
        check("Reached by a relative link" in t, "page2 body rendered")
        check("home" in t and "one level" in t, "page2 links rendered")

        # ─── 3. Назад по истории ─────────────────────────────────────────
        print("\n== back (h) ==")
        sendkey(s, "h")
        time.sleep(3.0)
        dump_vga(s)
        check("VitaminOS test site" in screen(), "history returned to index")

        # ─── 4. Вложенный абсолютный путь ───────────────────────────────
        print("\n== link 3: absolute deep path ==")
        sendkey(s, "3")
        time.sleep(0.3)
        sendkey(s, "ret")
        time.sleep(3.0)
        dump_vga(s)
        t = screen()
        check("Deep page 3" in t, "deep page loaded")

        # ─── 5. Редирект 301 ─────────────────────────────────────────────
        print("\n== link 4: 301 redirect ==")
        sendkey(s, "h")
        time.sleep(2.5)
        sendkey(s, "4")
        time.sleep(0.3)
        sendkey(s, "ret")
        time.sleep(3.5)
        dump_vga(s)
        t = screen()
        check("VitaminOS test site" in t, "redirect followed to target page")

        # ─── 6. Chunked transfer-encoding ────────────────────────────────
        print("\n== link 5: chunked ==")
        sendkey(s, "5")
        time.sleep(0.3)
        sendkey(s, "ret")
        time.sleep(3.0)
        dump_vga(s)
        t = screen()
        check("Chunked page" in t, "chunked response dechunked")
        check("dechunked" in t, "chunked body complete (no truncation)")

        # ─── 7. Навигация стрелками/TAB и скролл ─────────────────────────
        print("\n== arrow / tab navigation ==")
        sendkey(s, "h")
        time.sleep(2.5)
        sendkey(s, "h")
        time.sleep(2.5)
        sendkey(s, "h")
        time.sleep(2.5)
        dump_vga(s)
        check("VitaminOS test site" in screen(), "back at index for nav tests")
        sendkey(s, "tab")
        time.sleep(0.5)
        dump_vga(s)
        st = status_line()
        check("Lnk 1/" in st, f"TAB selects first link ({st.strip()!r})")
        sendkey(s, "tab")
        time.sleep(0.5)
        dump_vga(s)
        st = status_line()
        check("Lnk 2/" in st, f"TAB advances to second link ({st.strip()!r})")
        sendkey(s, "left")
        time.sleep(0.5)
        dump_vga(s)
        check("Lnk 1/" in status_line(), "LEFT goes back to previous link")
        # Enter с курсором на ссылке 1 открывает её (LEFT вернул курсор на [1]).
        sendkey(s, "ret")
        time.sleep(3.0)
        dump_vga(s)
        check("Test Document" in screen(), "Enter follows the cursor link")

        # ─── 8. Скролл стрелками на длинном документе ────────────────────
        print("\n== scrolling / goto long document ==")
        sendkey(s, "h")
        time.sleep(2.5)
        sendkey(s, "g")
        time.sleep(0.6)
        for ch in "10.0.2.2:8080/test.html":
            sendkey(s, KEY_MAP.get(ch, ch), 0.05)
        sendkey(s, "ret")
        time.sleep(3.5)
        dump_vga(s)
        t = screen()
        st = status_line()
        check("Document" in t or "Paragraph" in t, "test document loaded via goto")
        top0 = st
        sendkey(s, "down")
        time.sleep(0.4)
        sendkey(s, "down")
        time.sleep(0.4)
        sendkey(s, "down")
        time.sleep(0.5)
        dump_vga(s)
        st = status_line()
        check(st != top0, f"arrow down scrolls (Ln changed: {st.strip()!r})")
        sendkey(s, "pgdn")
        time.sleep(0.6)
        dump_vga(s)
        st2 = status_line()
        check(st2 != st, f"PageDown scrolls further ({st2.strip()!r})")
        sendkey(s, "home")
        time.sleep(0.5)
        dump_vga(s)
        check("Ln 1/" in status_line(), "HOME returns to top")

        # ─── 9. Выход по q ───────────────────────────────────────────────
        print("\n== quit ==")
        sendkey(s, "q")
        time.sleep(1.5)
        type_line(s, "echo back-in-shell")
        dump_vga(s)
        check("back-in-shell" in screen(), "q returns control to the shell")

        # ─── 10. Конфиг /etc/web.conf ────────────────────────────────────
        print("\n== /etc/web.conf ==")
        type_line(s, "cat /etc/web.conf", settle=1.5)
        dump_vga(s)
        t = screen()
        check("start_url" in t, "web.conf documents start_url")
        check("user_agent" in t and "VitaminOS-web/1.0" in t,
              "web.conf ships the default user_agent")
        check("key_quit" in t and "show_headers" in t,
              "web.conf lists key_quit and show_headers")

        # ─── 11. Справка: web --help и web без аргументов ─────────────────
        print("\n== web --help ==")
        type_line(s, "web --help", settle=2.0)
        dump_vga(s)
        t = screen()
        check("usage: web" in t, "--help prints the usage line")
        check("-c, --config PATH" in t, "--help documents -c/--config")
        check("start_url" in t and "max_download" in t,
              "--help lists the config keys")
        check("key_quit / key_back / key_goto" in t,
              "--help lists the key bindings")

        print("\n== web with no arguments (empty start_url) ==")
        type_line(s, "web", settle=2.0)
        dump_vga(s)
        t = screen()
        check("usage: web" in t,
              "no args + empty start_url falls back to the help text")
        type_line(s, "echo help-is-not-a-browser", settle=1.0)
        dump_vga(s)
        check("help-is-not-a-browser" in screen(),
              "web without a URL returns to the shell")

        # ─── 12. Свой конфиг: start_url + user_agent + show_headers ───────
        print("\n== custom config: start_url, user_agent, show_headers ==")
        type_line(s, "echo start_url = http://10.0.2.2:8080/echo > /tmp/w.conf",
                  settle=1.2)
        type_line(s, "echo user_agent = VitaminOS-CFG/9.9 >> /tmp/w.conf",
                  settle=1.2)
        type_line(s, "echo show_headers = on >> /tmp/w.conf", settle=1.2)
        type_line(s, "cat /tmp/w.conf", settle=1.2)
        dump_vga(s)
        check("start_url = http://10.0.2.2:8080/echo" in screen(),
              "config file written through shell redirects")
        type_line(s, "web -c /tmp/w.conf", settle=5.0)
        dump_vga(s)
        t = screen()
        check("Echo" in t, "start_url from the config was opened")
        check("VitaminOS-CFG/9.9" in t,
              "user_agent from the config reached the server")
        check("HTTP/1.0 200 OK" in t,
              "show_headers = on prints the response status line")
        check("Content-Type" in t, "show_headers = on prints response headers")
        sendkey(s, "q")
        time.sleep(1.5)
        type_line(s, "echo cfg1-done", settle=1.0)
        dump_vga(s)
        check("cfg1-done" in screen(), "q quits after a config-driven load")

        # ─── 13. link_numbers = off и status_bar = off ────────────────────
        print("\n== config: link_numbers off, status_bar off ==")
        type_line(s, "echo start_url = http://10.0.2.2:8080/ > /tmp/w2.conf",
                  settle=1.2)
        type_line(s, "echo link_numbers = off >> /tmp/w2.conf", settle=1.2)
        type_line(s, "echo status_bar = off >> /tmp/w2.conf", settle=1.2)
        type_line(s, "web -c /tmp/w2.conf", settle=5.0)
        dump_vga(s)
        t = screen()
        check("VitaminOS test site" in t, "index page loaded with status_bar off")
        check("[1]" not in t, "link_numbers = off removes the [N] markers")
        check("Lnk" not in status_line(),
              "status_bar = off leaves the last row to the document")
        check("Page 2 relative" in t,
              "link labels still render without the [N] suffix")
        sendkey(s, "q")
        time.sleep(1.5)
        type_line(s, "echo cfg2-done", settle=1.0)
        dump_vga(s)
        check("cfg2-done" in screen(), "status_bar = off still allows quitting")

        # ─── 14. width из конфига ─────────────────────────────────────────
        print("\n== config: width = 50 ==")
        type_line(s, "echo start_url = http://10.0.2.2:8080/test.html > /tmp/w3.conf",
                  settle=1.2)
        type_line(s, "echo width = 50 >> /tmp/w3.conf", settle=1.2)
        type_line(s, "web -c /tmp/w3.conf", settle=6.0)
        dump_vga(s)
        # Подпись [NN] вставляется в строку текста и сдвигает хвост вправо,
        # поэтому допускаем width + 6, а не ровно width.
        w = max_line_len()
        check(w <= 56, f"width = 50 wraps the text (longest row {w})")
        sendkey(s, "q")
        time.sleep(1.5)

        # ─── 15. default_port = 8080: адрес без порта ─────────────────────
        print("\n== config: default_port = 8080 ==")
        type_line(s, "echo default_port = 8080 > /tmp/w4.conf", settle=1.2)
        type_line(s, "web -c /tmp/w4.conf 10.0.2.2", settle=5.0)
        dump_vga(s)
        check("VitaminOS test site" in screen(),
              "default_port lets a bare host name find port 8080")
        sendkey(s, "q")
        time.sleep(1.5)

        # ─── 16. key_quit = x ─────────────────────────────────────────────
        print("\n== config: key_quit = x ==")
        type_line(s, "echo start_url = http://10.0.2.2:8080/ > /tmp/w5.conf",
                  settle=1.2)
        type_line(s, "echo key_quit = x >> /tmp/w5.conf", settle=1.2)
        type_line(s, "web -c /tmp/w5.conf", settle=5.0)
        dump_vga(s)
        check("VitaminOS test site" in screen(), "page loaded for the key test")
        sendkey(s, "q")
        time.sleep(1.5)
        dump_vga(s)
        check("VitaminOS test site" in screen(),
              "q no longer quits when key_quit = x")
        sendkey(s, "x")
        time.sleep(1.5)
        type_line(s, "echo cfg5-done", settle=1.0)
        dump_vga(s)
        check("cfg5-done" in screen(), "x quits when key_quit = x")

        # ─── 17. color = off: весь экран одним атрибутом ──────────────────
        print("\n== config: color = off ==")
        type_line(s, "echo start_url = http://10.0.2.2:8080/ > /tmp/w6.conf",
                  settle=1.2)
        type_line(s, "echo color = off >> /tmp/w6.conf", settle=1.2)
        type_line(s, "web -c /tmp/w6.conf", settle=5.0)
        dump_vga(s)
        t = screen()
        check("VitaminOS test site" in t, "index page loaded for the color test")
        attrs = doc_attrs()
        check(attrs <= {0x07},
              f"color = off paints the document grey (attrs {sorted(attrs)})")
        sendkey(s, "q")
        time.sleep(1.5)

        # ─── 18. Неизвестный ключ не ломает запуск ────────────────────────
        print("\n== unknown config key is ignored ==")
        type_line(s, "echo start_url = http://10.0.2.2:8080/ > /tmp/w7.conf",
                  settle=1.2)
        type_line(s, "echo width = banana >> /tmp/w7.conf", settle=1.2)
        type_line(s, "echo no_such_key = 1 >> /tmp/w7.conf", settle=1.2)
        type_line(s, "web -c /tmp/w7.conf", settle=5.0)
        dump_vga(s)
        check("VitaminOS test site" in screen(),
              "garbage value and unknown key do not break web")
        sendkey(s, "q")
        time.sleep(1.5)

        # ─── 19. Отсутствующий конфиг: заводские значения ─────────────────
        print("\n== missing config file falls back to defaults ==")
        type_line(s, "web -c /tmp/no-such-file.conf 10.0.2.2 8080 /", settle=5.0)
        dump_vga(s)
        check("VitaminOS test site" in screen(),
              "missing config does not stop web from loading a page")
        sendkey(s, "q")
        time.sleep(1.5)
        type_line(s, "echo all-config-tests-done", settle=1.2)
        dump_vga(s)
        check("all-config-tests-done" in screen(),
              "shell alive after the whole config suite")
        s.close()
    except Exception as e:
        import traceback
        traceback.print_exc()
        failures.append(f"harness error: {e}")
    finally:
        qemu.terminate()
        time.sleep(1)
        if qemu.poll() is None:
            qemu.kill()
        qemu.wait()

    print()
    if failures:
        print(f"WEB FAIL — {len(failures)} check(s) failed:")
        for f in failures:
            print("  -", f)
        sys.exit(1)
    print("WEB OK — all checks passed")


if __name__ == "__main__":
    main()
