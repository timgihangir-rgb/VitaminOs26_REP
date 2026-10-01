#!/usr/bin/env python3
"""E2E-тест мини-браузера VitaminOS (lynx-стиль): ссылки, навигация стрелками,
нумерованные ссылки, история, редиректы, chunked, goto.

Требует запущенный /tmp/opencode/web_test/srv.py на порту 8080.
"""
import json
import os
import shutil
import socket
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


KEY_MAP = {" ": "spc", "-": "minus", ".": "dot", "/": "slash", ":": "shift-semicolon"}


def type_line(s, text, settle=0.9):
    for ch in text:
        sendkey(s, KEY_MAP.get(ch, ch), 0.05)
    sendkey(s, "ret")
    time.sleep(settle)


def dump_vga(s):
    qmp_hmp(s, f'pmemsave 0xb8000 9600 "{VGA_DUMP}"')
    time.sleep(0.4)


def vga_text():
    try:
        data = open(VGA_DUMP, "rb").read()
    except FileNotFoundError:
        return []
    out = []
    for row in range(30):
        chars = []
        for col in range(80):
            ch = data[row * 160 + col * 2]
            chars.append(chr(ch) if 32 <= ch < 127 else " ")
        out.append("".join(chars))
    return out


def screen():
    return "\n".join(vga_text())


def status_line():
    return vga_text()[29]


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
        "-drive", f"file={img},format=raw,if=ide",
        "-display", "none",
        "-no-reboot",
        "-netdev", "user,id=n0",
        "-device", "rtl8139,netdev=n0",
        "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
        "-serial", "file:/tmp/web_serial.log",
    ]
    subprocess.run(["pkill", "-f", f"qmp tcp:127.0.0.1:{QMP_PORT}"],
                   stderr=subprocess.DEVNULL)
    time.sleep(0.5)
    print("Starting QEMU")
    qemu = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        s = qmp_connect(qemu)
        booted = False
        for i in range(20):
            time.sleep(1)
            dump_vga(s)
            if "press Enter to continue" in screen():
                booted = True
                print(f"  boot splash at t={i+1}s")
                break
        check(booted, "boot splash appeared")
        if not booted:
            return
        sendkey(s, "ret")
        for i in range(20):
            time.sleep(1)
            dump_vga(s)
            if "vitamin_os26" in screen():
                print(f"  shell ready at t={i+1}s after splash")
                break
        else:
            check(False, "shell prompt appeared")
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
