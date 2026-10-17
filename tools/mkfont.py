#!/usr/bin/env python3
"""Достраивает консольный шрифт `src/font8x16.bin` кириллицей.

Зачем
----
Ячейка текстового экрана - один байт, поэтому весь алфавит должен поместиться
в 256 глифов. Латиница (0x00..0x7F) уже на месте: это классический шрифт
VGA 8x16 (он же CP437), ради него шрифт и существует. Кириллица занимает
0x80..0xFF - то есть кодовая страница экрана становится CP1251:

    0x00..0x7F   латиница ASCII (из VGA-шрифта, не трогаем)
    0x80..0xA7   не занято (в CP1251 нет символов) - остаётся мусор CP437
    0xA8        Ё
    0xB8        ё
    0xC0..0xDF   А-Я
    0xE0..0xFF   а-я

Так делают все живые консоли: одна кодовая страница, один байт на символ,
никакой перекодировки в рантайме. Плата - рамки CP437 (0xB0..0xDF) больше
не рисуются; в системе они и не используются.

Глифы
-----
Русские буквы рисуются из `DejaVuSansMono.ttf` кеглем 13px с опорной линией
на строке 12 - x-height ложится на строки 5..11, как в VGA-шрифте, выносные
элементы (б, д, ф) идут в строки 2..4, а хвосты (у, ц, щ, р) - в 12..14.
Кегль и базовая линия подобраны так, чтобы строка кириллицы читалась как
буква; Latin из VGA-шрифта оставлен как был - он проверен временем, а
DejaVu в 13px выглядит заметно хуже.

Запуск (нужен Pillow, но только для пересборки - в образ шрифт попадает уже
скомпилированным в ядро):

    python3 tools/mkfont.py [путь-к-шрифту]

Скрипт идемпотентен: перезаписывает только слоты кириллицы, латиницу не
трогает, поэтому его можно гонять повторно.
"""

import sys
from PIL import Image, ImageDraw, ImageFont

# Кегль и базовая линия - см. модульный докстринг.
FONT_TTF = '/usr/share/fonts/TTF/DejaVuSansMono.ttf'
SIZE = 13
BASELINE = 12
THRESH = 128

CELL_W, CELL_H = 8, 16

# Слоты CP1251 -> символ.
CP1251_UPPER = 'АБВГДЕЖЗИЙКЛМНОПРСТУФХЦЧШЩЪЫЬЭЮЯ'   # 0xC0..0xDF
CP1251_LOWER = 'абвгдежзийклмнопрстуфхцчшщъыьэюя'   # 0xE0..0xFF
YO_UPPER = 0xA8    # Ё
YO_LOWER = 0xB8    # ё


def render(char, font):
    """Отрисовывает один символ в 8x16, возвращает 16 байт (1 бит = пиксель)."""
    img = Image.new('L', (CELL_W, CELL_H), 255)
    ImageDraw.Draw(img).text((0, BASELINE), char, font=font, fill=0, anchor='ls')
    px = img.load()
    return bytes(
        sum((1 << (7 - x)) for x in range(CELL_W) if px[x, y] < THRESH)
        for y in range(CELL_H)
    )


def show(glyph, char=''):
    return '\n'.join(
        f"{char} " if y == 0 else '  '
        + ''.join('#' if glyph[y] & (0x80 >> x) else '.' for x in range(CELL_W))
        for y in range(CELL_H)
    )


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else 'src/font8x16.bin'
    with open(path, 'rb') as f:
        font = bytearray(f.read())
    if len(font) != 256 * CELL_H:
        raise SystemExit(f'{path}: ждём {256 * CELL_H} байт, получили {len(font)}')

    ttf = ImageFont.truetype(FONT_TTF, SIZE)
    made = 0
    for i, ch in enumerate(CP1251_UPPER):
        font[(0xC0 + i) * CELL_H:(0xC0 + i + 1) * CELL_H] = render(ch, ttf)
        made += 1
    for i, ch in enumerate(CP1251_LOWER):
        font[(0xE0 + i) * CELL_H:(0xE0 + i + 1) * CELL_H] = render(ch, ttf)
        made += 1
    font[YO_UPPER * CELL_H:(YO_UPPER + 1) * CELL_H] = render('Ё', ttf)
    font[YO_LOWER * CELL_H:(YO_LOWER + 1) * CELL_H] = render('ё', ttf)
    made += 2

    with open(path, 'wb') as f:
        f.write(bytes(font))

    print(f'{path}: {len(font)} байт, {made} кириллических глифов '
          f'({SIZE}px, базовая {BASELINE})')
    for code in (0xC0, 0xA8, 0xFF, 0xE9):
        print(show(font[code * CELL_H:(code + 1) * CELL_H]))


if __name__ == '__main__':
    main()
