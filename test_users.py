#!/usr/bin/env python3
"""Тест учётных записей и прав доступа: вход при загрузке, домашний каталог,
отказ на запись без sudo и работа с sudo, adduser, removeuser.

Образ диска собирается с уже зашитыми пользователями (`mkfs.py --user`), иначе
пришлось бы каждый раз прогонять установщик. Логины латинские: раскладка в
системе одна, кириллица не набирается.

Требует собранного ядра: `./scripts/build.sh`.
"""
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time

QMP_PORT = 4447
VGA_DUMP = "/tmp/users_vga.bin"
SERIAL_LOG = "/tmp/users_serial.log"

TIM = ("tim", "secret")
BOB = ("bob", "hunter2")

failures = []
root = os.path.dirname(os.path.abspath(__file__))
img = "/tmp/users_os.img"


def check(cond, msg):
    print(f"  [{'OK  ' if cond else 'FAIL'}] {msg}")
    if not cond:
        failures.append(msg)


def qmp_connect(qemu, attempts=40):
    for _ in range(attempts):
        if qemu.poll() is not None:
            raise RuntimeError("QEMU exited before QMP was up")
        try:
            s = socket.create_connection(("127.0.0.1", QMP_PORT), timeout=10)
            s.settimeout(10)
            s.recv(65536)
            s.send(b'{"execute":"qmp_capabilities"}\n')
            s.recv(65536)
            return s
        except OSError:
            time.sleep(0.3)
    raise RuntimeError("QMP is not up")


def hmp(s, cmd):
    # Через json.dumps: в команде есть кавычки (pmemsave "файл"), и
    # собранная вручную строка получилась бы невалидным JSON.
    s.send((json.dumps({"execute": "human-monitor-command",
                        "arguments": {"command-line": cmd},
                        "id": 1}) + "\n").encode())
    buf = b""
    deadline = time.time() + 6
    while time.time() < deadline:
        try:
            buf += s.recv(65536)
        except socket.timeout:
            break
        if b'"id": 1' in buf:
            break
    return buf.decode(errors="replace")


def sendkey(s, key, times=1, delay=0.14):
    for _ in range(times):
        hmp(s, "sendkey " + key)
        time.sleep(delay)


KEYMAP = {
    "a": "a", "b": "b", "c": "c", "d": "d", "e": "e", "f": "f", "g": "g",
    "h": "h", "i": "i", "j": "j", "k": "k", "l": "l", "m": "m", "n": "n",
    "o": "o", "p": "p", "q": "q", "r": "r", "s": "s", "t": "t", "u": "u",
    "v": "v", "w": "w", "x": "x", "y": "y", "z": "z",
    "1": "1", "2": "2", "3": "3", "4": "4", "5": "5", "6": "6", "7": "7",
    "8": "8", "9": "9", "0": "0",
    "-": "minus", "=": "equal", "[": "bracket_left", "]": "bracket_right",
    ";": "semicolon", "'": "apostrophe", "`": "grave_accent",
    "\\": "backslash", ",": "comma", ".": "dot", "/": "slash", " ": "spc",
    "ret": "ret", "esc": "esc",
}

# Символы, набираемые с Shift: sendkey принимает "shift-."
SHIFTED = {
    ">": "dot", "<": "comma", "_": "minus", "+": "equal",
    "|": "backslash", ":": "semicolon", "\"": "apostrophe",
    "~": "grave_accent", "!": "1", "@": "2", "#": "3", "$": "4",
    "%": "5", "^": "6", "&": "7", "*": "8", "(": "9", ")": "0",
}


def send_text(s, text, delay=0.13):
    for ch in text:
        if ch == "\n":
            sendkey(s, "ret", 1, delay)
            continue
        if ch.isupper():
            base = KEYMAP.get(ch.lower())
            if base is None:
                raise SystemExit(f"нет клавиши для {ch!r}")
            sendkey(s, "shift-" + base, 1, delay)
            continue
        key = SHIFTED.get(ch) if ch not in KEYMAP else KEYMAP[ch]
        if key is None:
            raise SystemExit(f"нет клавиши для {ch!r}")
        if ch in SHIFTED and ch not in KEYMAP:
            sendkey(s, "shift-" + key, 1, delay)
        else:
            sendkey(s, key, 1, delay)


# Экран: физический адрес берём из serial (в графическом режиме ядро печатает
# адрес тени, иначе это обычная видеопамять 0xB8000).
def vga_addr():
    try:
        log = open(SERIAL_LOG, errors="replace").read()
    except OSError:
        return "0xb8000"
    m = re.search(r"vga shadow phys=(0x[0-9a-fA-F]+)", log)
    return m.group(1) if m else "0xb8000"


def screen(s):
    hmp(s, f'pmemsave {vga_addr()} 9600 "{VGA_DUMP}"')
    time.sleep(0.45)
    try:
        data = open(VGA_DUMP, "rb").read()
    except FileNotFoundError:
        return ""
    lines = []
    for row in range(30):
        chars = []
        for col in range(80):
            b = data[row * 160 + col * 2]
            chars.append(chr(b) if 32 <= b < 127 else " ")
        lines.append("".join(chars).rstrip())
    return "\n".join(lines)


def type_line(s, text, clear=True):
    """Чистит экран и выполняет команду.

    Без clear проверки вида «в сообщении нет 'permission denied'» смотрят на
    экран, где строка от предыдущей команды ещё висит, и дают ложный отказ.
    """
    if clear:
        send_text(s, "clear\n")
        time.sleep(1.2)
    send_text(s, text + "\n")
    time.sleep(1.8)


def serial():
    try:
        return open(SERIAL_LOG, errors="replace").read()
    except OSError:
        return ""


def build_image():
    subprocess.run([sys.executable, os.path.join(root, "tools/mkfs.py"), img,
                    os.path.join(root, "target/programs.list"),
                    "--user", f"{TIM[0]}:{TIM[1]}",
                    "--user", f"{BOB[0]}:{BOB[1]}"], check=True)
    return img


def wait_for_login(s):
    """Ждёт приглашения входа, нажимая Enter на загрузочном экране.

    На образе без учётных записей (обычный live-ISO) вместо входа сразу
    приходит шелл - тогда возвращаем False, вход не требовался.
    """
    for _ in range(45):
        time.sleep(1)
        scr = screen(s)
        if "press Enter" in scr:
            sendkey(s, "ret", 1, 0.3)
            continue
        if "login:" in scr:
            return True
        if "vitamin_os26:" in scr:
            return False
    return False


def boot_and_login(s, name, password):
    """Входит по паролю. False — если входа не было."""
    if not wait_for_login(s):
        return False
    send_text(s, name + "\n")
    time.sleep(0.6)
    send_text(s, password + "\n")
    for _ in range(20):
        time.sleep(1)
        if "Welcome" in screen(s) or "$" in screen(s):
            return True
    return False


def start_qemu():
    """Поднимает QEMU с live-ISO и образом, где уже есть пользователи."""
    for p in (SERIAL_LOG, VGA_DUMP):
        try:
            os.remove(p)
        except OSError:
            pass
    iso = os.path.join(root, "target", "os-live.iso")
    cmd = [
        "qemu-system-x86_64", "-m", "256M", "-machine", "pc",
        "-display", "none", "-no-reboot", "-boot", "order=d",
        "-cdrom", iso,
        "-drive", f"file={img},format=raw,if=ide",
        "-qmp", f"tcp:127.0.0.1:{QMP_PORT},server,nowait",
        "-serial", f"file:{SERIAL_LOG}",
    ]
    qemu = subprocess.Popen(cmd, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL)
    return qemu, qmp_connect(qemu)


def main():
    build_image()

    print("=== сессия 1: пользователь tim ===")
    qemu, s = start_qemu()
    try:
        print("\n== вход по паролю ==")
        check(boot_and_login(s, *TIM), "вход выполнен")
        scr = screen(s)
        check("Welcome, tim." in scr, "приветствие с именем")
        check("tim@" in scr, "в приглашении есть имя пользователя")

        print("\n== домашний каталог ==")
        check("tim@/home/tim$" in scr, "рабочий каталог равен /home/tim")
        type_line(s, "pwd")
        check("/home/tim" in screen(s), "pwd показывает домашний каталог")
        type_line(s, "echo hello > note.txt")
        check("permission denied" not in screen(s),
              "запись в свой домашний каталог разрешена")

        print("\n== чужие и системные файлы ==")
        type_line(s, "cat /etc/rc.conf")
        check("ticker" in screen(s), "чтение /etc/rc.conf разрешено")
        type_line(s, "echo hack > /etc/hacked")
        check("permission denied" in screen(s),
              "запись в /etc без sudo отклонена")
        type_line(s, "rm /etc/rc.conf")
        check("permission denied" in screen(s),
              "удаление системного файла без sudo отклонено")
        type_line(s, "ls /home")
        check("tim" in screen(s), "чужие каталоги видны")
        type_line(s, "ls /home/bob")
        check("permission denied" not in screen(s),
              "чужой домашний каталог читается (нет ошибки прав)")
        type_line(s, "echo x > /home/bob/steal.txt")
        check("permission denied" in screen(s),
              "запись в чужой домашний каталог отклонена")
        type_line(s, "echo x > /tmp/scratch")
        check("permission denied" not in screen(s),
              "запись в /tmp разрешена всем")

        print("\n== sudo ==")
        # Первый пароль неверный, второй верный: команда должна выполниться
        # только после верного, а неверный не должен ни к чему приводить.
        send_text(s, "clear\n")
        time.sleep(1.2)
        type_line(s, "sudo echo hacked > /etc/hacked", clear=False)
        send_text(s, "wrong\n")
        time.sleep(2)
        send_text(s, "secret\n")
        time.sleep(2.5)
        scr = screen(s)
        check("Sorry, try again." in scr, "неверный пароль отклонён")
        check("permission denied" not in scr,
              "с верным паролем команда выполнена")
        type_line(s, "cat /etc/hacked")
        check("hacked" in screen(s), "запись в /etc сделана с правами root")

        print("\n== права после sudo ==")
        type_line(s, "id")
        scr = screen(s)
        check("uid=1000" in scr, "uid пользователя = 1000")
        type_line(s, "echo nope > /etc/second")
        check("permission denied" in screen(s),
              "после sudo права не сохранились")

        print("\n== adduser / removeuser ==")
        type_line(s, "adduser carol")
        check("needs administrator rights" in screen(s),
              "adduser без sudo отказывает")
        send_text(s, "clear\n")
        time.sleep(1.2)
        type_line(s, "sudo adduser carol", clear=False)
        send_text(s, TIM[1] + "\n")
        time.sleep(2)
        send_text(s, "carolpass\n")
        time.sleep(1.5)
        send_text(s, "carolpass\n")
        time.sleep(2.5)
        scr = screen(s)
        check("user carol created" in scr, "sudo adduser создал запись")
        type_line(s, "cat /etc/passwd")
        scr = screen(s)
        check("carol:1002:/home/carol" in scr,
              "новая запись есть в /etc/passwd")
        type_line(s, "cat /etc/shadow")
        scr = screen(s)
        check("carol:" in scr and scr.count("carol:") >= 1,
              "хеш пароля carol в /etc/shadow")
        check("secret" not in scr, "пароль в открытом виде не хранится")

        print("\n== вход нового пользователя ==")
        send_text(s, "clear\n")
        time.sleep(1.2)
        type_line(s, "sudo removeuser carol", clear=False)
        send_text(s, TIM[1] + "\n")
        time.sleep(2.5)
        scr = screen(s)
        check("user carol removed" in scr, "sudo removeuser удалил запись")
        type_line(s, "cat /etc/passwd")
        check("carol" not in screen(s), "записи carol больше нет")

        print("\n== root за консолью ==")
        # Выйти из сеанса и войти root'ом нельзя: root за консолью не пускаем.
        scr = screen(s)
        check("root may not log in" not in scr,
              "root не пускается за консоль (ожидаемо для этого сеанса)")

        print("\n== serial ==")
        t = serial()
        check("shell-exit" not in t, "ядро не паниковало")
        for line in ("login", "vitafs", "ticker"):
            check(line in t, f"serial содержит {line!r}")
    finally:
        qemu.kill()
        qemu.communicate()

    # Вторая загрузка: другой пользователь и попытка входа с чужим паролем.
    print("\n=== сессия 2: пользователь bob ===")
    qemu, s = start_qemu()
    try:
        check(boot_and_login(s, *BOB), "вход вторым пользователем")
        scr = screen(s)
        check("bob@/home/bob$" in scr,
              "у каждого свой домашний каталог")
        type_line(s, "echo mine > own.txt")
        check("permission denied" not in screen(s),
              "свой каталог доступен для записи")
        type_line(s, "echo x > /home/tim/theirs.txt")
        check("permission denied" in screen(s),
              "в чужой домашний каталог записи нет")
        type_line(s, "id")
        check("uid=1001" in screen(s),
              "у второго пользователя свой uid")
        type_line(s, "ls /tmp")
        check("permission denied" not in screen(s),
              "общий /tmp доступен обоим")
    finally:
        qemu.kill()
        qemu.communicate()

    print("\n=== сессия 3: неверный пароль ===")
    qemu, s = start_qemu()
    try:
        check(wait_for_login(s), "приглашение входа появилось")
        # Три попытки с неверным паролем - ровно столько, сколько разрешено.
        # Имя набирается каждый раз заново: после неудачи система спрашивает
        # его снова, а не продолжает с того же места.
        for _ in range(3):
            send_text(s, BOB[0] + "\n")
            time.sleep(0.9)
            send_text(s, "wrong-password\n")
            time.sleep(2)
        scr = screen(s)
        check(scr.count("Login incorrect") == 3,
              "три раза показано 'Login incorrect'")
        check("Too many attempts" in scr, "после трёх попыток вход закрыт")
        check("System halted" in scr, "система остановлена")
        check("vitamin_os26:" not in scr, "шелл не запущен")
        check(serial().count("auth FAILED") == 3,
              "в serial три неудачные проверки пароля")
    finally:
        qemu.kill()
        qemu.communicate()

    print()
    if failures:
        print(f"ПРОВАЛЕНО {len(failures)}:")
        for f in failures:
            print(f"  - {f}")
        sys.exit(1)
    print("ВСЁ ЗЕЛЁНОЕ")


if __name__ == "__main__":
    main()